//! .NET adapter (`dotnet` SDK — C#, F#, VB).
//!
//! - Detects: any `*.csproj` / `*.fsproj` / `*.vbproj`, a `*.sln` /
//!   `*.slnx`, or a `global.json` at the dish root.
//! - Fingerprints: `global.json`, `Directory.Build.props`,
//!   `Directory.Packages.props`, `nuget.config`, `packages.lock.json`.
//!   Project and solution files are glob-shaped, so they ride in on the
//!   default tasks' `**` inputs rather than this literal-name list.
//! - Toolchain pin: `sdk.version` in `global.json`.
//! - Install: `dotnet restore` (`--locked-mode` when the dish ships a
//!   `packages.lock.json`).
//! - Default tasks: `build` / `test` (Release, `--no-restore` — the
//!   executor already ran install), `check` (Debug compile, the fast
//!   feedback loop), `lint` (`dotnet format --verify-no-changes`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{
    AddOptions, Added, DefaultTask, InstallProbe, LanguageAdapter, TaskContext, ToolVersion,
};

pub struct DotnetAdapter;

const FINGERPRINT: &[&str] = &[
    "global.json",
    "Directory.Build.props",
    "Directory.Packages.props",
    "nuget.config",
    "packages.lock.json",
];

const PROJECT_EXTS: &[&str] = &["csproj", "fsproj", "vbproj"];
const DETECT_EXTS: &[&str] = &["csproj", "fsproj", "vbproj", "sln", "slnx"];

/// Depth for the two "does this dish contain X?" walks below. Covers a
/// flat project (`./obj/`), a solution with `src/App/obj/`, and the
/// `tests/App.Tests/` sibling — the layouts `dotnet new sln` produces.
//
// ponytail: fixed depth, not a full walk. A project nested deeper than
// this just means `--locked-mode` is skipped and the install probe
// re-restores every run — slower, never wrong. Raise it if a real repo
// buries projects further down.
const WALK_DEPTH: usize = 4;

impl LanguageAdapter for DotnetAdapter {
    fn id(&self) -> &str {
        "dotnet"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("global.json").is_file() || first_file_with_ext(dir, DETECT_EXTS).is_some()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn derived_paths(&self) -> Vec<String> {
        // MSBuild's intermediate (`obj/`) and output (`bin/`) dirs, plus
        // the artifacts layout new SDKs opt into. All reproducible from
        // the sources — must not contaminate cache keys.
        vec![
            "**/bin/**".into(),
            "**/obj/**".into(),
            "artifacts/**".into(),
        ]
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        let path = dir.join("global.json");
        if !path.is_file() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(parse_global_json_sdk(&raw).map(|version| ToolVersion {
            tool: "dotnet".into(),
            version,
        }))
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let locked = contains_file(&ctx.dish_dir, "packages.lock.json");
        let mut cmd = Command::new("dotnet");
        cmd.arg("restore");
        if locked {
            cmd.arg("--locked-mode");
        }
        ctx.apply_env(&mut cmd);
        let label = if locked {
            "dotnet restore --locked-mode"
        } else {
            "dotnet restore"
        };
        crate::adapter::run_install_cmd(ctx, &mut cmd, label)
    }

    /// `dotnet add package` resolves the target project itself only when
    /// the directory holds exactly one; a solution dir with several is
    /// ambiguous and errors out. We pass the first project we find
    /// (sorted, so it's stable) and note the pick — targeting a specific
    /// project in a multi-project dish means running `dotnet add` by hand.
    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        let projects = project_files(&ctx.dish_dir);
        let target = projects.first();
        let mut note = if projects.len() > 1 {
            Some(format!(
                "dish has {} projects; added to {}",
                projects.len(),
                target.map(|p| p.display().to_string()).unwrap_or_default()
            ))
        } else {
            None
        };
        if opts.dev {
            // NuGet has no dev-dependency section — the closest thing is
            // per-package `PrivateAssets`, which `dotnet add package`
            // can't set. Surface the demotion rather than lying.
            let msg = "NuGet has no dev-dependency concept; --dev ignored.";
            note = Some(match note {
                Some(n) => format!("{n}. {msg}"),
                None => msg.to_string(),
            });
        }

        for p in packages {
            let mut cmd = Command::new("dotnet");
            cmd.arg("add");
            if let Some(project) = target {
                cmd.arg(project);
            }
            cmd.args(["package", p]);
            ctx.apply_env(&mut cmd);
            crate::adapter::run_add_cmd(ctx, &mut cmd, "dotnet add package")?;
        }

        Ok(packages
            .iter()
            .map(|p| Added {
                package: (*p).to_string(),
                version: None,
                note: note.clone(),
            })
            .collect())
    }

