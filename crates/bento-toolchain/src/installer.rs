//! [`Installer`] — orchestrates download, verify, extract, and atomic
//! commit of a single toolchain version.
//!
//! Network I/O happens here (via `ureq`). Retries and resumability are
//! deliberately absent — Go and Node downloads are small enough that a
//! clean re-run is fast. Revisit if a shared HTTP layer becomes
//! valuable for the remote cache too.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::store::Store;
use crate::target::Target;
use crate::tool::{ArchiveFormat, ChecksumFormat, DownloadSpec, Tool};

/// Wires a [`Store`] to a set of [`Tool`] implementations and exposes the
/// single high-level operation: "make sure this `(tool, version)` is
/// installed". Idempotent — a no-op when the version is already present.
pub struct Installer {
    store: Store,
    tools: Vec<Box<dyn Tool>>,
}

impl Installer {
    pub fn new(store: Store, tools: Vec<Box<dyn Tool>>) -> Self {
        Self { store, tools }
    }

    /// Built-in installer with the supported toolchains: Go, Node,
    /// Python (delegated to `uv python install`), and uv itself
    /// (which Python declares co-required so the install loop lays it
    /// down before Python runs).
    pub fn builtin() -> Result<Self> {
        let store = Store::new(Store::default_root()?);
        Ok(Self::new(
            store,
            vec![
                Box::new(crate::go::GoTool),
                Box::new(crate::node::NodeTool),
                Box::new(crate::python::PythonTool),
                Box::new(crate::uv::UvTool),
            ],
        ))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Look up a tool by its `name()`.
    pub fn tool(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// Ensure `<tool>` at `<version>` is installed for `target`. Returns
    /// the directory that should be prepended to `PATH` for child
    /// processes, or an error if the install failed.
    ///
    /// Idempotent: a no-op when the tool+version is already present.
    ///
    /// `version` may be a concrete version (`22.10.0`), a semver range
    /// (`^24`, `>=18`, `~22.10.0`), or anything the tool's
    /// [`Tool::resolve_version`] knows how to turn into a concrete
    /// version. Range resolution happens before the "already
    /// installed?" check so the resolved version is stable across
    /// callers with the same spec.
    pub fn ensure(&self, tool_name: &str, version: &str, target: Target) -> Result<PathBuf> {
        let tool = self
            .tool(tool_name)
            .ok_or_else(|| anyhow::anyhow!("no built-in tool registered for '{tool_name}'"))?;

        let resolved = tool
            .resolve_version(version)
            .with_context(|| format!("resolving {tool_name} version spec '{version}'"))?;

        // Delegated tools (Python via uv) own their own storage + idempotency
        // — bento doesn't manage their on-disk layout. Hand off cleanly.
        if tool.is_delegated() {
            tracing::info!(
                tool = tool_name,
                spec = version,
                version = %resolved,
                target = %target,
                "ensuring delegated toolchain",
            );
            return tool.delegated_ensure(&resolved, target);
        }

        if self.store.is_installed(tool_name, &resolved) {
            return Ok(self.store.bin_dir(tool_name, &resolved));
        }

        tracing::info!(
            tool = tool_name,
            spec = version,
            version = %resolved,
            target = %target,
            "installing toolchain",
        );

        let version = resolved.as_str();
        let spec = tool.download_spec(version, target);
        let stage = self.store.stage().with_context(|| "allocating stage dir")?;
        let stage_unwound = stage.clone();
        let result = (|| -> Result<()> {
            let archive_path = stage.join(format!("download.{}", extension_for(&spec.format)));
            download(&spec.url, &archive_path)?;

            if spec.checksum_url.is_some() {
                verify_checksum(&spec, &archive_path)?;
            }

            extract(&archive_path, &spec.format, &stage)?;
            std::fs::remove_file(&archive_path).ok();

            // Pull contents out of the wrapper dir if the tool has one.
            let final_root = match tool.extracted_wrapper_dir(version, target) {
                Some(wrapper) => stage.join(wrapper),
                None => stage.clone(),
            };
            if !final_root.is_dir() {
                anyhow::bail!(
                    "expected install root {} after extraction, but it doesn't exist",
                    final_root.display()
                );
            }

            // Tools with non-standard archive layouts (binary at wrapper
            // root rather than wrapper/bin/) restructure the tree here
            // so `<install>/bin/<binary>` lands as expected downstream.
            tool.post_extract(&final_root, version, target)
                .with_context(|| format!("post-extract for {tool_name}@{version}"))?;

            self.store.commit_stage(&final_root, tool_name, version)?;
            Ok(())
        })();

        // Clean up stage on failure.
        if result.is_err() && stage_unwound.exists() {
            std::fs::remove_dir_all(&stage_unwound).ok();
        }
        result?;

        Ok(self.store.bin_dir(tool_name, version))
    }
}

fn extension_for(fmt: &ArchiveFormat) -> &'static str {
    match fmt {
        ArchiveFormat::TarGz => "tar.gz",
    }
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("HTTP GET {url}"))?;

