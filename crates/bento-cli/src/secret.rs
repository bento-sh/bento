//! `bento secret put|list|delete` — thin wrapper over per-platform
//! secret CLIs (wrangler, railway, vercel). The integration trait
//! owns the per-platform invocation; this module's job is target
//! resolution, input/output shaping, and error surfacing.
//!
//! Values flow through `put` once (stdin → integration method) and
//! are never persisted, logged, or returned by bento.

use std::io::Read;

use anyhow::{Context, Result};

use crate::cli::{GlobalFlags, SecretAction};

pub fn run(global: &GlobalFlags, action: SecretAction) -> anyhow::Result<i32> {
    let root = crate::resolve_workspace_root(global)?;
    let workspace = bento_config::Workspace::load(&root)?;
    let integrations = bento_adapters::IntegrationRegistry::builtin();

    match action {
        SecretAction::Put { target, name } => {
            let (dish, integration, config) =
                resolve_target(&workspace, &integrations, &target, "put")?;
            let value = read_value_from_stdin()?;
            integration
                .put_secret(&dish.dir, &config, &name, &value)
                .with_context(|| format!("{}:{} put {name}", dish.config.name, integration.id()))?;
            eprintln!(
                "bento secret: set {} on {} ({})",
                name,
                dish.config.name,
                integration.id()
            );
            Ok(0)
        }
        SecretAction::List { target } => {
            let (dish, integration, config) =
                resolve_target(&workspace, &integrations, &target, "list")?;
            let mut names = integration
                .list_secrets(&dish.dir, &config)
                .with_context(|| format!("{}:{} list", dish.config.name, integration.id()))?;
            names.sort();
            for n in &names {
                println!("{n}");
            }
            Ok(0)
        }
        SecretAction::Delete { target, name } => {
            let (dish, integration, config) =
                resolve_target(&workspace, &integrations, &target, "delete")?;
            integration
                .delete_secret(&dish.dir, &config, &name)
                .with_context(|| {
                    format!("{}:{} delete {name}", dish.config.name, integration.id())
                })?;
            eprintln!(
                "bento secret: deleted {} on {} ({})",
                name,
                dish.config.name,
                integration.id()
            );
            Ok(0)
        }
    }
}

/// Resolve a `<dish>[:<integration>]` target string to the concrete
/// dish + integration + `[integrations.<id>]` config block. Errors
/// with a friendly message when the dish is unknown, has no secret-
/// capable integration, or has multiple without an explicit disambig.
fn resolve_target<'a>(
    workspace: &'a bento_config::Workspace,
    integrations: &'a bento_adapters::IntegrationRegistry,
    target: &str,
    op: &str,
) -> Result<(
    &'a bento_config::LoadedDish,
    &'a dyn bento_adapters::Integration,
    toml::Table,
)> {
    let (dish_name, integration_hint) = match target.split_once(':') {
        Some((d, i)) => (d, Some(i)),
        None => (target, None),
    };

    let dish = workspace
        .dishes_by_path
        .values()
        .find(|d| d.config.name == dish_name)
        .ok_or_else(|| {
            let known: Vec<_> = workspace
                .dishes_by_path
                .values()
                .map(|d| d.config.name.clone())
                .collect();
            anyhow::anyhow!(
                "no dish named '{dish_name}' in this workspace (known: {})",
                known.join(", ")
            )
        })?;

    // Integrations that both (a) declare secret support and (b) are
    // wired up on this dish — either via filesystem detect() or via an
    // explicit `[integrations.<id>]` block.
    let candidates: Vec<&dyn bento_adapters::Integration> = integrations
        .ids()
        .into_iter()
        .filter_map(|id| integrations.by_id(&id))
        .filter(|i| i.supports_secrets())
        .filter(|i| i.detect(&dish.dir) || dish.config.integrations.contains_key(i.id()))
        .collect();

    if candidates.is_empty() {
        anyhow::bail!(
            "dish '{dish_name}' has no secret-capable deploy integration \
             (cloudflare_worker, cloudflare_pages, railway). Add an \
             [integrations.<id>] block to {}/dish.toml, or use the \
             underlying CLI directly for {op}.",
            dish.rel.display()
        );
    }

    let integration: &dyn bento_adapters::Integration = match integration_hint {
        Some(id) => *candidates.iter().find(|i| i.id() == id).ok_or_else(|| {
            let names: Vec<_> = candidates.iter().map(|i| i.id().to_string()).collect();
            anyhow::anyhow!(
                "integration '{id}' not enabled on dish '{dish_name}' \
                     (available: {})",
                names.join(", "),
            )
        })?,
        None => {
            if candidates.len() > 1 {
                let names: Vec<_> = candidates.iter().map(|i| i.id().to_string()).collect();
                anyhow::bail!(
                    "dish '{dish_name}' has multiple secret-capable integrations \
                     ({}). Disambiguate: `bento secret {op} {dish_name}:<integration> ...`",
                    names.join(", "),
                );
            }
            candidates[0]
        }
    };

    // Empty table when the dish has no explicit `[integrations.<id>]`
    // block — mirrors the shape passed to `detected_tasks`. Most
    // integrations' secret methods read nothing from config (the
    // target is derived from cwd/wrangler.toml/railway config); the
    // Pages + Railway cases do pull `project` / `service`.
    let config = dish
        .config
        .integrations
        .get(integration.id())
        .cloned()
        .unwrap_or_default();

    Ok((dish, integration, config))
}

/// Read exactly one secret value from stdin. Strips a single trailing
/// newline so `echo "$VAL" | bento secret put ...` behaves the way
/// users expect; explicit multi-line secrets (rare — RSA keys, JWT
/// private keys) work if piped without a trailing newline, since we
/// only strip ONE terminator.
fn read_value_from_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading secret value from stdin")?;
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    if buf.is_empty() {
        anyhow::bail!(
            "empty stdin — pipe the secret value in, e.g. \
             `echo -n \"$VAL\" | bento secret put <target> NAME`"
        );
    }
    Ok(buf)
}
