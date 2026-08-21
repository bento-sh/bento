//! .NET ecosystem e2e. `dotnet restore` on a zero-PackageReference
//! project resolves from the SDK's bundled targeting pack, but the
//! SDK still probes its configured NuGet sources on a cold NuGet
//! home — `build_needs_network = true` keeps that off the PR path.
//! `dotnet test` needs a test project (xunit / MSTest, both NuGet
//! packages we deliberately don't vendor), so the test recipe is
//! `test_runs_offline = false`.

use super::common::{standard_suite, EcosystemSpec};

const SPEC: EcosystemSpec = EcosystemSpec {
    fixture: "dotnet-hello",
    toolchain: "dotnet",
    language_id: "dotnet",
    expected_tasks: &["build", "test", "check", "lint"],
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
