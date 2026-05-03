//! Local on-disk cache.
//!
//! Layout (flat):
//!   `<root>/<key>.tar`          — bundle for a cache entry
//!   `<root>/<key>.tar.tmp`      — in-flight write (renamed atomically)

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::key::CacheKey;
use crate::manifest::InputManifest;

/// Recorded result of executing a task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Minimal persisted metadata for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Metadata {
    /// Bundle format version. Bump when the on-disk layout changes.
    version: u32,
    exit_code: i32,
}

const BUNDLE_VERSION: u32 = 1;

/// Aggregate statistics for a local cache directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub total_bytes: u64,
    /// Oldest entry's modification time (seconds since UNIX epoch),
    /// or `None` when the cache is empty.
    pub oldest_unix_seconds: Option<u64>,
    /// Newest entry's modification time (seconds since UNIX epoch),
    /// or `None` when the cache is empty.
    pub newest_unix_seconds: Option<u64>,
}

/// Local cache rooted at `<root>`.
#[derive(Debug, Clone)]
pub struct LocalCache {
    root: PathBuf,
}

impl LocalCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        self.bundle_path(key).is_file()
    }

    /// Restore an entry into `dish_dir`, returning the captured [`TaskResult`].
    /// Returns `None` if the key isn't in the cache.
    ///
    /// Bundle entries prefixed `outputs/` extract under `dish_dir`. Entries
    /// prefixed `workspace_outputs/` extract under `workspace_root` (when
    /// `Some`); when `workspace_root` is `None` they're skipped — a dish
    /// that didn't opt in to workspace-scoped outputs on write also won't
    /// produce those entries, so this only matters for mixed-mode repos
    /// where some dishes opt in and others don't.
    pub fn get(
        &self,
        key: &CacheKey,
        dish_dir: &Path,
        workspace_root: Option<&Path>,
    ) -> Result<Option<TaskResult>> {
        let bundle = self.bundle_path(key);
        if !bundle.is_file() {
            return Ok(None);
        }
        let result = extract_bundle(&bundle, dish_dir, workspace_root)
            .with_context(|| format!("extracting bundle {}", bundle.display()))?;
        Ok(Some(result))
    }

    /// Store a new entry. Bundles outputs matching `output_globs` under
    /// `<dish_dir>` plus outputs matching `workspace_output_globs` under
    /// `<workspace_root>`, alongside the task's stdout/stderr/exit code,
    /// into a tarball at `<root>/<key>.tar`. Write is atomic via rename
    /// from a `.tmp` sibling.
    ///
    /// Errors when `workspace_output_globs` is non-empty and
    /// `workspace_root` is `None` — defends against silent cache-of-nothing
    /// in contexts that can't resolve a workspace anchor.
    pub fn put(
        &self,
        key: &CacheKey,
        dish_dir: &Path,
        output_globs: &[String],
        workspace_root: Option<&Path>,
        workspace_output_globs: &[String],
        result: &TaskResult,
    ) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating cache root {}", self.root.display()))?;

        let final_path = self.bundle_path(key);
        let tmp_path = tmp_path(&final_path);

        // write() closes the file before rename — important on Windows and
        // cheap insurance elsewhere.
        write_bundle(
            &tmp_path,
            dish_dir,
            output_globs,
            workspace_root,
            workspace_output_globs,
            result,
        )
        .with_context(|| format!("writing bundle {}", tmp_path.display()))?;

        std::fs::rename(&tmp_path, &final_path).with_context(|| {
            format!("renaming {} → {}", tmp_path.display(), final_path.display())
        })?;
        Ok(())
    }

    /// Delete every cache entry (but leave the root directory in place).
    pub fn clear(&self) -> Result<()> {
        if !self.root.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Write an explanation sidecar for a cache entry. Atomic via `.tmp`
    /// + rename. Intended to be called alongside [`Self::put`].
    pub fn put_manifest(&self, key: &CacheKey, manifest: &InputManifest) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let final_path = self.manifest_path(key);
        let tmp_path = tmp_path(&final_path);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        std::fs::write(&tmp_path, &bytes)
            .with_context(|| format!("writing manifest {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path).with_context(|| {
            format!("renaming {} → {}", tmp_path.display(), final_path.display())
        })?;
        Ok(())
    }

    /// Read the manifest sidecar for a cache entry, if one exists.
    pub fn read_manifest(&self, key: &CacheKey) -> Result<Option<InputManifest>> {
        let path = self.manifest_path(key);
        if !path.is_file() {
            return Ok(None);
        }
        let raw =
            std::fs::read(&path).with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest: InputManifest = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        Ok(Some(manifest))
    }

    /// Find every committed cache key whose hex begins with `prefix`.
    /// Useful for `bento why <12-char-prefix>`.
    pub fn find_by_prefix(&self, prefix: &str) -> Result<Vec<CacheKey>> {
        if !self.root.is_dir() {
            return Ok(Vec::new());
        }
        let mut matches = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if !is_bundle(&path) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with(prefix) {
                matches.push(CacheKey::from_hex(stem));
            }
        }
        matches.sort_by(|a, b| a.as_hex().cmp(b.as_hex()));
        Ok(matches)
    }

    /// Count + byte size of committed bundles (ignores `.tmp` files),
    /// plus the modification-time range for sanity-checking cache churn.
    pub fn stats(&self) -> Result<CacheStats> {
        let mut stats = CacheStats::default();
        if !self.root.is_dir() {
            return Ok(stats);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if !is_bundle(&path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            stats.entries += 1;
            stats.total_bytes += meta.len();
            if let Ok(mtime) = meta.modified() {
                if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    let secs = dur.as_secs();
                    stats.oldest_unix_seconds =
                        Some(stats.oldest_unix_seconds.map_or(secs, |o| o.min(secs)));
                    stats.newest_unix_seconds =
                        Some(stats.newest_unix_seconds.map_or(secs, |n| n.max(secs)));
                }
            }
        }
        Ok(stats)
    }

    /// Absolute path to the on-disk tar bundle for `key`. Exposed so
    /// upper layers can hand the path to a remote cache implementation
    /// for upload, without double-serialising through `TaskResult`.
    pub fn bundle_path(&self, key: &CacheKey) -> PathBuf {
        self.root.join(format!("{}.tar", key.as_hex()))
    }

    fn manifest_path(&self, key: &CacheKey) -> PathBuf {
        self.root.join(format!("{}.inputs.json", key.as_hex()))
    }
}

