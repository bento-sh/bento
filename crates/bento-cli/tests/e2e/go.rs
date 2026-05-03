//! Go ecosystem e2e. Every ecosystem follows this exact pattern —
//! a `SPEC` declaring fixture + expectations + per-ecosystem flags,
//! then four `#[test]` fns delegating to `standard_suite`. Keeps
//! behaviour uniform across the 12-adapter matrix; new test
//! invariants added to the suite propagate to every ecosystem.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "go-hello",
    toolchain: "go",
    language_id: "go",
    expected_tasks: &["build", "check", "test"],
    // `go build` resolves a zero-import module without hitting the
    // network; `go test` in the same module likewise works offline.
    build_needs_network: false,
    // Fixture ships a `main_test.go`, so `go test` has something
    // real to run.
    test_runs_offline: true,
};

#[test]
fn init_and_adopt() {
    standard_suite::init_and_adopt(&SPEC);
}

#[test]
fn plan_reports_expected_tasks() {
    standard_suite::plan_reports_expected_tasks(&SPEC);
}

#[test]
fn build_caches_across_runs() {
    standard_suite::build_caches_across_runs(&SPEC);
}

#[test]
fn test_runs_to_completion() {
    standard_suite::test_runs_to_completion(&SPEC);
}
