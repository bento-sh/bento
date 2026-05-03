//! Dep-graph cascade e2e. Fixture has `a` declaring `depends_on =
//! ["b"]` — mutating `b/main.go` must invalidate every task on `a`
//! (pessimistic-correct cascade), and flipping
//! `force_independent = true` on `a` must break that propagation
//! (the documented escape hatch).

use std::path::Path;

use super::common::{monorepo_suite, MonorepoSpec};

const SPEC: MonorepoSpec = MonorepoSpec {
    fixture: "monorepo-dep-cascade",
    toolchains: &["go"],
    dishes: &[("a", "go"), ("b", "go")],
    common_tasks: &["build", "test"],
};

// Rewrite b/main.go so its Value() returns a different string. The
// change has to land in source bento hashes (not just an output
// artefact), so we edit the committed file — the cache key derivation
// reads content via the adapter's declared inputs.
fn bump_b_source(dir: &Path) {
    let path = dir.join("b").join("main.go");
    let body = std::fs::read_to_string(&path).expect("read b/main.go");
    let bumped = body.replace("\"v1\"", "\"v2\"");
    assert_ne!(body, bumped, "expected v1 → v2 mutation to land");
    std::fs::write(&path, bumped).expect("write b/main.go");
}

#[test]
fn plan_lists_every_dish() {
    monorepo_suite::plan_lists_every_dish(&SPEC);
}

#[test]
fn build_caches_across_runs() {
    monorepo_suite::build_caches_across_runs(&SPEC);
}

#[test]
fn cascade_invalidates_dependent() {
    monorepo_suite::cascade_invalidates_dependent(&SPEC, "b", "a", bump_b_source);
}

#[test]
fn force_independent_breaks_cascade() {
    monorepo_suite::force_independent_breaks_cascade(&SPEC, "b", "a", bump_b_source);
}
