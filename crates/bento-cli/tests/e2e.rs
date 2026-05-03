//! End-to-end test harness entry point.
//!
//! One Rust integration-test file per ecosystem would produce a lot of
//! cargo-discovered test binaries (each a 10-20 MB compile); we instead
//! keep all e2e tests in this single binary and split them into modules.
//!
//! Run:
//!
//!     cargo test --test e2e              # run everything; missing
//!                                        # toolchains skip cleanly
//!     cargo test --test e2e -- --nocapture   # see skip messages etc.
//!     cargo test --test e2e go           # only Go tests
//!     BENTO_E2E_NETWORK=1 cargo test --test e2e   # incl. network-gated
//!
//! See `e2e/common.rs` for the shared harness helpers.

mod e2e {
    pub mod common;

    // Per-ecosystem modules. Each module is a thin wrapper around
    // `common::standard_suite::*` with an ecosystem-specific
    // `EcosystemSpec` declaring fixture + expectations.
    //
    // Keep this list sorted. Each entry maps to a fixture under
    // `tests/e2e/fixtures/<name>-hello/`. The python and python-uv
    // adapters share a single `python-hello` fixture (their detection
    // markers diverge but the test surface is the same), so the list
    // doesn't 1:1 with the built-in adapter count.
    pub mod bun;
    pub mod cargo;
    pub mod deno;
    pub mod go;
    pub mod gradle;
    pub mod maven;
    pub mod node_npm;
    pub mod node_pnpm;
    pub mod node_yarn;
    pub mod php;
    pub mod python;
    pub mod ruby;

    // Polyglot monorepo fixtures (Phase 3). Exercise multi-dish plan
    // + cache + dep-graph cascade invariants that the per-ecosystem
    // `standard_suite` can't reach because each of those operates on
    // a single-dish fixture.
    pub mod monorepo_cargo_pnpm;
    pub mod monorepo_dep_cascade;
    pub mod monorepo_go_node;

    // Container-mode smoke (Phase 4). Skipped on hosts without a
    // container runtime.
    pub mod container;

    // Deploy-integration stubs (Phase 4). Route railway/vercel calls
    // to stub CLIs under `tests/e2e/bin/` so we exercise the adapter
    // wiring end-to-end without touching real infra.
    pub mod deploy;

    // Migration end-to-end: drive `bento migrate <tool>` against
    // realistic fixture monorepos for each source tool (lerna, make,
    // moon, rush), then validate the migrated result with
    // `bento plan --json`. Per-migrator unit tests live in
    // `src/migrate/<tool>.rs`; this is the cross-binary contract.
    pub mod migrate;
}