    let mut reader = response.into_reader();
    let file =
        File::create(dest).with_context(|| format!("creating download file {}", dest.display()))?;
    let mut writer = BufWriter::new(file);
    std::io::copy(&mut reader, &mut writer)
        .with_context(|| format!("streaming download to {}", dest.display()))?;
    writer
        .flush()
        .with_context(|| format!("flushing {}", dest.display()))?;
    Ok(())
}

fn verify_checksum(spec: &DownloadSpec, archive: &Path) -> Result<()> {
    let checksum_url = spec
        .checksum_url
        .as_deref()
        .expect("verify_checksum should not be called when checksum_url is None");

    let response = ureq::get(checksum_url)
        .call()
        .with_context(|| format!("HTTP GET {checksum_url}"))?;
    let body = response
        .into_string()
        .with_context(|| format!("reading checksum body from {checksum_url}"))?;

    let archive_filename = filename_from_url(&spec.url);
    let expected = match spec.checksum_format {
        ChecksumFormat::Plain => body
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow::anyhow!("checksum file at {checksum_url} was empty"))?
            .to_lowercase(),
        ChecksumFormat::Sha256SumsFile => find_in_sums(&body, &archive_filename)
            .ok_or_else(|| {
                anyhow::anyhow!("no checksum line for '{archive_filename}' in {checksum_url}")
            })?
            .to_lowercase(),
        ChecksumFormat::GoDevJson => find_in_go_dev_json(&body, &archive_filename)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no entry for '{archive_filename}' in Go release index ({checksum_url})"
                )
            })?
            .to_lowercase(),
    };

    let actual = sha256_file(archive)?;
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            archive.display()
        );
    }
    Ok(())
}

/// Last path segment of a URL — used to match against entries in a
/// `SHASUMS256.txt`-style checksum file.
fn filename_from_url(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

/// Walk `https://go.dev/dl/?mode=json` output for a file matching
/// `target_filename` and return its `sha256`. The document is an array of
/// releases, each with a `files: [{filename, sha256, ...}]` array.
fn find_in_go_dev_json(body: &str, target_filename: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for release in value.as_array()? {
        let files = release.get("files")?.as_array()?;
        for file in files {
            let name = file.get("filename")?.as_str()?;
            if name == target_filename {
                return Some(file.get("sha256")?.as_str()?.to_string());
            }
        }
    }
    None
}

/// Find the hex digest for `target_filename` in a SHASUMS256.txt-style body.
/// Lines look like `<hex digest>  <filename>` (two spaces is canonical, but
/// any whitespace works).
fn find_in_sums(body: &str, target_filename: &str) -> Option<String> {
    for line in body.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        // Filenames can be prefixed with `*` (binary mode marker from
        // shasum -b). Strip it.
        let name = name.trim_start_matches('*');
        if name == target_filename {
            return Some(hash.to_string());
        }
    }
    None
}

