//! Gradle ecosystem e2e. Default `./gradlew build` downloads the
//! Gradle distribution on first run + resolves dependencies from
//! Maven Central, so `build_needs_network = true`. The fixture
//! ships without the Gradle wrapper binaries committed; CI runs
//! `gradle wrapper` beforehand so `./gradlew` is materialised.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "gradle-hello",
    toolchain: "gradle",
    language_id: "gradle",
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
