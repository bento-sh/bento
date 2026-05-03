//! Workspace-aware plugin registry construction.
//!
//! Wraps `AdapterRegistry::builtin()` with subprocess plugin discovery
//! based on the workspace's `[plugins]` config. Built-ins are registered
//! first, so on id collision the built-in always wins via
//! [`AdapterRegistry::by_id`]'s first-match semantics.
//!
//! Plugin handshakes happen synchronously here; once back, the registry
//! is owned by the caller and the plugin processes live until the
//! adapters drop. Discovery is best-effort: a misbehaving plugin is
//! logged and skipped, never fatal.

use bento_adapters::{discover_plugins, PluginSearchOptions};
use bento_config::Workspace;
use bento_core::AdapterRegistry;

/// Build a registry containing all built-in adapters plus any subprocess
/// plugins discovered on `$PATH` and not filtered out by the workspace's
/// `[plugins]` config.
pub fn build_registry(workspace: &Workspace) -> AdapterRegistry {
    let opts = PluginSearchOptions {
        disable: workspace.repo.plugins.disable.clone(),
        allowlist: workspace.repo.plugins.allowlist.clone(),
        bento_version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    };
    AdapterRegistry::builtin().with_plugins(discover_plugins(&opts).into_iter().map(|p| p.adapter))
}
