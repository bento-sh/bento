//! Workspace-aware plugin registry construction.
//!
//! Thin re-export: the implementation lives in [`bento_core::plugins`]
//! so `plan_at` / `ci_at` / `notify_at` (and therefore `bento-mcp`)
//! build the same plugin-aware registry the CLI's direct `Executor`
//! call sites do.

pub use bento_core::build_registry;