    fn install_probe(&self, dir: &Path) -> InstallProbe {
        // `dotnet restore` writes `obj/project.assets.json` per project;
        // without it every `--no-restore` task fails with NETSDK1004.
        if contains_file(dir, "project.assets.json") {
            InstallProbe::Ready
        } else {
            InstallProbe::missing("obj/project.assets.json absent")
        }
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("dotnet", &["--version"])
    }

    // ponytail: no diagnostic_hook. Structured build diagnostics need
    // `dotnet build -tl:off -p:ErrorLog=<sarif>` plus a SARIF parser —
    // a follow-up, not a blocker for the adapter.

    fn default_tasks(&self, _dir: &Path) -> Vec<DefaultTask> {
        // Whole tree: a solution-at-root dish owns every project under
        // it. `bin/` and `obj/` are derived, so the walker prunes them.
        let inputs = vec!["**".to_string()];
        // Install already restored; re-resolving per task is wasted
        // network and, worse, non-deterministic between cache probes.
        vec![
            DefaultTask {
                name: "build".into(),
                run: "dotnet build --no-restore -c Release".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "test".into(),
                run: "dotnet test --no-restore -c Release".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                // Debug compile — no optimiser, no Release-only
                // analyzers, so it's the fast "does it still build?".
                name: "check".into(),
                run: "dotnet build --no-restore".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "dotnet format --verify-no-changes".into(),
                inputs: Some(inputs),
                outputs: None,
            },
        ]
    }
}

/// Parse `sdk.version` out of `global.json`. Any other key
/// (`rollForward`, `allowPrerelease`, `msbuild-sdks`) is ignored, and
/// malformed JSON yields `None` rather than failing the plan — an
/// unreadable pin is the same as no pin.
fn parse_global_json_sdk(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let version = value.get("sdk")?.get("version")?.as_str()?.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// First file in `dir` (non-recursive) whose extension matches `exts`,
/// compared case-insensitively — `.csproj` files authored on Windows
/// can arrive as `.CSPROJ`.
fn first_file_with_ext(dir: &Path, exts: &[&str]) -> Option<PathBuf> {
    let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| exts.iter().any(|w| e.eq_ignore_ascii_case(w)))
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Project files at the dish root, sorted for a stable `add` target.
fn project_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut hits: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| PROJECT_EXTS.iter().any(|w| e.eq_ignore_ascii_case(w)))
        })
        .collect();
    hits.sort();
    hits
}