fn sha256_file(archive: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(archive)
        .with_context(|| format!("opening {} for hashing", archive.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", archive.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract(archive: &Path, fmt: &ArchiveFormat, dest: &Path) -> Result<()> {
    match fmt {
        ArchiveFormat::TarGz => {
            let file = File::open(archive)
                .with_context(|| format!("opening archive {}", archive.display()))?;
            let decoder = flate2::read::GzDecoder::new(file);
            let mut tar = tar::Archive::new(decoder);
            tar.unpack(dest)
                .with_context(|| format!("extracting tar.gz into {}", dest.display()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Arch, Os, Target};
    use crate::tool::{ArchiveFormat, DownloadSpec, Tool};

    /// In-memory tool that produces a fake archive on disk for testing the
    /// pipeline without hitting the network.
    struct FakeTool {
        name: &'static str,
        wrapper: Option<&'static str>,
    }

    impl Tool for FakeTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn download_spec(&self, _version: &str, _target: Target) -> DownloadSpec {
            DownloadSpec {
                url: "file:///does-not-matter".into(),
                checksum_url: None,
                checksum_format: ChecksumFormat::Plain,
                format: ArchiveFormat::TarGz,
            }
        }
        fn extracted_wrapper_dir(&self, _v: &str, _t: Target) -> Option<String> {
            self.wrapper.map(String::from)
        }
    }

    fn target() -> Target {
        Target::new(Os::Linux, Arch::X86_64)
    }

    #[test]
    fn ensure_is_noop_when_already_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        // Pretend it's already installed.
        let dir = store.install_dir("go", "1.22.3");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/go"), "fake").unwrap();

        let installer = Installer::new(
            store.clone(),
            vec![Box::new(FakeTool {
                name: "go",
                wrapper: Some("go"),
            })],
        );
        let bin = installer.ensure("go", "1.22.3", target()).unwrap();
        assert_eq!(bin, store.bin_dir("go", "1.22.3"));
        // We never touched the network — the FakeTool would have failed if we had.
    }

    #[test]
    fn ensure_errors_on_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let installer = Installer::new(Store::new(tmp.path()), vec![]);
        let err = installer.ensure("klingon", "1.0", target()).unwrap_err();
        assert!(err.to_string().contains("no built-in tool"), "got: {err}");
    }

    #[test]
    fn extract_tar_gz_produces_expected_tree() {
        // Build a tiny tar.gz on disk, then extract it.
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("test.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(enc);
            let payload = b"hello";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "go/bin/go", &payload[..])
                .unwrap();
            tar.finish().unwrap();
        }

        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        extract(&archive, &ArchiveFormat::TarGz, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("go/bin/go")).unwrap(), b"hello");
    }

    #[test]
    fn filename_from_url_takes_last_segment() {
        assert_eq!(
            filename_from_url("https://example.com/path/to/file.tar.gz"),
            "file.tar.gz"
        );
        assert_eq!(filename_from_url("plain"), "plain");
    }

    #[test]
    fn find_in_sums_matches_canonical_format() {
        let body =
            "abc123  node-v22.1.0-linux-x64.tar.gz\ndef456  node-v22.1.0-darwin-arm64.tar.gz\n";
        assert_eq!(
            find_in_sums(body, "node-v22.1.0-linux-x64.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            find_in_sums(body, "node-v22.1.0-darwin-arm64.tar.gz").as_deref(),
            Some("def456")
        );
        assert!(find_in_sums(body, "missing").is_none());
    }

    #[test]
    fn find_in_go_dev_json_extracts_file_hash() {
        let body = serde_json::json!([
            {
                "version": "go1.22.3",
                "files": [
                    { "filename": "go1.22.3.linux-amd64.tar.gz", "sha256": "abc123" },
                    { "filename": "go1.22.3.darwin-arm64.tar.gz", "sha256": "def456" },
                ],
            },
            {
                "version": "go1.22.4",
                "files": [
                    { "filename": "go1.22.4.linux-amd64.tar.gz", "sha256": "ghi789" },
                ],
            },
        ])
        .to_string();
        assert_eq!(
            find_in_go_dev_json(&body, "go1.22.3.linux-amd64.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            find_in_go_dev_json(&body, "go1.22.4.linux-amd64.tar.gz").as_deref(),
            Some("ghi789")
        );
        assert!(find_in_go_dev_json(&body, "go9.9.9.linux-amd64.tar.gz").is_none());
    }

    #[test]
    fn find_in_go_dev_json_handles_garbage_body() {
        assert!(find_in_go_dev_json("not-json", "anything").is_none());
        assert!(find_in_go_dev_json("{}", "anything").is_none());
    }

    #[test]
    fn find_in_sums_strips_binary_marker_asterisk() {
        // shasum -b output uses '<hash> *<filename>'.
        let body = "abc123 *node-v22.1.0-linux-x64.tar.gz\n";
        assert_eq!(
            find_in_sums(body, "node-v22.1.0-linux-x64.tar.gz").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn sha256_file_matches_known_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"hello world").unwrap();
        // SHA-256 of "hello world".
        assert_eq!(
            sha256_file(&path).unwrap(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
