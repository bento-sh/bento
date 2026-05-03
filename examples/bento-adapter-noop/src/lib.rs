//! Empty lib target. Exists so other workspace crates can declare
//! `bento-adapter-noop` as a dev-dependency to force cargo to build the
//! binary before their integration tests run. The actual plugin lives
//! in `src/main.rs`.
