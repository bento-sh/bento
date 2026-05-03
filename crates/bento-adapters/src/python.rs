//! Python adapter (pip / setuptools).
//!
//! - Detects: `pyproject.toml` or `requirements.txt` at the dish root.
//! - Fingerprints: `pyproject.toml`, `requirements*.txt`, `setup.cfg`,
//!   `setup.py`, `.python-version`, `poetry.lock`, `uv.lock`.
//! - Toolchain pin (priority): `.python-version` > `pyproject.toml`'s
//!   `project.requires-python` (stringly matched — we don't resolve a
//!   PEP 440 spec, we just cache-key on the raw string).
//! - Install: `pip install -e .` when a `pyproject.toml` exists;
//!   otherwise `pip install -r requirements.txt`.
//! - Default tasks: `python -m build`, `pytest`, `ruff check .`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, LanguageAdapter, TaskContext, ToolVersion};
use crate::diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};

pub struct PythonAdapter;

const FINGERPRINT: &[&str] = &[
    "pyproject.toml",
    "requirements.txt",
    "requirements-dev.txt",
    "setup.cfg",
    "setup.py",
    ".python-version",
    ".tool-versions",
    "poetry.lock",
    "uv.lock",
];

impl LanguageAdapter for PythonAdapter {
    fn id(&self) -> &str {
        "python"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("pyproject.toml").is_file() || dir.join("requirements.txt").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn derived_paths(&self) -> Vec<String> {
        // `pip install -e .` writes `src/<pkg>.egg-info/` and may
        // scatter compiled-bytecode sidecars. `python -m build`
        // writes `dist/` + intermediate `build/`. None of this is
        // source — pristine-clone reproducible from pyproject.toml
        // + the source tree, so exclude from cache keys.
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
        // 1. `.python-version` — pyenv convention, honoured by uv/rye.
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
        // 2. `project.requires-python` in pyproject.toml.
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
        // 3. .tool-versions (asdf/mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["python"])? {
            return Ok(Some(ToolVersion {
                tool: "python".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let args: Vec<&str> = if ctx.dish_dir.join("pyproject.toml").is_file() {
            vec!["install", "-e", "."]
        } else if ctx.dish_dir.join("requirements.txt").is_file() {
            vec!["install", "-r", "requirements.txt"]
        } else {
            // Nothing to install — treat as success.
            return Ok(());
        };
        let mut cmd = Command::new("pip");
        cmd.args(&args);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, &format!("pip {}", args.join(" ")))
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        // Try `python --version`; many distros ship only `python3` on PATH.
        crate::probe::memoised("python", &["--version"])
            .or_else(|| crate::probe::memoised("python3", &["--version"]))
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        // The default `lint` task is `ruff check .` — appending
        // `--output-format=json` is safe and gives us machine-readable
        // output. mypy / pylint diagnostics deferred (separate parsers).
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
            "setup.py".into(),
            "setup.cfg".into(),
            "requirements*.txt".into(),
        ];

        vec![
            DefaultTask {
                name: "build".into(),
                // Standard PEP 517 build. Users with non-packaging dishes
                // can override with a `[tasks.build]` in their dish.toml.
                run: "python -m build".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into(), "build/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "pytest".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                // Ruff has become the dominant Python linter; fall back
                // gracefully if it isn't installed — users can override.
                run: "ruff check .".into(),
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

/// Parse `project.requires-python` out of `pyproject.toml`. Returns the
/// raw spec (e.g. `">=3.11"`) — we don't resolve; we just cache-key on
/// the string.
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
        let a = PythonAdapter;
        assert_eq!(a.id(), "python");
        let fp = a.fingerprint_files();
        for f in [
            "pyproject.toml",
            "requirements.txt",
            ".python-version",
            "uv.lock",
        ] {
            assert!(fp.iter().any(|s| s == f));
        }
    }

    #[test]
    fn detect_pyproject_toml() {
        let tmp = tmp_with(&[("pyproject.toml", "[project]\nname = 'x'\n")]);
        assert!(PythonAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_requirements_txt() {
        let tmp = tmp_with(&[("requirements.txt", "flask\n")]);
        assert!(PythonAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_non_python() {
        let tmp = tmp_with(&[("package.json", "{}")]);
        assert!(!PythonAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_dot_python_version() {
        let tmp = tmp_with(&[
            (".python-version", "3.12.1\n"),
            (
                "pyproject.toml",
                "[project]\nrequires-python = \">=3.10\"\n",
            ),
        ]);
        let v = PythonAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.tool, "python");
        assert_eq!(v.version, "3.12.1");
    }

    #[test]
    fn toolchain_reads_requires_python() {
        let tmp = tmp_with(&[(
            "pyproject.toml",
            "[project]\nname = \"x\"\nrequires-python = \">=3.11\"\n",
        )]);
        let v = PythonAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, ">=3.11");
    }

    #[test]
    fn toolchain_returns_none_when_unpinned() {
        let tmp = tmp_with(&[("pyproject.toml", "[project]\nname = \"x\"\n")]);
        assert!(PythonAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn diagnostic_hook_only_for_lint_uses_ruff() {
        let a = PythonAdapter;
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
    fn default_tasks_use_python_tools() {
        let tasks = PythonAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "python -m build");
        assert_eq!(tasks[1].run, "pytest");
        assert!(tasks[2].run.starts_with("ruff check"));
    }
}
