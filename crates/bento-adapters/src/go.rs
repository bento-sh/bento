//! Go adapter.
//!
//! - Detects: `go.mod` at the dish root.
//! - Fingerprints: `go.mod`, `go.sum`.
//! - Toolchain pin: the `go <version>` directive in `go.mod`.
//! - Install: `go mod download`.
//! - Default tasks: `build`, `check` (via `go vet ./...`), `test`,
//!   `lint` (via `golangci-lint`).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{AddOptions, Added, DefaultTask, LanguageAdapter, TaskContext, ToolVersion};
use crate::diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};

pub struct GoAdapter;

const FINGERPRINT: &[&str] = &["go.mod", "go.sum", ".tool-versions"];

impl LanguageAdapter for GoAdapter {
    fn id(&self) -> &str {
        "go"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("go.mod").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // go.mod's `go x.y` directive is mandatory in well-formed modules
        // — try it first.
        let mod_path = dir.join("go.mod");
        if mod_path.is_file() {
            let content = std::fs::read_to_string(&mod_path)
                .with_context(|| format!("reading {}", mod_path.display()))?;
            if let Some(v) = parse_go_directive(&content) {
                return Ok(Some(v));
            }
        }
        // Fallback to .tool-versions for the rare repo without a clean
        // go.mod (asdf-golang uses 'golang'; some plugins use 'go').
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["golang", "go"])? {
            return Ok(Some(ToolVersion {
                tool: "go".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let mut cmd = Command::new("go");
        cmd.args(["mod", "download"]);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "go mod download")
    }

    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        // `go get pkg` writes to go.mod (and go.sum). Go modules don't
        // distinguish dev / runtime deps — `--dev` is a no-op here, but
        // we surface the silent demotion as a per-package note so an
        // agent driving this verb sees what actually happened.
        let mut cmd = Command::new("go");
        cmd.arg("get");
        for p in packages {
            cmd.arg(p);
        }
        ctx.apply_env(&mut cmd);
        crate::adapter::run_add_cmd(ctx, &mut cmd, "go get")?;
        let dev_note = if opts.dev {
            Some("Go modules don't distinguish dev / runtime deps; --dev ignored.".to_string())
        } else {
            None
        };
        Ok(packages
            .iter()
            .map(|p| Added {
                package: (*p).to_string(),
                version: None,
                note: dev_note.clone(),
            })
            .collect())
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("go", &["version"])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        // golangci-lint is the dominant linter for Go. The default
        // `lint` task already runs `golangci-lint run` — appending
        // `--out-format=json` works directly. Build/test diagnostics
        // need `go build -json` (Go 1.21+) with a different parser;
        // defer.
        match task {
            "lint" => Some(DiagnosticHook {
                rerun: DiagnosticRerun::AppendArgs(vec!["--out-format=json".into()]),
                parser: DiagnosticParser::Builtin(ParserId::GolangciLint),
            }),
            _ => None,
        }
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let go_inputs = vec!["**/*.go".into(), "go.mod".into(), "go.sum".into()];
        vec![
            DefaultTask {
                name: "build".into(),
                run: "go build ./...".into(),
                inputs: Some(go_inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "check".into(),
                run: "go vet ./...".into(),
                inputs: Some(go_inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "test".into(),
                run: "go test ./...".into(),
                inputs: Some(go_inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "golangci-lint run".into(),
                inputs: Some({
                    let mut v = go_inputs;
                    v.push(".golangci.yml".into());
                    v.push(".golangci.yaml".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

/// Parse the `go <version>` directive from go.mod contents.
///
/// Accepts `go 1.22`, `go 1.22.3`, and strips inline comments. Ignores
/// the newer `toolchain go<version>` directive (the `go` line is still
/// the authoritative pin in every go.mod we care about).
fn parse_go_directive(content: &str) -> Option<ToolVersion> {
    for raw_line in content.lines() {
        let line = raw_line
            .split_once("//")
            .map(|(code, _)| code)
            .unwrap_or(raw_line)
            .trim();

        let Some(rest) = line.strip_prefix("go ") else {
            continue;
        };
        let version = rest.split_whitespace().next()?;
        if version.is_empty() {
            continue;
        }
        return Some(ToolVersion {
            tool: "go".into(),
            version: version.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(mod_contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), mod_contents).unwrap();
        dir
    }

    #[test]
    fn diagnostic_hook_only_for_lint() {
        let go = GoAdapter;
        let h = go.diagnostic_hook("lint").expect("lint should have a hook");
        assert_eq!(h.parser, DiagnosticParser::Builtin(ParserId::GolangciLint));
        match h.rerun {
            DiagnosticRerun::AppendArgs(args) => assert_eq!(args, vec!["--out-format=json"]),
            _ => panic!("expected AppendArgs"),
        }
        assert!(go.diagnostic_hook("build").is_none());
        assert!(go.diagnostic_hook("test").is_none());
    }

    #[test]
    fn id_and_fingerprint() {
        let go = GoAdapter;
        assert_eq!(go.id(), "go");
        let fp = go.fingerprint_files();
        for f in ["go.mod", "go.sum", ".tool-versions"] {
            assert!(fp.iter().any(|s| s == f), "fingerprint missing: {f}");
        }
    }

    #[test]
    fn detect_finds_go_project() {
        let tmp = fixture("module example.com/x\n\ngo 1.22\n");
        assert!(GoAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_returns_false_without_go_mod() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!GoAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_returns_false_when_go_mod_is_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("go.mod")).unwrap();
        assert!(!GoAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_reads_major_minor() {
        let tmp = fixture("module example.com/x\n\ngo 1.22\n");
        let v = GoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(
            v,
            ToolVersion {
                tool: "go".into(),
                version: "1.22".into()
            }
        );
    }

    #[test]
    fn toolchain_reads_major_minor_patch() {
        let tmp = fixture("module example.com/x\n\ngo 1.22.3\n");
        let v = GoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "1.22.3");
    }

    #[test]
    fn toolchain_strips_inline_comment() {
        let tmp = fixture("module example.com/x\n\ngo 1.22 // pinned for CI\n");
        let v = GoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "1.22");
    }

    #[test]
    fn toolchain_returns_none_when_go_directive_absent() {
        let tmp = fixture("module example.com/x\n");
        let result = GoAdapter.required_toolchain(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn toolchain_ignores_commented_directive() {
        let tmp = fixture("module example.com/x\n\n// go 1.22\ngo 1.23\n");
        let v = GoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "1.23");
    }

    #[test]
    fn toolchain_handles_require_block_before_go_directive() {
        // Real go.mod files can have arbitrary ordering, but the `go`
        // directive usually comes second. Verify we find it regardless.
        let tmp = fixture("module example.com/x\n\nrequire (\n\tfoo v1.0.0\n)\n\ngo 1.22\n");
        let v = GoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "1.22");
    }

    #[test]
    fn toolchain_returns_none_when_go_mod_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = GoAdapter.required_toolchain(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn default_tasks_include_build_check_test_lint() {
        let tasks = GoAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "check", "test", "lint"]);
        assert!(tasks[0].run.starts_with("go build"));
        assert_eq!(tasks[1].run, "go vet ./...");
        assert!(tasks[2].run.starts_with("go test"));
        assert_eq!(tasks[3].run, "golangci-lint run");
    }

    #[test]
    fn default_task_inputs_include_go_sources_and_lockfiles() {
        let tasks = GoAdapter.default_tasks();
        // build / check / test all share the same go-source globs.
        for name in ["build", "check", "test"] {
            let t = tasks.iter().find(|t| t.name == name).unwrap();
            let inputs = t.inputs.as_ref().unwrap();
            assert!(inputs.contains(&"**/*.go".to_string()));
            assert!(inputs.contains(&"go.mod".to_string()));
            assert!(inputs.contains(&"go.sum".to_string()));
        }
        let lint = tasks.iter().find(|t| t.name == "lint").unwrap();
        let lint_inputs = lint.inputs.as_ref().unwrap();
        assert!(lint_inputs.contains(&".golangci.yml".to_string()));
    }
}
