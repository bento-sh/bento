//! Container-mode smoke. The fixture pins `container = "always"` +
//! `image = "golang:1.22-alpine"` in `bento.toml`, so every task runs
//! inside docker (or podman / nerdctl). We skip the test if no
//! runtime is reachable — most local dev machines have docker, most
//! CI runners do too; laptops without it get a clean pass.
//!
//! What this proves:
//! - `[execution] container = "always"` plus `image = "..."` actually
//!   spawns the runtime rather than shelling out natively.
//! - The dish dir gets mounted at `/work` and `go build ./...` inside
//!   the container can read its sources + write artefacts that the
//!   host cache key machinery sees.
//! - First run builds, second run hits the local cache — identical
//!   contract to the native path.

use super::common::{fixtures_dir, materialize_hand_crafted, run_bento_with_cache};

const FIXTURE: &str = "container-go";

/// Containerised workloads routinely need docker pulls, which rip
/// through public network mirrors and can be flaky on a cold cache.
/// We don't gate on [`require_network`] because the reliability
/// signal here is "can we reach a container runtime", not "does the
/// internet work" — but CI should pre-pull the image so the test
/// doesn't race the first-run pull.
fn require_runtime() -> Option<&'static str> {
    for runtime in ["docker", "podman", "nerdctl"] {
        if which::which(runtime).is_ok() {
            return Some(runtime);
        }
    }
    println!(
        "[e2e] skipping {FIXTURE}: no container runtime on PATH (tried docker/podman/nerdctl)"
    );
    None
}

#[test]
fn fixture_exists_on_disk() {
    // Cheap sanity: catches the "someone renamed the fixture dir"
    // footgun without needing the container runtime.
    let src = fixtures_dir().join(FIXTURE);
    assert!(src.is_dir(), "fixture missing at {}", src.display());
    assert!(
        src.join("bento.toml").is_file(),
        "fixture missing bento.toml at {}",
        src.display()
    );
    assert!(
        src.join("app").join("dish.toml").is_file(),
        "fixture missing app/dish.toml at {}",
        src.display()
    );
}

#[test]
fn build_runs_inside_container_and_caches() {
    if require_runtime().is_none() {
        return;
    }
    // The task inside the container is `go build ./...`, so we also
    // need `go` reachable from inside the image — the alpine golang
    // image carries that. But the harness runs on the host; we don't
    // require go on PATH here. What we DO require is the runtime,
    // which `require_runtime()` already checked.
    let (_tmp, dir) = materialize_hand_crafted(FIXTURE);

    let cache = tempfile::tempdir().expect("cache tempdir");
    let first = run_bento_with_cache(&dir, cache.path(), &["build", "--json"]);
    assert_eq!(
        first.exit_code,
        0,
        "[{FIXTURE}] first containerised build should succeed.\nstderr: {}\nstdout (first 800): {}",
        first.stderr,
        first.stdout.chars().take(800).collect::<String>(),
    );
    let summary = first.json().pointer("/summary").cloned().unwrap();
    assert_eq!(
        summary.pointer("/failed").and_then(|v| v.as_u64()),
        Some(0),
        "[{FIXTURE}] first build summary: {summary}"
    );
    assert!(
        summary
            .pointer("/built")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 1,
        "[{FIXTURE}] first build should build ≥1 task; summary: {summary}"
    );

    let second = run_bento_with_cache(&dir, cache.path(), &["build", "--json"]);
    assert_eq!(
        second.exit_code, 0,
        "[{FIXTURE}] second build stderr: {}",
        second.stderr
    );
    let second_summary = second.json().pointer("/summary").cloned().unwrap();
    assert_eq!(
        second_summary.pointer("/built").and_then(|v| v.as_u64()),
        Some(0),
        "[{FIXTURE}] second containerised build should hit cache; summary: {second_summary}"
    );
    assert!(
        second_summary
            .pointer("/hits")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 1,
        "[{FIXTURE}] second build should report ≥1 hit; summary: {second_summary}"
    );
}
