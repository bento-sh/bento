//! Deploy-integration e2e. Real Railway / Vercel deploys need tokens
//! and real infra; this module uses stub CLIs on PATH instead, so
//! the tests run offline and asserted behaviour stays deterministic.
//!
//! The stubs live under `tests/e2e/bin/`; the `run_bento_with_stubs`
//! helper prepends that dir to PATH so `railway` / `vercel` invoked
//! from the task's shell resolve to the stub rather than whatever's
//! installed on the host. A sentinel file in the fixture
//! (`.railway-fail`) flips the stub's exit code so we can exercise
//! both happy- and failure-path contracts without touching any real
//! cloud.

use super::common::{materialize_hand_crafted, run_bento_with_stubs};

const FIXTURE: &str = "deploy-railway";

#[test]
fn railway_happy_path_deploys_with_ci_flag() {
    let (_tmp, dir) = materialize_hand_crafted(FIXTURE);
    let cache = tempfile::tempdir().expect("cache tempdir");

    let outcome = run_bento_with_stubs(&dir, cache.path(), &["deploy", "--json"], &[]);
    assert_eq!(
        outcome.exit_code, 0,
        "[{FIXTURE}] happy-path deploy should exit 0.\nstderr: {}\nstdout: {}",
        outcome.stderr, outcome.stdout
    );

    let report = outcome.json();
    let summary = report.pointer("/summary").cloned().unwrap();
    assert_eq!(
        summary.pointer("/failed").and_then(|v| v.as_u64()),
        Some(0),
        "[{FIXTURE}] summary: {summary}"
    );

    // railway:deploy task should be present, succeeded, and its
    // output excerpt should carry the stub-emitted Build Logs URL.
    let tasks = report
        .pointer("/bentos/0/dishes/0/tasks")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("[{FIXTURE}] report missing tasks: {report}"));
    let deploy = tasks
        .iter()
        .find(|t| t.pointer("/name").and_then(|v| v.as_str()) == Some("railway:deploy"))
        .unwrap_or_else(|| panic!("[{FIXTURE}] report missing railway:deploy task: {report}"));
    assert_eq!(
        deploy.pointer("/outcome/kind").and_then(|v| v.as_str()),
        Some("built"),
        "[{FIXTURE}] railway:deploy outcome: {deploy}"
    );
    let excerpt = deploy
        .pointer("/output_excerpt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        excerpt.contains("Build Logs: https://railway.example/"),
        "[{FIXTURE}] output_excerpt should carry Build Logs URL; got:\n{excerpt}"
    );
    // Stub prints this only when --ci is on the cmdline — cheap
    // regression guard that the Railway adapter passes `--ci` to
    // `railway up` (plain `railway up` silently detaches in non-TTY
    // contexts).
    assert!(
        excerpt.contains("CI mode enabled"),
        "[{FIXTURE}] adapter must invoke stub with --ci (else stub omits this line); got:\n{excerpt}"
    );
    assert!(
        excerpt.contains("Deploy complete"),
        "[{FIXTURE}] adapter must block until stub's Deploy complete; got:\n{excerpt}"
    );
}

#[test]
fn railway_failure_path_surfaces_stderr_and_exits_non_zero() {
    let (_tmp, dir) = materialize_hand_crafted(FIXTURE);
    // Flip the sentinel so the stub exits 1.
    std::fs::write(dir.join("app").join(".railway-fail"), b"fail please\n")
        .expect("write sentinel");

    let cache = tempfile::tempdir().expect("cache tempdir");
    let outcome = run_bento_with_stubs(&dir, cache.path(), &["deploy", "--json"], &[]);
    assert_eq!(
        outcome.exit_code, 1,
        "[{FIXTURE}] failing stub should propagate exit 1 through bento.\nstderr: {}\nstdout: {}",
        outcome.stderr, outcome.stdout
    );

    let report = outcome.json();
    let summary = report.pointer("/summary").cloned().unwrap();
    assert_eq!(
        summary.pointer("/failed").and_then(|v| v.as_u64()),
        Some(1),
        "[{FIXTURE}] summary should report 1 failure: {summary}"
    );

    let deploy = report
        .pointer("/bentos/0/dishes/0/tasks")
        .and_then(|v| v.as_array())
        .expect("tasks")
        .iter()
        .find(|t| t.pointer("/name").and_then(|v| v.as_str()) == Some("railway:deploy"))
        .cloned()
        .unwrap_or_else(|| panic!("[{FIXTURE}] report missing railway:deploy: {report}"));
    assert_eq!(
        deploy.pointer("/outcome/kind").and_then(|v| v.as_str()),
        Some("failed"),
        "[{FIXTURE}] railway:deploy outcome kind: {deploy}"
    );
    assert_eq!(
        deploy
            .pointer("/outcome/exit_code")
            .and_then(|v| v.as_u64()),
        Some(1),
        "[{FIXTURE}] railway:deploy exit_code: {deploy}"
    );
    let stderr_excerpt = deploy
        .pointer("/outcome/stderr_excerpt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        stderr_excerpt.contains("Build failed: stub-sentinel-triggered"),
        "[{FIXTURE}] stderr excerpt should carry stub's failure message; got:\n{stderr_excerpt}"
    );
}
