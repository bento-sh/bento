//! Local on-disk cache.
//!
//! Layout (flat):
//!   `<root>/<key>.tar`          — bundle for a cache entry
//!   `<root>/bento*.tmp`         — in-flight write (renamed atomically)

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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

/// What one [`LocalCache::prune`] pass removed. Byte count covers
/// bundles only; the manifest sidecars that go with them are noise next
/// to a tarball.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    pub removed_entries: usize,
    pub removed_bytes: u64,
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
    /// from a uniquely-named temp file, so two processes putting the same
    /// key can't interleave into one shared scratch path.
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
        let mut tmp = new_temp_in(&self.root)?;

        write_bundle(
            tmp.as_file_mut(),
            dish_dir,
            output_globs,
            workspace_root,
            workspace_output_globs,
            result,
        )
        .with_context(|| format!("writing bundle for {}", key.short()))?;

        persist_temp(tmp, &final_path)
    }

    /// Delete every cache entry (but leave the root directory in place).
    pub fn clear(&self) -> Result<()> {
        if !self.root.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            // Only our own artefacts — `BENTO_CACHE_DIR` can point at
            // a shared dir, and `clear` must never become `rm *`.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ours = name.ends_with(".tar")
                || name.ends_with(".inputs.json")
                || name.ends_with(".tmp")
                || name.ends_with(".remote-tmp");
            if ours && path.is_file() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Evict entries until the cache satisfies both bounds: nothing older
    /// than `older_than`, and no more than `max_bytes` in total. Either
    /// may be `None`; both `None` is a no-op.
    ///
    /// Eviction is oldest-mtime-first, which is the closest thing to LRU
    /// available without tracking reads — `get` doesn't touch the bundle,
    /// so mtime is really "least recently *written*". Good enough for a
    /// disk-budget knob; a true LRU would mean writing on every hit.
    pub fn prune(
        &self,
        max_bytes: Option<u64>,
        older_than: Option<std::time::Duration>,
    ) -> Result<PruneOutcome> {
        let mut outcome = PruneOutcome::default();
        if !self.root.is_dir() || (max_bytes.is_none() && older_than.is_none()) {
            return Ok(outcome);
        }

        let now = std::time::SystemTime::now();
        let mut entries = Vec::new();
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
            entries.push((path, meta.len(), meta.modified().unwrap_or(now)));
        }
        entries.sort_by_key(|(_, _, mtime)| *mtime);

        let mut live: u64 = entries.iter().map(|(_, len, _)| len).sum();
        for (path, len, mtime) in entries {
            let too_old =
                older_than.is_some_and(|max| now.duration_since(mtime).is_ok_and(|age| age > max));
            let over_budget = max_bytes.is_some_and(|cap| live > cap);
            if !too_old && !over_budget {
                continue;
            }
            remove_entry(&path)?;
            live -= len;
            outcome.removed_entries += 1;
            outcome.removed_bytes += len;
        }
        Ok(outcome)
    }

    /// Write an explanation sidecar for a cache entry. Atomic via `.tmp`
    /// + rename. Intended to be called alongside [`Self::put`].
    pub fn put_manifest(&self, key: &CacheKey, manifest: &InputManifest) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let final_path = self.manifest_path(key);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        let mut tmp = new_temp_in(&self.root)?;
        tmp.as_file_mut()
            .write_all(&bytes)
            .with_context(|| format!("writing manifest for {}", key.short()))?;
        persist_temp(tmp, &final_path)
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

/// Scratch file for an about-to-be-published cache artefact. The `.tmp`
/// suffix is load-bearing: [`LocalCache::clear`] recognises it as ours and
/// [`is_bundle`] doesn't, so a crashed writer leaves something collectable
/// that never reads back as a cache hit.
pub(crate) fn new_temp_in(dir: &Path) -> Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix("bento")
        .suffix(".tmp")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))
}

pub(crate) fn persist_temp(tmp: tempfile::NamedTempFile, final_path: &Path) -> Result<()> {
    tmp.persist(final_path)
        .map_err(|e| e.error)
        .with_context(|| format!("publishing {}", final_path.display()))?;
    Ok(())
}

