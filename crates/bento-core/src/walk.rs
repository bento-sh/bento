//! One filesystem walk for every glob-matched file set: task inputs
//! ([`crate::plan`]), dish signatures ([`crate::cascade`]), and declared
//! artefacts ([`crate::artifacts`]).
//!
//! The input walkers set [`FileWalk::respect_ignores`] so `.gitignore`d
//! files never reach a cache key — editor swap files, `.DS_Store`,
//! `*.log` and local `.env.*` otherwise hash into keys and drift them
//! per machine. Output walkers must leave it off: build artefacts
//! (`dist/`, `target/`) are gitignored by definition.

use std::path::{Path, PathBuf};

use anyhow::Result;
use globset::GlobSet;

pub(crate) struct FileWalk<'a> {
    pub root: &'a Path,
    pub include: &'a GlobSet,
    /// Matched paths to drop — adapter `derived_paths()`, a task's own
    /// `outputs`. Checked before `include`.
    pub exclude: &'a [&'a GlobSet],
    /// Honour `.gitignore` / `.ignore` and prune noise dirs. Only
    /// repo-local, committed ignore files count: the user's global
    /// gitignore and `.git/info/exclude` are per-machine and would
    /// drift cache keys between checkouts of the same commit.
    pub respect_ignores: bool,
}

/// Root-relative paths of matching files, sorted, each flagged
/// `true` when the entry is a symlink.
pub(crate) fn walk(w: &FileWalk<'_>) -> Result<Vec<(PathBuf, bool)>> {
    if !w.root.is_dir() {
        return Ok(Vec::new());
    }

    let mut builder = ignore::WalkBuilder::new(w.root);
    builder
        .follow_links(false)
        // Dotfiles are real inputs (.nvmrc, .tool-versions, .eslintrc).
        .hidden(false)
        .git_ignore(w.respect_ignores)
        .ignore(w.respect_ignores)
        .parents(w.respect_ignores)
        .git_global(false)
        .git_exclude(false)
        // Ignore files apply outside a git repo too, so a plan is the
        // same before and after `git init`.
        .require_git(false);
    if w.respect_ignores {
        builder.filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && crate::discovery::is_noise_dir(&e.file_name().to_string_lossy()))
        });
    }

    let mut matched = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !(ft.is_file() || ft.is_symlink()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(w.root) else {
            continue;
        };
        if w.exclude.iter().any(|m| m.is_match(rel)) {
            continue;
        }
        if w.include.is_match(rel) {
            matched.push((rel.to_path_buf(), ft.is_symlink()));
        }
    }
    matched.sort();
    Ok(matched)
}

/// Bytes to hash for one walked entry. Symlinks hash by target path,
/// not by pointee content — cheap, deterministic, enough to invalidate
/// when the link is repointed, and immune to broken links.
pub(crate) fn hashable_content(full: &Path, is_symlink: bool) -> Result<Vec<u8>> {
    use anyhow::Context;
    if is_symlink {
        Ok(std::fs::read_link(full)
            .with_context(|| format!("reading link {}", full.display()))?
            .to_string_lossy()
            .into_owned()
            .into_bytes())
    } else {
        std::fs::read(full).with_context(|| format!("reading {}", full.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn all() -> GlobSet {
        let mut b = globset::GlobSetBuilder::new();
        b.add(globset::Glob::new("**").unwrap());
        b.build().unwrap()
    }

    fn names(files: &[(PathBuf, bool)]) -> Vec<String> {
        files
            .iter()
            .map(|(p, _)| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn gitignored_and_noise_paths_are_skipped_only_when_respecting_ignores() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\ndist/\n");
        write(root, "src/main.rs", "fn main() {}");
        write(root, ".nvmrc", "20");
        write(root, "debug.log", "noise");
        write(root, "dist/bundle.js", "built");
        write(root, "node_modules/pkg/index.js", "dep");

        let include = all();
        let respected = walk(&FileWalk {
            root,
            include: &include,
            exclude: &[],
            respect_ignores: true,
        })
        .unwrap();
        assert_eq!(
            names(&respected),
            vec![".gitignore", ".nvmrc", "src/main.rs"]
        );

        let raw = walk(&FileWalk {
            root,
            include: &include,
            exclude: &[],
            respect_ignores: false,
        })
        .unwrap();
        assert!(names(&raw).contains(&"dist/bundle.js".to_string()));
    }
}
