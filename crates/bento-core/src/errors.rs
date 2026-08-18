//! Structured, agent-friendly error envelope.
//!
//! [`classify`] walks an `anyhow::Error` chain for known bento error
//! types and produces a [`BentoError`] with a stable `kind`, a `hint`,
//! and ordered `next_steps`. Both binaries share it: the CLI emits it
//! as the `--json` error object, `bento-mcp` returns it as the
//! structured content of a failed `CallToolResult`.
//!
//! CLI-only error types (scaffold, login) are classified in
//! `bento-cli`'s `errors` module, which falls through to this one.

use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::why::WhyTargetError;

/// Classified failures from the deploy / notify preflight. Constructed
/// by the caller (CLI or MCP) when it knows the user explicitly
/// targeted a single dish that has no integration task of the
/// requested kind.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error(
        "dish '{dish}' has no integration task of kind '{kind}' — \
         nothing to {kind}"
    )]
    IntegrationNotConfigured {
        dish: String,
        kind: String,
        /// Integration ids (from `[integrations.*]` keys) the dish
        /// DOES declare, even if they don't contribute a task of this
        /// kind. Informational — helps the agent understand why the
        /// kind mismatched.
        configured_integrations: Vec<String>,
    },
}

/// Stable, agent-friendly error envelope. Every command failure with
/// `--json` produces exactly one of these on stdout.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BentoError {
    /// Stable machine identifier. Agents should switch on this string.
    pub kind: String,
    /// Human-readable description of what failed.
    pub message: String,
    /// Suggested next action, if any. For a single primary suggestion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Ordered recovery steps. Always an array (may be empty). Use when
    /// the fix is multi-step or enumerates structured options (e.g.
    /// "here are the available dishes: a, b, c"). Prefer this over
    /// `hint` for anything an agent would want to pick from rather than
    /// read.
    pub next_steps: Vec<String>,
    /// File path or locator where the error originated, if applicable.
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    /// Link to documentation for this error kind, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

impl BentoError {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            hint: None,
            next_steps: Vec::new(),
            locator: None,
            docs_url: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_next_steps<I, S>(mut self, steps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.next_steps = steps.into_iter().map(Into::into).collect();
        self
    }

    pub fn at(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }
}

/// Classify an `anyhow::Error` by walking its source chain for
/// core-owned error types. Unknown errors fall through to
/// `kind = "internal"`.
pub fn classify(err: &anyhow::Error) -> BentoError {
    classify_known(err).unwrap_or_else(|| BentoError::new("internal", err.to_string()))
}

/// Same walk as [`classify`] but reports "not one of ours" as `None`,
/// so a caller with extra error types can try theirs first and still
/// share this table.
pub fn classify_known(err: &anyhow::Error) -> Option<BentoError> {
    for cause in err.chain() {
        if let Some(cfg) = cause.downcast_ref::<bento_config::ConfigError>() {
            return Some(classify_config(cfg));
        }
        if let Some(w) = cause.downcast_ref::<crate::WorkspaceNotFound>() {
            return Some(
                BentoError::new("workspace_not_found", w.to_string())
                    .at(w.start.display().to_string())
                    .with_hint(
                        "run this command inside a bento workspace, \
                         or run `bento init` to create one",
                    )
                    .with_next_steps([
                        "cd into an existing bento workspace (one containing bento.toml)",
                        "or run `bento init` here to create a new workspace",
                    ]),
            );
        }
        if let Some(t) = cause.downcast_ref::<crate::TargetRefError>() {
            return Some(classify_target_ref(t));
        }
        if let Some(w) = cause.downcast_ref::<WhyTargetError>() {
            return Some(classify_why_target(w));
        }
        if let Some(d) = cause.downcast_ref::<DeployError>() {
            return Some(classify_deploy(d));
        }
    }
    None
}

