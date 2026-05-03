//! Cargo adapter.
//!
//! - Detects: `Cargo.toml` at the dish root.
//! - Fingerprints: `Cargo.toml`, `Cargo.lock`, `rust-toolchain`,
//!   `rust-toolchain.toml`, `clippy.toml`, `.clippy.toml`, `rustfmt.toml`,
//!   `.rustfmt.toml`.
//! - Toolchain pin (priority): `rust-toolchain.toml`'s `toolchain.channel`,
//!   then the legacy `rust-toolchain` single-line file.
//! - Install: `cargo fetch --locked`.
//! - Default tasks: `cargo build`, `cargo check`, `cargo test`,
//!   `cargo clippy --all-targets`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{AddOptions, Added, DefaultTask, LanguageAdapter, TaskContext, ToolVersion};
use crate::diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};

pub struct CargoAdapter;

const FINGERPRINT: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    "clippy.toml",
    ".clippy.toml",
    "rustfmt.toml",
    ".rustfmt.toml",
    ".tool-versions",
];

impl LanguageAdapter for CargoAdapter {
    fn id(&self) -> &str {
        "cargo"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("Cargo.toml").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // Preferred: `rust-toolchain.toml` under `[toolchain] channel = ...`.
        let toml_path = dir.join("rust-toolchain.toml");
        if toml_path.is_file() {
            let raw = std::fs::read_to_string(&toml_path)
                .with_context(|| format!("reading {}", toml_path.display()))?;
            if let Some(channel) = parse_toolchain_toml(&raw) {
                return Ok(Some(ToolVersion {
                    tool: "rust".into(),
                    version: channel,
                }));
            }
        }
        // Legacy single-line file: just a channel name, e.g. "1.75.0" or "stable".
        let legacy = dir.join("rust-toolchain");
        if legacy.is_file() {
            let raw = std::fs::read_to_string(&legacy)
                .with_context(|| format!("reading {}", legacy.display()))?;
            let line = raw.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                return Ok(Some(ToolVersion {
                    tool: "rust".into(),
                    version: line.to_string(),
                }));
            }
        }
        // .tool-versions (asdf/mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["rust"])? {
            return Ok(Some(ToolVersion {
                tool: "rust".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let mut cmd = Command::new("cargo");
        cmd.args(["fetch", "--locked"]);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "cargo fetch --locked")
    }

    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        // `cargo add` (cargo 1.62+) writes to Cargo.toml + updates
        // Cargo.lock; same auth/registry config as `cargo build`.
        let mut cmd = Command::new("cargo");
        cmd.arg("add");
        if opts.dev {
            cmd.arg("--dev");
        }
        for p in packages {
            cmd.arg(p);
        }
        ctx.apply_env(&mut cmd);
        let label = if opts.dev {
            "cargo add --dev"
        } else {
            "cargo add"
        };
        // cargo prints status lines to stderr, not stdout — so the
        // captured stdout is empty and there's nothing useful to parse.
        // Echo the requested specs back as Added rows; resolved versions
        // would require a second `cargo metadata` round-trip we can skip
        // for v1 (the user can read Cargo.toml themselves).
        crate::adapter::run_add_cmd(ctx, &mut cmd, label)?;
        Ok(packages
            .iter()
            .map(|p| Added {
                package: (*p).to_string(),
                version: None,
                note: None,
            })
            .collect())
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("rustc", &["--version"])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        // cargo's --message-format=json works for build, check, test
        // (compile errors get reported), and clippy (lint). Same parser
        // for all.
        match task {
            "build" | "check" | "test" | "lint" => Some(DiagnosticHook {
                rerun: DiagnosticRerun::AppendArgs(vec!["--message-format=json".into()]),
                parser: DiagnosticParser::Builtin(ParserId::CargoMessage),
            }),
            _ => None,
        }
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = vec![
            "src/**".into(),
            "benches/**".into(),
            "examples/**".into(),
            "tests/**".into(),
            "build.rs".into(),
            "Cargo.toml".into(),
            "Cargo.lock".into(),
        ];

        vec![
            DefaultTask {
                name: "build".into(),
                run: "cargo build --locked".into(),
                inputs: Some(inputs.clone()),
                // Leave outputs unset: cargo's target dir lives outside
                // the dish tree by default, and users tune --target-dir
                // in their own workflows. Capturing it generically would
                // bundle gigabytes.
                outputs: None,
            },
            DefaultTask {
                name: "check".into(),
                run: "cargo check --locked --all-targets".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "test".into(),
                run: "cargo test --locked".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "cargo clippy --locked --all-targets -- -D warnings".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push("clippy.toml".into());
                    v.push(".clippy.toml".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

/// Pluck `toolchain.channel` out of a `rust-toolchain.toml`. Accepts the
/// string-only form; the richer form with `components`, `targets`,
/// `profile` is parsed but only the channel is returned — that's all
/// bento uses for cache-keying.
fn parse_toolchain_toml(s: &str) -> Option<String> {
    let value: toml::Value = s.parse().ok()?;
    value
        .get("toolchain")?
        .get("channel")?
        .as_str()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn diagnostic_hook_appends_message_format_for_build_check_test_lint() {
        let a = CargoAdapter;
        for task in ["build", "check", "test", "lint"] {
            let h = a.diagnostic_hook(task).expect(task);
            assert_eq!(h.parser, DiagnosticParser::Builtin(ParserId::CargoMessage));
            match h.rerun {
                DiagnosticRerun::AppendArgs(args) => {
                    assert_eq!(args, vec!["--message-format=json"]);
                }
                _ => panic!("expected AppendArgs"),
            }
        }
        assert!(a.diagnostic_hook("migrate").is_none());
    }

    #[test]
    fn id_and_fingerprint() {
        let a = CargoAdapter;
        assert_eq!(a.id(), "cargo");
        let fp = a.fingerprint_files();
        for f in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
            assert!(fp.iter().any(|s| s == f));
        }
    }

    #[test]
    fn detect_finds_rust_project() {
        let tmp = tmp_with(&[(
            "Cargo.toml",
            r#"[package]
name = "x"
version = "0.1.0"
"#,
        )]);
        assert!(CargoAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_non_rust() {
        let tmp = tmp_with(&[("package.json", "{}")]);
        assert!(!CargoAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_reads_rust_toolchain_toml_channel() {
        let tmp = tmp_with(&[(
            "rust-toolchain.toml",
            r#"[toolchain]
channel = "1.82.0"
components = ["rustfmt"]
"#,
        )]);
        let v = CargoAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.tool, "rust");
        assert_eq!(v.version, "1.82.0");
    }

    #[test]
    fn toolchain_falls_back_to_legacy_single_line() {
        let tmp = tmp_with(&[("rust-toolchain", "stable\n")]);
        let v = CargoAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "stable");
    }

    #[test]
    fn toolchain_returns_none_when_unpinned() {
        let tmp = tmp_with(&[(
            "Cargo.toml",
            r#"[package]
name = "x"
version = "0"
"#,
        )]);
        assert!(CargoAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn default_tasks_include_build_check_test_clippy() {
        let tasks = CargoAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "check", "test", "lint"]);
        assert_eq!(tasks[0].run, "cargo build --locked");
        assert_eq!(tasks[1].run, "cargo check --locked --all-targets");
        assert_eq!(tasks[2].run, "cargo test --locked");
        assert!(tasks[3].run.starts_with("cargo clippy"));
        assert!(tasks[3].run.contains("-D warnings"));
    }

    #[test]
    fn check_inputs_match_build_inputs() {
        // bento check shares cargo build's lockfile + source set so
        // an unrelated source edit invalidates both task caches in
        // lockstep — running check once won't poison build's hit.
        let tasks = CargoAdapter.default_tasks();
        let build = tasks.iter().find(|t| t.name == "build").unwrap();
        let check = tasks.iter().find(|t| t.name == "check").unwrap();
        assert_eq!(build.inputs, check.inputs);
    }

    #[test]
    fn lint_inputs_include_clippy_config() {
        let tasks = CargoAdapter.default_tasks();
        let lint = tasks.iter().find(|t| t.name == "lint").unwrap();
        let inputs = lint.inputs.as_ref().unwrap();
        assert!(inputs.iter().any(|i| i == "clippy.toml"));
    }
}
