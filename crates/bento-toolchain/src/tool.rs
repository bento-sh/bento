//! [`Tool`] trait — the per-tool extension point for downloads.
//!
//! Each language toolchain (Go, Node, Bun, …) implements this trait with
//! the URL pattern, archive format, and any post-extract layout knowledge
//! needed to install one of its versions onto disk.

use std::path::PathBuf;

use anyhow::Result;

use crate::target::Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// `.tar.gz` — used by Go, Node (gzip variant), and most Linux
    /// distributions of compiled tools.
    TarGz,
}

/// Shape of the body served at a [`DownloadSpec::checksum_url`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChecksumFormat {
    /// Body is just the hex digest (optionally followed by whitespace and
    /// a filename).
    #[default]
    Plain,
    /// Body is one line per file in the form `<hex digest>  <filename>`.
    /// The verifier picks the line whose filename matches the asset.
    /// Node's `SHASUMS256.txt` uses this format.
    Sha256SumsFile,
    /// Body is the JSON document served by `https://go.dev/dl/?mode=json`
    /// (or `…&include=all` for older versions). Each release lists its
    /// files with `filename` and `sha256` fields; the verifier walks the
    /// document for a filename match.
    GoDevJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSpec {
    pub url: String,
    /// Optional URL serving the SHA-256 checksum for `url`. None = skip
    /// checksum verification (still atomic via stage-then-rename).
    pub checksum_url: Option<String>,
    /// Format of the checksum body. Only meaningful when `checksum_url`
    /// is set.
    pub checksum_format: ChecksumFormat,
    pub format: ArchiveFormat,
}

pub trait Tool: Send + Sync {
    /// Stable identifier — the key used in `[toolchain]` blocks.
    /// Examples: `"go"`, `"node"`, `"python"`.
    fn name(&self) -> &'static str;

    /// Turn a user- or adapter-supplied version spec into a concrete
    /// `major.minor.patch` string suitable for [`Self::download_spec`].
    ///
    /// The default is a pass-through — tools whose version spec is
    /// already concrete (Go's `1.22.3`) don't need to override. Node
    /// overrides this because its adapters commonly return npm-style
    /// ranges (`^24`, `>=22`) that the upstream distribution server
    /// can't resolve.
    ///
    /// Called by [`Installer::ensure`] before the "already installed?"
    /// check, so every subsequent step sees a concrete version.
    fn resolve_version(&self, spec: &str) -> Result<String> {
        Ok(spec.to_string())
    }

    /// True when this tool delegates installation to an external CLI
    /// (e.g. `python` via `uv python install`) instead of using bento's
    /// own download/extract path. When true, [`Installer::ensure`] calls
    /// [`Self::delegated_ensure`] and skips [`Self::download_spec`] /
    /// [`Self::extracted_wrapper_dir`] entirely.
    fn is_delegated(&self) -> bool {
        false
    }

    /// For delegated tools: ensure `version` is installed via the
    /// external CLI and return the bin dir to prepend to child PATH.
    /// Idempotent — the implementation handles "already installed"
    /// short-circuiting.
    ///
    /// Default panics. Only called when [`Self::is_delegated`] is true,
    /// so non-delegated tools never need to override.
    fn delegated_ensure(&self, version: &str, target: Target) -> Result<PathBuf> {
        let _ = (version, target);
        unreachable!(
            "delegated_ensure called on non-delegated tool {}",
            self.name()
        )
    }

    /// Where to download `version` for `target` from. Direct-download
    /// tools (Go, Node) override this; delegated tools (Python via uv)
    /// can panic since the installer never reaches this codepath when
    /// [`Self::is_delegated`] is true.
    fn download_spec(&self, version: &str, target: Target) -> DownloadSpec;

    /// Most distributions wrap their files in a top-level dir inside the
    /// archive (e.g. Go's tarball extracts to `go/`, Node's to
    /// `node-v22.1.0-linux-x64/`). Returning that name lets the installer
    /// strip the wrapper so `<install_dir>/bin/<binary>` ends up at the
    /// expected layout.
    ///
    /// Return `None` when the archive contents already sit at the root.
    fn extracted_wrapper_dir(&self, version: &str, target: Target) -> Option<String>;
}
