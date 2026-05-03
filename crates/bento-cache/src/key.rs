//! Streaming blake3 hasher and hex-encoded [`CacheKey`].

use std::fmt;
use std::path::Path;

/// Hex-encoded 256-bit blake3 hash identifying a cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    /// Build a key from an already-computed hex string. Intended for tests
    /// and for deserialising a key that came over the wire.
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }

    /// Short prefix useful for human-readable logs (`bento why abcd1234`).
    pub fn short(&self) -> &str {
        &self.0[..12.min(self.0.len())]
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Streaming hasher — accept file contents and named extras in any order,
/// finalize into a [`CacheKey`].
///
/// Hash format prefixes every addition with a type tag (`file:` / `extra:`)
/// and a NUL separator so that the same bytes delivered via different calls
/// never collide.
pub struct Hasher {
    inner: blake3::Hasher,
}

impl Hasher {
    const FORMAT_TAG: &'static [u8] = b"bento-cas-v1\0";

    pub fn new() -> Self {
        let mut inner = blake3::Hasher::new();
        inner.update(Self::FORMAT_TAG);
        Self { inner }
    }

    /// Mix in a file's relative path + content. Path is included so that two
    /// files with identical bytes at different locations hash differently.
    pub fn add_file(&mut self, rel: &Path, content: &[u8]) {
        self.inner.update(b"file:");
        self.inner.update(rel.to_string_lossy().as_bytes());
        self.inner.update(b"\0");
        // Include length so a 10-byte file followed by a 0-byte file can't
        // produce the same hash as an 8-byte file followed by a 2-byte one.
        self.inner.update(&(content.len() as u64).to_le_bytes());
        self.inner.update(content);
    }

    /// Read a file from disk and mix it in.
    pub fn add_file_from_disk(&mut self, rel: &Path, full: &Path) -> std::io::Result<()> {
        let content = std::fs::read(full)?;
        self.add_file(rel, &content);
        Ok(())
    }

    /// Mix in a key=value extra (task command, toolchain version, env var).
    pub fn add_extra(&mut self, key: &str, value: &str) {
        self.inner.update(b"extra:");
        self.inner.update(key.as_bytes());
        self.inner.update(b"=");
        self.inner.update(value.as_bytes());
        self.inner.update(b"\0");
    }

    pub fn finalize(self) -> CacheKey {
        CacheKey(self.inner.finalize().to_hex().to_string())
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_inputs_same_key() {
        let mut a = Hasher::new();
        a.add_file(Path::new("main.rs"), b"fn main() {}");
        a.add_extra("cmd", "cargo build");

        let mut b = Hasher::new();
        b.add_file(Path::new("main.rs"), b"fn main() {}");
        b.add_extra("cmd", "cargo build");

        assert_eq!(a.finalize(), b.finalize());
    }

    #[test]
    fn different_contents_different_keys() {
        let mut a = Hasher::new();
        a.add_file(Path::new("x"), b"aaa");
        let mut b = Hasher::new();
        b.add_file(Path::new("x"), b"bbb");
        assert_ne!(a.finalize(), b.finalize());
    }

    #[test]
    fn different_paths_different_keys() {
        let mut a = Hasher::new();
        a.add_file(Path::new("main.rs"), b"x");
        let mut b = Hasher::new();
        b.add_file(Path::new("other.rs"), b"x");
        assert_ne!(a.finalize(), b.finalize());
    }

    #[test]
    fn different_extras_different_keys() {
        let mut a = Hasher::new();
        a.add_extra("cmd", "go build");
        let mut b = Hasher::new();
        b.add_extra("cmd", "go build -v");
        assert_ne!(a.finalize(), b.finalize());
    }

    #[test]
    fn length_prefix_prevents_smuggling() {
        // With no length prefix, these two sequences could collide because
        // the file bytes run directly into the extra bytes.
        let mut a = Hasher::new();
        a.add_file(Path::new("x"), b"hello");
        a.add_extra("k", "world");

        let mut b = Hasher::new();
        b.add_file(Path::new("x"), b"helloworld");
        b.add_extra("k", "");

        assert_ne!(a.finalize(), b.finalize());
    }

    #[test]
    fn hex_is_64_chars() {
        let key = Hasher::new().finalize();
        assert_eq!(key.as_hex().len(), 64);
    }

    #[test]
    fn short_is_12_chars() {
        let key = Hasher::new().finalize();
        assert_eq!(key.short().len(), 12);
    }

    #[test]
    fn from_hex_round_trips() {
        let original = Hasher::new().finalize();
        let rebuilt = CacheKey::from_hex(original.as_hex());
        assert_eq!(original, rebuilt);
    }

    #[test]
    fn add_file_from_disk_matches_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        std::fs::write(&file, b"hello disk").unwrap();

        let mut disk = Hasher::new();
        disk.add_file_from_disk(Path::new("data.bin"), &file)
            .unwrap();

        let mut mem = Hasher::new();
        mem.add_file(Path::new("data.bin"), b"hello disk");

        assert_eq!(disk.finalize(), mem.finalize());
    }
}
