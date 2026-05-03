//! Node toolchain — downloads from `nodejs.org/dist/`.

use anyhow::{anyhow, Context, Result};
use semver::{Version, VersionReq};

use crate::target::Target;
use crate::tool::{ArchiveFormat, ChecksumFormat, DownloadSpec, Tool};

/// URL of the index document that lists every published Node release.
/// Separated out so tests can exercise [`select_latest_matching`]
/// against a fixture without needing network access.
const NODEJS_INDEX_URL: &str = "https://nodejs.org/dist/index.json";

pub struct NodeTool;

impl NodeTool {
    fn stem(&self, version: &str, target: Target) -> String {
        format!(
            "node-v{version}-{os}-{arch}",
            os = target.os.slug(),
            arch = target.arch.node_slug()
        )
    }
}

impl Tool for NodeTool {
    fn name(&self) -> &'static str {
        "node"
    }

    fn resolve_version(&self, spec: &str) -> Result<String> {
        // Fast path: the spec is already a concrete major.minor.patch
        // version. This is the common case for specs coming out of
        // `.nvmrc`, `.node-version`, or an explicit `[toolchain]` pin.
        let normalized = normalize_range(spec);
        if let Ok(v) = Version::parse(normalized.trim_start_matches(['v', 'V'])) {
            return Ok(v.to_string());
        }

        // Anything else gets resolved through the Node distribution
        // index. Typical case: `engines.node = "^24"` in package.json.
        let req = VersionReq::parse(&normalized)
            .with_context(|| format!("parsing node version requirement '{spec}'"))?;

        let body = ureq::get(NODEJS_INDEX_URL)
            .call()
            .with_context(|| format!("HTTP GET {NODEJS_INDEX_URL}"))?
            .into_string()
            .context("reading Node distribution index body")?;

        select_latest_matching(&body, &req)
            .with_context(|| format!("no Node release matches version requirement '{spec}'"))
    }

    fn download_spec(&self, version: &str, target: Target) -> DownloadSpec {
        // https://nodejs.org/dist/v22.1.0/node-v22.1.0-linux-x64.tar.gz
        // https://nodejs.org/dist/v22.1.0/SHASUMS256.txt
        let stem = self.stem(version, target);
        let url = format!("https://nodejs.org/dist/v{version}/{stem}.tar.gz");
        let checksum_url = format!("https://nodejs.org/dist/v{version}/SHASUMS256.txt");
        DownloadSpec {
            url,
            checksum_url: Some(checksum_url),
            checksum_format: ChecksumFormat::Sha256SumsFile,
            format: ArchiveFormat::TarGz,
        }
    }

    fn extracted_wrapper_dir(&self, version: &str, target: Target) -> Option<String> {
        // Node's tarball wraps everything in node-v<version>-<os>-<arch>/.
        Some(self.stem(version, target))
    }
}

/// Rewrite common npm-flavoured range tokens into forms the `semver`
/// crate accepts. Covers the cases that show up in real `engines.node`
/// or `[toolchain]` fields but don't parse as Cargo-style semver: `x`
/// placeholders (`24.x`, `24.x.x`) and space-separated intersections
/// (`>=18 <21`).
fn normalize_range(spec: &str) -> String {
    let spec = spec.trim();
    // `24.x`, `24.x.x`, `24.*`, `24.*.*` → `^24` (match the major).
    // Only the two-or-three-segment shape; we bail on anything weirder.
    if let Some(major) = strip_x_range(spec) {
        return format!("^{major}");
    }
    // `>=18 <21` style: semver wants the requirements comma-separated.
    // Only rewrite when every whitespace-split segment already starts
    // with a known range operator; otherwise leave the spec alone so
    // we surface a parse error instead of silently mangling it.
    if spec.contains(char::is_whitespace) {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        if parts.iter().all(|p| starts_with_range_op(p)) {
            return parts.join(",");
        }
    }
    spec.to_string()
}

fn starts_with_range_op(s: &str) -> bool {
    s.starts_with(">=")
        || s.starts_with("<=")
        || s.starts_with('>')
        || s.starts_with('<')
        || s.starts_with('^')
        || s.starts_with('~')
        || s.starts_with('=')
}

/// If `spec` is `N.x`, `N.x.x`, `N.*`, or `N.*.*` (with optional
/// leading `v`), return the major as a string. Otherwise `None`.
fn strip_x_range(spec: &str) -> Option<&str> {
    let body = spec.trim_start_matches(['v', 'V']);
    let mut parts = body.split('.');
    let major = parts.next()?;
    if major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() || rest.len() > 2 {
        return None;
    }
    if rest.iter().all(|p| matches!(*p, "x" | "X" | "*")) {
        Some(major)
    } else {
        None
    }
}

