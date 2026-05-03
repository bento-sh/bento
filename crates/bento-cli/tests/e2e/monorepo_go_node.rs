//! Polyglot monorepo e2e: Go backend + Node (npm) frontend. Mirrors
//! proyecto-shape. Asserts plan lists both dishes, ci builds + caches
//! across the pair, and init auto-detect rebuilds the dish set from
//! sources alone.

use super::common::{monorepo_suite, MonorepoSpec};

const SPEC: MonorepoSpec = MonorepoSpec {
    fixture: "monorepo-go-node",
    toolchains: &["go", "node", "npm"],
    dishes: &[("backend", "go"), ("frontend", "node-npm")],
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
