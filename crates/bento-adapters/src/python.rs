//! Python adapter (pip / setuptools).
//!
//! - Detects: `requirements.txt` / `setup.py`, or a `pyproject.toml`
//!   that declares `[project]`, `[build-system]`, or `[tool.poetry]`.
//! - Fingerprints: `pyproject.toml`, `requirements*.txt`, `setup.cfg`,
//!   `setup.py`, `.python-version`, `poetry.lock`, `uv.lock`.
//! - Toolchain pin (priority): `.python-version` > `pyproject.toml`'s
//!   `project.requires-python` (stringly matched — we don't resolve a
//!   PEP 440 spec, we just cache-key on the raw string).
//! - Install: `poetry install` when `poetry.lock` is present; otherwise
//!   into the dish's own `.venv/` — `uv venv` + `uv pip install` when
//!   `uv` is on PATH, else `python3 -m venv` + `.venv/bin/pip install`.
//! - Default tasks: `python -m build`, `pytest`, `ruff check .`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, InstallProbe, LanguageAdapter, TaskContext, ToolVersion};
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
        dir.join("requirements.txt").is_file()
            || dir.join("setup.py").is_file()
            || pyproject_declares_python(&dir.join("pyproject.toml"))
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
        // pyenv resolves it by walking up; a monorepo commits one at the
        // root for every package.
        if let Some(dot) = crate::adapter::find_up(dir, ".python-version") {
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
        let dir = &ctx.dish_dir;
        if dir.join("poetry.lock").is_file() {
            let mut cmd = Command::new("poetry");
            cmd.arg("install");
            ctx.apply_env(&mut cmd);
            return crate::adapter::run_install_cmd(ctx, &mut cmd, "poetry install");
        }

        let target: Vec<&str> =
            if dir.join("pyproject.toml").is_file() || dir.join("setup.py").is_file() {
                vec!["-e", "."]
            } else if dir.join("requirements.txt").is_file() {
                vec!["-r", "requirements.txt"]
            } else {
                return Ok(());
            };

        // PEP 668: Debian 12+, Ubuntu 23.04+ and homebrew mark the system
        // interpreter externally-managed, so `pip install` against it
        // exits 1 with "externally-managed-environment". Everything goes
        // into the dish's own `.venv/` instead.
        let uv = crate::probe::memoised("uv", &["--version"]).is_some();
        if !dir.join(".venv").is_dir() {
            let mut cmd = if uv {
                let mut c = Command::new("uv");
                c.arg("venv");
                c
            } else {
                let mut c = Command::new(system_python());
                c.args(["-m", "venv", ".venv"]);
                c
            };
            ctx.apply_env(&mut cmd);
            crate::adapter::run_install_cmd(ctx, &mut cmd, "creating .venv")?;
        }

        let mut cmd = if uv {
            let mut c = Command::new("uv");
            c.args(["pip", "install"]);
            c
        } else {
            let mut c = Command::new(venv_bin(dir, "pip"));
            c.arg("install");
            c
        };
        cmd.args(&target);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, &format!("pip install {}", target.join(" ")))
    }

    fn install_probe(&self, dir: &Path) -> InstallProbe {
        // ponytail: `.venv/` presence only. Poetry's default is an
        // out-of-tree venv under ~/.cache, so those dishes re-run
        // `poetry install` (idempotent, ~1s) once per bento invocation
        // unless they set `virtualenvs.in-project`.
        if dir.join(".venv").is_dir() {
            InstallProbe::Ready
        } else {
            InstallProbe::missing(".venv/ absent")
        }
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

    fn default_tasks(&self, _dir: &Path) -> Vec<DefaultTask> {
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

/// A `pyproject.toml` on its own proves nothing: Node and Rust repos
/// routinely carry one purely to configure ruff / black / mypy. Only
/// the tables a real Python distribution declares count.
fn pyproject_declares_python(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = raw.parse::<toml::Value>() else {
        return false;
    };
    value.get("project").is_some()
        || value.get("build-system").is_some()
        || value.get("tool").and_then(|t| t.get("poetry")).is_some()
}

/// `python3` where it exists (every Linux / macOS host), `python`
/// otherwise (Windows, and pyenv shims that only expose the short name).
fn system_python() -> &'static str {
    if crate::probe::memoised("python3", &["--version"]).is_some() {
        "python3"
    } else {
        "python"
    }
}

fn venv_bin(dir: &Path, exe: &str) -> PathBuf {
    let bin = if cfg!(windows) { "Scripts" } else { "bin" };
    dir.join(".venv").join(bin).join(exe)
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
    fn detect_rejects_pyproject_that_only_configures_a_linter() {
        // A Node repo carrying pyproject.toml for ruff/black settings is
        // not a Python dish.
        let tmp = tmp_with(&[
            ("package.json", "{}"),
            ("pyproject.toml", "[tool.ruff]\nline-length = 100\n"),
        ]);
        assert!(!PythonAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_accepts_build_system_and_poetry_pyprojects() {
        for body in [
            "[build-system]\nrequires = [\"setuptools\"]\n",
            "[tool.poetry]\nname = \"x\"\n",
        ] {
            let tmp = tmp_with(&[("pyproject.toml", body)]);
            assert!(PythonAdapter.detect(tmp.path()), "should detect: {body}");
        }
    }

    #[test]
    fn detect_accepts_setup_py() {
        let tmp = tmp_with(&[("setup.py", "from setuptools import setup\nsetup()\n")]);
        assert!(PythonAdapter.detect(tmp.path()));
    }

    #[test]
    fn install_probe_tracks_the_dish_venv() {
        let tmp = tmp_with(&[("pyproject.toml", "[project]\nname = 'x'\n")]);
        assert!(matches!(
            PythonAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
        std::fs::create_dir(tmp.path().join(".venv")).unwrap();
        assert_eq!(PythonAdapter.install_probe(tmp.path()), InstallProbe::Ready);
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
        let tasks = PythonAdapter.default_tasks(Path::new("."));
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "python -m build");
        assert_eq!(tasks[1].run, "pytest");
        assert!(tasks[2].run.starts_with("ruff check"));
    }
}
