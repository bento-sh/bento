//! Structured, agent-friendly error output.
//!
//! The envelope ([`BentoError`]) and every classifier for core-owned
//! error types live in [`bento_core::errors`] so `bento-mcp` emits the
//! same `{kind, message, hint, next_steps}` shape on a failed tool
//! call. This module adds the CLI-only types (`CliError`, scaffold,
//! login) and the stdout/stderr emitters.

use bento_core::errors::classify_known;

pub use bento_core::errors::{BentoError, DeployError};

use crate::login::LoginError;
use crate::scaffold::ScaffoldError;

/// Failures raised by the CLI layer itself — bad targets, missing
/// config blocks, verbs used out of context. They used to be ad-hoc
/// `anyhow::bail!` strings, which every agent saw as
/// `kind: "internal"` with no recovery path; every variant here gets
/// a stable kind plus next_steps in [`classify`].
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("workspace already initialised (found {path}) — refusing to overwrite")]
    AlreadyInitialised { path: String },

    #[error("bento '{name}' already exists at {path}")]
    BoxExists { name: String, path: String },

    #[error(
        "{what} name must be non-empty and contain only ASCII letters, \
         digits, '-', or '_' (got {value:?})"
    )]
    InvalidName { what: &'static str, value: String },

    #[error("no dish named '{name}' in this workspace")]
    DishNotFound {
        name: String,
        available: Vec<String>,
    },

    #[error("no bento named '{name}' in this workspace")]
    BoxNotFound {
        name: String,
        available: Vec<String>,
    },

    #[error("this workspace has no dishes")]
    NoDishes,

    #[error("workspace has {} dishes — pass `--dish <name>`", available.len())]
    DishAmbiguous { available: Vec<String> },

    #[error("dish '{dish}' has no task '{task}'")]
    TaskNotFound {
        dish: String,
        task: String,
        available: Vec<String>,
    },

    #[error(
        "task '{task}' in dish '{dish}' inherits its `run` from the \
         adapter default, so there is nothing to invoke ad-hoc"
    )]
    TaskNotAdHoc { dish: String, task: String },

    #[error("dish '{dish}' has no [serve] block in dish.toml")]
    ServeNotConfigured { dish: String },

    #[error("bento '{bento}' has no dishes with a [serve] block — nothing to serve")]
    NoServeDishes { bento: String },

    #[error("environment '{name}' is not defined in bento.toml")]
    EnvNotDefined {
        name: String,
        available: Vec<String>,
    },

    #[error("no remote cache configured")]
    NoRemoteCache,

    #[error("dish '{dish}' has no secret-capable deploy integration")]
    SecretBackendNotConfigured { dish: String },

    #[error("dish '{dish}' has multiple secret-capable integrations")]
    SecretTargetAmbiguous {
        dish: String,
        op: String,
        available: Vec<String>,
    },

    #[error("empty stdin — nothing to store as the secret value")]
    SecretValueEmpty,

    #[error("no agent clients detected on this machine")]
    McpNoClients,

    #[error("{client} doesn't support project-local MCP config")]
    McpScopeUnsupported { client: &'static str, path: String },

    #[error("expected `<tool>=<version>` (e.g. `go=1.22.3`), got {spec:?}")]
    ToolchainPinInvalid { spec: String },
}

/// Classify an `anyhow::Error`. CLI-owned error types are matched
/// first; everything else falls through to the shared core table
/// (which ends at `kind = "internal"`).
pub fn classify(err: &anyhow::Error) -> BentoError {
    for cause in err.chain() {
        if let Some(s) = cause.downcast_ref::<ScaffoldError>() {
            return classify_scaffold(s);
        }
        if let Some(l) = cause.downcast_ref::<LoginError>() {
            return classify_login(l);
        }
        if let Some(c) = cause.downcast_ref::<CliError>() {
            return classify_cli(c);
        }
    }
    classify_known(err).unwrap_or_else(|| BentoError::new("internal", err.to_string()))
}

/// One `dish list` / `box list` suggestion, prefixed by the names we
/// already know — the list is what the agent actually needs.
fn names_or_bootstrap(
    available: &[String],
    verb: &str,
    plural: &str,
    bootstrap: &str,
) -> Vec<String> {
    if available.is_empty() {
        return vec![bootstrap.to_string()];
    }
    vec![
        format!("available {plural}: {}", available.join(", ")),
        format!("run `bento {verb} list` to see them with their paths"),
    ]
}

