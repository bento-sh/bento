//! `bento prime` data layer.
//!
//! Computes a structured snapshot of a workspace: what dishes and
//! bentos exist, cache state, a plan preview, and a ranked list of
//! recommended next verbs. Shared by the `bento` CLI (via
//! `bento prime` / `bento prime --json`) and `bento-mcp` (via the
//! `bento_prime` tool).
//!
//! Advisory only — every field is informational. Pure read; does not
//! execute tasks and does not make network calls. For reachability /
//! credential checks, use `bento doctor --cloud`.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use bento_config::Workspace;

use crate::{plan_at, scan_orphan_dishes, MissReason, PlanOptions, TaskStatus};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Output {
    /// Absolute path to the workspace root.
    pub workspace_root: String,
    pub bentos: Vec<BentoRef>,
    pub dishes: Vec<DishRef>,
    /// Workspace-relative paths of `dish.toml` files on disk that aren't
    /// referenced by any bento. Mirrors the `orphans` field of
    /// `bento dish list --json`.
    pub orphan_dishes: Vec<String>,
    pub cache: CacheStatus,
    pub plan: PlanSnapshot,
    /// Ordered next-step suggestions. Agents should follow the first
    /// and fall back to later ones. Always at least one entry.
    pub recommended_next: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BentoRef {
    pub name: String,
    pub source: String,
    pub dish_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DishRef {
    pub name: String,
    pub path: String,
    pub language: Option<String>,
    pub bentos: Vec<String>,
    /// Stable IDs of any `[integrations.*]` blocks on this dish (e.g.
    /// `cloudflare_pages`, `railway`). Empty when no integrations are
    /// configured.
    pub integrations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CacheStatus {
    /// Local cache directory from `[cache] local` (or the default).
    pub local_dir: String,
    /// Whether `local_dir` exists on disk.
    pub local_exists: bool,
    /// Remote cache URL from `[cache] remote`, if configured.
    pub remote_url: Option<String>,
    /// Env var name holding the remote JWT, if `[cache] remote_token_env`
    /// is set. The value is never read into prime output.
    pub remote_token_env: Option<String>,
    /// `true` when `remote_token_env` is set AND that env var is
    /// present in the current environment. When false with a
    /// configured remote, the remote tier will silently degrade to
    /// local-only.
    pub remote_token_resolved: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanSnapshot {
    /// First handful of tasks bento would run right now. Ordered by the
    /// default planner traversal (alphabetical over bentos and dishes).
    /// Capped at 5 entries to keep prime output compact.
    pub preview: Vec<PlanTask>,
    /// Total task count across every bento in the workspace.
    pub total_tasks: usize,
    /// Hit / miss / skipped counts across every bento.
    pub hits: usize,
    pub misses: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanTask {
    pub dish: String,
    pub task: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miss_reason: Option<MissReason>,
}

/// Produce the prime [`Output`] for a loaded workspace.
///
/// Runs `plan_at` internally to compute the preview — read-only, no
/// task execution.
pub fn compute(workspace: &Workspace) -> Result<Output> {
    let bentos = collect_bentos(workspace);
    let dishes = collect_dishes(workspace);
    let orphan_dishes: Vec<String> = scan_orphan_dishes(workspace)
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let cache = collect_cache(workspace);
    let plan = collect_plan(&workspace.root)?;
    let recommended_next = recommend_next(workspace, &orphan_dishes, &cache, &plan);

    Ok(Output {
        workspace_root: workspace.root.display().to_string(),
        bentos,
        dishes,
        orphan_dishes,
        cache,
        plan,
        recommended_next,
    })
}

fn collect_bentos(ws: &Workspace) -> Vec<BentoRef> {
    let mut out: Vec<BentoRef> = ws
        .bentos
        .values()
        .map(|b| BentoRef {
            name: b.config.name.clone(),
            source: b
                .source
                .strip_prefix(&ws.root)
                .unwrap_or(&b.source)
                .to_string_lossy()
                .to_string(),
            dish_count: b.config.dishes.len(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn collect_dishes(ws: &Workspace) -> Vec<DishRef> {
    let mut out: Vec<DishRef> = ws
        .dishes_by_name
        .values()
        .map(|d| {
            let rel = d.rel.to_string_lossy().to_string();
            let bentos = ws
                .bentos
                .values()
                .filter(|b| b.config.dishes.iter().any(|dp| dp == &rel))
                .map(|b| b.config.name.clone())
                .collect();
            let integrations: Vec<String> = d.config.integrations.keys().cloned().collect();
            DishRef {
                name: d.config.name.clone(),
                path: rel,
                language: d.config.language.clone(),
                bentos,
                integrations,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn collect_cache(ws: &Workspace) -> CacheStatus {
    let local_dir = crate::default_cache_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".to_string());
    let local_exists = std::path::PathBuf::from(&local_dir).is_dir();

    let (remote_url, remote_token_env, remote_token_resolved) = {
        let cache = &ws.repo.cache;
        let url = cache.remote.clone();
        let env_name = cache.remote_token_env.clone();
        let resolved = env_name
            .as_ref()
            .map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()).is_some())
            .unwrap_or(false);
        (url, env_name, resolved)
    };

    CacheStatus {
        local_dir,
        local_exists,
        remote_url,
        remote_token_env,
        remote_token_resolved,
    }
}

fn collect_plan(root: &std::path::Path) -> Result<PlanSnapshot> {
    let plan = plan_at(root, &PlanOptions::default())?;
    let mut preview: Vec<PlanTask> = Vec::new();
    for bento in &plan.bentos {
        for dish in &bento.dishes {
            for task in &dish.tasks {
                if preview.len() >= 5 {
                    break;
                }
                preview.push(PlanTask {
                    dish: dish.name.clone(),
                    task: task.name.clone(),
                    status: task.status,
                    miss_reason: task.miss_reason,
                });
            }
            if preview.len() >= 5 {
                break;
            }
        }
        if preview.len() >= 5 {
            break;
        }
    }
    Ok(PlanSnapshot {
        preview,
        total_tasks: plan.summary.tasks,
        hits: plan.summary.hits,
        misses: plan.summary.misses,
        skipped: plan.summary.skipped,
    })
}

fn recommend_next(
    ws: &Workspace,
    orphans: &[String],
    cache: &CacheStatus,
    plan: &PlanSnapshot,
) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();

    if ws.bentos.is_empty() {
        steps.push("run `bento box add <name>` to create your first bento".to_string());
    }
    if ws.dishes_by_name.is_empty() {
        steps.push("run `bento dish add <path> --lang <lang>` to scaffold a dish".to_string());
    }
    if !orphans.is_empty() {
        steps.push(format!(
            "{} orphan dish.toml file(s) not in any bento — `bento dish list --json` to see them, \
             then `bento dish add <path>` to wire each one",
            orphans.len()
        ));
    }
    if cache.remote_url.is_some() && !cache.remote_token_resolved {
        steps.push(format!(
            "remote cache is configured but {} is not set in the environment — export it or \
             unset `remote_token_env` in [cache]",
            cache
                .remote_token_env
                .as_deref()
                .unwrap_or("the token env var")
        ));
    }
    if plan.misses > 0 && plan.hits == 0 {
        steps.push(
            "no cache yet — run `bento install` then `bento ci` to prime the cache".to_string(),
        );
    } else if plan.misses > 0 {
        steps.push(format!(
            "{} task(s) would miss cache — run `bento ci` to build+cache them",
            plan.misses
        ));
    }
    if steps.is_empty() {
        steps.push(
            "workspace is cache-warm; run `bento plan` to inspect, \
             `bento build <target>` to build, or `bento deploy <target>` to ship"
                .to_string(),
        );
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_cache() -> CacheStatus {
        CacheStatus {
            local_dir: "/tmp".into(),
            local_exists: false,
            remote_url: None,
            remote_token_env: None,
            remote_token_resolved: false,
        }
    }

    fn mk_plan() -> PlanSnapshot {
        PlanSnapshot {
            preview: vec![],
            total_tasks: 0,
            hits: 0,
            misses: 0,
            skipped: 0,
        }
    }

    fn empty_workspace() -> Workspace {
        let tmp = tempfile::tempdir().unwrap();
        Workspace {
            root: tmp.path().to_path_buf(),
            repo: bento_config::RepoConfig::default(),
            bentos: Default::default(),
            dishes_by_path: Default::default(),
            dishes_by_name: Default::default(),
        }
    }

    #[test]
    fn recommend_next_always_returns_at_least_one_step() {
        let ws = empty_workspace();
        let steps = recommend_next(&ws, &[], &mk_cache(), &mk_plan());
        assert!(!steps.is_empty());
        assert!(
            steps.iter().any(|s| s.contains("bento box add")),
            "empty workspace should suggest creating a bento first, got {steps:?}"
        );
    }

    #[test]
    fn recommend_flags_missing_remote_token() {
        let ws = empty_workspace();
        let cache = CacheStatus {
            local_dir: "/tmp".into(),
            local_exists: true,
            remote_url: Some("bento://cache.bento.build".into()),
            remote_token_env: Some("BENTO_CACHE_TOKEN".into()),
            remote_token_resolved: false,
        };
        let steps = recommend_next(&ws, &[], &cache, &mk_plan());
        assert!(
            steps.iter().any(|s| s.contains("BENTO_CACHE_TOKEN")),
            "should flag missing remote token env var, got {steps:?}"
        );
    }

    #[test]
    fn recommend_warm_cache_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bentos = std::collections::BTreeMap::new();
        bentos.insert(
            "prod".to_string(),
            bento_config::LoadedBento {
                config: bento_config::BentoConfig {
                    name: "prod".into(),
                    dishes: vec!["apps/api".into()],
                },
                source: tmp.path().join("bentos/prod.toml"),
            },
        );
        let mut dishes = std::collections::BTreeMap::new();
        dishes.insert(
            "api".to_string(),
            bento_config::LoadedDish {
                config: bento_config::DishConfig {
                    name: "api".into(),
                    language: Some("bun".into()),
                    ..Default::default()
                },
                dir: tmp.path().join("apps/api"),
                rel: "apps/api".into(),
            },
        );
        let ws = Workspace {
            root: tmp.path().to_path_buf(),
            repo: bento_config::RepoConfig::default(),
            bentos,
            dishes_by_path: Default::default(),
            dishes_by_name: dishes,
        };
        let plan = PlanSnapshot {
            preview: vec![],
            total_tasks: 3,
            hits: 3,
            misses: 0,
            skipped: 0,
        };
        let cache = CacheStatus {
            local_dir: "/tmp".into(),
            local_exists: true,
            remote_url: None,
            remote_token_env: None,
            remote_token_resolved: false,
        };
        let steps = recommend_next(&ws, &[], &cache, &plan);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("cache-warm"));
    }
}
