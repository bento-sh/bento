//! Structured tool diagnostics — file/line/severity/message records
//! lifted from compiler & linter output and surfaced on the
//! ExecutionReport.
//!
//! This module is the **shape only**: the `Diagnostic` struct, the
//! `Severity` enum, and the JSON-Schema derive. Parser implementations
//! and executor integration live in adapter modules + the executor.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A single structured diagnostic — one error/warning/etc. produced by
/// a compiler or linter and parsed into bento's normalised shape.
///
/// Field ordering chosen so JSON output reads naturally for humans
/// (file/line/severity/message first; tool-specific metadata last).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    /// Path **relative to the workspace root**, forward slashes.
    /// Agents can `Read(file)` directly without further resolution.
    pub file: String,

    /// 1-based line number.
    pub line: u32,

    /// 1-based column. Omitted when the tool didn't report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,

    /// 1-based last line of the diagnostic range, inclusive. Omitted
    /// for single-line / point diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,

    /// 1-based last column of the diagnostic range, exclusive
    /// (LSP convention). Omitted unless the tool reported a range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_col: Option<u32>,

    pub severity: Severity,

    /// Human-readable description, normalised to a single line where
    /// the tool emits multi-line output.
    pub message: String,

    /// Tool-specific rule id (`E0308`, `no-unused-vars`,
    /// `clippy::needless_clone`). Omitted when the tool didn't tag
    /// the diagnostic with a rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,

    /// Stable identifier of the tool that produced the diagnostic
    /// (`cargo`, `eslint`, `golangci-lint`, ...). Lets agents key off
    /// the tool when ergonomics differ.
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Diagnostic {
    /// Convenience builder for tests and parsers — the smallest valid
    /// diagnostic is file + line + severity + message + source.
    pub fn new(
        file: impl Into<String>,
        line: u32,
        severity: Severity,
        message: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            line,
            col: None,
            end_line: None,
            end_col: None,
            severity,
            message: message.into(),
            rule: None,
            source: source.into(),
        }
    }

    pub fn with_col(mut self, col: u32) -> Self {
        self.col = Some(col);
        self
    }

    pub fn with_range(mut self, end_line: u32, end_col: u32) -> Self {
        self.end_line = Some(end_line);
        self.end_col = Some(end_col);
        self
    }

    pub fn with_rule(mut self, rule: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_diagnostic_serialises_omitting_nulls() {
        let d = Diagnostic::new(
            "apps/api/main.go",
            42,
            Severity::Error,
            "undefined: Foo",
            "go-build",
        );
        let json = serde_json::to_string(&d).unwrap();
        // Optional fields must NOT appear as `null` in the JSON.
        assert!(!json.contains("null"));
        assert!(!json.contains("col"));
        assert!(!json.contains("end_line"));
        assert!(!json.contains("rule"));
        // Required fields are present.
        assert!(json.contains(r#""file":"apps/api/main.go""#));
        assert!(json.contains(r#""line":42"#));
        assert!(json.contains(r#""severity":"error""#));
        assert!(json.contains(r#""source":"go-build""#));
    }

    #[test]
    fn full_diagnostic_serialises_all_fields() {
        let d = Diagnostic::new(
            "apps/web/src/App.tsx",
            10,
            Severity::Warning,
            "'foo' is defined but never used",
            "eslint",
        )
        .with_col(7)
        .with_range(10, 12)
        .with_rule("no-unused-vars");
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(r#""col":7"#));
        assert!(json.contains(r#""end_line":10"#));
        assert!(json.contains(r#""end_col":12"#));
        assert!(json.contains(r#""rule":"no-unused-vars""#));
    }

    #[test]
    fn severity_serialises_lowercase() {
        for (sev, want) in [
            (Severity::Error, "\"error\""),
            (Severity::Warning, "\"warning\""),
            (Severity::Info, "\"info\""),
            (Severity::Hint, "\"hint\""),
        ] {
            assert_eq!(serde_json::to_string(&sev).unwrap(), want);
        }
    }

    #[test]
    fn diagnostic_roundtrips_through_json() {
        let d = Diagnostic::new("x.rs", 1, Severity::Error, "boom", "rustc")
            .with_col(5)
            .with_range(2, 9)
            .with_rule("E0308");
        let json = serde_json::to_string(&d).unwrap();
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn schema_emits_required_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(Diagnostic)).unwrap();
        let required = schema
            .pointer("/required")
            .expect("Diagnostic schema must have required[]")
            .as_array()
            .unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        for must in ["file", "line", "severity", "message", "source"] {
            assert!(names.contains(&must), "{must} should be required");
        }
        // Optional fields stay out of required[].
        for optional in ["col", "end_line", "end_col", "rule"] {
            assert!(!names.contains(&optional), "{optional} must be optional");
        }
    }
}
