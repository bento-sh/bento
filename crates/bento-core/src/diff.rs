//! Coarse, dir-level change detection via `git`.
//!
//! **Phase 1**: for the planner we only need to know *which dish dirs
//! contain any changed file* since a base ref. The content-hash-based
//! cache (see `bento-cache`) is still the authoritative gate on whether
//! a task needs to rebuild — this helper is a pre-filter that lets the
//! planner skip the majority of dishes entirely.
//!
//! Implementation shells out to `git` for two reasons:
//!
//! 1. It's the lowest-risk way to stay bit-for-bit compatible with
//!    whatever ref resolution (SHAs, tags, `HEAD~5`, `origin/main`) the
//!    user throws at `--since`.
//! 2. gix is a great library but the diff API surface we'd need is
//!    bigger than the problem justifies at this stage. A future phase
//!    (input-glob-level invalidation) can migrate if it helps.
//!
//! We combine two commands so we also catch untracked files:
//!
//! - `git diff --name-only <base>`     → tracked changes vs. base
//! - `git ls-files --others --exclude-standard` → new (untracked) files

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Handle to a git repo rooted at [`repo_root`](Self::repo_root).
#[derive(Debug, Clone)]
pub struct GitDiff {
    repo_root: PathBuf,
}

impl GitDiff {
    /// Wrap an explicit repo root. The caller asserts this directory
    /// contains a `.git/` (checked on first use, not here).
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// Walk up from `start` until we find a `.git` entry.
    pub fn discover(start: &Path) -> Result<Self> {
        let canonical = start
            .canonicalize()
            .with_context(|| format!("canonicalising start path {}", start.display()))?;
        let mut cursor = canonical.as_path();
        loop {
            if cursor.join(".git").exists() {
                return Ok(Self::new(cursor.to_path_buf()));
            }
            match cursor.parent() {
                Some(parent) => cursor = parent,
                None => anyhow::bail!("no git repository found at or above {}", start.display()),
            }
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Every file whose state differs from `base_ref` in the working tree,
    /// plus every untracked file the user has added.
    ///
    /// Returned paths are relative to [`repo_root`](Self::repo_root).
    pub fn changed_files(&self, base_ref: &str) -> Result<BTreeSet<PathBuf>> {
        let mut all = BTreeSet::new();
        all.extend(self.run_git_names(&["diff", "--name-only", base_ref])?);
        all.extend(self.run_git_names(&["ls-files", "--others", "--exclude-standard"])?);
        Ok(all)
    }

    /// Subset of `dish_dirs` that contain at least one changed file.
    ///
    /// `dish_dirs` are paths relative to [`repo_root`](Self::repo_root).
    /// Matching is by path prefix — a file under `apps/api/` marks
    /// `apps/api` as dirty.
    pub fn changed_dirs<I>(&self, base_ref: &str, dish_dirs: I) -> Result<BTreeSet<PathBuf>>
    where
        I: IntoIterator,
        I::Item: Into<PathBuf>,
    {
        let files = self.changed_files(base_ref)?;
        let dirs: Vec<PathBuf> = dish_dirs.into_iter().map(Into::into).collect();

        let mut dirty = BTreeSet::new();
        for file in &files {
            for dir in &dirs {
                if file_is_inside(file, dir) {
                    dirty.insert(dir.clone());
                }
            }
        }
        Ok(dirty)
    }

    fn run_git_names(&self, args: &[&str]) -> Result<Vec<PathBuf>> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_root)
            .output()
            .with_context(|| format!("running `git {}`", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("`git {}` failed: {stderr}", args.join(" "));
        }
        Ok(parse_names(&output.stdout))
    }
}

fn parse_names(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// `true` if `file` is strictly inside `dir` (not a sibling whose path
/// happens to share a prefix string — e.g. `apps/api-v2` vs. `apps/api`).
fn file_is_inside(file: &Path, dir: &Path) -> bool {
    if dir.as_os_str().is_empty() {
        return true;
    }
    file.starts_with(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "bento-test")
            .env("GIT_AUTHOR_EMAIL", "test@bento.local")
            .env("GIT_COMMITTER_NAME", "bento-test")
            .env("GIT_COMMITTER_EMAIL", "test@bento.local")
            .status()
            .unwrap_or_else(|e| panic!("failed to exec git {args:?}: {e}"));
        assert!(status.success(), "git {args:?} exited with {status}");
    }

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "--quiet", "--initial-branch=main"]);
        // Seed a baseline commit so HEAD is a valid ref.
        std::fs::write(tmp.path().join("README.md"), "init\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "--quiet", "-m", "init"]);
        tmp
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    #[test]
    fn discover_finds_repo_root() {
        let tmp = init_repo();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let diff = GitDiff::discover(&nested).unwrap();
        assert_eq!(
            diff.repo_root().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_errors_outside_repo() {
        // /tmp is (on this harness) not inside a git repo.
        let tmp = tempfile::tempdir().unwrap();
        let err = GitDiff::discover(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("no git repository"), "got: {err}");
    }

    #[test]
    fn changed_files_picks_up_unstaged_modification() {
        let tmp = init_repo();
        write(tmp.path(), "apps/api/main.go", "package main\n");
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "--quiet", "-m", "add api"]);

        // Modify the tracked file.
        write(tmp.path(), "apps/api/main.go", "package main\n// edited\n");

        let diff = GitDiff::new(tmp.path());
        let changed = diff.changed_files("HEAD").unwrap();
        assert!(
            changed.contains(&PathBuf::from("apps/api/main.go")),
            "{changed:?}"
        );
    }

    #[test]
    fn changed_files_picks_up_untracked_files() {
        let tmp = init_repo();
        write(
            tmp.path(),
            "apps/web/src/App.tsx",
            "export default () => <div/>;",
        );

        let diff = GitDiff::new(tmp.path());
        let changed = diff.changed_files("HEAD").unwrap();
        assert!(
            changed.contains(&PathBuf::from("apps/web/src/App.tsx")),
            "{changed:?}"
        );
    }

    #[test]
    fn changed_files_picks_up_staged_files_not_yet_committed() {
        let tmp = init_repo();
        write(tmp.path(), "staged.txt", "hello\n");
        git(tmp.path(), &["add", "staged.txt"]);

        let diff = GitDiff::new(tmp.path());
        let changed = diff.changed_files("HEAD").unwrap();
        assert!(
            changed.contains(&PathBuf::from("staged.txt")),
            "{changed:?}"
        );
    }

    #[test]
    fn changed_dirs_matches_only_affected_dishes() {
        let tmp = init_repo();
        write(tmp.path(), "apps/api/main.go", "package main\n");
        write(tmp.path(), "apps/web/src/App.tsx", "export {};");
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "--quiet", "-m", "two apps"]);

        // Touch only the api.
        write(tmp.path(), "apps/api/main.go", "package main\n// v2\n");

        let diff = GitDiff::new(tmp.path());
        let dirty = diff
            .changed_dirs(
                "HEAD",
                vec![PathBuf::from("apps/api"), PathBuf::from("apps/web")],
            )
            .unwrap();

        assert!(dirty.contains(&PathBuf::from("apps/api")));
        assert!(!dirty.contains(&PathBuf::from("apps/web")));
    }

    #[test]
    fn changed_dirs_does_not_confuse_prefixed_siblings() {
        // apps/api vs. apps/api-v2 share a string prefix but the second
        // isn't inside the first.
        let tmp = init_repo();
        write(tmp.path(), "apps/api/main.go", "x\n");
        write(tmp.path(), "apps/api-v2/main.go", "y\n");
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "--quiet", "-m", "two"]);

        write(tmp.path(), "apps/api-v2/main.go", "y\n// edit\n");

        let diff = GitDiff::new(tmp.path());
        let dirty = diff
            .changed_dirs(
                "HEAD",
                vec![PathBuf::from("apps/api"), PathBuf::from("apps/api-v2")],
            )
            .unwrap();

        assert!(!dirty.contains(&PathBuf::from("apps/api")));
        assert!(dirty.contains(&PathBuf::from("apps/api-v2")));
    }

    #[test]
    fn changed_files_returns_empty_when_working_tree_matches_head() {
        let tmp = init_repo();
        let diff = GitDiff::new(tmp.path());
        let changed = diff.changed_files("HEAD").unwrap();
        assert!(changed.is_empty(), "unexpected: {changed:?}");
    }

    #[test]
    fn unknown_ref_returns_error() {
        let tmp = init_repo();
        let diff = GitDiff::new(tmp.path());
        let err = diff.changed_files("nonsuch-ref").unwrap_err();
        assert!(err.to_string().contains("git diff"), "got: {err}");
    }
}
