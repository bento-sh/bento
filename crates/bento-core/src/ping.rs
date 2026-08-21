//! Anonymous install / activation / retention pings —
//! `POST <api>/v1/cli/ping`.
//!
//! Answers one question: how many people installed bento and actually
//! ran it. Build reports ([`crate::report`]) only fire once a
//! `bento://` remote + token are wired up, so they miss everyone who
//! never signs up for the hosted cache.
//!
//! # What leaves the machine
//!
//! Exactly six fields, all of them either a random id, a constant, or
//! an allowlisted enum the server rejects if it doesn't recognise:
//! `event`, `machine_id`, `bento_version`, `os`, `arch`,
//! `install_method`. Never: repo names, paths, dish names, task names,
//! env var names or values, command lines, usernames, hostnames.
//!
//! `machine_id` is a UUIDv4 generated from the OS CSPRNG and persisted
//! at `~/.bento/state/machine_id` — never derived from hostname, MAC,
//! disk UUID or username. Deleting the file resets the identity.
//!
//! # Consent
//!
//! Same gate as build reports ([`telemetry_posture`]): `[telemetry]
//! enabled = false` in bento.toml, or `BENTO_TELEMETRY=0|false|no|off`
//! — either says off → off. Off means nothing is sent *and* nothing is
//! written under `~/.bento/state`.
//!
//! The first run that would ping instead prints a one-line stderr
//! notice and sends nothing, so a user who opts out after reading it
//! has never emitted a single ping.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::report::telemetry_posture;

/// Shorter than the build report's timeout: this fires on the exit
/// path of every `bento ci`, with the user already waiting on their
/// prompt and nothing to gain from the response.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

const WEEK_SECS: u64 = 7 * 24 * 60 * 60;

const NOTICE: &str = "bento sends an anonymous install/activity ping (random id, version, event name — nothing about your code). Disable: [telemetry] enabled = false or BENTO_TELEMETRY=0. Docs: docs/configuration.md#telemetry";

/// Server-side allowlist (control-plane `cli_ping.rs`). A value
/// outside it is a 400, so we fall back to `unknown` rather than
/// forward whatever the marker file happens to hold.
const INSTALL_METHODS: &[&str] = &["brew", "install.sh", "cargo", "manual", "unknown"];

/// Wire shape of `POST /v1/cli/ping`. Every field is required by the
/// control plane and every enum-shaped one is allowlisted there —
/// adding a field without the server half means a 400.
#[derive(Debug, Serialize)]
struct Ping<'a> {
    event: &'a str,
    machine_id: &'a str,
    bento_version: &'a str,
    os: &'a str,
    arch: &'a str,
    install_method: &'a str,
}

/// `~/.bento/state/telemetry.json`. Machine-scoped only — nothing in
/// here is per-repo, so it can't leak which projects exist locally.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    notice_shown: bool,
    #[serde(default)]
    install_sent: bool,
    #[serde(default)]
    first_run_sent: bool,
    /// Unix seconds of the last `weekly_active`. `0` means the clock
    /// has never been seeded (fresh state file).
    #[serde(default)]
    last_weekly: u64,
}

/// Fire whatever pings are due. Best-effort in every direction: no
/// state dir, no entropy, no network — all silent no-ops.
///
/// `telemetry_enabled` is `workspace.repo.telemetry.enabled`; callers
/// that can't load the config must pass `false` (we can't have
/// consented to something we couldn't read).
pub fn send(telemetry_enabled: bool) {
    let posture = telemetry_posture(telemetry_enabled);
    if !posture.is_enabled() {
        tracing::debug!(?posture, "cli/ping: skipped (telemetry disabled)");
        return;
    }
    let (Some(os), Some(arch)) = (wire_os(), wire_arch()) else {
        return; // platform outside the server's allowlist — don't 400 it.
    };
    let Some(dir) = state_dir() else { return };
    let Some((machine_id, events)) = plan(&dir, now(), true) else {
        return;
    };

    let url = format!("{}/v1/cli/ping", crate::cloud::api_base());
    let install_method = install_method(&dir);
    for event in events {
        post(
            &url,
            &Ping {
                event,
                machine_id: &machine_id,
                bento_version: env!("CARGO_PKG_VERSION"),
                os,
                arch,
                install_method: &install_method,
            },
        );
    }
}

