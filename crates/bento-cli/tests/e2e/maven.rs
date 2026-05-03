//! Maven ecosystem e2e. `mvn package -DskipTests` pulls the
//! compiler / jar plugins from Maven Central on first run, so
//! `build_needs_network = true`. Test recipe invokes JUnit which
//! we don't ship in the pom.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "maven-hello",
    toolchain: "mvn",
    language_id: "maven",
    expected_tasks: &["build", "test", "lint"],
    build_needs_network: true,
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
