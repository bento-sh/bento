//! `bento why` data layer.
//!
//! Looks up a cache entry by either `<dish>:<task>` (resolved via a
//! plan pass) or a cache-key hex prefix, and returns the stored
//! [`InputManifest`] + key for each match. Pure read; no mutation, no
//! network. Shared by the `bento` CLI (via `bento why`) and by
//! `bento-mcp` (via the `bento_why` tool).
//!
//! Two input forms:
//!   - `<dish>:<task>` — resolved via [`resolve_dish_task_key`]; misspellings
//!     hit [`WhyTargetError::DishNotFound`] / [`WhyTargetError::TaskNotFound`]
//!     with next_steps enumerating the available names.
//!   - `<hex-prefix>` — any prefix of a cache key, passed straight to
//!     [`explain`].

use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use bento_cache::{InputManifest, LocalCache};
use bento_config::Workspace;

use crate::{plan_at, PlanOptions, TaskStatus};

#[derive(Debug, Serialize, JsonSchema)]
pub struct Explanation {
    pub key: String,
    pub manifest: Option<InputManifest>,
}

/// Classified failures when resolving a `bento why` target. Downcast
/// through the CLI's error classifier so each variant becomes a
/// distinct `kind` in the structured envelope.
#[derive(Debug, thiserror::Error)]
pub enum WhyTargetError {
    #[error("invalid target '{input}' — must be `<dish>:<task>` or a cache-key hex prefix")]
    InvalidDishTask { input: String },

    #[error("no dish named '{dish}' in this workspace")]
    DishNotFound {
        dish: String,
        available: Vec<String>,
    },

    #[error("dish '{dish}' has no task named '{task}'")]
    TaskNotFound {
        dish: String,
        task: String,
        available: Vec<String>,
    },

    #[error("no cache entry for {dish}:{task} yet (key {key})")]
    NoCacheEntry {
        dish: String,
        task: String,
        key: String,
    },
}

/// Look up a single cache key by `dish:task`. Runs a plan pass to get
/// the key — cheap because planning is read-only (no adapter execution).
pub fn resolve_dish_task_key(workspace_root: &Path, target: &str) -> anyhow::Result<String> {
    let (dish_name, task_name) =
        target
            .split_once(':')
            .ok_or_else(|| WhyTargetError::InvalidDishTask {
                input: target.to_string(),
            })?;

    let workspace = Workspace::load(workspace_root)?;
    let available_dishes: Vec<String> = workspace.dishes_by_name.keys().cloned().collect();
    if !workspace.dishes_by_name.contains_key(dish_name) {
        return Err(WhyTargetError::DishNotFound {
            dish: dish_name.to_string(),
            available: available_dishes,
        }
        .into());
    }

    let plan = plan_at(
        workspace_root,
        &PlanOptions {
            dish_filter: Some(dish_name.to_string()),
            ..Default::default()
        },
    )?;

    for bento in &plan.bentos {
        for dish in &bento.dishes {
            if dish.name != dish_name {
                continue;
            }
            if let Some(task) = dish.tasks.iter().find(|t| t.name == task_name) {
                // `NoAdapter` stubs have no real key — they shouldn't reach
                // the `bento why` flow.
                if matches!(task.status, TaskStatus::NoAdapter) {
                    return Err(WhyTargetError::TaskNotFound {
                        dish: dish_name.to_string(),
                        task: task_name.to_string(),
                        available: dish
                            .tasks
                            .iter()
                            .filter(|t| !matches!(t.status, TaskStatus::NoAdapter))
                            .map(|t| t.name.clone())
                            .collect(),
                    }
                    .into());
                }
                return Ok(task.key.clone());
            }
            return Err(WhyTargetError::TaskNotFound {
                dish: dish_name.to_string(),
                task: task_name.to_string(),
                available: dish.tasks.iter().map(|t| t.name.clone()).collect(),
            }
            .into());
        }
    }
    // Dish exists in dishes_by_name but didn't appear in the plan — this
    // shouldn't happen with the current planner, but keep the error
    // typed in case it ever does.
    Err(WhyTargetError::DishNotFound {
        dish: dish_name.to_string(),
        available: available_dishes,
    }
    .into())
}

/// Look up every cache entry with a key starting with `prefix` and
/// return the stored [`InputManifest`] for each match. Empty vec when
/// no keys match the prefix.
pub fn explain(cache: &LocalCache, prefix: &str) -> anyhow::Result<Vec<Explanation>> {
    let keys = cache.find_by_prefix(prefix)?;
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let manifest = cache.read_manifest(&key)?;
        out.push(Explanation {
            key: key.as_hex().to_string(),
            manifest,
        });
    }
    Ok(out)
}
