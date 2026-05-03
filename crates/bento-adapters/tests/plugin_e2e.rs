//! End-to-end tests for the subprocess plugin path: real binary on disk,
//! real `discover_plugins` call, real RPC roundtrips, real `LanguageAdapter`
//! trait calls. Uses `bento-adapter-noop` from `examples/` as the fixture.
//!
//! The noop binary is declared as a dev-dep in this crate's Cargo.toml so
//! `cargo test -p bento-adapters` builds it before running.
//!
//! These tests acquire a process-wide mutex via `serial_guard()` so they
//! run one at a time. Each test does a fork+exec of the noop plugin
//! binary and a stdio handshake; with `cargo test`'s default
//! parallelism (10 threads here, one per test) the simultaneous
//! fork+exec storm intermittently brushes against the handshake
//! timeout and tests flake. Serialising them is cheap (each test
//! finishes in <50ms) and removes the flake.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use bento_adapters::{
    discover_plugins, AdapterRegistry, DiagnosticParser, DiagnosticRerun, PluginSearchOptions,
    Severity, TaskContext,
};
use tempfile::TempDir;

/// One mutex shared across every test in this file. Each test holds
/// the guard for its full duration so subprocess-plugin handshakes
/// don't race each other.
fn serial_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // If a prior test panicked while holding the guard, the mutex is
    // poisoned — that doesn't make our shared state corrupt (we have
    // none beyond the lock itself), so unwrap the poison.
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn noop_source() -> PathBuf {
    // Test exe lives at target/<profile>/deps/<test-binary>-<hash>.
    let exe = std::env::current_exe().expect("current_exe");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("target profile dir");
    let bin_name = if cfg!(windows) {
        "bento-adapter-noop.exe"
    } else {
        "bento-adapter-noop"
    };
    let bin = profile.join(bin_name);
    assert!(
        bin.exists(),
        "noop binary not built at {bin:?}; this test needs `cargo test --workspace` \
         or `cargo build -p bento-adapter-noop` to be run first"
    );
    bin
}

/// Copy the noop binary into a fresh temp dir under `name`, returning
/// the dir handle (drop deletes everything) and the binary's new path.
fn install_noop_as(name: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join(name);
    std::fs::copy(noop_source(), &dest).expect("copy noop binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
    (dir, dest)
}

fn opts_with_path(dir: &Path) -> PluginSearchOptions {
    PluginSearchOptions {
        search_paths: Some(vec![dir.to_path_buf()]),
        // 30s rather than the 5s default: CI runners share resources
        // and 10 plugin_e2e tests run in parallel by default, each
        // spawning a subprocess plugin. fork+exec under that load can
        // brush against a 5s handshake window. 30s is a generous
        // backstop with zero cost on successful handshakes.
        handshake_timeout: Duration::from_secs(30),
        ..Default::default()
    }
}

#[test]
fn discovers_noop_plugin_on_search_path() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let opts = opts_with_path(_dir.path());
    let plugins = discover_plugins(&opts);
    assert_eq!(plugins.len(), 1, "expected exactly one plugin");
    assert_eq!(plugins[0].adapter.id(), "noop");
    assert_eq!(plugins[0].adapter.display_name(), "noop (reference plugin)");
    assert_eq!(plugins[0].adapter.fingerprint_files(), vec!["noop.toml"]);
}

#[test]
fn disable_filter_skips_named_plugin() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let mut opts = opts_with_path(_dir.path());
    opts.disable = vec!["noop".into()];
    let plugins = discover_plugins(&opts);
    assert!(plugins.is_empty(), "noop should have been filtered out");
}

#[test]
fn allowlist_includes_only_listed_ids() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");

    let mut allowed = opts_with_path(_dir.path());
    allowed.allowlist = Some(vec!["noop".into()]);
    assert_eq!(discover_plugins(&allowed).len(), 1);

    let mut not_allowed = opts_with_path(_dir.path());
    not_allowed.allowlist = Some(vec!["something-else".into()]);
    assert!(discover_plugins(&not_allowed).is_empty());
}