fn classify_deploy(err: &DeployError) -> BentoError {
    use DeployError::*;
    match err {
        IntegrationNotConfigured {
            dish,
            kind,
            configured_integrations,
        } => {
            let mut steps = Vec::new();
            if configured_integrations.is_empty() {
                steps.push(format!(
                    "add `[integrations.<platform>]` (cloudflare_pages, \
                     railway, cloudflare_worker, …) to {dish}/dish.toml"
                ));
            } else {
                steps.push(format!(
                    "dish '{dish}' has these integrations configured: {} — \
                     none of them emit a '{kind}' task",
                    configured_integrations.join(", ")
                ));
                steps.push(format!(
                    "either add an integration that supports '{kind}', or \
                     drop the `bento {verb}` call for this dish",
                    verb = match kind.as_str() {
                        "deploy" | "deploy-preview" => "deploy",
                        "rollback" => "deploy --rollback",
                        "notify" => "notify",
                        _ => "deploy",
                    }
                ));
            }
            steps.push("run `bento doctor --env <env>` to see integration readiness".to_string());
            BentoError::new("integration_not_configured", err.to_string())
                .with_hint(format!(
                    "dish '{dish}' has no '{kind}' integration task — \
                     add an `[integrations.*]` block that covers '{kind}'"
                ))
                .with_next_steps(steps)
        }
    }
}

fn classify_why_target(err: &WhyTargetError) -> BentoError {
    use WhyTargetError::*;
    match err {
        InvalidDishTask { input } => BentoError::new("why_invalid_target", err.to_string())
            .with_hint(format!(
                "'{input}' is not valid — use `<dish>:<task>` (e.g. `marketing:lint`) \
                 or a cache-key hex prefix"
            ))
            .with_next_steps(vec![
                format!("try `bento why marketing:lint` — replace with your dish:task pair"),
                "or run `bento plan --json` and copy a task's `key` field".to_string(),
            ]),
        DishNotFound { dish, available } => {
            let mut steps = vec![];
            if available.is_empty() {
                steps.push(
                    "this workspace has no dishes — run `bento dish add <path>` first".to_string(),
                );
            } else {
                steps.push(format!("available dishes: {}", available.join(", ")));
                steps.push("run `bento dish list` to see every dish with its bentos".into());
            }
            BentoError::new("why_dish_not_found", err.to_string())
                .with_hint(format!(
                    "no dish named '{dish}' — check `bento dish list` for the canonical name"
                ))
                .with_next_steps(steps)
        }
        TaskNotFound {
            dish,
            task,
            available,
        } => BentoError::new("why_task_not_found", err.to_string())
            .with_hint(format!("dish '{dish}' has no task named '{task}'"))
            .with_next_steps(vec![
                format!("available tasks on '{dish}': {}", available.join(", ")),
                format!("run `bento plan {dish}` to see every task + its key"),
            ]),
        NoCacheEntry { dish, task, key } => BentoError::new("why_no_cache_entry", err.to_string())
            .with_hint(format!(
                "no cache entry yet for {dish}:{task} (key {}) — run `bento build {dish}` or \
                 `bento ci` to produce one",
                &key[..12.min(key.len())]
            ))
            .with_next_steps(vec![
                format!("run `bento build {dish}` (or `bento ci`) to execute + cache this task"),
                format!("then retry `bento why {dish}:{task}`"),
            ]),
    }
}

fn classify_target_ref(err: &crate::TargetRefError) -> BentoError {
    use crate::TargetRefError::*;
    match err {
        NotFound {
            available_bentos,
            available_dishes,
            ..
        } => {
            let mut steps = Vec::new();
            if !available_bentos.is_empty() {
                steps.push(format!("available bentos: {}", available_bentos.join(", ")));
            }
            if !available_dishes.is_empty() {
                steps.push(format!("available dishes: {}", available_dishes.join(", ")));
            }
            if available_bentos.is_empty() && available_dishes.is_empty() {
                steps.push(
                    "this workspace has no bentos or dishes yet — run `bento init` \
                     or `bento dish add <path>`"
                        .into(),
                );
            } else {
                steps.push("run `bento plan` to see the full dependency graph".into());
            }
            BentoError::new("target_not_found", err.to_string()).with_next_steps(steps)
        }
        Ambiguous { target } => {
            let hint = format!(
                "'{target}' is used by both a bento and a dish; \
                 rename one so the verb is unambiguous"
            );
            BentoError::new("target_ambiguous", err.to_string())
                .with_hint(hint)
                .with_next_steps(vec![
                    format!(
                        "rename either the bento or the dish named '{target}' so the verb is unambiguous"
                    ),
                    "run `bento dish list` to see all known dishes".to_string(),
                ])
        }
    }
}

