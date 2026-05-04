//! [`UvTool`] — astral-sh/uv installs from GitHub releases.
//!
//! uv is a co-required tool for [`crate::PythonTool`]: bento delegates
//! Python interpreter installation to `uv python install`, so uv has
//! to land first. The installer downloads the standalone tarball
//! (uv ships rust-built single-file binaries — no bootstrap dance).
//!
//! Archive layout: the upstream tarball extracts to
//! `uv-<rust-triple>/uv` + `uv-<rust-triple>/uvx`, with the binaries
//! at the wrapper-dir root. Bento's store expects
//! `<install>/bin/<binary>`, so [`UvTool::post_extract`] synthesises a
//! `bin/` and moves both binaries into it.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::target::{Arch, Os, Target};
use crate::tool::{ArchiveFormat, ChecksumFormat, DownloadSpec, Tool};

pub struct UvTool;

impl UvTool {
    /// Map a bento [`Target`] to uv's release asset suffix. uv names
    /// assets after Rust target triples (`uv-x86_64-unknown-linux-gnu`,
    /// `uv-aarch64-apple-darwin`, …).
    fn triple(target: Target) -> &'static str {
        match (target.os, target.arch) {
            (Os::Linux, Arch::X86_64) => "x86_64-unknown-linux-gnu",
            (Os::Linux, Arch::Aarch64) => "aarch64-unknown-linux-gnu",
            (Os::Darwin, Arch::X86_64) => "x86_64-apple-darwin",
            (Os::Darwin, Arch::Aarch64) => "aarch64-apple-darwin",
        }
    }

    fn stem(_version: &str, target: Target) -> String {
        // version intentionally unused: uv ships per-target
        // archives keyed only on the rust triple. The `_version`
        // parameter exists to mirror the `Tool::download_spec` /
        // `Tool::extracted_wrapper_dir` call sites for symmetry.
        format!("uv-{}", Self::triple(target))
    }
}

impl Tool for UvTool {
    fn name(&self) -> &'static str {
        "uv"
    }

    fn download_spec(&self, version: &str, target: Target) -> DownloadSpec {
        // https://github.com/astral-sh/uv/releases/download/<version>/uv-<triple>.tar.gz
        // Each asset has a sibling `<asset>.sha256` file (Plain hex).
        let stem = Self::stem(version, target);
        let url =
            format!("https://github.com/astral-sh/uv/releases/download/{version}/{stem}.tar.gz");
        let checksum_url = format!("{url}.sha256");
        DownloadSpec {
            url,
            checksum_url: Some(checksum_url),
            checksum_format: ChecksumFormat::Plain,
            format: ArchiveFormat::TarGz,
        }
    }

    fn extracted_wrapper_dir(&self, version: &str, target: Target) -> Option<String> {
        Some(Self::stem(version, target))
    }

    fn post_extract(&self, root: &Path, _version: &str, _target: Target) -> Result<()> {
        // Wrapper-dir layout: `uv` + `uvx` at the root. Move both into
        // a synthesised `bin/` so the store's
        // `<install>/bin/<binary>` invariant holds.
        let bin = root.join("bin");
        fs::create_dir_all(&bin)
            .with_context(|| format!("creating {} for uv binaries", bin.display()))?;
        for binary in ["uv", "uvx"] {
            let src = root.join(binary);
            if !src.exists() {
                // `uvx` was added in 0.4.0; older releases ship `uv`
                // only. Tolerate the missing case so an old pin still
                // installs cleanly — uv alone is what python.rs needs.
                continue;
            }
            let dst = bin.join(binary);
            fs::rename(&src, &dst)
                .with_context(|| format!("moving {} → {}", src.display(), dst.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Arch, Os};

    #[test]
    fn name_is_uv() {
        assert_eq!(UvTool.name(), "uv");
    }

    #[test]
    fn linux_x86_64_url() {
        let t = Target::new(Os::Linux, Arch::X86_64);
        let spec = UvTool.download_spec("0.5.0", t);
        assert_eq!(
            spec.url,
            "https://github.com/astral-sh/uv/releases/download/0.5.0/uv-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            spec.checksum_url.as_deref(),
            Some("https://github.com/astral-sh/uv/releases/download/0.5.0/uv-x86_64-unknown-linux-gnu.tar.gz.sha256")
        );
        assert_eq!(spec.checksum_format, ChecksumFormat::Plain);
        assert_eq!(spec.format, ArchiveFormat::TarGz);
    }

    #[test]
    fn darwin_aarch64_url() {
        let t = Target::new(Os::Darwin, Arch::Aarch64);
        let spec = UvTool.download_spec("0.5.0", t);
        assert!(spec.url.ends_with("uv-aarch64-apple-darwin.tar.gz"));
    }

    #[test]
    fn extracted_wrapper_matches_archive_stem() {
        let t = Target::new(Os::Linux, Arch::X86_64);
        assert_eq!(
            UvTool.extracted_wrapper_dir("0.5.0", t),
            Some("uv-x86_64-unknown-linux-gnu".to_string())
        );
    }

    #[test]
    fn post_extract_wraps_binaries_into_bin_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Simulate a freshly-extracted wrapper dir.
        fs::write(root.join("uv"), b"#!fake\n").unwrap();
        fs::write(root.join("uvx"), b"#!fake\n").unwrap();

        UvTool
            .post_extract(root, "0.5.0", Target::new(Os::Linux, Arch::X86_64))
            .unwrap();

        assert!(
            root.join("bin/uv").is_file(),
            "expected uv at bin/uv after post_extract"
        );
        assert!(
            root.join("bin/uvx").is_file(),
            "expected uvx at bin/uvx after post_extract"
        );
        assert!(
            !root.join("uv").exists(),
            "uv should have been moved out of root"
        );
    }

    #[test]
    fn post_extract_tolerates_missing_uvx() {
        // uvx was added in 0.4.0. An older pin shipping just `uv` still
        // installs cleanly — uv alone is what python.rs needs.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("uv"), b"#!fake\n").unwrap();

        UvTool
            .post_extract(root, "0.3.0", Target::new(Os::Linux, Arch::X86_64))
            .unwrap();

        assert!(root.join("bin/uv").is_file());
        assert!(!root.join("bin/uvx").exists());
    }
}
