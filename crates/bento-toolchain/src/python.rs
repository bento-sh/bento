//! [`PythonTool`] — Python toolchain via `uv`.
//!
//! Bento doesn't directly download Python — building / packaging
//! interpreter releases for every platform is uv's job, and uv already
//! does it well (faster than pyenv, includes Indygreg's pre-built
//! distributions, handles macOS / Linux / Windows). Instead, bento
//! delegates: when a dish pins `[toolchain] python = "3.12"`, bento
//! calls `uv python install 3.12`, then asks uv where the interpreter
//! landed so the dish's child processes get it on `PATH`.
//!
//! Same shape as the `python-uv` language adapter on the package-
//! management side: bento's value is in *routing to uv idiomatically*,
//! not in re-implementing what uv already does.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::target::Target;
use crate::tool::{ArchiveFormat, ChecksumFormat, DownloadSpec, Tool};

pub struct PythonTool;

impl Tool for PythonTool {
    fn name(&self) -> &'static str {
        "python"
    }

    fn is_delegated(&self) -> bool {
        true
    }

    fn delegated_ensure(&self, version: &str, _target: Target) -> Result<PathBuf> {
        // Probe for uv first so the error is friendly when it's missing.
        // Headless CI hosts that haven't bootstrapped uv get a clear
        // installation hint instead of a cryptic ENOENT.
        if !uv_is_available() {
            anyhow::bail!(
                "Python toolchain installation requires `uv` on PATH. \
                 Install via `curl -LsSf https://astral.sh/uv/install.sh | sh` \
                 (or `brew install uv`), then retry. Alternatively pin \
                 `[toolchain] use_system = true` in your dish.toml to skip \
                 toolchain management entirely."
            );
        }

        // `uv python install` is idempotent: a no-op when the version is
        // already present. Cheaper to always call than to probe first.
        let install = Command::new("uv")
            .args(["python", "install", version])
            .output()
            .with_context(|| format!("running `uv python install {version}`"))?;
        if !install.status.success() {
            let stderr = String::from_utf8_lossy(&install.stderr);
            anyhow::bail!(
                "`uv python install {version}` failed (exit {}): {}",
                install.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }

        // Resolve the interpreter path uv just wrote.
        let find = Command::new("uv")
            .args(["python", "find", version])
            .output()
            .with_context(|| format!("running `uv python find {version}`"))?;
        if !find.status.success() {
            let stderr = String::from_utf8_lossy(&find.stderr);
            anyhow::bail!(
                "`uv python find {version}` failed after install (exit {}): {}",
                find.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }
        let interpreter_path = String::from_utf8(find.stdout)
            .context("`uv python find` stdout not valid UTF-8")?
            .trim()
            .to_string();
        if interpreter_path.is_empty() {
            anyhow::bail!("`uv python find {version}` returned empty stdout");
        }

        let bin_dir = PathBuf::from(&interpreter_path)
            .parent()
            .ok_or_else(|| anyhow::anyhow!("interpreter path {interpreter_path:?} has no parent"))?
            .to_path_buf();
        Ok(bin_dir)
    }

    fn download_spec(&self, _version: &str, _target: Target) -> DownloadSpec {
        // Python is delegated — Installer::ensure() never calls this.
        // The default-feature impl returns a sentinel that would fail
        // loudly if the contract is ever broken.
        DownloadSpec {
            url: String::new(),
            checksum_url: None,
            checksum_format: ChecksumFormat::Plain,
            format: ArchiveFormat::TarGz,
        }
    }

    fn extracted_wrapper_dir(&self, _version: &str, _target: Target) -> Option<String> {
        None
    }
}

fn uv_is_available() -> bool {
    Command::new("uv")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Arch, Os, Target};

    fn target() -> Target {
        Target {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    #[test]
    fn name_is_python() {
        assert_eq!(PythonTool.name(), "python");
    }

    #[test]
    fn is_delegated_true() {
        assert!(PythonTool.is_delegated());
    }

    #[test]
    fn delegated_ensure_errors_helpfully_when_uv_missing() {
        // Skip the test when uv IS on PATH — the error path is the only
        // thing we can deterministically exercise without coupling tests
        // to a specific Python install state.
        if uv_is_available() {
            eprintln!("skipping: uv is on PATH, error path not exercised");
            return;
        }
        let err = PythonTool
            .delegated_ensure("3.12", target())
            .expect_err("expected error when uv missing");
        let msg = format!("{err}");
        assert!(
            msg.contains("uv"),
            "error should mention uv requirement: {msg}"
        );
        assert!(
            msg.contains("astral.sh")
                || msg.contains("brew install uv")
                || msg.contains("use_system"),
            "error should hint at install or fallback: {msg}"
        );
    }

    #[test]
    fn download_spec_returns_sentinel() {
        // The contract is "delegated tools never go through download".
        // We don't promise download_spec returns anything useful; just
        // that calling it doesn't panic.
        let spec = PythonTool.download_spec("3.12", target());
        assert!(spec.url.is_empty());
    }
}
