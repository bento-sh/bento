//! Filesystem layout for installed toolchains.
//!
//! Layout:
//!
//! ```text
//! <root>/
//!   <tool>/
//!     <version>/        ← canonical install dir
//!       bin/<binary>
//!       … rest of the tool's tree
//!   .stage/             ← scratch space for in-flight installs
//!     <random>/
//! ```
//!
//! Installs are atomic: they happen in `.stage/<random>/` and only get
//! renamed into `<tool>/<version>/` after extraction + checksum succeed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default root: `$HOME/.bento/tools/`.
    pub fn default_root() -> Result<PathBuf> {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        Ok(home.join(".bento").join("tools"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn install_dir(&self, tool: &str, version: &str) -> PathBuf {
        self.root.join(tool).join(version)
    }

    pub fn bin_dir(&self, tool: &str, version: &str) -> PathBuf {
        self.install_dir(tool, version).join("bin")
    }

    pub fn is_installed(&self, tool: &str, version: &str) -> bool {
        let dir = self.install_dir(tool, version);
        dir.is_dir()
            && dir
                .read_dir()
                .map(|mut it| it.next().is_some())
                .unwrap_or(false)
    }

    /// Allocate a fresh staging directory under `<root>/.stage/<n>/`.
    /// Caller extracts into it; on success they call [`commit_stage`].
    pub fn stage(&self) -> Result<PathBuf> {
        let stage_root = self.root.join(".stage");
        std::fs::create_dir_all(&stage_root)
            .with_context(|| format!("creating stage root {}", stage_root.display()))?;

        // Use a per-process counter for the stage dir name. We don't need
        // crypto-strong randomness; collisions just retry.
        for _ in 0..16 {
            let name = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let path = stage_root.join(&name);
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("creating stage dir {}", path.display())));
                }
            }
        }
        anyhow::bail!(
            "could not allocate a unique stage dir under {}",
            stage_root.display()
        )
    }

    /// Atomically promote a staged install into the canonical location.
    /// `staged_root` is what the caller wants to become `<tool>/<version>/`.
    /// If the target already exists, the staged tree is removed (idempotent).
    pub fn commit_stage(&self, staged_root: &Path, tool: &str, version: &str) -> Result<()> {
        let dest = self.install_dir(tool, version);
        if dest.exists() {
            // Already installed (a concurrent install won the race).
            std::fs::remove_dir_all(staged_root).ok();
            return Ok(());
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent {}", parent.display()))?;
        }
        std::fs::rename(staged_root, &dest).with_context(|| {
            format!(
                "promoting staged install {} → {}",
                staged_root.display(),
                dest.display()
            )
        })?;
        Ok(())
    }

    /// Every (tool, version) pair currently installed.
    pub fn list(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        for tool_entry in std::fs::read_dir(&self.root)? {
            let tool_entry = tool_entry?;
            let tool = match tool_entry.file_name().into_string() {
                Ok(s) if !s.starts_with('.') => s,
                _ => continue,
            };
            let tool_dir = tool_entry.path();
            if !tool_dir.is_dir() {
                continue;
            }
            for version_entry in std::fs::read_dir(&tool_dir)? {
                let version_entry = version_entry?;
                if let Ok(version) = version_entry.file_name().into_string() {
                    if version_entry.path().is_dir() {
                        out.push((tool.clone(), version));
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("tools"));
        (tmp, store)
    }

    #[test]
    fn install_dir_layout() {
        let store = Store::new("/cache/tools");
        assert_eq!(
            store.install_dir("go", "1.22.3"),
            PathBuf::from("/cache/tools/go/1.22.3")
        );
        assert_eq!(
            store.bin_dir("go", "1.22.3"),
            PathBuf::from("/cache/tools/go/1.22.3/bin")
        );
    }

    #[test]
    fn is_installed_false_for_missing() {
        let (_tmp, store) = fresh();
        assert!(!store.is_installed("go", "1.22.3"));
    }

    #[test]
    fn is_installed_true_when_dir_has_contents() {
        let (_tmp, store) = fresh();
        let dir = store.install_dir("go", "1.22.3");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/go"), "fake binary").unwrap();
        assert!(store.is_installed("go", "1.22.3"));
    }

    #[test]
    fn is_installed_false_for_empty_dir() {
        let (_tmp, store) = fresh();
        std::fs::create_dir_all(store.install_dir("go", "1.22.3")).unwrap();
        assert!(!store.is_installed("go", "1.22.3"));
    }

    #[test]
    fn stage_allocates_unique_dir_under_root() {
        let (_tmp, store) = fresh();
        let s1 = store.stage().unwrap();
        let s2 = store.stage().unwrap();
        assert_ne!(s1, s2);
        assert!(s1.starts_with(store.root().join(".stage")));
        assert!(s2.starts_with(store.root().join(".stage")));
    }

    #[test]
    fn commit_stage_atomically_promotes() {
        let (_tmp, store) = fresh();
        let stage = store.stage().unwrap();
        std::fs::create_dir_all(stage.join("bin")).unwrap();
        std::fs::write(stage.join("bin/go"), "binary").unwrap();

        store.commit_stage(&stage, "go", "1.22.3").unwrap();

        let installed = store.install_dir("go", "1.22.3");
        assert!(installed.is_dir());
        assert_eq!(std::fs::read(installed.join("bin/go")).unwrap(), b"binary");
        assert!(!stage.exists(), "stage dir should be gone after commit");
    }

    #[test]
    fn commit_stage_idempotent_when_dest_exists() {
        let (_tmp, store) = fresh();
        // Pre-existing install.
        let dest = store.install_dir("go", "1.22.3");
        std::fs::create_dir_all(dest.join("bin")).unwrap();
        std::fs::write(dest.join("bin/go"), "incumbent").unwrap();

        let stage = store.stage().unwrap();
        std::fs::create_dir_all(stage.join("bin")).unwrap();
        std::fs::write(stage.join("bin/go"), "loser").unwrap();

        store.commit_stage(&stage, "go", "1.22.3").unwrap();

        // Incumbent untouched.
        assert_eq!(std::fs::read(dest.join("bin/go")).unwrap(), b"incumbent");
        // Loser stage cleaned up.
        assert!(!stage.exists());
    }

    #[test]
    fn list_enumerates_installed_versions() {
        let (_tmp, store) = fresh();
        for (tool, version) in [("go", "1.22.3"), ("go", "1.23.0"), ("node", "22.1.0")] {
            let dir = store.install_dir(tool, version);
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin/x"), "").unwrap();
        }

        let listed = store.list().unwrap();
        assert_eq!(
            listed,
            vec![
                ("go".to_string(), "1.22.3".to_string()),
                ("go".to_string(), "1.23.0".to_string()),
                ("node".to_string(), "22.1.0".to_string()),
            ]
        );
    }

    #[test]
    fn list_skips_dotted_dirs() {
        let (_tmp, store) = fresh();
        // .stage/ is a real internal dir; should not appear in list().
        store.stage().unwrap();
        let listed = store.list().unwrap();
        assert!(listed.is_empty());
    }
}