fn tmp_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

fn is_bundle(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "tar")
}

fn write_bundle(
    out: &Path,
    dish_dir: &Path,
    output_globs: &[String],
    workspace_root: Option<&Path>,
    workspace_output_globs: &[String],
    result: &TaskResult,
) -> Result<()> {
    if !workspace_output_globs.is_empty() && workspace_root.is_none() {
        anyhow::bail!(
            "workspace_outputs declared but no workspace root resolved — refusing to \
             silently cache nothing"
        );
    }

    let file = File::create(out)?;
    let mut tar = tar::Builder::new(file);

    let meta = Metadata {
        version: BUNDLE_VERSION,
        exit_code: result.exit_code,
    };
    let meta_bytes = serde_json::to_vec(&meta)?;
    append_bytes(&mut tar, "meta.json", &meta_bytes)?;
    append_bytes(&mut tar, "stdout", &result.stdout)?;
    append_bytes(&mut tar, "stderr", &result.stderr)?;

    bundle_tree(&mut tar, dish_dir, output_globs, "outputs")?;
    if let Some(root) = workspace_root {
        bundle_tree(&mut tar, root, workspace_output_globs, "workspace_outputs")?;
    }

    let mut file = tar.into_inner()?;
    file.flush()?;
    Ok(())
}

/// Walk `root`, match files against `globs`, and archive matches under
/// `<archive_prefix>/<rel>` in `tar`. A no-op when `globs` is empty, so
/// callers can invoke unconditionally without dispatching on opt-in.
fn bundle_tree<W: Write>(
    tar: &mut tar::Builder<W>,
    root: &Path,
    globs: &[String],
    archive_prefix: &str,
) -> Result<()> {
    if globs.is_empty() || !root.is_dir() {
        return Ok(());
    }
    let matcher = build_matcher(globs)?;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let full = entry.path();
        let rel = match full.strip_prefix(root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if matcher.is_match(rel) {
            let archive_name = PathBuf::from(archive_prefix).join(rel);
            tar.append_path_with_name(full, archive_name)?;
        }
    }
    Ok(())
}

fn append_bytes<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, name, bytes)?;
    Ok(())
}

fn build_matcher(globs: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for g in globs {
        builder.add(globset::Glob::new(g).with_context(|| format!("compiling output glob `{g}`"))?);
    }
    Ok(builder.build()?)
}

