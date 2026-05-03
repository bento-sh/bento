//! Bento-specific payload types that travel inside JSON-RPC messages.
//!
//! These mirror types from `bento-adapters` (`ToolVersion`, `DefaultTask`)
//! but are duplicated here on purpose: this crate is the wire boundary.
//! `bento-plugin` MUST NOT depend on `bento-adapters`, and the wire types
//! must not silently drift if the in-process trait evolves.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The handshake response from the plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub protocol_version: u32,
    pub adapter_id: String,
    pub display_name: String,
    pub fingerprint_files: Vec<String>,
    pub default_tasks: Vec<DefaultTask>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Per-task diagnostic hooks. Keyed by task name. Optional;
    /// plugins without diagnostic support omit this field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub diagnostic_hooks: BTreeMap<String, ManifestDiagnosticHook>,
}

/// One entry in `Manifest.diagnostic_hooks` — declares how bento
/// should re-run a failed task to capture parseable output and which
/// parser owns that output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestDiagnosticHook {
    pub rerun: ManifestRerun,
    /// Either a built-in parser id (`"cargo-message"`, `"eslint"`, ...)
    /// or the literal `"plugin"` to dispatch back to the plugin's
    /// `parseDiagnostics` method.
    pub parser: String,
}

/// Wire-format equivalent of [`bento_adapters::DiagnosticRerun`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestRerun {
    AppendArgs { args: Vec<String> },
    Replace { command: String },
}

/// Request payload for the `parseDiagnostics` method (host → plugin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseDiagnosticsParams {
    pub task_name: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: i32,
    pub dish_dir: String,
    pub workspace_root: String,
}

/// Response payload from `parseDiagnostics` (plugin → host).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseDiagnosticsResult {
    pub diagnostics: Vec<DiagnosticPayload>,
}

/// Wire-format diagnostic — mirrors `bento_adapters::Diagnostic` but
/// kept independent so the wire schema doesn't drift if the in-process
/// type evolves. Field set is intentionally identical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticPayload {
    pub file: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_col: Option<u32>,
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub source: String,
}

/// What the plugin chooses to implement. A `false` capability means bento
/// uses the trait's default (e.g. `resolved_toolchain_fingerprint = false`
/// → declared toolchain version is the only thing that goes into the
/// cache key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default = "default_true")]
    pub detect: bool,
    #[serde(default = "default_true")]
    pub required_toolchain: bool,
    #[serde(default)]
    pub resolved_toolchain_fingerprint: bool,
    #[serde(default = "default_true")]
    pub install: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            detect: true,
            required_toolchain: true,
            resolved_toolchain_fingerprint: false,
            install: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolVersion {
    pub tool: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultTask {
    pub name: String,
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
}

/// Notification payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogParams {
    pub level: LogLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<LogStream>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_default_when_omitted() {
        let m: Manifest = serde_json::from_str(
            r#"{"protocol_version":1,"adapter_id":"x","display_name":"X","fingerprint_files":[],"default_tasks":[]}"#,
        )
        .unwrap();
        assert!(m.capabilities.detect);
        assert!(m.capabilities.install);
        assert!(!m.capabilities.resolved_toolchain_fingerprint);
    }

    #[test]
    fn capabilities_explicit_overrides_default() {
        let m: Manifest = serde_json::from_str(
            r#"{
                "protocol_version":1,"adapter_id":"x","display_name":"X",
                "fingerprint_files":[],"default_tasks":[],
                "capabilities":{"detect":false,"required_toolchain":false,"resolved_toolchain_fingerprint":true,"install":false}
            }"#,
        )
        .unwrap();
        assert!(!m.capabilities.detect);
        assert!(!m.capabilities.install);
        assert!(m.capabilities.resolved_toolchain_fingerprint);
    }

    #[test]
    fn default_task_outputs_optional() {
        let t: DefaultTask = serde_json::from_str(r#"{"name":"build","run":"go build"}"#).unwrap();
        assert!(t.inputs.is_none());
        assert!(t.outputs.is_none());
    }

    #[test]
    fn manifest_omits_diagnostic_hooks_when_unset() {
        let m: Manifest = serde_json::from_str(
            r#"{"protocol_version":1,"adapter_id":"x","display_name":"X","fingerprint_files":[],"default_tasks":[]}"#,
        )
        .unwrap();
        assert!(m.diagnostic_hooks.is_empty());
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("diagnostic_hooks"));
    }

    #[test]
    fn manifest_diagnostic_hook_with_append_args() {
        let m: Manifest = serde_json::from_str(
            r#"{
                "protocol_version":1,"adapter_id":"erlang","display_name":"E",
                "fingerprint_files":[],"default_tasks":[],
                "diagnostic_hooks": {
                    "lint": {
                        "rerun": {"kind": "append_args", "args": ["--format", "json"]},
                        "parser": "plugin"
                    }
                }
            }"#,
        )
        .unwrap();
        let hook = m.diagnostic_hooks.get("lint").unwrap();
        assert_eq!(hook.parser, "plugin");
        assert_eq!(
            hook.rerun,
            ManifestRerun::AppendArgs {
                args: vec!["--format".into(), "json".into()]
            }
        );
    }

    #[test]
    fn manifest_diagnostic_hook_with_replace() {
        let m: Manifest = serde_json::from_str(
            r#"{
                "protocol_version":1,"adapter_id":"x","display_name":"X",
                "fingerprint_files":[],"default_tasks":[],
                "diagnostic_hooks": {
                    "build": {
                        "rerun": {"kind": "replace", "command": "rebar3 compile --json"},
                        "parser": "cargo-message"
                    }
                }
            }"#,
        )
        .unwrap();
        let hook = m.diagnostic_hooks.get("build").unwrap();
        assert_eq!(hook.parser, "cargo-message");
        assert_eq!(
            hook.rerun,
            ManifestRerun::Replace {
                command: "rebar3 compile --json".into()
            }
        );
    }

    #[test]
    fn diagnostic_payload_minimal_serialises_without_nulls() {
        let d = DiagnosticPayload {
            file: "x.erl".into(),
            line: 1,
            col: None,
            end_line: None,
            end_col: None,
            severity: "error".into(),
            message: "boom".into(),
            rule: None,
            source: "rebar3".into(),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("null"));
        assert!(!json.contains("col"));
        assert!(!json.contains("rule"));
    }

    #[test]
    fn log_level_lowercase() {
        assert_eq!(serde_json::to_string(&LogLevel::Info).unwrap(), "\"info\"");
        let l: LogLevel = serde_json::from_str("\"warn\"").unwrap();
        assert_eq!(l, LogLevel::Warn);
    }
}
