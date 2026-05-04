//! bento core engine — graph, planner, executor.
//!
//! - [`diff::GitDiff`] — coarse dir-level change detection (P0 pre-filter).
//! - [`plan`] — compute a [`Plan`] of which tasks will run, which will cache-hit.
//! - [`run`] — execute a plan: run misses, restore hits, cache successes.

pub mod artifacts;
pub mod cascade;
pub mod deploy_state;
pub mod diagnostic_parsers;
pub mod diff;
pub mod discovery;
pub mod doctor;
pub mod garnish;
pub mod graph;
pub mod inventory;
pub mod plan;
pub mod prime;
pub mod report;
pub mod run;
pub mod why;

pub use diff::GitDiff;
pub use discovery::{scan_orphan_dishes, scan_orphans};
pub use doctor::{CheckStatus, DoctorCheck, DoctorReport, DoctorSummary};
pub use garnish::{GarnishPayload, GarnishPayloadTrigger, GARNISH_PAYLOAD_SCHEMA_VERSION};
pub use graph::{build as build_graph, BentoGraph, GraphError};
pub use plan::{
    default_cache_root, find_workspace_root, plan_at, MissReason, Plan, PlanOptions, PlannedBento,
    PlannedDish, PlannedTask, Planner, TaskStatus, WorkspaceNotFound,
};
pub use run::{
    ci_at, notify_at, resolve_target, CiOptions, ExecutedBento, ExecutedDish, ExecutedTask,
    ExecutionReport, ExecutionSummary, Executor, InstallRecord, TargetRef, TargetRefError,
    TaskOutcome,
};

// Re-exports so the CLI can compose these without a direct dep on
// bento-config / bento-cache / bento-adapters / bento-toolchain.
pub use bento_adapters::{
    AdapterRegistry, CliRequirement, Diagnostic, DiagnosticHook, DiagnosticParser, DiagnosticRerun,
    InstallProbe, Integration, IntegrationRegistry, IntegrationTask, IntegrationTaskKind,
    LanguageAdapter, ParserId, Severity,
};
pub use bento_cache::{
    build_remote, BearerRemote, CacheKey, InputManifest, LocalCache, ManifestFile, RemoteCache,
    S3Remote,
};
pub use bento_config::{LoadedDish, Workspace};
pub use bento_toolchain::{
    CoRequired, Installer, Resolution, ResolutionSource, Resolver, Store, Target, Tool,
};
