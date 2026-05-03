//! Shared parser for `.tool-versions` (asdf / mise convention).
//!
//! Each line is `<tool> <version>` whitespace-separated; `#` is a comment
//! marker. `mise.toml` is supported by mise too but uses TOML — we'd need
//! a separate parser for that; defer until someone asks.
//!
//! Tool naming differs slightly between asdf plugins (e.g. asdf-nodejs
//! calls Node `nodejs`, asdf-golang calls Go `golang`). Callers pass
//! every accepted alias for their tool; first hit wins.

use std::path::Path;

use anyhow::{Context, Result};

/// Read `<dir>/.tool-versions` and return the version string for the
/// first alias that matches a line. Returns `Ok(None)` when the file is
/// absent or the tool isn't listed.
pub fn read_tool_version(dir: &Path, aliases: &[&str]) -> Result<Option<String>> {
    let path = dir.join(".tool-versions");
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_tool_version(&raw, aliases))
}

/// Parse content of a `.tool-versions` file. Whitespace-tolerant and
/// strips line comments (`# ...` after a tool entry is ignored — the
/// version is the first whitespace-delimited token after the tool name).
pub fn parse_tool_version(content: &str, aliases: &[&str]) -> Option<String> {
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let tool = parts.next()?;
        if !aliases.contains(&tool) {
            continue;
        }
        let version = parts.next()?;
        if version.is_empty() || version.starts_with('#') {
            continue;
        }
        return Some(version.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with_file(name: &str, body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), body).unwrap();
        dir
    }

    #[test]
    fn parses_tool_with_single_alias() {
        let raw = "nodejs 22.1.0\npython 3.12\n";
        assert_eq!(parse_tool_version(raw, &["nodejs"]), Some("22.1.0".into()));
    }

    #[test]
    fn first_matching_alias_wins() {
        let raw = "nodejs 22.1.0\nnode 20.0.0\n";
        // 'node' alias matches the second line; 'nodejs' matches the first.
        // We try aliases per-line in order, so the FIRST matching LINE wins.
        assert_eq!(
            parse_tool_version(raw, &["node", "nodejs"]),
            Some("22.1.0".into())
        );
    }

    #[test]
    fn skips_comments_and_blanks() {
        let raw = "\n# pinned for CI\n\nnodejs 22.1.0\n# python 3.10\n";
        assert_eq!(parse_tool_version(raw, &["nodejs"]), Some("22.1.0".into()));
    }

    #[test]
    fn returns_none_when_tool_absent() {
        let raw = "python 3.12\nruby 3.2.2\n";
        assert_eq!(parse_tool_version(raw, &["nodejs", "node"]), None);
    }

    #[test]
    fn returns_none_when_no_version_after_tool() {
        let raw = "nodejs\n";
        assert_eq!(parse_tool_version(raw, &["nodejs"]), None);
    }

    #[test]
    fn ignores_extra_tokens_after_version() {
        // asdf/mise both accept `nodejs system` or multiple versions.
        // We always take the first version token.
        let raw = "nodejs 22.1.0 lts/iron\n";
        assert_eq!(parse_tool_version(raw, &["nodejs"]), Some("22.1.0".into()));
    }

    #[test]
    fn read_returns_none_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_tool_version(dir.path(), &["nodejs"])
            .unwrap()
            .is_none());
    }

    #[test]
    fn read_finds_tool_in_file() {
        let dir = tmp_with_file(".tool-versions", "nodejs 22.1.0\n");
        let v = read_tool_version(dir.path(), &["nodejs"]).unwrap();
        assert_eq!(v, Some("22.1.0".into()));
    }
}
