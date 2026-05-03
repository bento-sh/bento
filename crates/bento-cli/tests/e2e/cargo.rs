//! Rust / Cargo ecosystem e2e. See `e2e/go.rs` for the pattern
//! — each ecosystem file is the same shape, varying the `SPEC`.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "cargo-hello",
    toolchain: "cargo",
    language_id: "cargo",
    expected_tasks: &["build", "check", "test", "lint"],
    // Zero-dep crate — `cargo build --locked` + `cargo test --locked`
    // resolve without hitting crates.io. `Cargo.lock` is shipped in
    // the fixture so `--locked` is satisfied on first run.
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