/// Land a bundle downloaded from a remote tier at `dest`. Shared by both
/// remote backends so neither can invent its own promotion rules.
///
/// The download is validated at its temp path and only then renamed into
/// place: a truncated or wrong-version body must never occupy `<key>.tar`,
/// because the local tier trusts that path unconditionally and would serve
/// the corruption to every later run.
pub(crate) fn promote_remote_bundle(dest: &Path, data: &[u8]) -> Result<()> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating cache root {}", dir.display()))?;
    let mut tmp = new_temp_in(dir)?;
    tmp.as_file_mut()
        .write_all(data)
        .with_context(|| format!("writing remote bundle to {}", tmp.path().display()))?;
    validate_bundle(tmp.path()).context("remote bundle failed validation — not promoted")?;
    persist_temp(tmp, dest)
}

/// Bytes of a minimal, valid bundle — the remote backends' tests need a
/// body that survives [`validate_bundle`], and only this module knows the
/// format.
#[cfg(test)]
pub(crate) fn sample_bundle_bytes() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let cache = LocalCache::new(dir.path());
    let key = CacheKey::from_hex("5a3b1e");
    cache
        .put(&key, dir.path(), &[], None, &[], &TaskResult::default())
        .unwrap();
    std::fs::read(cache.bundle_path(&key)).unwrap()
}

/// Read a bundle end to end without extracting anything: it must be
/// terminated, every entry header and body must be present, and
/// `meta.json` must parse at the version we speak.
fn validate_bundle(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    // A body cut at a 512-byte boundary reads as a *clean* end-of-archive
    // to the entry walk below, so check for the trailer first —
    // `tar::Builder::finish` always writes those two zero blocks.
    let len = file.metadata()?.len();
    if len < 1024 || len % 512 != 0 {
        anyhow::bail!("bundle is {len} bytes, not a whole number of tar blocks");
    }
    let mut trailer = [0u8; 1024];
    file.seek(SeekFrom::End(-1024))?;
    file.read_exact(&mut trailer)?;
    if trailer.iter().any(|b| *b != 0) {
        anyhow::bail!("bundle has no end-of-archive marker — truncated in transit");
    }
    file.rewind()?;

    let mut archive = tar::Archive::new(file);
    let mut meta_bytes = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == Path::new("meta.json") {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            meta_bytes = Some(buf);
        } else {
            // Drain the body so a body truncated mid-entry errors here
            // rather than at restore time.
            std::io::copy(&mut entry, &mut std::io::sink())?;
        }
    }
    check_meta(meta_bytes.as_deref()).map(|_| ())
}

/// Parse + version-check a bundle's `meta.json`. `None` means the archive
/// held no such entry.
fn check_meta(bytes: Option<&[u8]>) -> Result<Metadata> {
    let bytes = bytes.ok_or_else(|| anyhow::anyhow!("cache bundle missing meta.json"))?;
    let meta: Metadata = serde_json::from_slice(bytes).context("parsing cache bundle meta.json")?;
    if meta.version != BUNDLE_VERSION {
        anyhow::bail!(
            "cache bundle version {} does not match expected {}",
            meta.version,
            BUNDLE_VERSION
        );
    }
    Ok(meta)
}

fn is_bundle(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "tar")
}

/// Delete a bundle and the manifest sidecar that explains it. A sidecar
/// left without its bundle is what `cache pull` treats as "fetch me", so
/// leaving one behind would resurrect what we just evicted.
fn remove_entry(bundle: &Path) -> Result<()> {
    std::fs::remove_file(bundle).with_context(|| format!("removing {}", bundle.display()))?;
    if let Some(hex) = bundle.file_stem().and_then(|s| s.to_str()) {
        let _ = std::fs::remove_file(bundle.with_file_name(format!("{hex}.inputs.json")));
    }
    Ok(())
}