/// Pick the highest version in `index_body` (the JSON served at
/// [`NODEJS_INDEX_URL`]) that satisfies `req`. Pure: takes the body
/// as a string so tests can drive it with a fixture.
fn select_latest_matching(index_body: &str, req: &VersionReq) -> Result<String> {
    // The index is an array of objects; only the `version` field matters
    // here (format: "vMAJOR.MINOR.PATCH"). Anything else — lts labels,
    // security flags, npm bundle metadata — we ignore by design.
    #[derive(serde::Deserialize)]
    struct Entry {
        version: String,
    }

    let entries: Vec<Entry> =
        serde_json::from_str(index_body).context("parsing Node distribution index JSON")?;

    let best = entries
        .into_iter()
        .filter_map(|e| {
            let stripped = e.version.trim_start_matches('v');
            Version::parse(stripped).ok()
        })
        .filter(|v| req.matches(v))
        .max()
        .ok_or_else(|| anyhow!("no version in index satisfies requirement"))?;
    Ok(best.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Arch, Os};

    #[test]
    fn name_is_node() {
        assert_eq!(NodeTool.name(), "node");
    }

    #[test]
    fn linux_x86_64_url() {
        let t = Target::new(Os::Linux, Arch::X86_64);
        let spec = NodeTool.download_spec("22.1.0", t);
        assert_eq!(
            spec.url,
            "https://nodejs.org/dist/v22.1.0/node-v22.1.0-linux-x64.tar.gz"
        );
        assert_eq!(
            spec.checksum_url.as_deref(),
            Some("https://nodejs.org/dist/v22.1.0/SHASUMS256.txt")
        );
        assert_eq!(spec.checksum_format, ChecksumFormat::Sha256SumsFile);
        assert_eq!(spec.format, ArchiveFormat::TarGz);
    }

    #[test]
    fn darwin_aarch64_url() {
        let t = Target::new(Os::Darwin, Arch::Aarch64);
        let spec = NodeTool.download_spec("22.1.0", t);
        assert_eq!(
            spec.url,
            "https://nodejs.org/dist/v22.1.0/node-v22.1.0-darwin-arm64.tar.gz"
        );
    }

    #[test]
    fn extracted_wrapper_matches_archive_stem() {
        let t = Target::new(Os::Linux, Arch::X86_64);
        assert_eq!(
            NodeTool.extracted_wrapper_dir("22.1.0", t),
            Some("node-v22.1.0-linux-x64".to_string())
        );
    }

    #[test]
    fn resolve_concrete_version_passes_through() {
        // No network traffic — Version::parse is enough, no index hit.
        assert_eq!(NodeTool.resolve_version("22.1.0").unwrap(), "22.1.0");
        assert_eq!(NodeTool.resolve_version("v22.1.0").unwrap(), "22.1.0");
        assert_eq!(NodeTool.resolve_version("  22.10.0 ").unwrap(), "22.10.0");
    }

    #[test]
    fn normalize_range_rewrites_x_placeholders() {
        assert_eq!(normalize_range("24.x"), "^24");
        assert_eq!(normalize_range("24.x.x"), "^24");
        assert_eq!(normalize_range("24.*"), "^24");
        assert_eq!(normalize_range("24.*.*"), "^24");
        assert_eq!(normalize_range("v24.x"), "^24");
    }

    #[test]
    fn normalize_range_rewrites_whitespace_intersections() {
        assert_eq!(normalize_range(">=18 <21"), ">=18,<21");
        assert_eq!(normalize_range(">=22.10.0 <24"), ">=22.10.0,<24");
    }

    #[test]
    fn normalize_range_leaves_parseable_forms_alone() {
        for s in ["^24", "~24.0.0", ">=18.0.0", "=22.10.0", "22.1.0"] {
            assert_eq!(normalize_range(s), s);
        }
    }

    #[test]
    fn normalize_range_refuses_to_guess_on_unknown_whitespace() {
        // "18 to 21" is not a pair of range ops — leave it untouched so
        // the caller gets a real parse error rather than a silently
        // wrong rewrite.
        assert_eq!(normalize_range("18 to 21"), "18 to 21");
    }

    fn fixture_index() -> &'static str {
        // Trimmed but structurally identical to the live index: the
        // resolver only reads `version`, so everything else can be
        // omitted safely.
        r#"[
            {"version":"v24.2.0"},
            {"version":"v24.1.0"},
            {"version":"v24.0.0"},
            {"version":"v22.10.1"},
            {"version":"v22.10.0"},
            {"version":"v22.9.0"},
            {"version":"v20.18.0"}
        ]"#
    }

    #[test]
    fn select_latest_matching_picks_highest_in_caret_range() {
        let req = VersionReq::parse("^24").unwrap();
        assert_eq!(
            select_latest_matching(fixture_index(), &req).unwrap(),
            "24.2.0"
        );
    }

    #[test]
    fn select_latest_matching_picks_highest_in_tilde_range() {
        let req = VersionReq::parse("~22.10.0").unwrap();
        assert_eq!(
            select_latest_matching(fixture_index(), &req).unwrap(),
            "22.10.1"
        );
    }

    #[test]
    fn select_latest_matching_honours_intersection() {
        let req = VersionReq::parse(">=22,<24").unwrap();
        assert_eq!(
            select_latest_matching(fixture_index(), &req).unwrap(),
            "22.10.1"
        );
    }

    #[test]
    fn select_latest_matching_errors_when_nothing_fits() {
        let req = VersionReq::parse("^99").unwrap();
        let err = select_latest_matching(fixture_index(), &req).unwrap_err();
        assert!(err.to_string().contains("no version in index satisfies"));
    }

    /// Hits `nodejs.org/dist/index.json` — ignored by default so the
    /// regular test matrix never depends on the public network. Run
    /// manually with `cargo test -p bento-toolchain --
    /// node::tests::resolve_range_end_to_end --ignored --nocapture`
    /// after changing anything in the resolver.
    #[test]
    #[ignore = "requires network"]
    fn resolve_range_end_to_end() {
        let resolved = NodeTool.resolve_version("^24").expect("resolve ^24");
        let v = Version::parse(&resolved).expect("resolved should be concrete semver");
        assert_eq!(v.major, 24, "expected a 24.x release, got {resolved}");
    }
}
