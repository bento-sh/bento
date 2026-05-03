//! Per-workspace record of "what's already deployed".
//!
//! Deploy tasks declare `no_cache = true` so their *results* never get
//! served from the content cache — we don't want to silently replay an
//! old deploy — but that conflates two things. The other concern,
//! "don't *re-run* the deploy command when nothing has changed since
//! the last successful run," gets handled here.
//!
//! The state lives at `.bento/state/deploys.json` under the workspace
//! root. It's a JSON document mapping
//! `<env> → <"dish:task"> → DeployRecord`, where `DeployRecord.input_hash`
//! is the Deploy task's content-addressable key at the moment of the
//! successful deploy. On the next invocation the planner recomputes the
//! key and, if it matches, short-circuits execution with
//! [`crate::run::TaskOutcome::DeploySkipped`].
//!
//! `bento deploy --force` bypasses the skip; the state is still
//! updated afterwards so subsequent non-force runs return to the
//! skip-when-unchanged behaviour.
//!
//! Concurrent writers (two `bento deploy` invocations racing) rely on
//! atomic-rename semantics: each write goes to a sibling tempfile then
//! is renamed into place. The worst case is last-writer-wins, which is
//! acceptable because losing a write only causes one extra deploy next
//! time — never a silent skip of a changed artefact.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Outer key used when a deploy wasn't scoped to a named environment
/// (i.e. `bento deploy` with no `--env`). Kept as a fixed string so
/// "no env" records live in the same document as named envs without a
/// `None` key collapsing to ambiguous behaviour.
pub const DEFAULT_ENV_KEY: &str = "<default>";

/// Record of a single successful deploy for one `(env, dish, task)`
/// triple. `deploy_url` is whatever the integration surfaced as the
/// task's output excerpt — typically the live URL or a deploy-id; we
/// keep it so `bento plan` can point at the still-live version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeployRecord {
    /// Content-addressable key of the Deploy task inputs at the time
    /// of the last successful run. Hex string, matching
    /// [`bento_cache::CacheKey::as_hex`].
    pub input_hash: String,
    /// RFC 3339 timestamp of the last successful deploy.
    pub deployed_at: String,
    /// Whatever the integration surfaced as its output excerpt — Vercel
    /// URL, Cloudflare Pages URL, Railway deploy id. May be absent when
    /// the integration didn't produce a useful excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_url: Option<String>,
}

/// In-memory mirror of `.bento/state/deploys.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct DeployState {
    /// `env → "dish:task" → DeployRecord`.
    envs: BTreeMap<String, BTreeMap<String, DeployRecord>>,
}

impl DeployState {
    /// Conventional path inside a workspace root.
    pub fn path_for(workspace_root: &Path) -> PathBuf {
        workspace_root
            .join(".bento")
            .join("state")
            .join("deploys.json")
    }

    /// Compose the inner map key (`"dish:task"`). Task name may itself
    /// contain `:` (integration tasks look like `cloudflare_worker:deploy`);
    /// we keep the whole thing so distinctness round-trips.
    pub fn entry_key(dish: &str, task: &str) -> String {
        format!("{dish}:{task}")
    }

    /// Load from disk. Missing file is not an error — a fresh workspace
    /// simply has no prior deploys to compare against. Parse errors
    /// ARE surfaced so a corrupt state file becomes a visible warning
    /// at the call site (we'd rather not silently lose history).
    pub fn load_from(workspace_root: &Path) -> Result<Self> {
        let path = Self::path_for(workspace_root);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(parsed)
    }

    /// Look up a record by (env, dish, task). Pass `None` for `env`
    /// when the deploy wasn't scoped.
    pub fn get(&self, env: Option<&str>, dish: &str, task: &str) -> Option<&DeployRecord> {
        let bucket = self.envs.get(env.unwrap_or(DEFAULT_ENV_KEY))?;
        bucket.get(&Self::entry_key(dish, task))
    }

    /// Overwrite (or insert) a record. The outer env bucket is created
    /// on demand.
    pub fn set(&mut self, env: Option<&str>, dish: &str, task: &str, record: DeployRecord) {
        let bucket = self
            .envs
            .entry(env.unwrap_or(DEFAULT_ENV_KEY).to_string())
            .or_default();
        bucket.insert(Self::entry_key(dish, task), record);
    }