/// Is there a file named `name` anywhere within [`WALK_DEPTH`] of `dir`?
fn contains_file(dir: &Path, name: &str) -> bool {
    walkdir::WalkDir::new(dir)
        .max_depth(WALK_DEPTH)
        .into_iter()
        .filter_map(Result::ok)
        .any(|e| e.file_name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        dir
    }

    #[test]
    fn id_and_fingerprint() {
        let a = DotnetAdapter;
        assert_eq!(a.id(), "dotnet");
        let fp = a.fingerprint_files();
        for f in [
            "global.json",
            "Directory.Build.props",
            "Directory.Packages.props",
            "nuget.config",
            "packages.lock.json",
        ] {
            assert!(fp.iter().any(|s| s == f), "fingerprint missing: {f}");
        }
    }

    #[test]
    fn detect_finds_csproj() {
        let tmp = tmp_with(&[("App.csproj", "<Project/>")]);
        assert!(DotnetAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_finds_fsproj_and_vbproj() {
        for name in ["App.fsproj", "App.vbproj"] {
            let tmp = tmp_with(&[(name, "<Project/>")]);
            assert!(DotnetAdapter.detect(tmp.path()), "{name}");
        }
    }

    #[test]
    fn detect_finds_solution() {
        for name in ["App.sln", "App.slnx"] {
            let tmp = tmp_with(&[(name, "")]);
            assert!(DotnetAdapter.detect(tmp.path()), "{name}");
        }
    }

    #[test]
    fn detect_finds_global_json_only() {
        let tmp = tmp_with(&[("global.json", r#"{"sdk":{"version":"8.0.100"}}"#)]);
        assert!(DotnetAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_is_case_insensitive_on_project_extension() {
        let tmp = tmp_with(&[("App.CSPROJ", "<Project/>")]);
        assert!(DotnetAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_unrelated_dir() {
        let tmp = tmp_with(&[
            ("README.md", "# x"),
            ("package.json", "{}"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert!(!DotnetAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_ignores_projects_in_subdirs() {
        // Detection is per-dish-root; a solution dir would carry the
        // .sln itself. Nested-only projects belong to their own dish.
        let tmp = tmp_with(&[("src/App/App.csproj", "<Project/>")]);
        assert!(!DotnetAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_returns_false_when_csproj_is_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("App.csproj")).unwrap();
        assert!(!DotnetAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_reads_sdk_version() {
        let tmp = tmp_with(&[("global.json", r#"{"sdk":{"version":"8.0.404"}}"#)]);
        let v = DotnetAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            v,
            ToolVersion {
                tool: "dotnet".into(),
                version: "8.0.404".into()
            }
        );
    }

    #[test]
    fn toolchain_tolerates_roll_forward_and_siblings() {
        let tmp = tmp_with(&[(
            "global.json",
            r#"{"sdk":{"version":"9.0.100","rollForward":"latestFeature","allowPrerelease":false},
                "msbuild-sdks":{"Foo.Sdk":"1.0.0"}}"#,
        )]);
        let v = DotnetAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "9.0.100");
    }

    #[test]
    fn toolchain_returns_none_without_sdk_version() {
        let tmp = tmp_with(&[("global.json", r#"{"sdk":{"rollForward":"latestMajor"}}"#)]);
        assert!(DotnetAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_returns_none_on_malformed_global_json() {
        let tmp = tmp_with(&[("global.json", "{ this is not json")]);
        assert!(DotnetAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_returns_none_without_global_json() {
        let tmp = tmp_with(&[("App.csproj", "<Project/>")]);
        assert!(DotnetAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn install_probe_missing_before_restore() {
        let tmp = tmp_with(&[("App.csproj", "<Project/>")]);
        assert_eq!(
            DotnetAdapter.install_probe(tmp.path()),
            InstallProbe::missing("obj/project.assets.json absent")
        );
    }

    #[test]
    fn install_probe_ready_after_restore() {
        let tmp = tmp_with(&[
            ("App.csproj", "<Project/>"),
            ("obj/project.assets.json", "{}"),
        ]);
        assert_eq!(DotnetAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }

    #[test]
    fn install_probe_ready_for_a_solution_layout() {
        let tmp = tmp_with(&[
            ("App.sln", ""),
            ("src/App/App.csproj", "<Project/>"),
            ("src/App/obj/project.assets.json", "{}"),
        ]);
        assert_eq!(DotnetAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }

    #[test]
    fn packages_lock_detected_across_the_dish() {
        let tmp = tmp_with(&[
            ("App.sln", ""),
            ("src/App/packages.lock.json", "{}"),
            ("src/App/App.csproj", "<Project/>"),
        ]);
        assert!(contains_file(tmp.path(), "packages.lock.json"));
        let bare = tmp_with(&[("App.csproj", "<Project/>")]);
        assert!(!contains_file(bare.path(), "packages.lock.json"));
    }

    #[test]
    fn default_tasks_are_restore_free_and_whole_tree() {
        let tasks = DotnetAdapter.default_tasks(Path::new("."));
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "check", "lint"]);
        assert_eq!(tasks[0].run, "dotnet build --no-restore -c Release");
        assert_eq!(tasks[1].run, "dotnet test --no-restore -c Release");
        assert_eq!(tasks[2].run, "dotnet build --no-restore");
        assert_eq!(tasks[3].run, "dotnet format --verify-no-changes");
        for t in &tasks {
            assert_eq!(
                t.inputs.as_deref(),
                Some(&["**".to_string()][..]),
                "{}",
                t.name
            );
        }
    }

    #[test]
    fn derived_paths_cover_msbuild_output() {
        let derived = DotnetAdapter.derived_paths();
        for want in ["**/bin/**", "**/obj/**", "artifacts/**"] {
            assert!(derived.iter().any(|d| d == want), "missing derived: {want}");
        }
    }

    #[test]
    fn detected_tasks_are_none() {
        // MSBuild targets aren't scripts — nothing project-specific to
        // surface at init time.
        let tmp = tmp_with(&[("App.csproj", "<Project/>")]);
        assert!(DotnetAdapter.detected_tasks(tmp.path()).is_none());
    }

    #[test]
    fn project_files_are_sorted_and_root_only() {
        let tmp = tmp_with(&[
            ("Zed.csproj", "<Project/>"),
            ("Alpha.fsproj", "<Project/>"),
            ("nested/Deep.csproj", "<Project/>"),
        ]);
        let found: Vec<String> = project_files(tmp.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(found, vec!["Alpha.fsproj", "Zed.csproj"]);
    }
}