fn classify_config(err: &bento_config::ConfigError) -> BentoError {
    use bento_config::ConfigError::*;
    match err {
        Read { path, .. } => BentoError::new("config_read", err.to_string())
            .at(path_string(path))
            .with_hint("check that the file exists and is readable")
            .with_next_steps(vec![
                format!("check that {} exists", path.display()),
                format!("verify read permissions on {}", path.display()),
            ]),
        Parse { path, .. } => BentoError::new("config_parse", err.to_string())
            .at(path_string(path))
            .with_hint("the file is not valid TOML — see the line/column above")
            .with_next_steps(vec![format!(
                "open {} and fix the TOML syntax at the line/column shown in the message",
                path.display()
            )]),
        Invalid { path, .. } => BentoError::new("config_invalid", err.to_string())
            .at(path_string(path))
            .with_hint("see the schema at `bento schema` (coming soon)")
            .with_next_steps(vec![
                format!("correct the invalid field in {}", path.display()),
                "run `bento schema` to see the expected shape".to_string(),
            ]),
        Missing { path } => BentoError::new("config_missing", err.to_string())
            .at(path_string(path))
            .with_hint(format!("create {} or run the command from a directory that contains it", path.display()))
            .with_next_steps(vec![format!(
                "create {} with the expected schema",
                path.display()
            )]),
        Duplicate { kind, name, .. } => BentoError::new("config_duplicate", err.to_string())
            .with_hint(format!("rename one of the conflicting {kind}s ('{name}')"))
            .with_next_steps(vec![format!(
                "rename one of the duplicate {kind}s named '{name}' so every {kind} has a unique name"
            )]),
        DanglingDishRef { bento, dish_path } => {
            BentoError::new("config_dangling_dish", err.to_string())
                .at(path_string(dish_path))
                .with_hint(format!(
                    "either create {}/dish.toml or remove the entry from bento '{bento}'",
                    dish_path.display()
                ))
                .with_next_steps(vec![
                    format!(
                        "create {}/dish.toml to register the dish",
                        dish_path.display()
                    ),
                    format!(
                        "or remove '{}' from the dishes list in bento '{bento}'",
                        dish_path.display()
                    ),
                ])
        }
    }
}