/// Decide what's due, print the one-time notice, persist the result.
/// Split from the POST so the whole consent + dedupe gate is testable
/// without a network or a `$HOME`.
///
/// Returns `None` when nothing should leave the machine this run.
fn plan(dir: &Path, now: u64, posture_enabled: bool) -> Option<(String, Vec<&'static str>)> {
    if !posture_enabled {
        return None;
    }
    let state_path = dir.join("telemetry.json");
    let mut state: State = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Consent before data. The run that prints the notice sends
    // nothing, giving the user a chance to opt out first.
    if !state.notice_shown {
        eprintln!("bento: {NOTICE}");
        state.notice_shown = true;
        let _ = write_state(&state_path, &state);
        return None;
    }

    let events = due_events(&mut state, now);
    if events.is_empty() {
        return None;
    }
    let machine_id = machine_id(&dir.join("machine_id"))?;
    // Persist before sending: a state dir we can't write would
    // otherwise re-send the same event on every invocation forever.
    write_state(&state_path, &state).ok()?;
    Some((machine_id, events))
}

fn due_events(state: &mut State, now: u64) -> Vec<&'static str> {
    let mut events = Vec::new();
    if !state.install_sent {
        state.install_sent = true;
        events.push("install");
    }
    // A fresh machine's first verb *is* its activation — both fire in
    // the same run rather than holding `first_run` back for run #2.
    if !state.first_run_sent {
        state.first_run_sent = true;
        events.push("first_run");
    }
    if state.last_weekly == 0 {
        // Fresh state seeds the heartbeat clock without emitting one:
        // `install` already says "this machine is alive today".
        state.last_weekly = now;
    } else if now.saturating_sub(state.last_weekly) >= WEEK_SECS {
        state.last_weekly = now;
        events.push("weekly_active");
    }
    events
}

fn machine_id(path: &Path) -> Option<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        // Length is the whole validation: the server parses this as a
        // UUID and 400s anything else. A truncated / hand-edited file
        // is cheaper to regenerate than to debug.
        if trimmed.len() == 36 {
            return Some(trimmed.to_string());
        }
    }
    let id = uuid_v4()?;
    write_private(path, &id).ok()?;
    Some(id)
}

/// UUIDv4 from the OS CSPRNG. No `uuid` crate in the tree and this is
/// the only caller — 16 random bytes plus the two RFC 4122 fixups.
fn uuid_v4() -> Option<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).ok()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02x}");
    }
    Some(s)
}

