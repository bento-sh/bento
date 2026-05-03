//! Bun ecosystem e2e. `bun test` is Bun's built-in zero-dep test
//! runner — picks up `*.test.ts` files — so the test recipe runs
//! offline against the fixture's `hello.test.ts`.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "bun-hello",
    toolchain: "bun",
    language_id: "bun",
    expected_tasks: &["build", "test", "lint"],
    build_needs_network: false,
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