fn classify_cli(err: &CliError) -> BentoError {
    use CliError::*;
    let e = |kind: &str| BentoError::new(kind, err.to_string());
    match err {
        AlreadyInitialised { path } => e("init_already_initialised")
            .at(path.clone())
            .with_hint("this directory is already a bento workspace")
            .with_next_steps([
                "run `bento prime` to see what's already configured",
                "or `bento dish add <path>` to register another dish",
            ]),
        BoxExists { name, path } => e("box_exists")
            .at(path.clone())
            .with_hint(format!("pick a different name — '{name}' is taken"))
            .with_next_steps([
                "run `bento box list` to see the existing bentos".to_string(),
                format!("or edit {path} directly to change bento '{name}'"),
            ]),
        InvalidName { .. } => e("invalid_name")
            .with_hint("use ASCII letters, digits, '-' or '_'")
            .with_next_steps(["re-run with a name matching [A-Za-z0-9_-]+"]),
        DishNotFound { available, .. } => e("dish_not_found").with_next_steps(names_or_bootstrap(
            available,
            "dish",
            "dishes",
            "this workspace has no dishes — run `bento dish add <path>` first",
        )),
        BoxNotFound { available, .. } => e("box_not_found").with_next_steps(names_or_bootstrap(
            available,
            "box",
            "bentos",
            "this workspace has no bentos — run `bento box add <name>` first",
        )),
        NoDishes => e("no_dishes")
            .with_hint("register a dish before running per-dish verbs")
            .with_next_steps([
                "run `bento dish add <path>` to scaffold or adopt a dish",
                "or `bento init` if this repo has no bento config yet",
            ]),
        DishAmbiguous { available } => e("dish_ambiguous")
            .with_hint("pass `--dish <name>` — the workspace has more than one")
            .with_next_steps([format!("available dishes: {}", available.join(", "))]),
        TaskNotFound {
            dish,
            available,
            task,
        } => e("task_not_found")
            .with_hint(format!("dish '{dish}' declares no `[tasks.{task}]` block"))
            .with_next_steps(if available.is_empty() {
                vec![format!(
                    "add a `[tasks.<name>]` block to {dish}/dish.toml — it has none"
                )]
            } else {
                vec![
                    format!("tasks on '{dish}': {}", available.join(", ")),
                    format!("run `bento plan {dish}` to see every task with its cache key"),
                ]
            }),
        TaskNotAdHoc { dish, task } => e("task_not_ad_hoc")
            .with_hint("`bento run` only invokes tasks with an explicit `run = \"...\"`")
            .with_next_steps([
                format!("run `bento {task} {dish}` — the cached lifecycle verb"),
                format!("or give `[tasks.{task}]` an explicit `run = \"...\"` in {dish}/dish.toml"),
            ]),
        ServeNotConfigured { dish } => e("serve_not_configured")
            .with_hint("add a `[serve]` block with the long-running command")
            .with_next_steps([format!(
                "add `[serve]` + `run = \"...\"` to dish '{dish}'s dish.toml, then re-run"
            )]),
        NoServeDishes { bento } => e("no_serve_dishes")
            .with_hint(format!(
                "no dish in bento '{bento}' declares a `[serve]` block"
            ))
            .with_next_steps([
                "add `[serve]` + `run = \"...\"` to at least one dish in this bento".to_string(),
                format!("run `bento box list` to see which dishes '{bento}' contains"),
            ]),
        EnvNotDefined { name, available } => e("env_not_defined")
            .at("bento.toml")
            .with_hint(format!(
                "add an `[environments.{name}]` block with \
                 `secrets.<VAR> = \"<SOURCE_VAR>\"` entries"
            ))
            .with_next_steps(if available.is_empty() {
                vec![format!(
                    "bento.toml defines no environments — add `[environments.{name}]`"
                )]
            } else {
                vec![
                    format!(
                        "environments defined in bento.toml: {}",
                        available.join(", ")
                    ),
                    format!("or add an `[environments.{name}]` block"),
                ]
            }),
        NoRemoteCache => e("no_remote_cache")
            .at("bento.toml")
            .with_hint("set `[cache] remote = \"...\"` before pushing or pulling")
            .with_next_steps([
                "add `[cache]` + `remote = \"s3://<bucket>/<prefix>\"` to bento.toml",
                "or `remote = \"bento://<host>\"` for the hosted cache, then `bento login`",
            ]),
        SecretBackendNotConfigured { dish } => e("secret_backend_not_configured")
            .with_hint(
                "secrets are pushed through a deploy integration \
                 (cloudflare_worker, cloudflare_pages, railway)",
            )
            .with_next_steps([
                format!("add an `[integrations.<id>]` block to {dish}/dish.toml"),
                "or set the secret with the platform's own CLI".to_string(),
            ]),
        SecretTargetAmbiguous {
            dish,
            op,
            available,
        } => e("secret_target_ambiguous")
            .with_hint(format!(
                "dish '{dish}' has more than one secret-capable integration"
            ))
            .with_next_steps([format!(
                "disambiguate: `bento secret {op} {dish}:<{}>` …",
                available.join("|")
            )]),
        SecretValueEmpty => e("secret_value_empty")
            .with_hint("the value is read from stdin")
            .with_next_steps(["pipe it in: `echo -n \"$VAL\" | bento secret put <target> NAME`"]),
        McpNoClients => e("mcp_no_clients")
            .with_hint("nothing to auto-detect — name the client explicitly")
            .with_next_steps([
                "run `bento mcp install claude-code` (or cursor / codex / zed / …)",
                "run `bento mcp install --help` for every supported client",
            ]),
        McpScopeUnsupported { path, .. } => e("mcp_scope_unsupported")
            .with_hint("drop `--local` — this client only has a user-global config")
            .with_next_steps([format!("re-run without `--local`; it writes {path}")]),
        ToolchainPinInvalid { .. } => e("toolchain_pin_invalid")
            .with_hint("the argument is a single `<tool>=<version>` pair")
            .with_next_steps([
                "re-run as `bento toolchain pin go=1.22.3` (tool, `=`, version)",
                "run `bento toolchain list` to see what's already installed",
            ]),
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

/// Print a classified error. When `as_json` is true, emit exactly one JSON
/// object on stdout. Otherwise print a terse `error:` line on stderr.
pub fn emit(err: &anyhow::Error, as_json: bool) {
    if as_json {
        let structured = classify(err);
        // If serde_json somehow fails, fall back to a human line on stderr
        // so the user isn't left with nothing.
        match crate::json::to_string(&structured) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!(
                "{}: {err:#}\n(json emit failed: {e})",
                crate::style::red("error")
            ),
        }
    } else {
        // Humans get the same hint + next_steps the JSON envelope
        // carries — the message alone ("'x' is not a known dish") left
        // them without the list of names agents already had.
        let structured = classify(err);
        eprintln!("{}: {}", crate::style::red("error"), structured.message);
        if let Some(hint) = &structured.hint {
            eprintln!("  hint: {hint}");
        }
        for step in &structured.next_steps {
            eprintln!("  → {step}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_unknown_falls_back_to_internal() {
        let b = classify(&anyhow::anyhow!("something weird"));
        assert_eq!(b.kind, "internal");
        assert_eq!(b.message, "something weird");
    }

    /// Core-owned kinds still classify through the CLI entry point.
    #[test]
    fn classify_delegates_core_types() {
        let cfg = bento_config::ConfigError::Missing {
            path: std::path::PathBuf::from("bento.toml"),
        };
        let err = anyhow::Error::new(cfg).context("loading workspace");
        assert_eq!(classify(&err).kind, "config_missing");
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
            assert_has_next_steps(&classify(&anyhow::Error::new(s)));
        }
    }

    #[test]
    fn every_cli_error_classifies_with_next_steps() {
        // The whole point of CliError: none of these may fall through
        // to `internal`, and every one hands the agent a recovery
        // path.
        let cases: Vec<CliError> = vec![
            CliError::AlreadyInitialised {
                path: "bento.toml".into(),
            },
            CliError::BoxExists {
                name: "prod".into(),
                path: "bentos/prod.toml".into(),
            },
            CliError::InvalidName {
                what: "bento",
                value: "no spaces!".into(),
            },
            CliError::DishNotFound {
                name: "api".into(),
                available: vec!["web".into()],
            },
            CliError::DishNotFound {
                name: "api".into(),
                available: vec![],
            },
            CliError::BoxNotFound {
                name: "prod".into(),
                available: vec!["staging".into()],
            },
            CliError::NoDishes,
            CliError::DishAmbiguous {
                available: vec!["web".into(), "api".into()],
            },
            CliError::TaskNotFound {
                dish: "api".into(),
                task: "seed".into(),
                available: vec!["build".into()],
            },
            CliError::TaskNotFound {
                dish: "api".into(),
                task: "seed".into(),
                available: vec![],
            },
            CliError::TaskNotAdHoc {
                dish: "api".into(),
                task: "build".into(),
            },
            CliError::ServeNotConfigured { dish: "api".into() },
            CliError::NoServeDishes {
                bento: "prod".into(),
            },
            CliError::EnvNotDefined {
                name: "staging".into(),
                available: vec!["prod".into()],
            },
            CliError::EnvNotDefined {
                name: "staging".into(),
                available: vec![],
            },
            CliError::NoRemoteCache,
            CliError::SecretBackendNotConfigured { dish: "api".into() },
            CliError::SecretTargetAmbiguous {
                dish: "api".into(),
                op: "put".into(),
                available: vec!["railway".into(), "cloudflare_worker".into()],
            },
            CliError::SecretValueEmpty,
            CliError::McpNoClients,
            CliError::McpScopeUnsupported {
                client: "Windsurf",
                path: "~/.codeium/windsurf/mcp_config.json".into(),
            },
            CliError::ToolchainPinInvalid {
                spec: "nonsense".into(),
            },
        ];
        for c in cases {
            let b = classify(&anyhow::Error::new(c));
            assert_ne!(b.kind, "internal", "unclassified: {}", b.message);
            assert_has_next_steps(&b);
        }
    }

    #[test]
    fn dish_not_found_lists_the_available_dishes() {
        let b = classify(&anyhow::Error::new(CliError::DishNotFound {
            name: "api".into(),
            available: vec!["web".into(), "worker".into()],
        }));
        assert_eq!(b.kind, "dish_not_found");
        assert!(
            b.next_steps.iter().any(|s| s.contains("web, worker")),
            "expected the dish names in next_steps, got {:?}",
            b.next_steps
        );
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
