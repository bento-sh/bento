//! Server-wide workspace resolution + (soon) cached Workspace loads.
//!
//! Today this is a thin wrapper that resolves a workspace root at
//! startup. The 5-second-TTL `Workspace` cache + write-path
//! invalidation will follow once a real tool needs to
//! `Workspace::load` — there's no point caching emptiness.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Per-server context.
///
/// Construct via [`WorkspaceCtx::resolve`]; pass a flag-supplied path
/// or `None` to fall back to `$BENTO_WORKSPACE_ROOT` / cwd.
pub struct WorkspaceCtx {
    /// Canonicalised workspace root the server is pinned to. `None`
    /// when resolution couldn't settle on a path (e.g. server spawned
    /// outside any bento tree). Tools should return a structured
    /// `workspace_not_found` error rather than panic.
    workspace_root: Option<PathBuf>,
}

impl WorkspaceCtx {
    /// Resolve a workspace root from the given path (usually
    /// `--workspace`) or fall back to the current directory. Mirrors
    /// the CLI's `resolve_workspace_root` helper so behaviour across
    /// `bento` and `bento-mcp` stays identical.
    pub fn resolve(explicit: Option<&Path>) -> Result<Self> {
        let start = match explicit {
            Some(p) => p.to_path_buf(),
            None => {
                std::env::current_dir().context("reading current_dir for workspace fallback")?
            }
        };
        // Don't hard-fail here — a client may legitimately launch
        // bento-mcp from outside a workspace and rely on per-call
        // `workspace` overrides once Phase 1 ships. Tools that need a
        // workspace must call `workspace_root()?` and handle `None`.
        let workspace_root = bento_core::find_workspace_root(&start).ok();
        Ok(Self { workspace_root })
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mk_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("bento.toml"), "").unwrap();
        fs::create_dir_all(tmp.path().join("bentos")).unwrap();
        tmp
    }

    #[test]
    fn resolves_explicit_workspace() {
        let tmp = mk_workspace();
        let ctx = WorkspaceCtx::resolve(Some(tmp.path())).unwrap();
        assert_eq!(
            ctx.workspace_root().unwrap().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn returns_none_when_no_workspace_found() {
        // A dir outside any workspace. Use the OS temp dir directly
        // — it has no bento.toml.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = WorkspaceCtx::resolve(Some(tmp.path())).unwrap();
        assert!(
            ctx.workspace_root().is_none(),
            "expected None for non-workspace dir, got {:?}",
            ctx.workspace_root()
        );
    }
}
