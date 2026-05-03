//! [`InputManifest`] — a human-readable explainer for a cache key.
//!
//! Written to `<root>/<key>.inputs.json` alongside the tar bundle whenever a
//! task is cached. Read back by `bento why <hash>` to explain exactly what
//! went into the hash.
//!
//! Deliberately does NOT record env var *values* — those may be secrets.
//! Only the variable names are stored; the values live only in the cache
//! key itself.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InputManifest {
    /// Bump when the manifest schema changes.
    pub version: u32,
    pub task_name: String,
    /// The command string as executed (post-substitution).
    pub run: String,
    /// Dish name (not path) for display.
    pub dish: String,
    /// Language adapter id, if any (`"go"`, `"node-npm"`, ...).
    pub adapter: Option<String>,
    /// `tool:version` resolved from the adapter, e.g. `"go:1.22.3"`.
    pub toolchain: Option<String>,
    /// Bento version mixed into the key (major.minor).
    pub bento_version: String,
    /// Host triple the cache entry was built on — `"<arch>-<os>"`, e.g.
    /// `"x86_64-linux"` or `"aarch64-macos"`. Mixed into the cache key
    /// so an x86_64 builder and an aarch64 puller can never share a
    /// false hit on a remote cache. `None` for entries written by
    /// pre-v0.1 bento (manifest version 1); those are correctly
    /// invalidated by the v2 hashing change so the absence here is
    /// only ever a back-compat read of stale on-disk state.
    #[serde(default)]
    pub host: Option<String>,
    /// Env var names whose values were hashed. Values are NOT stored.
    pub env_vars: Vec<String>,
    /// Every file hashed, with its individual blake3 digest.
    pub files: Vec<ManifestFile>,
}

impl InputManifest {
    /// Bump on every schema change. `2` adds `host`.
    pub const CURRENT_VERSION: u32 = 2;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManifestFile {
    pub path: PathBuf,
    /// Hex-encoded blake3 hash of this file's contents.
    pub blake3: String,
    pub size_bytes: u64,
}