fn write_bundle<W: Write>(
    out: W,
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

    let mut tar = tar::Builder::new(out);

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

/// Dirs never worth descending on the output walk unless a glob names
/// one. They're the three biggest file-count sinks in a working tree and
/// none of them is a build output anybody declares by accident.
const UNLIKELY_OUTPUT_DIRS: [&str; 3] = ["node_modules", ".git", "target"];

/// Walk `root`, match files against `globs`, and archive matches under
/// `<archive_prefix>/<rel>` in `tar`. A no-op when `globs` is empty, so
/// callers can invoke unconditionally without dispatching on opt-in.
///
/// Walks from each glob's literal prefix rather than from `root`: an
/// `outputs = ["dist/**"]` dish shouldn't pay to stat `node_modules` on
/// every put.
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
    let skip: Vec<&str> = UNLIKELY_OUTPUT_DIRS
        .into_iter()
        .filter(|d| !globs.iter().any(|g| g.split('/').any(|part| part == *d)))
        .collect();

    let mut seen = std::collections::HashSet::new();
    for start in walk_roots(globs) {
        let from = root.join(&start);
        if !from.exists() {
            continue;
        }
        let walk = walkdir::WalkDir::new(&from)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0
                    || !e.file_type().is_dir()
                    || !e
                        .file_name()
                        .to_str()
                        .is_some_and(|name| skip.contains(&name))
            })
            .filter_map(|r| match r {
                Ok(e) => Some(e),
                // One unreadable path shouldn't cost the whole cache
                // entry — a stale symlink or a permission hole in the
                // output tree is the user's problem, not a build failure.
                Err(e) => {
                    tracing::warn!("skipping unreadable path while bundling outputs: {e}");
                    None
                }
            });

        for entry in walk {
            if !entry.file_type().is_file() {
                continue;
            }
            let full = entry.path();
            let Ok(rel) = full.strip_prefix(root) else {
                continue;
            };
            // Overlapping globs ("dist/**" and "dist/app.js") give
            // overlapping walk roots; tar would happily hold both copies.
            if matcher.is_match(rel) && seen.insert(rel.to_path_buf()) {
                let archive_name = PathBuf::from(archive_prefix).join(rel);
                tar.append_path_with_name(full, archive_name)?;
            }
        }
    }
    Ok(())
}

