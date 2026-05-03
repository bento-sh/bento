//! Parser for `cargo --message-format=json` output.
//!
//! cargo emits one JSON value per line. We care about lines with
//! `reason = "compiler-message"`, which carry an embedded rustc/clippy
//! diagnostic in `message.spans[]`. Only primary spans become
//! Diagnostic records; non-primary spans are context (the "expected
//! type was here" arrows) that the user can read inline in the
//! original tool output.

use std::path::Path;

use serde_json::Value;

use super::normalise_path;
use bento_adapters::{Diagnostic, Severity};

pub(super) fn parse(
    stdout: &str,
    _stderr: &str,
    dish_dir: &Path,
    workspace_root: &Path,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };

        let level = message
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("warning");
        let severity = match level {
            "error" | "failure-note" | "error: internal compiler error" => Severity::Error,
            "warning" => Severity::Warning,
            "note" => Severity::Info,
            "help" => Severity::Hint,
            _ => Severity::Warning,
        };

        let msg = message
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let rule = message
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let Some(spans) = message.get("spans").and_then(Value::as_array) else {
            continue;
        };
        for span in spans {
            if span.get("is_primary").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some(file_name) = span.get("file_name").and_then(Value::as_str) else {
                continue;
            };
            let line_start = span.get("line_start").and_then(Value::as_u64).unwrap_or(1) as u32;
            let line_end = span
                .get("line_end")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            let col_start = span
                .get("column_start")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            let col_end = span
                .get("column_end")
                .and_then(Value::as_u64)
                .map(|v| v as u32);

            let mut diag = Diagnostic::new(
                normalise_path(file_name, dish_dir, workspace_root),
                line_start,
                severity,
                msg.clone(),
                "cargo",
            );
            if let Some(c) = col_start {
                diag = diag.with_col(c);
            }
            if let (Some(le), Some(ce)) = (line_end, col_end) {
                diag = diag.with_range(le, ce);
            }
            if let Some(r) = &rule {
                diag = diag.clone().with_rule(r);
            }
            out.push(diag);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dish() -> PathBuf {
        PathBuf::from("/repo/crates/foo")
    }
    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn parses_a_compiler_message_with_primary_span() {
        // Real shape captured from `cargo build --message-format=json`,
        // trimmed to the fields we care about.
        let line = r#"{
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "cannot find value `Foo` in this scope",
                "code": {"code": "E0425", "explanation": "..."},
                "spans": [{
                    "file_name": "src/main.rs",
                    "is_primary": true,
                    "line_start": 5, "line_end": 5,
                    "column_start": 9, "column_end": 12
                }]
            }
        }"#
        .replace('\n', " ");

        let diags = parse(&line, "", &dish(), &root());
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.file, "crates/foo/src/main.rs");
        assert_eq!(d.line, 5);
        assert_eq!(d.col, Some(9));
        assert_eq!(d.end_line, Some(5));
        assert_eq!(d.end_col, Some(12));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "cannot find value `Foo` in this scope");
        assert_eq!(d.rule.as_deref(), Some("E0425"));
        assert_eq!(d.source, "cargo");
    }

    #[test]
    fn ignores_non_compiler_message_lines() {
        let stdout = r#"{"reason":"build-script-executed","package_id":"x"}
{"reason":"compiler-artifact","target":{"name":"x"}}"#;
        assert!(parse(stdout, "", &dish(), &root()).is_empty());
    }

    #[test]
    fn skips_non_primary_spans() {
        let line = r#"{
            "reason": "compiler-message",
            "message": {
                "level": "warning",
                "message": "unused import",
                "spans": [
                    {"file_name": "src/lib.rs", "is_primary": false, "line_start": 1, "line_end": 1, "column_start": 1, "column_end": 5},
                    {"file_name": "src/lib.rs", "is_primary": true,  "line_start": 3, "line_end": 3, "column_start": 1, "column_end": 9}
                ]
            }
        }"#.replace('\n', " ");
        let diags = parse(&line, "", &dish(), &root());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 3);
    }

    #[test]
    fn maps_levels_to_severities() {
        for (level, want) in [
            ("error", Severity::Error),
            ("warning", Severity::Warning),
            ("note", Severity::Info),
            ("help", Severity::Hint),
        ] {
            let line = format!(
                r#"{{"reason":"compiler-message","message":{{"level":"{level}","message":"x","spans":[{{"file_name":"a.rs","is_primary":true,"line_start":1,"line_end":1,"column_start":1,"column_end":2}}]}}}}"#
            );
            let d = &parse(&line, "", &dish(), &root())[0];
            assert_eq!(d.severity, want, "level={level}");
        }
    }

    #[test]
    fn handles_message_with_no_rule_code() {
        let line = r#"{"reason":"compiler-message","message":{"level":"warning","message":"hi","spans":[{"file_name":"a.rs","is_primary":true,"line_start":1,"line_end":1,"column_start":1,"column_end":2}]}}"#;
        let d = &parse(line, "", &dish(), &root())[0];
        assert!(d.rule.is_none());
    }
}
