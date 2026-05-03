//! Python ecosystem e2e. Default build / test recipes (`python -m
//! build` / `pytest`) need extra packages that aren't part of a
//! vanilla Python install — `build_needs_network` and
//! `test_runs_offline = false` gate those tests behind the network
//! flag / external tooling.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "python-hello",
    toolchain: "python3",
    language_id: "python",
    expected_tasks: &["build", "test", "lint"],
    // `python -m build` needs the `build` frontend package +
    // setuptools pulled in at runtime — both go through PyPI on
    // first run.
    build_needs_network: true,
    // Default `pytest` isn't installed by a vanilla Python.
    test_runs_offline: false,
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