fn extract_bundle(
    archive: &Path,
    dish_dir: &Path,
    workspace_root: Option<&Path>,
) -> Result<TaskResult> {
    let file = File::open(archive)?;
    let mut tar = tar::Archive::new(file);

    let mut result = TaskResult::default();
    let mut meta_bytes: Option<Vec<u8>> = None;

    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.to_string_lossy();

        if name == "meta.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            meta_bytes = Some(buf);
        } else if name == "stdout" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            result.stdout = buf;
        } else if name == "stderr" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            result.stderr = buf;
        } else if let Ok(rel) = path.strip_prefix("outputs") {
            unpack_at(&mut entry, dish_dir, rel)?;
        } else if let Ok(rel) = path.strip_prefix("workspace_outputs") {
            if let Some(root) = workspace_root {
                unpack_at(&mut entry, root, rel)?;
            }
            // workspace_root absent: silently skip. A non-workspace-
            // aware caller restoring a bundle that had workspace_outputs
            // is a rare cross-config scenario; skipping is safer than
            // failing the whole restore.
        }
    }

    if let Some(bytes) = meta_bytes {
        let meta: Metadata =
            serde_json::from_slice(&bytes).context("parsing cache bundle meta.json")?;
        if meta.version != BUNDLE_VERSION {
            anyhow::bail!(
                "cache bundle version {} does not match expected {}",
                meta.version,
                BUNDLE_VERSION
            );
        }
        result.exit_code = meta.exit_code;
    } else {
        anyhow::bail!("cache bundle missing meta.json");
    }

    Ok(result)
}

