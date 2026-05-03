//! Node + pnpm ecosystem e2e. Fixture layout identical to node-npm
//! but with a `pnpm-lock.yaml` so pnpm wins the adapter race.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "node-pnpm-hello",
    toolchain: "pnpm",
    language_id: "node-pnpm",
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
