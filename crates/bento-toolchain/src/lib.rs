//! Embedded toolchain version manager — installs and pins per-dish
//! language toolchains (Go, Node, Bun, Deno, Rust) into `~/.bento/tools/`.
//!
//! Bento opts you in at the **repo** level (`bento.toml` `[toolchain]`),
//! lets you override at the **dish** level (`dish.toml` `[toolchain]`),
//! and falls back to whatever the **adapter** parses from the project
//! (e.g. `go.mod`'s `go <version>` directive). If nothing is pinned, the
//! system's own `PATH` answers.
//!
//! The toolchain is only ever prepended to a child process's `PATH` —
//! it never modifies the user's shell.

mod go;
mod installer;
mod node;
mod python;
mod resolver;
mod store;
mod target;
mod tool;

pub use go::GoTool;
pub use installer::Installer;
pub use node::NodeTool;
pub use python::PythonTool;
pub use resolver::{Resolution, ResolutionSource, Resolver};
pub use store::Store;
pub use target::{Arch, Os, Target};
pub use tool::{ArchiveFormat, ChecksumFormat, DownloadSpec, Tool};
