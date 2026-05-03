//! Parser for `golangci-lint --out-format=json` output.
//!
//! Top-level object with an `Issues` array. Each issue carries a
//! `FromLinter` (the specific linter id), a `Text`, and a `Pos` with
//! `Filename`/`Line`/`Column`. Severity is per-linter and frequently
//! empty — we default to Warning since golangci-lint findings are
//! lint, not compile errors.

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
    let Ok(root) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(issues) = root.get("Issues").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for issue in issues {
        let Some(pos) = issue.get("Pos") else {
            continue;
        };
        let Some(filename) = pos.get("Filename").and_then(Value::as_str) else {
            continue;
        };
        let line_no = pos.get("Line").and_then(Value::as_u64).unwrap_or(1) as u32;
        let col = pos.get("Column").and_then(Value::as_u64).map(|v| v as u32);

        let text = issue
            .get("Text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let from_linter = issue
            .get("FromLinter")
            .and_then(Value::as_str)
            .map(str::to_string);

        let severity = match issue.get("Severity").and_then(Value::as_str) {
            Some("error") => Severity::Error,
            Some("info") => Severity::Info,
            Some("hint") => Severity::Hint,
            // Most linters leave Severity empty; default to warning
            // since these findings are lint, not compile failures.
            _ => Severity::Warning,
        };

        let mut diag = Diagnostic::new(
            normalise_path(filename, dish_dir, workspace_root),
            line_no,
            severity,
            text,
            "golangci-lint",
        );
        if let Some(c) = col {
            // golangci-lint emits 0 when the linter didn't report a
            // column; treat 0 as "no column" rather than "column 0".
            if c > 0 {
                diag = diag.with_col(c);
            }
        }
        if let Some(r) = from_linter {
            diag = diag.with_rule(r);
        }
        out.push(diag);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dish() -> PathBuf {
        PathBuf::from("/repo/apps/api")
    }
    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn parses_an_issue_into_a_diagnostic() {
        let stdout = r#"{
            "Issues": [{
                "FromLinter": "errcheck",
                "Text": "Error return value of `os.Setenv` is not checked",
                "Severity": "",
                "Pos": {"Filename": "main.go", "Offset": 0, "Line": 42, "Column": 5}
            }],
            "Report": {}
        }"#;
        let diags = parse(stdout, "", &dish(), &root());
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.file, "apps/api/main.go");
        assert_eq!(d.line, 42);
        assert_eq!(d.col, Some(5));
        assert_eq!(d.severity, Severity::Warning);
        assert!(d.message.contains("Error return value"));
        assert_eq!(d.rule.as_deref(), Some("errcheck"));
        assert_eq!(d.source, "golangci-lint");
    }

    #[test]
    fn explicit_severity_overrides_default() {
        let stdout = r#"{"Issues":[{"FromLinter":"x","Text":"t","Severity":"error","Pos":{"Filename":"a.go","Line":1,"Column":1}}]}"#;
        assert_eq!(
            parse(stdout, "", &dish(), &root())[0].severity,
            Severity::Error
        );
    }

    #[test]
    fn omits_zero_column_as_no_column() {
        let stdout = r#"{"Issues":[{"FromLinter":"x","Text":"t","Pos":{"Filename":"a.go","Line":5,"Column":0}}]}"#;
        let d = &parse(stdout, "", &dish(), &root())[0];
        assert!(d.col.is_none(), "column 0 should mean 'no column reported'");
    }

    #[test]
    fn empty_issues_array_returns_empty() {
        let stdout = r#"{"Issues": [], "Report": {}}"#;
        assert!(parse(stdout, "", &dish(), &root()).is_empty());
    }

    #[test]
    fn missing_issues_field_returns_empty() {
        let stdout = r#"{"Report": {}}"#;
        assert!(parse(stdout, "", &dish(), &root()).is_empty());
    }
}
