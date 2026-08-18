//! Minimal JSONC → JSON pre-pass.
//!
//! Several agent clients ship *commented* JSON: Zed's
//! `settings.json`, VS Code-family `*.json` configs, and hand-edited
//! `.mcp.json` files all tolerate `//` and `/* */`. `serde_json` does
//! not, so anything reading those files needs this first.
//!
//! Comments are replaced by spaces rather than removed, so byte
//! offsets (and line/column numbers in parse errors) still line up
//! with the original text.

/// Blank out `//` line comments and `/* … */` block comments,
/// preserving newlines and the total byte length. Comment markers
/// inside string literals are left alone.
pub fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let mut in_string = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 2,
                b'"' => {
                    in_string = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match (b, bytes.get(i + 1)) {
            (b'"', _) => {
                in_string = true;
                i += 1;
            }
            (b'/', Some(b'/')) => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                while i < bytes.len() {
                    let end = bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/');
                    if bytes[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                    if end {
                        out[i] = b' ';
                        i += 1;
                        break;
                    }
                }
            }
            _ => i += 1,
        }
    }

    // Only ASCII bytes inside comments were overwritten, and only with
    // ASCII spaces — every retained byte keeps its original position,
    // so multi-byte sequences outside comments stay intact.
    String::from_utf8(out).expect("blanking comment bytes preserves UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> serde_json::Value {
        serde_json::from_str(&strip_comments(src)).unwrap()
    }

    #[test]
    fn strips_line_and_block_comments() {
        let src =
            "{\n  // leading\n  \"a\": 1, /* inline */\n  \"b\": 2\n  /* multi\n     line */\n}";
        assert_eq!(parse(src), serde_json::json!({"a": 1, "b": 2}));
    }

    #[test]
    fn preserves_length_and_newlines() {
        let src = "{\n// x\n\"a\":1}";
        let out = strip_comments(src);
        assert_eq!(out.len(), src.len());
        assert_eq!(out.matches('\n').count(), src.matches('\n').count());
    }

    #[test]
    fn leaves_comment_markers_inside_strings() {
        let src = r#"{"url": "https://x.dev/a", "esc": "a\"// b", "blk": "/* c */"}"#;
        assert_eq!(strip_comments(src), src);
    }

    #[test]
    fn handles_unterminated_block_comment() {
        let src = "{\"a\":1} /* dangling";
        assert_eq!(parse(src), serde_json::json!({"a": 1}));
    }

    #[test]
    fn keeps_non_ascii_outside_comments() {
        let src = "{\"café\": \"☕\"} // ☕ noted";
        let out = strip_comments(src);
        assert_eq!(out.len(), src.len());
        assert_eq!(parse(src), serde_json::json!({"café": "☕"}));
    }
}
