//! Polyglot monorepo e2e: Rust (cargo) API + pnpm web. A second
//! monorepo shape — different adapters, different package managers —
//! exercising the same invariants as monorepo-go-node so cache/plan
//! correctness isn't accidentally coupled to one toolchain pair.

use super::common::{monorepo_suite, MonorepoSpec};

const SPEC: MonorepoSpec = MonorepoSpec {
    fixture: "monorepo-cargo-pnpm",
    toolchains: &["cargo", "pnpm"],
    dishes: &[("api", "cargo"), ("web", "node-pnpm")],
    common_tasks: &["build", "test"],
};

#[test]
fn plan_lists_every_dish() {
    monorepo_suite::plan_lists_every_dish(&SPEC);
}

#[test]
fn build_caches_across_runs() {
    monorepo_suite::build_caches_across_runs(&SPEC);
}

#[test]
fn init_auto_detects_every_subdir() {
    monorepo_suite::init_auto_detects_every_subdir(&SPEC);
}