/// Unpack a tar entry at `root/rel`, refusing any `rel` that contains
/// `..` or an absolute component — blocks tarball-traversal bundles from
/// writing outside the anchor root.
fn unpack_at<R: Read>(entry: &mut tar::Entry<R>, root: &Path, rel: &Path) -> Result<()> {
    for component in rel.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) => {}
            _ => anyhow::bail!(
                "cache bundle entry has unsafe path component `{}` — refusing extract",
                rel.display()
            ),
        }
    }
    let dest = root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    entry.unpack(&dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Hasher;

    fn make_key(seed: &str) -> CacheKey {
        let mut h = Hasher::new();
        h.add_extra("seed", seed);
        h.finalize()
    }

    fn make_dish(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in files {
            let full = dir.path().join(name);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, bytes).unwrap();
        }
        dir
    }

    #[test]
    fn miss_returns_none_without_side_effects() {
        let cache = tempfile::tempdir().unwrap();
        let dish = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let key = make_key("x");

        let result = local.get(&key, dish.path(), None).unwrap();
        assert!(result.is_none());
        // Cache dir shouldn't be touched on miss.
        assert!(!local.contains(&key));
    }

    #[test]
    fn put_then_get_restores_outputs_and_result() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());

        let source = make_dish(&[
            ("dist/app.js", b"console.log('hi')"),
            ("dist/nested/assets/logo.svg", b"<svg/>"),
            ("src/main.ts", b"// source, not an output"),
        ]);

        let key = make_key("npm-build");
        let result = TaskResult {
            exit_code: 0,
            stdout: b"built dist/app.js\n".to_vec(),
            stderr: Vec::new(),
        };

        local
            .put(&key, source.path(), &["dist/**".into()], None, &[], &result)
            .unwrap();
        assert!(local.contains(&key));

        let restore = tempfile::tempdir().unwrap();
        let got = local.get(&key, restore.path(), None).unwrap().unwrap();

        assert_eq!(got.exit_code, 0);
        assert_eq!(got.stdout, b"built dist/app.js\n");
        assert!(got.stderr.is_empty());

        let restored_js = std::fs::read(restore.path().join("dist/app.js")).unwrap();
        assert_eq!(restored_js, b"console.log('hi')");
        let restored_logo =
            std::fs::read(restore.path().join("dist/nested/assets/logo.svg")).unwrap();
        assert_eq!(restored_logo, b"<svg/>");

        // src/main.ts was not in the output glob so it must not be restored.
        assert!(!restore.path().join("src/main.ts").exists());
    }

    #[test]
    fn workspace_outputs_roundtrip_to_workspace_root() {
        // Cargo-workspace shape: `dish/` is `crates/foo/`; the compiled
        // binary lives at `<workspace>/target/release/foo`, outside the
        // dish. `workspace_outputs` anchored at the workspace root should
        // capture it on put and restore it back to the workspace root on
        // get, independent of the dish_dir walk.
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());

        let workspace = tempfile::tempdir().unwrap();
        let dish_dir = workspace.path().join("crates/foo");
        std::fs::create_dir_all(&dish_dir).unwrap();
        std::fs::write(dish_dir.join("src.rs"), b"fn main() {}").unwrap();

        let target_dir = workspace.path().join("target/release");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("foo"), b"\x7fELF...binary").unwrap();
        std::fs::write(target_dir.join("other-crate"), b"not ours").unwrap();

        let key = make_key("cargo-ws");
        let result = TaskResult {
            exit_code: 0,
            stdout: b"compiled\n".to_vec(),
            stderr: Vec::new(),
        };

        local
            .put(
                &key,
                &dish_dir,
                &[],
                Some(workspace.path()),
                &["target/release/foo".into()],
                &result,
            )
            .unwrap();

        let restore_ws = tempfile::tempdir().unwrap();
        let restore_dish = restore_ws.path().join("crates/foo");
        std::fs::create_dir_all(&restore_dish).unwrap();

        let got = local
            .get(&key, &restore_dish, Some(restore_ws.path()))
            .unwrap()
            .unwrap();
        assert_eq!(got.stdout, b"compiled\n");

        let restored = std::fs::read(restore_ws.path().join("target/release/foo")).unwrap();
        assert_eq!(restored, b"\x7fELF...binary");
        // Unrelated sibling binary NOT captured (precise glob).
        assert!(!restore_ws
            .path()
            .join("target/release/other-crate")
            .exists());
    }

    #[test]
    fn workspace_outputs_without_workspace_root_is_an_error() {
        // Declaring workspace_outputs without resolving a workspace root
        // would silently cache nothing — worse than the opt-out default.
        // Refuse loudly instead.
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();
        let key = make_key("missing-root");

        let err = local
            .put(
                &key,
                dish.path(),
                &[],
                None,
                &["target/release/foo".into()],
                &TaskResult::default(),
            )
            .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("workspace_outputs") && chain.contains("no workspace root"),
            "got: {chain}"
        );
    }

    // NB: no explicit test for tar-path-traversal rejection. `unpack_at`'s
    // `..`-component check is defence-in-depth; the `tar` crate itself
    // rejects archive entries containing `..` both on write (append_*) and
    // on read (Entry::path). Forging a traversal bundle requires going
    // around both layers, which isn't exercisable from safe Rust.

    #[test]
    fn put_captures_stderr_and_nonzero_exit() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();
        let key = make_key("fail");

        let result = TaskResult {
            exit_code: 2,
            stdout: Vec::new(),
            stderr: b"error: something failed\n".to_vec(),
        };
        local
            .put(&key, dish.path(), &[], None, &[], &result)
            .unwrap();

        let restore = tempfile::tempdir().unwrap();
        let got = local.get(&key, restore.path(), None).unwrap().unwrap();
        assert_eq!(got.exit_code, 2);
        assert_eq!(got.stderr, b"error: something failed\n");
    }

    #[test]
    fn empty_output_globs_is_valid() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();

        let key = make_key("lint");
        let result = TaskResult {
            exit_code: 0,
            stdout: b"0 issues\n".to_vec(),
            stderr: Vec::new(),
        };
        local
            .put(&key, dish.path(), &[], None, &[], &result)
            .unwrap();

        let restore = tempfile::tempdir().unwrap();
        let got = local.get(&key, restore.path(), None).unwrap().unwrap();
        assert_eq!(got.stdout, b"0 issues\n");
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = make_dish(&[("out.bin", b"first")]);
        let key = make_key("same");

        local
            .put(
                &key,
                dish.path(),
                &["out.bin".into()],
                None,
                &[],
                &TaskResult {
                    exit_code: 0,
                    stdout: b"first run\n".to_vec(),
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::write(dish.path().join("out.bin"), b"second").unwrap();
        local
            .put(
                &key,
                dish.path(),
                &["out.bin".into()],
                None,
                &[],
                &TaskResult {
                    exit_code: 0,
                    stdout: b"second run\n".to_vec(),
                    ..Default::default()
                },
            )
            .unwrap();

        let restore = tempfile::tempdir().unwrap();
        let got = local.get(&key, restore.path(), None).unwrap().unwrap();
        assert_eq!(got.stdout, b"second run\n");
        assert_eq!(
            std::fs::read(restore.path().join("out.bin")).unwrap(),
            b"second"
        );
    }

    #[test]
    fn stats_reflects_puts_and_clear() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();

        assert_eq!(local.stats().unwrap(), CacheStats::default());

        for name in ["a", "b", "c"] {
            let key = make_key(name);
            local
                .put(
                    &key,
                    dish.path(),
                    &[],
                    None,
                    &[],
                    &TaskResult {
                        exit_code: 0,
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let stats = local.stats().unwrap();
        assert_eq!(stats.entries, 3);
        assert!(stats.total_bytes > 0);

        local.clear().unwrap();
        assert_eq!(local.stats().unwrap(), CacheStats::default());
    }

    #[test]
    fn stats_ignores_tmp_files() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        std::fs::create_dir_all(cache.path()).unwrap();
        std::fs::write(cache.path().join("stray.tar.tmp"), b"in-flight").unwrap();

        let stats = local.stats().unwrap();
        assert_eq!(stats.entries, 0);
    }

    #[test]
    fn put_manifest_then_read_roundtrips() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let key = make_key("manifest");

        let manifest = InputManifest {
            version: InputManifest::CURRENT_VERSION,
            task_name: "build".into(),
            run: "go build ./...".into(),
            dish: "api".into(),
            adapter: Some("go".into()),
            toolchain: Some("go:1.22".into()),
            bento_version: "0.1".into(),
            host: Some("x86_64-linux".into()),
            env_vars: vec!["CGO_ENABLED".into()],
            files: vec![crate::manifest::ManifestFile {
                path: "main.go".into(),
                blake3: "deadbeef".into(),
                size_bytes: 42,
            }],
        };
        local.put_manifest(&key, &manifest).unwrap();

        let loaded = local.read_manifest(&key).unwrap().unwrap();
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn read_manifest_is_none_when_absent() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        assert!(local.read_manifest(&make_key("missing")).unwrap().is_none());
    }

    #[test]
    fn find_by_prefix_matches_bundle_stems() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();

        for seed in ["alpha", "alphabet", "beta"] {
            local
                .put(
                    &make_key(seed),
                    dish.path(),
                    &[],
                    None,
                    &[],
                    &TaskResult::default(),
                )
                .unwrap();
        }

        // Collect the expected alpha-prefixed keys.
        let alpha_key = make_key("alpha");
        let alphabet_key = make_key("alphabet");

        // Use the shortest common prefix of both alpha keys to match them.
        let shared: String = alpha_key
            .as_hex()
            .chars()
            .zip(alphabet_key.as_hex().chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect();

        if !shared.is_empty() {
            let matches = local.find_by_prefix(&shared).unwrap();
            assert!(
                matches.iter().any(|k| k == &alpha_key),
                "expected alpha in matches"
            );
            assert!(
                matches.iter().any(|k| k == &alphabet_key),
                "expected alphabet in matches"
            );
        }

        // Long-enough prefix picks out exactly one key.
        let exact_prefix = &alpha_key.as_hex()[..16];
        let only_alpha = local.find_by_prefix(exact_prefix).unwrap();
        assert_eq!(only_alpha.len(), 1);
        assert_eq!(only_alpha[0], alpha_key);
    }

    #[test]
    fn find_by_prefix_is_empty_for_no_matches() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        assert!(local.find_by_prefix("ffffffffffff").unwrap().is_empty());
    }

    #[test]
    fn get_errors_on_bundle_version_mismatch() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();
        let key = make_key("bump");

        local
            .put(
                &key,
                dish.path(),
                &[],
                None,
                &[],
                &TaskResult {
                    exit_code: 0,
                    ..Default::default()
                },
            )
            .unwrap();

        // Rewrite meta.json with a future version to simulate an upgrade.
        let bundle = cache.path().join(format!("{}.tar", key.as_hex()));
        let forged = cache.path().join("forged.tar");
        {
            let input = File::open(&bundle).unwrap();
            let output = File::create(&forged).unwrap();
            let mut reader = tar::Archive::new(input);
            let mut writer = tar::Builder::new(output);
            for entry in reader.entries().unwrap() {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().into_owned();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                let name = path.to_string_lossy().to_string();
                if name == "meta.json" {
                    data = serde_json::to_vec(&Metadata {
                        version: 999,
                        exit_code: 0,
                    })
                    .unwrap();
                }
                append_bytes(&mut writer, &name, &data).unwrap();
            }
            writer.finish().unwrap();
        }
        std::fs::rename(&forged, &bundle).unwrap();

        let restore = tempfile::tempdir().unwrap();
        let err = local.get(&key, restore.path(), None).unwrap_err();
        assert!(err.to_string().contains("bundle"), "got: {err}");
    }
}