    /// Write to disk at the conventional path, atomically. Creates the
    /// parent directory if missing.
    pub fn save_to(&self, workspace_root: &Path) -> Result<()> {
        let path = Self::path_for(workspace_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self).context("serialising deploy state")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Current UTC instant as an RFC 3339 string with second precision.
/// Separate fn so tests can compare against a stable format without
/// pulling in a chrono dependency for one call-site.
pub fn now_rfc3339() -> String {
    // Second-precision Z-suffixed format. Matches what deploy logs
    // already use elsewhere in the codebase.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Round-trip via a minimal formatter — avoids pulling in chrono
    // for one timestamp. 60-sec precision is enough for an audit log.
    let days = now / 86_400;
    let sod = now % 86_400;
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Proleptic Gregorian date from day-count since 1970-01-01. Source:
/// H. Neumann, "Date Algorithms" — same algorithm used by the `time`
/// crate, inlined here to avoid the dep.
fn civil_from_days(mut z: i64) -> (i64, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_resolves_under_workspace_root() {
        let root = PathBuf::from("/tmp/bento-test");
        assert_eq!(
            DeployState::path_for(&root),
            PathBuf::from("/tmp/bento-test/.bento/state/deploys.json")
        );
    }

    #[test]
    fn entry_key_joins_dish_and_task() {
        assert_eq!(
            DeployState::entry_key("dashboard", "cloudflare_pages:deploy"),
            "dashboard:cloudflare_pages:deploy"
        );
    }

    #[test]
    fn load_missing_file_yields_empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = DeployState::load_from(tmp.path()).unwrap();
        assert_eq!(loaded, DeployState::default());
    }

    #[test]
    fn roundtrip_preserves_records_and_segregates_envs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = DeployState::default();
        state.set(
            Some("prod"),
            "dashboard",
            "cloudflare_pages:deploy",
            DeployRecord {
                input_hash: "abc123".into(),
                deployed_at: "2026-04-19T12:00:00Z".into(),
                deploy_url: Some("https://example.pages.dev".into()),
            },
        );
        state.set(
            None,
            "dashboard",
            "cloudflare_pages:deploy",
            DeployRecord {
                input_hash: "def456".into(),
                deployed_at: "2026-04-19T12:01:00Z".into(),
                deploy_url: None,
            },
        );

        state.save_to(tmp.path()).unwrap();
        let reloaded = DeployState::load_from(tmp.path()).unwrap();
        assert_eq!(reloaded, state);

        // env segregation — prod and default are independent.
        assert_eq!(
            reloaded
                .get(Some("prod"), "dashboard", "cloudflare_pages:deploy")
                .unwrap()
                .input_hash,
            "abc123"
        );
        assert_eq!(
            reloaded
                .get(None, "dashboard", "cloudflare_pages:deploy")
                .unwrap()
                .input_hash,
            "def456"
        );
        // Unrecorded env misses cleanly.
        assert!(reloaded
            .get(Some("staging"), "dashboard", "cloudflare_pages:deploy")
            .is_none());
    }

    #[test]
    fn save_uses_atomic_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = DeployState::default();
        state.set(
            Some("prod"),
            "d",
            "t",
            DeployRecord {
                input_hash: "h".into(),
                deployed_at: "2026-04-19T00:00:00Z".into(),
                deploy_url: None,
            },
        );
        state.save_to(tmp.path()).unwrap();

        // No leftover tempfile after a successful save.
        let leftover = DeployState::path_for(tmp.path()).with_extension("json.tmp");
        assert!(
            !leftover.exists(),
            "tempfile leaked: {}",
            leftover.display()
        );
    }

    #[test]
    fn now_rfc3339_is_rfc3339_shaped() {
        let s = now_rfc3339();
        assert_eq!(
            s.len(),
            20,
            "expected 2026-04-19T12:34:56Z shape, got {s:?}"
        );
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
        assert_eq!(s.as_bytes()[13], b':');
        assert_eq!(s.as_bytes()[16], b':');
        assert_eq!(s.as_bytes()[19], b'Z');
    }

    #[test]
    fn civil_from_days_known_epoch_points() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-04-19 ≈ 20_562 days after 1970-01-01.
        assert_eq!(civil_from_days(20_562), (2026, 4, 19));
    }
}
