//! Structured, agent-friendly error output.
//!
//! Every command can fail via `anyhow::Error`. When the CLI is run with
//! `--json`, [`classify`] walks the error chain to produce a [`BentoError`]
//! with a stable `kind` string, plus an optional `hint` and `where`
//! (location) — emitted as one JSON object on stdout.
//!
//! Without `--json`, errors stay human-readable and go to stderr.

use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use bento_core::why::WhyTargetError;

use crate::login::LoginError;
use crate::scaffold::ScaffoldError;

/// Classified failures from the deploy / notify preflight.
/// Constructed in `main.rs` when we know the user explicitly targeted
/// a single dish and it has no integration task of the requested kind.
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

/// Classify an `anyhow::Error` by walking its source chain for known types.
/// Unknown errors fall through to `kind = "internal"`.
pub fn classify(err: &anyhow::Error) -> BentoError {
    for cause in err.chain() {
        if let Some(cfg) = cause.downcast_ref::<bento_config::ConfigError>() {
            return classify_config(cfg);
        }
        if let Some(s) = cause.downcast_ref::<ScaffoldError>() {
            return classify_scaffold(s);
        }
        if let Some(w) = cause.downcast_ref::<bento_core::WorkspaceNotFound>() {
            return BentoError::new("workspace_not_found", w.to_string())
                .at(w.start.display().to_string())
                .with_hint(
                    "run this command inside a bento workspace, \
                     or run `bento init` to create one",
                )
                .with_next_steps([
                    "cd into an existing bento workspace (one containing bento.toml)",
                    "or run `bento init` here to create a new workspace",
                ]);
        }
        if let Some(t) = cause.downcast_ref::<bento_core::TargetRefError>() {
            return classify_target_ref(t);
        }
        if let Some(w) = cause.downcast_ref::<WhyTargetError>() {
            return classify_why_target(w);
        }
        if let Some(l) = cause.downcast_ref::<LoginError>() {
            return classify_login(l);
        }
        if let Some(d) = cause.downcast_ref::<DeployError>() {
            return classify_deploy(d);
        }
    }
    BentoError::new("internal", err.to_string())
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

fn classify_login(err: &LoginError) -> BentoError {
    use LoginError::*;
    match err {
        Expired => BentoError::new("login_expired", err.to_string())
            .with_hint("re-run `bento login` — the device code was revoked or expired before approval")
            .with_next_steps(vec![
                "re-run `bento login` and approve quickly in the browser".to_string(),
            ]),
        Timeout { timeout_secs } => BentoError::new("login_timeout", err.to_string())
            .with_hint(format!(
                "login poll timed out after {timeout_secs}s — re-run `bento login`"
            ))
            .with_next_steps(vec![
                "re-run `bento login`".to_string(),
                "if this keeps happening, check your network reach to api.bento.build".to_string(),
            ]),
        ServerError { stage, status, body } => {
            let short_body: String = body.chars().take(160).collect();
            BentoError::new("login_server_error", err.to_string())
                .with_hint(format!(
                    "api.bento.build {stage} endpoint returned HTTP {status} — \
                     {short_body}"
                ))
                .with_next_steps(vec![
                    format!(
                        "wait a minute + re-run `bento login` (transient {status} responses \
                         usually clear)"
                    ),
                    "if the error persists, report it with the status + body from --json \
                     output"
                        .to_string(),
                ])
        }
        Transport { stage, .. } => BentoError::new("login_transport", err.to_string())
            .with_hint(format!(
                "network error while talking to {stage} — check connectivity to \
                 api.bento.build"
            ))
            .with_next_steps(vec![
                "verify network reach to api.bento.build (try `curl https://api.bento.build/healthz`)"
                    .to_string(),
                "re-run `bento login` once the network settles".to_string(),
            ]),
        InvalidResponse { stage, detail } => {
            BentoError::new("login_invalid_response", err.to_string())
                .with_hint(format!(
                    "api.bento.build {stage} returned a body we couldn't parse — {detail}"
                ))
                .with_next_steps(vec![
                    "this is a remote-cache server issue — try again in a minute, then report if it persists".to_string(),
                ])
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

fn classify_target_ref(err: &bento_core::TargetRefError) -> BentoError {
    use bento_core::TargetRefError::*;
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

fn classify_scaffold(err: &ScaffoldError) -> BentoError {
    use ScaffoldError::*;
    const SUPPORTED_LANGS: &str =
        "go, cargo, python, python-uv, ruby, php, maven, gradle, node-npm, node-pnpm, \
         node-yarn, bun, deno";
    match err {
        MissingLanguage => BentoError::new("scaffold_missing_language", err.to_string())
            .with_hint(format!("pass --lang <one of: {SUPPORTED_LANGS}>"))
            .with_next_steps(vec![format!(
                "re-run with --lang <one of: {SUPPORTED_LANGS}>"
            )]),
        UnsupportedLanguage { .. } => BentoError::new("scaffold_unsupported_language", err.to_string())
            .with_hint(format!("supported: {SUPPORTED_LANGS}"))
            .with_next_steps(vec![format!(
                "pass --lang with one of the supported values: {SUPPORTED_LANGS}"
            )]),
        InvalidDishPath { path, .. } => BentoError::new("scaffold_invalid_path", err.to_string())
            .at(path.clone())
            .with_hint("pick a path inside the workspace that doesn't escape via `..`")
            .with_next_steps(vec![
                "pick a dish path inside the workspace root".to_string(),
                "avoid `..` or absolute paths — dish paths must be workspace-relative".to_string(),
            ]),
        DishPathRegistered { path } => BentoError::new("scaffold_dish_exists", err.to_string())
            .at(path.clone())
            .with_hint("pick a different path, or remove the existing dish from the bento")
            .with_next_steps(vec![
                format!("pick a different path (not '{path}') for the new dish"),
                format!("or remove '{path}' from the existing bento first"),
            ]),
        DishNameCollision { name } => BentoError::new("scaffold_dish_exists", err.to_string())
            .with_hint(format!("pick a different directory name — '{name}' is already in use"))
            .with_next_steps(vec![format!(
                "pick a different directory name — '{name}' is already in use by another dish"
            )]),
        DishAlreadyConfigured { path } => BentoError::new("scaffold_already_configured", err.to_string())
            .at(path.clone())
            .with_hint("remove the existing dish.toml or pick a different path")
            .with_next_steps(vec![
                format!("remove the existing dish.toml at {path} if you want to re-scaffold"),
                "or pick a different path for the new dish".to_string(),
            ]),
        LanguageUnknown { path } => BentoError::new("scaffold_language_unknown", err.to_string())
            .at(path.clone())
            .with_hint("pass --lang explicitly, or check that the project has a known manifest (go.mod, package.json, Cargo.toml, …)")
            .with_next_steps(vec![
                format!("pass --lang explicitly (one of: {SUPPORTED_LANGS})"),
                format!("or add a known manifest to {path} (go.mod, package.json, Cargo.toml, …) and retry"),
            ]),
        NoBentos => BentoError::new("scaffold_no_bentos", err.to_string())
            .with_hint("run `bento box add <name>` first")
            .with_next_steps(vec![
                "run `bento box add <name>` to create a bento first".to_string(),
                "then re-run `bento dish add`".to_string(),
            ]),
        MultipleBentos { available } => BentoError::new("scaffold_bento_ambiguous", err.to_string())
            .with_hint(format!("pass --bento <one of: {available}>"))
            .with_next_steps(vec![format!(
                "re-run with --bento <one of: {available}> to pick which bento owns this dish"
            )]),
        UnknownBento { name, available } => BentoError::new("scaffold_bento_not_found", err.to_string())
            .with_hint(format!(
                "no bento named '{name}' — known bentos: {available}"
            ))
            .with_next_steps(vec![format!(
                "pass --bento with a known name — available: {available}"
            )]),
        BentoConfigShape { path } => BentoError::new("scaffold_bento_shape", err.to_string())
            .at(path.clone())
            .with_hint("bento TOML must have a `dishes = [...]` array")
            .with_next_steps(vec![format!(
                "edit {path} so it has a `dishes = [...]` array at the top level"
            )]),
        Io { source, .. } => BentoError::new("scaffold_io", source.to_string())
            .with_hint("check that the target directory is writable and has free disk space")
            .with_next_steps(vec![
                "verify the target path is writable (check permissions)".to_string(),
                "verify there is free disk space".to_string(),
            ]),
    }
}

fn path_string(p: &Path) -> String {
    p.display().to_string()
}

/// Print a classified error. When `as_json` is true, emit exactly one JSON
/// object on stdout. Otherwise print a terse `error:` line on stderr.
pub fn emit(err: &anyhow::Error, as_json: bool) {
    if as_json {
        let structured = classify(err);
        // If serde_json somehow fails, fall back to a human line on stderr
        // so the user isn't left with nothing.
        match serde_json::to_string_pretty(&structured) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!(
                "{}: {err:#}\n(json emit failed: {e})",
                crate::style::red("error")
            ),
        }
    } else {
        eprintln!("{}: {err:#}", crate::style::red("error"));
    }
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
    }

    #[test]
    fn classify_config_parse_error() {
        let cfg = bento_config::ConfigError::Parse {
            kind: "dish.toml",
            path: PathBuf::from("apps/api/dish.toml"),
            message: "expected `=`".into(),
        };
        let err = anyhow::Error::new(cfg);
        let b = classify(&err);
        assert_eq!(b.kind, "config_parse");
        assert_eq!(b.locator.as_deref(), Some("apps/api/dish.toml"));
        assert!(b.hint.is_some());
    }

    #[test]
    fn classify_scaffold_unsupported_language() {
        let err = anyhow::Error::new(ScaffoldError::UnsupportedLanguage {
            lang: "rust".into(),
        });
        let b = classify(&err);
        assert_eq!(b.kind, "scaffold_unsupported_language");
        assert!(b.message.contains("rust"));
        let hint = b.hint.as_deref().unwrap();
        // Hint should enumerate the full SUPPORTED_LANGS set, not a
        // partial subset (regression: the hint used to drift from the
        // SUPPORTED_LANGS const, listing fewer languages).
        for lang in [
            "go",
            "cargo",
            "python",
            "python-uv",
            "ruby",
            "php",
            "maven",
            "gradle",
            "node-npm",
            "node-pnpm",
            "node-yarn",
            "bun",
            "deno",
        ] {
            assert!(hint.contains(lang), "hint missing {lang}: {hint}");
        }
    }

    #[test]
    fn classify_walks_through_anyhow_context() {
        let cfg = bento_config::ConfigError::Missing {
            path: PathBuf::from("bento.toml"),
        };
        let err = anyhow::Error::new(cfg).context("loading workspace");
        let b = classify(&err);
        assert_eq!(b.kind, "config_missing");
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
    }

    #[test]
    fn empty_next_steps_serializes_as_empty_array() {
        let b = BentoError::new("k", "m");
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            json.contains("\"next_steps\":[]"),
            "next_steps should always be present as an array, got: {json}"
        );
    }

    #[test]
    fn next_steps_serialize_as_array_when_present() {
        let b = BentoError::new("k", "m").with_next_steps(["step one", "step two"]);
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            json.contains("\"next_steps\":[\"step one\",\"step two\"]"),
            "got: {json}"
        );
    }

    #[test]
    fn classify_target_not_found_emits_target_not_found_with_next_steps() {
        let cause = bento_core::TargetRefError::NotFound {
            target: "api".into(),
            available_bentos: vec!["prod".into()],
            available_dishes: vec!["web".into(), "worker".into()],
        };
        let err = anyhow::Error::new(cause);
        let b = classify(&err);
        assert_eq!(b.kind, "target_not_found");
        assert!(b.message.contains("'api'"));
        assert!(
            b.next_steps.iter().any(|s| s.contains("prod")),
            "expected 'prod' in next_steps, got {:?}",
            b.next_steps
        );
        assert!(
            b.next_steps.iter().any(|s| s.contains("web")),
            "expected 'web' in next_steps, got {:?}",
            b.next_steps
        );
    }

    #[test]
    fn classify_target_not_found_empty_workspace_suggests_init() {
        let cause = bento_core::TargetRefError::NotFound {
            target: "anything".into(),
            available_bentos: vec![],
            available_dishes: vec![],
        };
        let err = anyhow::Error::new(cause);
        let b = classify(&err);
        assert_eq!(b.kind, "target_not_found");
        assert!(
            b.next_steps.iter().any(|s| s.contains("bento init")),
            "expected init hint when workspace is empty, got {:?}",
            b.next_steps
        );
    }

    #[test]
    fn classify_target_ambiguous_emits_target_ambiguous() {
        let cause = bento_core::TargetRefError::Ambiguous {
            target: "shared".into(),
        };
        let err = anyhow::Error::new(cause);
        let b = classify(&err);
        assert_eq!(b.kind, "target_ambiguous");
        assert!(b.hint.is_some());
    }

    // Shape-consistency invariant: every classified error populates
    // `next_steps` with at least one entry. Agents iterate next_steps
    // uniformly without branching on hint presence.

    fn assert_has_next_steps(b: &BentoError) {
        assert!(
            !b.next_steps.is_empty(),
            "{}: next_steps must be non-empty for agent recovery",
            b.kind
        );
    }

    #[test]
    fn workspace_not_found_has_next_steps() {
        let cause = bento_core::WorkspaceNotFound {
            start: PathBuf::from("/tmp/nowhere"),
        };
        let b = classify(&anyhow::Error::new(cause));
        assert_eq!(b.kind, "workspace_not_found");
        assert_has_next_steps(&b);
        assert!(
            b.next_steps.iter().any(|s| s.contains("bento init")),
            "expected init guidance in next_steps, got {:?}",
            b.next_steps
        );
    }

    #[test]
    fn target_ambiguous_has_next_steps() {
        let cause = bento_core::TargetRefError::Ambiguous {
            target: "shared".into(),
        };
        let b = classify(&anyhow::Error::new(cause));
        assert_has_next_steps(&b);
    }

    #[test]
    fn every_config_error_has_next_steps() {
        let cases: Vec<bento_config::ConfigError> = vec![
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
        for cfg in cases {
            let b = classify(&anyhow::Error::new(cfg));
            assert_has_next_steps(&b);
        }
    }

    #[test]
    fn every_scaffold_error_has_next_steps() {
        let cases: Vec<ScaffoldError> = vec![
            ScaffoldError::MissingLanguage,
            ScaffoldError::UnsupportedLanguage { lang: "x".into() },
            ScaffoldError::InvalidDishPath {
                path: "..".into(),
                reason: "escapes root".into(),
            },
            ScaffoldError::DishPathRegistered {
                path: "apps/api".into(),
            },
            ScaffoldError::DishNameCollision { name: "api".into() },
            ScaffoldError::DishAlreadyConfigured {
                path: "apps/api".into(),
            },
            ScaffoldError::LanguageUnknown {
                path: "apps/api".into(),
            },
            ScaffoldError::NoBentos,
            ScaffoldError::MultipleBentos {
                available: "prod, staging".into(),
            },
            ScaffoldError::UnknownBento {
                name: "x".into(),
                available: "prod".into(),
            },
            ScaffoldError::BentoConfigShape {
                path: "bentos/prod.toml".into(),
            },
            ScaffoldError::Io {
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x"),
            },
        ];
        for s in cases {
            let b = classify(&anyhow::Error::new(s));
            assert_has_next_steps(&b);
        }
    }

    #[test]
    fn internal_error_has_empty_next_steps() {
        // Unclassified failures stay next_steps-empty — the invariant
        // is for CLASSIFIED errors, not the catch-all.
        let b = classify(&anyhow::anyhow!("weird"));
        assert_eq!(b.kind, "internal");
        assert!(b.next_steps.is_empty());
    }
}
