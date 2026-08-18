//! `bento cloud health` — the hosted-cache health read, on its own.
//!
//! Same fetch `bento prime` folds into its `cloud` section; this verb
//! exists so an agent can re-check the cache mid-session without
//! re-running a full workspace scan. Output is always JSON — there is
//! no human rendering to keep in sync, and prime already has one.

use std::time::Duration;

use anyhow::{Context, Result};
use bento_cache::token::{resolve_cache_token, token_env_name};
use bento_config::Workspace;

use crate::cli::GlobalFlags;

/// Longer than prime's 2s budget: this verb has nothing else to show
/// if the fetch fails, so it's worth waiting a little for.
const TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(global: &GlobalFlags) -> Result<i32> {
    // The token's env-var name is per-repo config, so read it when
    // there's a workspace to read it from — but don't require one.
    let configured = crate::resolve_workspace_root(global)
        .ok()
        .and_then(|root| Workspace::load(&root).ok())
        .and_then(|ws| ws.repo.cache.remote_token_env.clone());
    let env_name = token_env_name(configured.as_deref());
    let token = resolve_cache_token(env_name)
        .with_context(|| format!("no cache token — run `bento login` or export ${env_name}"))?;

    let health = bento_core::cloud::fetch_health(&token, TIMEOUT)?;
    crate::json::emit(&health)?;
    Ok(0)
}
