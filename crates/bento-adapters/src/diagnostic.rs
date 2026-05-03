//! Adapter-level diagnostic types — `DiagnosticHook`, `DiagnosticRerun`,
//! `ParserId`. The trait method [`crate::LanguageAdapter::diagnostic_hook`]
//! returns one of these to declare how a failed task's output can be
//! captured and parsed into structured diagnostics.
//!
//! Parser implementations live in `bento-core::diagnostic_parsers` —
//! the executor (also in bento-core) dispatches via `ParserId` from
//! adapters, parses, attaches the resulting `Vec<Diagnostic>` to the
//! task's report entry.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What an adapter declares for a task to make its diagnostics
/// structurally accessible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticHook {
    /// How to invoke the task to capture parseable output.
    pub rerun: DiagnosticRerun,
    /// Who parses the captured output.
    pub parser: DiagnosticParser,
}

/// Which side parses the diagnostic re-run output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticParser {
    /// Use one of bento's built-in parsers (the common case).
    Builtin(ParserId),
    /// Send the captured output back to the adapter for parsing
    /// via [`crate::LanguageAdapter::parse_diagnostics`]. Only
    /// meaningful for subprocess plugin adapters whose tools emit
    /// custom formats; built-in adapters that return `Plugin` will
    /// just yield no diagnostics (the trait default returns empty).
    Plugin,
}

/// How bento should construct the diagnostic-capture command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticRerun {
    /// Append these args to the task's existing `run` command.
    /// Works when the user-declared command is just the bare tool
    /// invocation (the common case: `cargo clippy`, `go build`, ...).
    AppendArgs(Vec<String>),
    /// Replace the task's `run` command entirely with a different one
    /// known to produce parseable output. Used when the user-declared
    /// command is a wrapper (`npm run lint` → `eslint --format=json`)
    /// or when the tool's flags can't be safely appended.
    Replace(String),
}

/// Identifier for a built-in diagnostic parser. The actual parsing
/// lives in `bento-core::diagnostic_parsers`; this enum is the wire
/// between adapters (declarative, no parser code) and the executor
/// (dispatches via this id).
///
/// Kept as a small enum (not `&'static str`) so the type system catches
/// typos at compile time and exhaustiveness at parser-impl time.
/// Serialises kebab-case for the plugin wire protocol (ideas-7lg).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ParserId {
    /// `cargo --message-format=json` — wraps rustc and clippy.
    CargoMessage,
    /// `golangci-lint --out-format=json`.
    GolangciLint,
    /// `eslint --format=json`.
    Eslint,
    /// `ruff check --output-format=json`.
    Ruff,
}

impl ParserId {
    /// Every parser id, useful for exhaustive tests and CLI listings.
    pub fn all() -> &'static [ParserId] {
        &[
            ParserId::CargoMessage,
            ParserId::GolangciLint,
            ParserId::Eslint,
            ParserId::Ruff,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_id_serialises_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ParserId::CargoMessage).unwrap(),
            "\"cargo-message\""
        );
        assert_eq!(
            serde_json::to_string(&ParserId::GolangciLint).unwrap(),
            "\"golangci-lint\""
        );
        assert_eq!(
            serde_json::to_string(&ParserId::Eslint).unwrap(),
            "\"eslint\""
        );
        assert_eq!(serde_json::to_string(&ParserId::Ruff).unwrap(), "\"ruff\"");
    }

    #[test]
    fn parser_id_all_lists_every_v1_variant() {
        let ids = ParserId::all();
        assert_eq!(ids.len(), 4);
        for want in [
            ParserId::CargoMessage,
            ParserId::GolangciLint,
            ParserId::Eslint,
            ParserId::Ruff,
        ] {
            assert!(ids.contains(&want), "missing: {want:?}");
        }
    }

    #[test]
    fn diagnostic_hook_with_builtin_parser() {
        let h = DiagnosticHook {
            rerun: DiagnosticRerun::AppendArgs(vec!["--message-format=json".into()]),
            parser: DiagnosticParser::Builtin(ParserId::CargoMessage),
        };
        assert_eq!(h.parser, DiagnosticParser::Builtin(ParserId::CargoMessage));
    }

    #[test]
    fn diagnostic_hook_with_plugin_parser() {
        let h = DiagnosticHook {
            rerun: DiagnosticRerun::Replace("rebar3 compile --json".into()),
            parser: DiagnosticParser::Plugin,
        };
        assert_eq!(h.parser, DiagnosticParser::Plugin);
    }
}
