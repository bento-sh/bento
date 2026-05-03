//! PHP / Composer ecosystem e2e. `composer install` on a zero-dep
//! composer.json is offline-safe (writes `composer.lock` + an empty
//! `vendor/autoload.php`). `phpunit` / `phpstan` aren't bundled, so
//! the test / lint recipes are network-gated.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "php-hello",
    toolchain: "composer",
    language_id: "php",
    expected_tasks: &["build", "test", "lint"],
    build_needs_network: false,
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
