//! Parser for `ruff check --output-format=json` output.
//!
//! Top-level array of issues. Each issue has `filename`, `code`,
//! `message`, `location: {row, column}`, optionally `end_location`.
//! Ruff doesn't emit a per-issue severity field — every finding is
//! a lint warning by definition (you opted into the rule).

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
    let Ok(issues) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(issues) = issues.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for issue in issues {
        let Some(filename) = issue.get("filename").and_then(Value::as_str) else {
            continue;
        };
        let Some(loc) = issue.get("location") else {
            continue;
        };
        let line_no = loc.get("row").and_then(Value::as_u64).unwrap_or(1) as u32;
        let col = loc.get("column").and_then(Value::as_u64).map(|v| v as u32);

        let end_line = issue
            .get("end_location")
            .and_then(|e| e.get("row"))
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        let end_col = issue
            .get("end_location")
            .and_then(|e| e.get("column"))
            .and_then(Value::as_u64)
            .map(|v| v as u32);

        let message = issue
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let rule = issue
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut diag = Diagnostic::new(
            normalise_path(filename, dish_dir, workspace_root),
            line_no,
            Severity::Warning,
            message,
            "ruff",
        );
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dish() -> PathBuf {
        PathBuf::from("/repo/services/worker")
    }
    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn parses_an_issue_into_a_diagnostic() {
        let stdout = r#"[{
            "code": "F401",
            "url": "https://docs.astral.sh/ruff/rules/unused-import",
            "message": "`os` imported but unused",
            "fix": null,
            "location": {"row": 1, "column": 1},
            "end_location": {"row": 1, "column": 10},
            "filename": "/repo/services/worker/src/main.py",
            "noqa_row": 1
        }]"#;
        let d = &parse(stdout, "", &dish(), &root())[0];
        assert_eq!(d.file, "services/worker/src/main.py");
        assert_eq!(d.line, 1);
        assert_eq!(d.col, Some(1));
        assert_eq!(d.end_line, Some(1));
        assert_eq!(d.end_col, Some(10));
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.message, "`os` imported but unused");
        assert_eq!(d.rule.as_deref(), Some("F401"));
        assert_eq!(d.source, "ruff");
    }

    #[test]
    fn empty_array_yields_empty() {
        assert!(parse("[]", "", &dish(), &root()).is_empty());
    }

    #[test]
    fn issue_without_end_location_omits_range_fields() {
        let stdout = r#"[{
            "code": "F401",
            "message": "x",
            "location": {"row": 5, "column": 3},
            "filename": "src/main.py"
        }]"#;
        let d = &parse(stdout, "", &dish(), &root())[0];
        assert_eq!(d.line, 5);
        assert_eq!(d.col, Some(3));
        assert!(d.end_line.is_none());
        assert!(d.end_col.is_none());
    }
}