fn path_string(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_unknown_falls_back_to_internal() {
        let err = anyhow::anyhow!("something weird");
        let b = classify(&err);
        assert_eq!(b.kind, "internal");
        assert_eq!(b.message, "something weird");
        assert!(b.hint.is_none());
        assert!(classify_known(&err).is_none());
    }

    #[test]
    fn classify_config_parse_error() {
        let cfg = bento_config::ConfigError::Parse {
            kind: "dish.toml",
            path: PathBuf::from("apps/api/dish.toml"),
            message: "expected `=`".into(),
        };
        let b = classify(&anyhow::Error::new(cfg));
        assert_eq!(b.kind, "config_parse");
        assert_eq!(b.locator.as_deref(), Some("apps/api/dish.toml"));
        assert!(b.hint.is_some());
    }

    #[test]
    fn classify_walks_through_anyhow_context() {
        let cfg = bento_config::ConfigError::Missing {
            path: PathBuf::from("bento.toml"),
        };
        let err = anyhow::Error::new(cfg).context("loading workspace");
        assert_eq!(classify(&err).kind, "config_missing");
    }

    #[test]
    fn error_serializes_where_as_where_key() {
        let b = BentoError::new("k", "m").at("apps/api");
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"where\":\"apps/api\""), "got: {json}");
    }

    #[test]
    fn error_omits_optional_fields_when_absent() {
        let b = BentoError::new("k", "m");
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("hint"));
        assert!(!json.contains("where"));
        assert!(!json.contains("docs_url"));
        assert!(json.contains("\"next_steps\":[]"), "got: {json}");
    }

    #[test]
    fn classify_target_not_found_lists_available_names() {
        let cause = crate::TargetRefError::NotFound {
            target: "api".into(),
            available_bentos: vec!["prod".into()],
            available_dishes: vec!["web".into(), "worker".into()],
        };
        let b = classify(&anyhow::Error::new(cause));
        assert_eq!(b.kind, "target_not_found");
        assert!(b.message.contains("'api'"));
        assert!(b.next_steps.iter().any(|s| s.contains("prod")));
        assert!(b.next_steps.iter().any(|s| s.contains("web")));
    }

    #[test]
    fn classify_target_not_found_empty_workspace_suggests_init() {
        let cause = crate::TargetRefError::NotFound {
            target: "anything".into(),
            available_bentos: vec![],
            available_dishes: vec![],
        };
        let b = classify(&anyhow::Error::new(cause));
        assert!(b.next_steps.iter().any(|s| s.contains("bento init")));
    }

    #[test]
    fn classify_target_ambiguous_emits_target_ambiguous() {
        let cause = crate::TargetRefError::Ambiguous {
            target: "shared".into(),
        };
        let b = classify(&anyhow::Error::new(cause));
        assert_eq!(b.kind, "target_ambiguous");
        assert!(b.hint.is_some());
    }

    /// Shape-consistency invariant: every CLASSIFIED error populates
    /// `next_steps` with at least one entry, so agents iterate it
    /// uniformly without branching on hint presence.
    #[test]
    fn every_classified_error_has_next_steps() {
        let mut errs: Vec<anyhow::Error> = vec![
            anyhow::Error::new(crate::WorkspaceNotFound {
                start: PathBuf::from("/tmp/nowhere"),
            }),
            anyhow::Error::new(crate::TargetRefError::Ambiguous {
                target: "shared".into(),
            }),
            anyhow::Error::new(WhyTargetError::InvalidDishTask { input: "x".into() }),
            anyhow::Error::new(DeployError::IntegrationNotConfigured {
                dish: "web".into(),
                kind: "deploy".into(),
                configured_integrations: vec![],
            }),
        ];
        let cfgs: Vec<bento_config::ConfigError> = vec![
            bento_config::ConfigError::Read {
                path: PathBuf::from("a/dish.toml"),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "x"),
            },
            bento_config::ConfigError::Parse {
                kind: "dish.toml",
                path: PathBuf::from("a/dish.toml"),
                message: "bad".into(),
            },
            bento_config::ConfigError::Invalid {
                kind: "dish.toml",
                path: PathBuf::from("a/dish.toml"),
                message: "no tasks".into(),
            },
            bento_config::ConfigError::Missing {
                path: PathBuf::from("bento.toml"),
            },
            bento_config::ConfigError::Duplicate {
                kind: "dish",
                name: "api".into(),
                path_a: PathBuf::from("apps/a/dish.toml"),
                path_b: PathBuf::from("apps/b/dish.toml"),
            },
            bento_config::ConfigError::DanglingDishRef {
                bento: "prod".into(),
                dish_path: PathBuf::from("crates/missing"),
            },
        ];
        errs.extend(cfgs.into_iter().map(anyhow::Error::new));
        for err in &errs {
            let b = classify(err);
            assert!(
                !b.next_steps.is_empty(),
                "{}: next_steps must be non-empty for agent recovery",
                b.kind
            );
        }
    }
}