/// Deepest wildcard-free directory prefix of each glob — the subtree
/// that could possibly match it. `dist/**` → `dist`; `**/*.js` → `` (the
/// whole tree, no saving available).
fn walk_roots(globs: &[String]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = globs
        .iter()
        .map(|g| {
            g.split('/')
                .take_while(|part| !part.contains(['*', '?', '[', '{']))
                .collect::<PathBuf>()
        })
        .collect();
    roots.sort();
    // Sorted, so a root always precedes anything nested under it — and
    // walking the ancestor already covers the descendant.
    let mut deduped: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        if deduped.last().is_some_and(|prev| root.starts_with(prev)) {
            continue;
        }
        deduped.push(root);
    }
    deduped
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
    let mut meta: Option<Metadata> = None;

    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.to_string_lossy();

        if name == "meta.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            // Checked here, not after the loop: `write_bundle` puts
            // meta.json first, so a version we don't speak aborts before
            // any of its `outputs/` land in the dish.
            meta = Some(check_meta(Some(&buf))?);
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

    let meta = meta.ok_or_else(|| anyhow::anyhow!("cache bundle missing meta.json"))?;
    result.exit_code = meta.exit_code;
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
    // Legit bundles hold regular files only (`bundle_tree` skips
    // symlinks + dirs). A symlink or hard-link entry could redirect
    // a later entry outside `root`, so refuse them outright, and
    // check the *resolved* parent in case an earlier symlink on disk
    // already points elsewhere.
    if !entry.header().entry_type().is_file() {
        anyhow::bail!(
            "cache bundle entry `{}` is not a regular file — refusing extract",
            rel.display()
        );
    }
    let dest = root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
        let root_real = root.canonicalize()?;
        if !parent.canonicalize()?.starts_with(&root_real) {
            anyhow::bail!(
                "cache bundle entry `{}` resolves outside the dish — refusing extract",
                rel.display()
            );
        }
    }
    // Restored files get "now", not the mtime baked into the archive.
    // Downstream incremental tools (tsc, webpack, make, cargo) compare
    // source mtime against output mtime; a bundle built last week
    // restores outputs that look older than the sources they came from,
    // and every one of them rebuilds on the spot — which is exactly the
    // work the cache hit was meant to skip.
    entry.set_preserve_mtime(false);
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
    fn concurrent_puts_of_one_key_never_interleave() {
        // Two writers racing on the same key used to share a single
        // `<key>.tar.tmp`, so the loser's tar bytes could land inside the
        // winner's file. Whoever wins the rename must produce a bundle
        // that extracts cleanly.
        let cache = tempfile::tempdir().unwrap();
        let key = make_key("contended");
        // Bodies differ in length so a spliced file can't accidentally
        // still parse as one of them.
        let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 1 << (10 + i)]).collect();
        let dishes: Vec<_> = bodies
            .iter()
            .map(|b| make_dish(&[("out.bin", b)]))
            .collect();

        std::thread::scope(|s| {
            for dish in &dishes {
                let root = cache.path().to_path_buf();
                let key = key.clone();
                s.spawn(move || {
                    LocalCache::new(root)
                        .put(
                            &key,
                            dish.path(),
                            &["out.bin".into()],
                            None,
                            &[],
                            &TaskResult::default(),
                        )
                        .unwrap();
                });
            }
        });

        let restore = tempfile::tempdir().unwrap();
        LocalCache::new(cache.path())
            .get(&key, restore.path(), None)
            .unwrap()
            .expect("bundle must exist");
        let restored = std::fs::read(restore.path().join("out.bin")).unwrap();
        assert!(
            restored.iter().all(|b| *b == restored[0]) && restored.len().is_power_of_two(),
            "restored output is a splice of two writers"
        );

        // No scratch files survive a clean run.
        let strays: Vec<_> = std::fs::read_dir(cache.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
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

    /// Backdate a bundle (and its sidecar) so age-based pruning is
    /// testable without sleeping.
    fn age_entry(local: &LocalCache, key: &CacheKey, seconds: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds);
        let f = File::options()
            .write(true)
            .open(local.bundle_path(key))
            .unwrap();
        f.set_modified(when).unwrap();
    }

    fn seed(local: &LocalCache, dish: &Path, seed: &str, bytes: usize) -> CacheKey {
        std::fs::write(dish.join("out.bin"), vec![b'x'; bytes]).unwrap();
        let key = make_key(seed);
        local
            .put(
                &key,
                dish,
                &["out.bin".into()],
                None,
                &[],
                &TaskResult::default(),
            )
            .unwrap();
        local
            .put_manifest(
                &key,
                &InputManifest {
                    version: InputManifest::CURRENT_VERSION,
                    task_name: seed.into(),
                    run: "true".into(),
                    dish: "d".into(),
                    adapter: None,
                    toolchain: None,
                    bento_version: "0.1".into(),
                    host: None,
                    env_vars: vec![],
                    files: vec![],
                },
            )
            .unwrap();
        key
    }

    #[test]
    fn walk_roots_are_the_literal_prefixes_minus_nested_ones() {
        let roots = |globs: &[&str]| {
            walk_roots(&globs.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(roots(&["dist/**"]), ["dist"]);
        assert_eq!(roots(&["target/release/foo"]), ["target/release/foo"]);
        assert_eq!(roots(&["dist/**", "build/*.js"]), ["build", "dist"]);
        // A wildcard in the first segment means no saving is available.
        assert_eq!(roots(&["**/*.js"]), [""]);
        assert_eq!(roots(&["**/*.js", "dist/**"]), [""]);
        // Nested prefixes collapse into their ancestor.
        assert_eq!(roots(&["dist/**", "dist/app.js"]), ["dist"]);
        // Sibling dirs with a shared string prefix are NOT nested.
        assert_eq!(roots(&["dist/**", "dist-old/**"]), ["dist", "dist-old"]);
    }

    #[test]
    fn output_walk_skips_noise_dirs_but_not_when_a_glob_names_one() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = make_dish(&[
            ("dist/app.js", b"built"),
            ("node_modules/dep/index.js", b"vendored"),
            ("target/debug/thing", b"compiled"),
            (".git/HEAD", b"ref: x"),
        ]);

        // `**` would match everything, but the noise dirs are pruned.
        let key = make_key("noise");
        local
            .put(
                &key,
                dish.path(),
                &["**".into()],
                None,
                &[],
                &TaskResult::default(),
            )
            .unwrap();
        let restore = tempfile::tempdir().unwrap();
        local.get(&key, restore.path(), None).unwrap().unwrap();
        assert!(restore.path().join("dist/app.js").exists());
        assert!(!restore.path().join("node_modules").exists());
        assert!(!restore.path().join("target").exists());
        assert!(!restore.path().join(".git").exists());

        // Naming one opts back in — cargo dishes really do declare
        // `target/...` as an output.
        let key = make_key("named");
        local
            .put(
                &key,
                dish.path(),
                &["target/debug/thing".into()],
                None,
                &[],
                &TaskResult::default(),
            )
            .unwrap();
        let restore = tempfile::tempdir().unwrap();
        local.get(&key, restore.path(), None).unwrap().unwrap();
        assert!(restore.path().join("target/debug/thing").exists());
    }

    #[test]
    fn overlapping_globs_archive_each_file_once() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = make_dish(&[("dist/app.js", b"built")]);
        let key = make_key("overlap");

        local
            .put(
                &key,
                dish.path(),
                &["dist/**".into(), "dist/app.js".into()],
                None,
                &[],
                &TaskResult::default(),
            )
            .unwrap();

        let mut archive = tar::Archive::new(File::open(local.bundle_path(&key)).unwrap());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.iter().filter(|n| *n == "outputs/dist/app.js").count(),
            1,
            "{names:?}"
        );
    }

    #[test]
    fn prune_by_age_drops_stale_entries_and_their_sidecars() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();

        let old = seed(&local, dish.path(), "old", 64);
        let fresh = seed(&local, dish.path(), "fresh", 64);
        age_entry(&local, &old, 10 * 24 * 3600);

        let outcome = local
            .prune(None, Some(std::time::Duration::from_secs(7 * 24 * 3600)))
            .unwrap();
        assert_eq!(outcome.removed_entries, 1);
        assert!(outcome.removed_bytes > 0);
        assert!(!local.contains(&old));
        assert!(local.contains(&fresh));
        // The sidecar has to go too, or `cache pull` would fetch it back.
        assert!(local.read_manifest(&old).unwrap().is_none());
        assert!(local.read_manifest(&fresh).unwrap().is_some());
    }

    #[test]
    fn prune_by_size_evicts_oldest_first_until_under_budget() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();

        // Same payload size each, backdated in order.
        let keys: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let key = seed(&local, dish.path(), name, 4096);
                age_entry(&local, &key, (4 - i as u64) * 3600);
                key
            })
            .collect();

        let total = local.stats().unwrap().total_bytes;
        let budget = total / 2;
        let outcome = local.prune(Some(budget), None).unwrap();

        assert!(outcome.removed_entries >= 2, "{outcome:?}");
        assert!(local.stats().unwrap().total_bytes <= budget);
        // Oldest go first: whatever survives must be a suffix of the list.
        let survivors: Vec<bool> = keys.iter().map(|k| local.contains(k)).collect();
        assert!(
            survivors.windows(2).all(|w| !w[0] || w[1]),
            "evicted out of age order: {survivors:?}"
        );
    }

    #[test]
    fn prune_without_bounds_removes_nothing() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();
        seed(&local, dish.path(), "keep", 64);

        assert_eq!(local.prune(None, None).unwrap(), PruneOutcome::default());
        assert_eq!(local.stats().unwrap().entries, 1);
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
    fn get_refuses_symlink_entries_and_escapes_through_them() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let key = make_key("hostile");

        // Hand-built bundle: a symlink `outputs/l -> <outside>` then a
        // regular file `outputs/l/pwned` that would be written through it.
        let bundle = local.bundle_path(&key);
        {
            let mut w = tar::Builder::new(File::create(&bundle).unwrap());
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            h.set_cksum();
            w.append_link(&mut h, "outputs/l", outside.path()).unwrap();
            let data = b"pwned";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            w.append_data(&mut h, "outputs/l/pwned", &data[..]).unwrap();
            w.finish().unwrap();
        }

        let err = local.get(&key, dish.path(), None).unwrap_err();
        assert!(format!("{err:#}").contains("not a regular file"), "{err:#}");
        assert!(!outside.path().join("pwned").exists());
        assert!(!dish.path().join("l").exists());
    }

    #[test]
    fn clear_only_removes_bento_artefacts() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        std::fs::write(cache.path().join("keep.txt"), b"x").unwrap();
        std::fs::write(cache.path().join("abc.tar"), b"x").unwrap();
        std::fs::write(cache.path().join("abc.inputs.json"), b"x").unwrap();
        local.clear().unwrap();
        assert!(cache.path().join("keep.txt").exists());
        assert!(!cache.path().join("abc.tar").exists());
        assert!(!cache.path().join("abc.inputs.json").exists());
    }

    #[test]
    fn get_errors_on_bundle_version_mismatch_without_extracting() {
        let cache = tempfile::tempdir().unwrap();
        let local = LocalCache::new(cache.path());
        let dish = make_dish(&[("dist/app.js", b"built")]);
        let key = make_key("bump");

        local
            .put(
                &key,
                dish.path(),
                &["dist/**".into()],
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
        assert!(
            format!("{err:#}").contains("does not match expected"),
            "got: {err:#}"
        );
        // meta.json is the first entry, so nothing should have landed.
        assert!(
            !restore.path().join("dist").exists(),
            "extracted outputs from a bundle we can't read"
        );
    }
}
