//! Parser for `eslint --format=json` output.
//!
//! Top-level array of `{filePath, messages: [...]}`. Each message
//! carries a numeric severity (1=warn, 2=error), a ruleId, and
//! line/column ranges.

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
    let Ok(files) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(files) = files.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for file_result in files {
        let Some(file_path) = file_result.get("filePath").and_then(Value::as_str) else {
            continue;
        };
        let Some(messages) = file_result.get("messages").and_then(Value::as_array) else {
            continue;
        };
        let normalised = normalise_path(file_path, dish_dir, workspace_root);

        for msg in messages {
            let line_no = msg.get("line").and_then(Value::as_u64).unwrap_or(1) as u32;
            let col = msg.get("column").and_then(Value::as_u64).map(|v| v as u32);
            let end_line = msg.get("endLine").and_then(Value::as_u64).map(|v| v as u32);
            let end_col = msg
                .get("endColumn")
                .and_then(Value::as_u64)
                .map(|v| v as u32);

            let severity = match msg.get("severity").and_then(Value::as_u64) {
                Some(2) => Severity::Error,
                Some(1) => Severity::Warning,
                _ => Severity::Warning,
            };

            let text = msg
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let rule = msg
                .get("ruleId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let mut diag = Diagnostic::new(normalised.clone(), line_no, severity, text, "eslint");
            if let Some(c) = col {
                diag = diag.with_col(c);
            }
            if let (Some(le), Some(ce)) = (end_line, end_col) {
                diag = diag.with_range(le, ce);
            }
            if let Some(r) = rule {
                diag = diag.with_rule(r);
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
        PathBuf::from("/repo/apps/web")
    }
    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn parses_a_message_into_a_diagnostic() {
        let stdout = r#"[
            {
                "filePath": "/repo/apps/web/src/App.tsx",
                "messages": [{
                    "ruleId": "no-unused-vars",
                    "severity": 2,
                    "message": "'foo' is defined but never used.",
                    "line": 10, "column": 7,
                    "endLine": 10, "endColumn": 12
                }]
            }
        ]"#;
        let d = &parse(stdout, "", &dish(), &root())[0];
        assert_eq!(d.file, "apps/web/src/App.tsx");
        assert_eq!(d.line, 10);
        assert_eq!(d.col, Some(7));
        assert_eq!(d.end_line, Some(10));
        assert_eq!(d.end_col, Some(12));
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("not been used") || d.message.contains("never used"));
        assert_eq!(d.rule.as_deref(), Some("no-unused-vars"));
        assert_eq!(d.source, "eslint");
    }

    #[test]
    fn severity_one_maps_to_warning() {
        let stdout = r#"[{"filePath":"/repo/apps/web/x.js","messages":[{"ruleId":"r","severity":1,"message":"m","line":1,"column":1}]}]"#;
        assert_eq!(
            parse(stdout, "", &dish(), &root())[0].severity,
            Severity::Warning
        );
    }

    #[test]
    fn missing_rule_id_yields_no_rule_field() {
        let stdout = r#"[{"filePath":"/repo/apps/web/x.js","messages":[{"ruleId":null,"severity":2,"message":"m","line":1,"column":1}]}]"#;
        assert!(parse(stdout, "", &dish(), &root())[0].rule.is_none());
    }

    #[test]
    fn file_with_no_messages_yields_no_diagnostics() {
        let stdout = r#"[{"filePath":"/repo/apps/web/clean.js","messages":[]}]"#;
        assert!(parse(stdout, "", &dish(), &root()).is_empty());
    }

    #[test]
    fn multiple_files_each_contribute_diagnostics() {
        let stdout = r#"[
            {"filePath":"/repo/apps/web/a.js","messages":[{"ruleId":"r1","severity":2,"message":"a","line":1,"column":1}]},
            {"filePath":"/repo/apps/web/b.js","messages":[{"ruleId":"r2","severity":1,"message":"b","line":2,"column":2}]}
        ]"#;
        let diags = parse(stdout, "", &dish(), &root());
        assert_eq!(diags.len(), 2);
        assert!(diags
            .iter()
            .any(|d| d.file == "apps/web/a.js" && d.severity == Severity::Error));
        assert!(diags
            .iter()
            .any(|d| d.file == "apps/web/b.js" && d.severity == Severity::Warning));
    }
}
