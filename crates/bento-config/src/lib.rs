//! Configuration for bento: `bento.toml`, `bentos/*.toml`, `dish.toml`.
//!
//! Exposes strongly-typed schemas and a [`Workspace`] that walks a repo,
//! parses every config file, and returns a validated in-memory model.

mod error;
mod schema;
mod workspace;

pub use error::ConfigError;
pub use schema::{
    BentoConfig, CacheConfig, ContainerMode, Defaults, DishConfig, Environment, ExecutionConfig,
    GarnishSpec, GhaCache, PluginsConfig, RepoConfig, ServeConfig, Task, TelemetryConfig,
    ToolchainPin,
};
pub use workspace::{LoadedBento, LoadedDish, Workspace};
