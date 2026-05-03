//! Deno ecosystem e2e. Default build (`deno task build || deno
//! check **/*.ts`) and test (`deno test --allow-read`) both work
//! offline — the fixture's test file has zero imports so no jsr /
//! https fetches happen.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "deno-hello",
    toolchain: "deno",
    language_id: "deno",
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
