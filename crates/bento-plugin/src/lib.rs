//! Host side of the bento subprocess plugin protocol.
//!
//! Spawns a child binary, handshakes (`initialize`), exposes a typed
//! synchronous `Client::call` for further requests, and tears the child
//! down cleanly (`shutdown` request → 2s grace → SIGTERM → SIGKILL).

pub mod client;
pub mod framing;
pub mod protocol;
pub mod wire;

pub use client::{Client, NoopNotifier, Notifier, PROTOCOL_VERSION};
pub use protocol::{ErrorCode, RpcError};
pub use wire::{Capabilities, DefaultTask, LogLevel, LogStream, Manifest, ToolVersion};

/// Default per-call timeout for query methods (detect, manifest, …).
pub const DEFAULT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Default per-call timeout for `install`. Long-running by design.
pub const DEFAULT_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
