//! Go toolchain — downloads from `go.dev/dl/`.

use crate::target::Target;
use crate::tool::{ArchiveFormat, ChecksumFormat, DownloadSpec, Tool};

pub struct GoTool;

impl Tool for GoTool {
    fn name(&self) -> &'static str {
        "go"
    }

    fn download_spec(&self, version: &str, target: Target) -> DownloadSpec {
        // https://go.dev/dl/go1.22.3.linux-amd64.tar.gz
        let stem = format!(
            "go{version}.{os}-{arch}",
            os = target.os.slug(),
            arch = target.arch.go_slug()
        );
        let url = format!("https://go.dev/dl/{stem}.tar.gz");
        DownloadSpec {
            // go.dev does NOT serve per-asset .sha256 files; the checksums
            // live in the JSON release index instead.
            checksum_url: Some("https://go.dev/dl/?mode=json&include=all".to_string()),
            checksum_format: ChecksumFormat::GoDevJson,
            url,
            format: ArchiveFormat::TarGz,
        }
    }

    fn extracted_wrapper_dir(&self, _version: &str, _target: Target) -> Option<String> {
        // Every Go tarball extracts to `go/`.
        Some("go".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Arch, Os};

    #[test]
    fn name_is_go() {
        assert_eq!(GoTool.name(), "go");
    }

    #[test]
    fn linux_x86_64_url() {
        let t = Target::new(Os::Linux, Arch::X86_64);
        let spec = GoTool.download_spec("1.22.3", t);
        assert_eq!(spec.url, "https://go.dev/dl/go1.22.3.linux-amd64.tar.gz");
        // go.dev does not host per-asset .sha256 files; the index lives
        // in the JSON release feed instead.
        assert_eq!(
            spec.checksum_url.as_deref(),
            Some("https://go.dev/dl/?mode=json&include=all")
        );
        assert_eq!(spec.checksum_format, ChecksumFormat::GoDevJson);
        assert_eq!(spec.format, ArchiveFormat::TarGz);
    }

    #[test]
    fn darwin_aarch64_url() {
        let t = Target::new(Os::Darwin, Arch::Aarch64);
        let spec = GoTool.download_spec("1.23.0", t);
        assert_eq!(spec.url, "https://go.dev/dl/go1.23.0.darwin-arm64.tar.gz");
    }

    #[test]
    fn linux_aarch64_url() {
        let t = Target::new(Os::Linux, Arch::Aarch64);
        let spec = GoTool.download_spec("1.22.3", t);
        assert_eq!(spec.url, "https://go.dev/dl/go1.22.3.linux-arm64.tar.gz");
    }

    #[test]
    fn extracted_wrapper_is_always_go() {
        let t = Target::new(Os::Linux, Arch::X86_64);
        assert_eq!(
            GoTool.extracted_wrapper_dir("1.22.3", t),
            Some("go".to_string())
        );
    }
}