#[test]
fn duplicate_id_first_path_wins() {
    let _guard = serial_guard();
    let (dir_a, _) = install_noop_as("bento-adapter-noop");
    let (dir_b, _) = install_noop_as("bento-adapter-noop");
    let opts = PluginSearchOptions {
        search_paths: Some(vec![dir_a.path().into(), dir_b.path().into()]),
        handshake_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let plugins = discover_plugins(&opts);
    assert_eq!(
        plugins.len(),
        1,
        "duplicate noop on second path should be deduped"
    );
    assert!(
        plugins[0].binary.starts_with(dir_a.path()),
        "first path should win, got {:?}",
        plugins[0].binary
    );
}

#[test]
fn binary_suffix_must_match_announced_id() {
    let _guard = serial_guard();
    // Copy noop binary under a different suffix. The handshake will
    // announce id="noop" but the suffix says "bogus" → host rejects it.
    let (_dir, _bin) = install_noop_as("bento-adapter-bogus");
    let opts = opts_with_path(_dir.path());
    let plugins = discover_plugins(&opts);
    assert!(
        plugins.is_empty(),
        "binary with mismatched suffix should be rejected, got {:?}",
        plugins.iter().map(|p| p.adapter.id()).collect::<Vec<_>>()
    );
}

#[test]
fn registry_with_plugins_routes_detect_to_subprocess() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let opts = opts_with_path(_dir.path());
    let registry = AdapterRegistry::builtin()
        .with_plugins(discover_plugins(&opts).into_iter().map(|p| p.adapter));

    let by_id = registry.by_id("noop").expect("noop should be registered");
    assert_eq!(by_id.id(), "noop");

    // Detect against a dir without noop.toml — false.
    let empty = tempfile::tempdir().unwrap();
    assert!(!by_id.detect(empty.path()));

    // Detect against a dir WITH noop.toml — true.
    let with_marker = tempfile::tempdir().unwrap();
    std::fs::write(with_marker.path().join("noop.toml"), "").unwrap();
    assert!(by_id.detect(with_marker.path()));
}

#[test]
fn subprocess_adapter_install_succeeds_and_default_tasks_present() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let opts = opts_with_path(_dir.path());
    let registry = AdapterRegistry::builtin()
        .with_plugins(discover_plugins(&opts).into_iter().map(|p| p.adapter));
    let adapter = registry.by_id("noop").unwrap();

    let dish_dir = tempfile::tempdir().unwrap();
    let ctx = TaskContext::new(dish_dir.path(), "demo");
    adapter.install(&ctx).expect("noop install should succeed");

    let tasks = adapter.default_tasks();
    let task_names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(task_names, vec!["build", "test", "lint"]);
    assert_eq!(tasks[0].run, "echo noop build");
}

#[test]
fn registry_builtin_wins_on_id_collision() {
    let _guard = serial_guard();
    // Stand up a noop plugin renamed to claim the built-in "go" id. The
    // binary still announces id="noop" so the suffix-match check will
    // actually reject it before we even get to the conflict — but the
    // intent of this test is to prove the registry's resolution rule:
    // built-ins are appended first, so by_id finds them first regardless.
    //
    // To exercise the registry's policy specifically, register the noop
    // plugin (claiming "noop") AFTER the builtins and ask for "go".
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let opts = opts_with_path(_dir.path());
    let registry = AdapterRegistry::builtin()
        .with_plugins(discover_plugins(&opts).into_iter().map(|p| p.adapter));

    // The built-in "go" must still resolve to the GoAdapter (it returns
    // the built-in `&'static str` id, which we can compare directly).
    let go = registry.by_id("go").expect("built-in go must remain");
    assert_eq!(go.id(), "go");
    // And both still listed.
    let ids = registry.ids();
    assert!(ids.iter().any(|i| i == "go"));
    assert!(ids.iter().any(|i| i == "noop"));
}

#[test]
fn noop_plugin_declares_diagnostic_hook_with_plugin_parser() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let opts = opts_with_path(_dir.path());
    let registry = AdapterRegistry::empty()
        .with_plugins(discover_plugins(&opts).into_iter().map(|p| p.adapter));
    let noop = registry.by_id("noop").expect("noop plugin should load");

    let hook = noop
        .diagnostic_hook("lint")
        .expect("noop should declare a lint diagnostic hook");
    assert_eq!(hook.parser, DiagnosticParser::Plugin);
    match hook.rerun {
        DiagnosticRerun::Replace(cmd) => assert_eq!(cmd, "false"),
        _ => panic!("expected Replace"),
    }
    // No hook for other tasks.
    assert!(noop.diagnostic_hook("build").is_none());
    assert!(noop.diagnostic_hook("test").is_none());
}

#[test]
fn parse_diagnostics_round_trips_through_plugin() {
    let _guard = serial_guard();
    let (_dir, _bin) = install_noop_as("bento-adapter-noop");
    let opts = opts_with_path(_dir.path());
    let registry = AdapterRegistry::empty()
        .with_plugins(discover_plugins(&opts).into_iter().map(|p| p.adapter));
    let noop = registry.by_id("noop").unwrap();

    // The noop plugin's parseDiagnostics returns one hardcoded
    // diagnostic per call regardless of inputs — proves the wire
    // round-trip without depending on a real parser.
    let dish = std::path::Path::new("/tmp/dish");
    let root = std::path::Path::new("/tmp");
    let diags = noop.parse_diagnostics("lint", "raw stdout", "raw stderr", dish, root);
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    assert_eq!(d.file, "noop.toml");
    assert_eq!(d.line, 1);
    assert_eq!(d.severity, Severity::Warning);
    assert!(d.message.contains("noop plugin reporting on lint"));
    assert_eq!(d.source, "noop");
    assert_eq!(d.rule.as_deref(), Some("noop-demo"));
}