fn install_method(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("install_method"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| INSTALL_METHODS.contains(&s.as_str()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn post(url: &str, body: &Ping) {
    let Ok(json) = serde_json::to_string(body) else {
        return;
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(PING_TIMEOUT)
        .user_agent(&bento_cache::client_id::user_agent())
        .build();
    if let Err(e) = agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(&json)
    {
        // debug only — a dropped ping is never the user's problem.
        tracing::debug!("cli/ping {url}: {e}");
    }
}

fn write_state(path: &Path, state: &State) -> std::io::Result<()> {
    let json = serde_json::to_string(state).map_err(std::io::Error::other)?;
    write_private(path, &json)
}

/// 0600 write with the parent created — same posture as
/// `~/.bento/credentials`, which this directory sits next to. Nothing
/// here is secret, but a world-readable machine id is a correlation
/// handle we'd rather not hand out.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `.mode()` only applies on create — tighten an existing file
        // that a previous bento (or an editor) left readable.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(contents.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

fn state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bento").join("state"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn wire_os() -> Option<&'static str> {
    match std::env::consts::OS {
        os @ ("linux" | "macos" | "windows") => Some(os),
        _ => None,
    }
}

fn wire_arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => Some(arch),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skips the consent run so a test can get straight to the send
    /// path, exactly as the second-ever invocation would.
    fn after_notice(dir: &Path) {
        assert!(plan(dir, 1_000, true).is_none(), "notice run must not send");
    }

    #[test]
    fn notice_run_prints_and_sends_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");

        assert!(plan(&dir, 1_000, true).is_none());
        // Notice persisted, but no identity minted — nothing was sent,
        // so there's nothing to be identified by yet.
        assert!(dir.join("telemetry.json").exists());
        assert!(!dir.join("machine_id").exists());
    }

    #[test]
    fn first_send_is_install_plus_first_run_then_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        after_notice(&dir);

        let (id, events) = plan(&dir, 1_000, true).expect("second run sends");
        assert_eq!(events, vec!["install", "first_run"]);
        assert_eq!(id.len(), 36);

        // Every later run inside the week is silent.
        assert!(plan(&dir, 1_000, true).is_none());
        assert!(plan(&dir, 1_000 + WEEK_SECS - 1, true).is_none());
    }

    #[test]
    fn machine_id_is_stable_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        after_notice(&dir);

        let (first, _) = plan(&dir, 1_000, true).unwrap();
        let (second, _) = plan(&dir, 1_000 + WEEK_SECS, true).unwrap();
        assert_eq!(first, second);

        // Deleting the file resets the identity — the documented
        // opt-out-of-continuity escape hatch.
        std::fs::remove_file(dir.join("machine_id")).unwrap();
        let (third, _) = plan(&dir, 1_000 + 2 * WEEK_SECS, true).unwrap();
        assert_ne!(first, third);
    }

    #[test]
    fn weekly_active_fires_at_most_once_per_week() {
        let mut state = State {
            notice_shown: true,
            install_sent: true,
            first_run_sent: true,
            last_weekly: 0,
        };
        // Fresh clock: seeded, not emitted.
        assert!(due_events(&mut state, 1_000).is_empty());
        assert_eq!(state.last_weekly, 1_000);

        assert!(due_events(&mut state, 1_000 + WEEK_SECS - 1).is_empty());
        assert_eq!(
            due_events(&mut state, 1_000 + WEEK_SECS),
            vec!["weekly_active"]
        );
        assert_eq!(state.last_weekly, 1_000 + WEEK_SECS);
        assert!(due_events(&mut state, 1_000 + WEEK_SECS + 1).is_empty());
    }

    #[test]
    fn posture_off_sends_nothing_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");

        assert!(plan(&dir, 1_000, false).is_none());
        assert!(!dir.exists(), "opted out: nothing may touch the state dir");

        // Still true once state already exists from an opted-in past.
        after_notice(&dir);
        plan(&dir, 1_000, true).unwrap();
        let before = std::fs::read_to_string(dir.join("telemetry.json")).unwrap();
        assert!(plan(&dir, 1_000 + 2 * WEEK_SECS, false).is_none());
        assert_eq!(
            std::fs::read_to_string(dir.join("telemetry.json")).unwrap(),
            before
        );
    }

    #[test]
    fn uuid_v4_is_canonical_and_random() {
        let id = uuid_v4().unwrap();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&id[14..15], "4", "version nibble");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "variant");
        assert_ne!(id, uuid_v4().unwrap());
    }

    #[test]
    fn install_method_falls_back_to_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(install_method(tmp.path()), "unknown");

        std::fs::write(tmp.path().join("install_method"), "install.sh\n").unwrap();
        assert_eq!(install_method(tmp.path()), "install.sh");

        // Anything off the server's allowlist would be a 400.
        std::fs::write(tmp.path().join("install_method"), "telepathy").unwrap();
        assert_eq!(install_method(tmp.path()), "unknown");
    }

    #[test]
    fn payload_carries_only_the_six_agreed_fields() {
        let json = serde_json::to_value(Ping {
            event: "install",
            machine_id: "6ba7b810-9dad-41d1-80b4-00c04fd430c8",
            bento_version: "0.1.2",
            os: "linux",
            arch: "x86_64",
            install_method: "cargo",
        })
        .unwrap();
        // serde_json orders map keys, so this is the field *set* —
        // which is the actual contract: anything extra is a field the
        // server never agreed to receive.
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(|k| &**k).collect();
        assert_eq!(
            keys,
            vec![
                "arch",
                "bento_version",
                "event",
                "install_method",
                "machine_id",
                "os"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        after_notice(&dir);
        plan(&dir, 1_000, true).unwrap();

        for name in ["telemetry.json", "machine_id"] {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }
}
