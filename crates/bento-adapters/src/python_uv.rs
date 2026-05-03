//! Python adapter — `uv` variant.
//!
//! Built for projects that pin `uv` as their package manager. Runs
//! `uv sync` / `uv run` inside the dish's `.venv/`, sidestepping
//! PEP-668 ("externally-managed-environment") on Arch / Debian /
//! recent Ubuntu where the pip-based path can't install against the
//! system interpreter.
//!
//! - Detects: `uv.lock` at the dish root.
//! - Fingerprints: `pyproject.toml`, `uv.lock`, `requirements*.txt`,
//!   `setup.cfg`, `setup.py`, `.python-version`, `.tool-versions`.
//! - Toolchain pin (priority): `.python-version` > `pyproject.toml`'s
//!   `project.requires-python` > `.tool-versions`.
//! - Install: `uv sync --frozen`. Same posture as `npm ci` /
//!   `pnpm install --frozen-lockfile`.
//! - Default tasks: `uv build`, `uv run pytest`, `uv run ruff check .`.
//!
//! Sits next to [`PythonAdapter`](super::python::PythonAdapter) in the
//! registry. The registry tries `python-uv` first so a dish carrying
//! both `pyproject.toml` and `uv.lock` lands here rather than on the
//! pip path.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, InstallProbe, LanguageAdapter, TaskContext, ToolVersion};
use crate::diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};

pub struct PythonUvAdapter;

const FINGERPRINT: &[&str] = &[
    "pyproject.toml",
    "uv.lock",
    "requirements.txt",
    "requirements-dev.txt",
    "setup.cfg",
    "setup.py",
    ".python-version",
    ".tool-versions",
];

impl LanguageAdapter for PythonUvAdapter {
    fn id(&self) -> &str {
        "python-uv"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("uv.lock").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn derived_paths(&self) -> Vec<String> {
        vec![
            "**/*.egg-info/**".into(),
            "dist/**".into(),
            "build/**".into(),
            "**/__pycache__/**".into(),
            "**/*.pyc".into(),
            ".venv/**".into(),
            "venv/**".into(),
        ]
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        let dot = dir.join(".python-version");
        if dot.is_file() {
            let raw = std::fs::read_to_string(&dot)
                .with_context(|| format!("reading {}", dot.display()))?;
            let line = raw.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                return Ok(Some(ToolVersion {
                    tool: "python".into(),
                    version: line.to_string(),
                }));
            }
        }
        let pyproject = dir.join("pyproject.toml");
        if pyproject.is_file() {
            let raw = std::fs::read_to_string(&pyproject)
                .with_context(|| format!("reading {}", pyproject.display()))?;
            if let Some(version) = parse_requires_python(&raw) {
                return Ok(Some(ToolVersion {
                    tool: "python".into(),
                    version,
                }));
            }
        }
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["python"])? {
            return Ok(Some(ToolVersion {
                tool: "python".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let mut cmd = Command::new("uv");
        cmd.args(["sync", "--frozen"]);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "uv sync --frozen")
    }

    fn install_probe(&self, dir: &Path) -> InstallProbe {
        if dir.join(".venv").is_dir() {
            InstallProbe::Ready
        } else {
            InstallProbe::Missing {
                reason: ".venv/ absent".into(),
            }
        }
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        let uv = crate::probe::memoised("uv", &["--version"])?;
        let py = crate::probe::memoised("uv", &["run", "python", "--version"])
            .unwrap_or_else(|| "python:unknown".to_string());
        Some(format!("{uv} | {py}"))
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        match task {
            "lint" => Some(DiagnosticHook {
                rerun: DiagnosticRerun::AppendArgs(vec!["--output-format=json".into()]),
                parser: DiagnosticParser::Builtin(ParserId::Ruff),
            }),
            _ => None,
        }
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = vec![
            "src/**".into(),
            "**/*.py".into(),
            "pyproject.toml".into(),
            "uv.lock".into(),
            "setup.py".into(),
            "setup.cfg".into(),
        ];

        vec![
            DefaultTask {
                name: "build".into(),
                run: "uv build".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "uv run pytest".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "uv run ruff check .".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push("ruff.toml".into());
                    v.push(".ruff.toml".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

fn parse_requires_python(s: &str) -> Option<String> {
    let value: toml::Value = s.parse().ok()?;
    value
        .get("project")?
        .get("requires-python")?
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    fn id_and_fingerprint() {
        let a = PythonUvAdapter;
        assert_eq!(a.id(), "python-uv");
        let fp = a.fingerprint_files();
        for f in ["pyproject.toml", "uv.lock", ".python-version"] {
            assert!(fp.iter().any(|s| s == f));
        }
    }

    #[test]
    fn detect_requires_uv_lock() {
        let tmp = tmp_with(&[("uv.lock", "version = 1\n")]);
        assert!(PythonUvAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_pyproject_without_uv_lock() {
        // The pip-based python adapter handles plain pyproject.toml.
        // python-uv must NOT claim a dish that hasn't opted into uv.
        let tmp = tmp_with(&[("pyproject.toml", "[project]\nname = 'x'\n")]);
        assert!(!PythonUvAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_non_python() {
        let tmp = tmp_with(&[("package.json", "{}")]);
        assert!(!PythonUvAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_dot_python_version() {
        let tmp = tmp_with(&[
            ("uv.lock", ""),
            (".python-version", "3.12.1\n"),
            (
                "pyproject.toml",
                "[project]\nrequires-python = \">=3.10\"\n",
            ),
        ]);
        let v = PythonUvAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.tool, "python");
        assert_eq!(v.version, "3.12.1");
    }

    #[test]
    fn toolchain_reads_requires_python() {
        let tmp = tmp_with(&[
            ("uv.lock", ""),
            (
                "pyproject.toml",
                "[project]\nname = \"x\"\nrequires-python = \">=3.11\"\n",
            ),
        ]);
        let v = PythonUvAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, ">=3.11");
    }

    #[test]
    fn install_probe_missing_when_no_venv() {
        let tmp = tmp_with(&[("uv.lock", "")]);
        match PythonUvAdapter.install_probe(tmp.path()) {
            InstallProbe::Missing { reason } => {
                assert!(
                    reason.contains(".venv"),
                    "reason should name .venv: {reason}"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn install_probe_ready_when_venv_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("uv.lock"), "").unwrap();
        std::fs::create_dir(tmp.path().join(".venv")).unwrap();
        assert_eq!(
            PythonUvAdapter.install_probe(tmp.path()),
            InstallProbe::Ready
        );
    }

    #[test]
    fn diagnostic_hook_only_for_lint_uses_ruff() {
        let a = PythonUvAdapter;
        let h = a.diagnostic_hook("lint").expect("lint should have a hook");
        assert_eq!(h.parser, DiagnosticParser::Builtin(ParserId::Ruff));
        match h.rerun {
            DiagnosticRerun::AppendArgs(args) => assert_eq!(args, vec!["--output-format=json"]),
            _ => panic!("expected AppendArgs"),
        }
        assert!(a.diagnostic_hook("build").is_none());
        assert!(a.diagnostic_hook("test").is_none());
    }

    #[test]
    fn default_tasks_use_uv_run() {
        let tasks = PythonUvAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "uv build");
        assert_eq!(tasks[1].run, "uv run pytest");
        assert!(tasks[2].run.starts_with("uv run ruff"));
    }
}
