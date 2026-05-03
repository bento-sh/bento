//! Workspace filesystem scanning — finds things that Workspace::load
//! deliberately ignores.
//!
//! [`scan_orphan_dishes`] walks the workspace tree for `dish.toml` files
//! that aren't referenced by any `bentos/*.toml`'s `dishes = [...]` list.
//! `Workspace::load` only loads referenced dishes, so orphans are
//! invisible to `bento plan` / `bento doctor` unless explicitly scanned
//! for — this helper makes that visible.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bento_config::Workspace;

/// Directory names we never descend into when looking for orphans.
/// These are build-output / VCS / lockfile noise that happens to
/// contain `dish.toml` in pathological cases (e.g. a `target/` build
/// artefact after test runs).
const NOISE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".bento",
    "dist",
    "build",
    ".next",
];

fn is_noise_dir(name: &str) -> bool {
    NOISE_DIRS.contains(&name)
}

/// Walk `root` for `dish.toml` files and return workspace-relative paths
/// of dish directories that aren't in `wired_paths`. Skips build-output
/// / VCS noise directories (target/, node_modules/, .git/, .bento/,
/// dist/, build/, .next/).
///
/// Ordered alphabetically, deduplicated.
pub fn scan_orphans(root: &Path, wired_paths: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut orphans: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                !is_noise_dir(&name)
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .filter_map(|e| {
            let dish_toml = e.path().join("dish.toml");
            if !dish_toml.is_file() {
                return None;
            }
            let rel = e.path().strip_prefix(root).ok()?.to_path_buf();
            if rel.as_os_str().is_empty() || wired_paths.contains(&rel) {
                return None;
            }
            Some(rel)
        })
        .collect();
    orphans.sort();
    orphans.dedup();
    orphans
}

/// Convenience wrapper: pull `wired_paths` from a loaded Workspace and
/// scan for orphans from its root.
pub fn scan_orphan_dishes(workspace: &Workspace) -> Vec<PathBuf> {
    let wired: BTreeSet<PathBuf> = workspace.dishes_by_path.keys().cloned().collect();
    scan_orphans(&workspace.root, &wired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, "# stub\n").unwrap();
    }

    #[test]
    fn excludes_wired_and_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("apps/api/dish.toml"));
        touch(&root.join("apps/legacy/dish.toml"));
        touch(&root.join("crates/experimental/dish.toml"));
        touch(&root.join("target/debug/spurious/dish.toml"));
        touch(&root.join("node_modules/pkg/dish.toml"));
        touch(&root.join(".git/hooks/dish.toml"));

        let mut wired = BTreeSet::new();
        wired.insert(PathBuf::from("apps/api"));

        let orphans = scan_orphans(root, &wired);
        assert_eq!(
            orphans,
            vec![
                PathBuf::from("apps/legacy"),
                PathBuf::from("crates/experimental"),
            ]
        );
    }

    #[test]
    fn returns_empty_when_all_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("apps/api/dish.toml"));
        let mut wired = BTreeSet::new();
        wired.insert(PathBuf::from("apps/api"));
        assert!(scan_orphans(root, &wired).is_empty());
    }
}
