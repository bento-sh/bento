//! Parsers that turn captured tool output into [`Diagnostic`] records.
//!
//! Each parser is a pure function: `(stdout, stderr, dish_dir,
//! workspace_root) -> Vec<Diagnostic>`. No side effects; no executor
//! coupling. They're invoked by the executor's two-pass-on-failure
//! path (ideas-cgj) but kept here as standalone modules so they can
//! be unit-tested against captured fixtures.
//!
//! All file paths in the returned `Diagnostic.file` are normalised
//! to **forward-slash, workspace-relative** so agents can `Read(path)`
//! without further resolution. See [`normalise_path`].

mod cargo_message;
mod eslint;
mod golangci_lint;
mod ruff;

use std::path::Path;

use bento_adapters::{Diagnostic, ParserId};

/// Dispatch captured tool output to the matching parser. Never panics
/// — malformed input returns an empty `Vec`. Parser failures are
/// strictly additive: callers see fewer diagnostics, never an error.
pub fn parse(
    parser: ParserId,
    stdout: &str,
    stderr: &str,
    dish_dir: &Path,
    workspace_root: &Path,
) -> Vec<Diagnostic> {
    match parser {
        ParserId::CargoMessage => cargo_message::parse(stdout, stderr, dish_dir, workspace_root),
        ParserId::GolangciLint => golangci_lint::parse(stdout, stderr, dish_dir, workspace_root),
        ParserId::Eslint => eslint::parse(stdout, stderr, dish_dir, workspace_root),
        ParserId::Ruff => ruff::parse(stdout, stderr, dish_dir, workspace_root),
    }
}

/// Normalise a file path emitted by a tool into the bento convention:
/// forward-slash, relative to the workspace root.
///
/// Tools emit either absolute paths or paths relative to their cwd
/// (typically the dish dir). We handle both:
/// - Absolute path: strip the workspace-root prefix when possible.
/// - Relative path: resolve against `dish_dir` first, then strip
///   the workspace-root prefix.
///
/// When neither strategy yields a path under the workspace root (e.g.
/// a tool reported a vendored path outside the workspace), return the
/// original string unchanged so the diagnostic is still usable.
pub(crate) fn normalise_path(raw: &str, dish_dir: &Path, workspace_root: &Path) -> String {
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        dish_dir.join(p)
    };
    // Strip workspace root prefix when possible.
    let rel = abs.strip_prefix(workspace_root).unwrap_or(abs.as_path());
    // Collapse `./` components — `dish_dir.join("./foo")` keeps the `.`
    // verbatim, which would surface as `apps/api/./foo` to agents.
    let mut clean = std::path::PathBuf::new();
    for c in rel.components() {
        match c {
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    clean.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_returns_empty_for_malformed_input() {
        let dish = PathBuf::from("/repo/apps/api");
        let root = PathBuf::from("/repo");
        for parser in ParserId::all() {
            assert_eq!(
                parse(*parser, "garbage{", "", &dish, &root),
                Vec::<Diagnostic>::new(),
                "{parser:?} should swallow malformed input"
            );
        }
    }

    #[test]
    fn normalise_path_strips_workspace_prefix_for_absolute_paths() {
        let abs = "/repo/apps/api/main.go";
        let dish = PathBuf::from("/repo/apps/api");
        let root = PathBuf::from("/repo");
        assert_eq!(normalise_path(abs, &dish, &root), "apps/api/main.go");
    }

    #[test]
    fn normalise_path_resolves_relative_against_dish_dir() {
        let dish = PathBuf::from("/repo/apps/api");
        let root = PathBuf::from("/repo");
        assert_eq!(normalise_path("main.go", &dish, &root), "apps/api/main.go");
        assert_eq!(
            normalise_path("./cmd/api/main.go", &dish, &root),
            "apps/api/cmd/api/main.go"
        );
    }

    #[test]
    fn normalise_path_returns_original_when_outside_workspace() {
        let dish = PathBuf::from("/repo/apps/api");
        let root = PathBuf::from("/repo");
        // Outside-workspace absolute path stays as-is — agents can
        // still see it, just won't be able to Read() it as a workspace
        // file.
        let outside = "/usr/lib/go/src/runtime/proc.go";
        let out = normalise_path(outside, &dish, &root);
        assert!(out.contains("runtime/proc.go"));
    }

    #[test]
    fn normalise_path_uses_forward_slashes() {
        let dish = PathBuf::from("/repo/apps/api");
        let root = PathBuf::from("/repo");
        let out = normalise_path("src\\nested\\foo.rs", &dish, &root);
        assert!(!out.contains('\\'), "got: {out}");
    }
}
