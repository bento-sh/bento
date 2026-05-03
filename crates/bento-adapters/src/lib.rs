//! Language adapters — the extension point that teaches bento about a
//! specific language / package manager (Go, Node, Bun, Deno, Cargo, ...).
//!
//! An adapter is a small trait implementation that knows how to:
//!
//! - detect its language in a directory,
//! - list the files worth fingerprinting for cache keys (lockfiles, pins),
//! - read the project's required toolchain version,
//! - install dependencies,
//! - and supply default task recipes (build / test / lint).
//!
//! The built-in adapters live in submodules; [`AdapterRegistry::builtin`]
//! collects them into a single lookup.

mod adapter;
mod bun;
mod cargo;
mod cloudflare_pages;
mod cloudflare_worker;
mod deno;
mod diagnostic;
mod diagnostic_record;
mod go;
mod gradle;
mod integration;
mod linear;
mod maven;
mod node_common;
mod node_npm;
mod php;
mod plugin_adapter;
mod plugin_search;
mod pnpm;
mod probe;
mod python;
mod python_uv;
mod railway;
mod registry;
mod ruby;
mod slack;
mod tool_versions;
mod vercel;
mod yarn;

pub use adapter::{
    run_add_cmd, run_install_cmd, AddOptions, Added, DefaultTask, DetectedTask, InstallProbe,
    LanguageAdapter, TaskContext, ToolVersion,
};
pub use bun::BunAdapter;
pub use cargo::CargoAdapter;
pub use cloudflare_pages::CloudflarePagesIntegration;
pub use cloudflare_worker::CloudflareWorkerIntegration;
pub use deno::DenoAdapter;
pub use diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};
pub use diagnostic_record::{Diagnostic, Severity};
pub use go::GoAdapter;
pub use gradle::GradleAdapter;
pub use integration::{
    CliRequirement, Integration, IntegrationRegistry, IntegrationTask, IntegrationTaskKind,
};
pub use linear::LinearIntegration;
pub use maven::MavenAdapter;
pub use node_npm::NodeNpmAdapter;
pub use php::PhpAdapter;
pub use plugin_adapter::SubprocessAdapter;
pub use plugin_search::{discover_plugins, DiscoveredPlugin, PluginSearchOptions};
pub use pnpm::PnpmAdapter;
pub use python::PythonAdapter;
pub use python_uv::PythonUvAdapter;
pub use railway::RailwayIntegration;
pub use registry::AdapterRegistry;
pub use ruby::RubyAdapter;
pub use slack::SlackIntegration;
pub use vercel::VercelIntegration;
pub use yarn::YarnAdapter;
