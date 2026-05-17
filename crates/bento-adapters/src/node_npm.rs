//! node-npm adapter.
//!
//! - Detects: `package-lock.json` at the dish root.
//! - Fingerprints: `package.json`, `package-lock.json`, `.nvmrc`, `.node-version`.
//! - Toolchain pin (priority): `.nvmrc` > `.node-version` > `package.json` `engines.node`.
//! - Install: `npm ci`.
//! - Default tasks: `build`, `test`, `lint` via `npm run <task>`
//!   (`test` uses the `npm test` shortcut).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::adapter::{
    AddOptions, Added, DefaultTask, DetectedTask, InstallProbe, LanguageAdapter, TaskContext,
    ToolVersion,
};
use crate::diagnostic::DiagnosticHook;
use crate::node_common::node_eslint_hook;
use crate::node_common::{
    base_inputs, detected_npm_scripts, find_node_workspace_root, resolve_node_version,
};

pub struct NodeNpmAdapter;

const FINGERPRINT: &[&str] = &[
    "package.json",
    "package-lock.json",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
];

impl LanguageAdapter for NodeNpmAdapter {
    fn id(&self) -> &str {
        "node-npm"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("package-lock.json").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        resolve_node_version(dir, "node")
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        // `npm ci` is strict — requires a lockfile. On a cold project
        // without one (fresh `bento dish add`, for example), fall back
        // to `npm install` so the lockfile is generated on first run.
        let has_lockfile = ctx.dish_dir.join("package-lock.json").is_file();
        let verb = if has_lockfile { "ci" } else { "install" };

        let mut cmd = Command::new("npm");
        cmd.arg(verb);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, &format!("npm {verb}"))
    }

    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        let mut cmd = Command::new("npm");
        cmd.arg("install");
        cmd.arg(if opts.dev { "--save-dev" } else { "--save" });
        for p in packages {
            cmd.arg(p);
        }
        ctx.apply_env(&mut cmd);
        let label = if opts.dev {
            "npm install --save-dev"
        } else {
            "npm install --save"
        };
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

    fn install_probe(&self, dir: &Path) -> InstallProbe {
        // `npm ci` writes `.package-lock.json` inside `node_modules/` as
        // its canonical "install completed" sentinel. A partial / torn
        // `node_modules` without the sentinel still counts as missing.
        if dir
            .join("node_modules")
            .join(".package-lock.json")
            .is_file()
        {
            InstallProbe::Ready
        } else {
            InstallProbe::missing("node_modules/.package-lock.json absent")
        }
    }

    fn install_scope(&self, dir: &Path) -> PathBuf {
        find_node_workspace_root(dir).unwrap_or_else(|| dir.to_path_buf())
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        // Capture both the runtime and the package manager so a node
        // bump OR an npm bump invalidates — they can both subtly shift
        // install behaviour.
        let node = crate::probe::memoised("node", &["--version"]);
        let npm = crate::probe::memoised("npm", &["--version"]);
        crate::node_common::combine_probes(&[("node", node), ("npm", npm)])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        node_eslint_hook(task)
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        detected_npm_scripts(dir, "npm run")
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = base_inputs("package-lock.json");

        vec![
            DefaultTask {
                name: "build".into(),
                run: "npm run build".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into(), "build/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "npm test".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "npm run lint".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push(".eslintrc*".into());
                    v.push("eslint.config.*".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
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
        let a = NodeNpmAdapter;
        assert_eq!(a.id(), "node-npm");
        let fp = a.fingerprint_files();
        for f in [
            "package.json",
            "package-lock.json",
            ".nvmrc",
            ".node-version",
            ".tool-versions",
        ] {
            assert!(fp.iter().any(|s| s == f), "fingerprint missing: {f}");
        }
    }

    #[test]
    fn detect_finds_npm_project_with_lockfile() {
        let tmp = tmp_with(&[
            ("package.json", r#"{"name":"x"}"#),
            ("package-lock.json", "{}"),
        ]);
        assert!(NodeNpmAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_returns_false_without_lockfile() {
        let tmp = tmp_with(&[("package.json", r#"{"name":"x"}"#)]);
        assert!(!NodeNpmAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_returns_false_when_lockfile_is_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("package-lock.json")).unwrap();
        assert!(!NodeNpmAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_nvmrc() {
        let tmp = tmp_with(&[
            (".nvmrc", "22.1.0\n"),
            (".node-version", "20.0.0\n"),
            ("package.json", r#"{"engines":{"node":"18"}}"#),
        ]);
        let v = NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            v,
            ToolVersion {
                tool: "node".into(),
                version: "22.1.0".into()
            }
        );
    }

    #[test]
    fn toolchain_strips_leading_v_in_nvmrc() {
        let tmp = tmp_with(&[(".nvmrc", "v22.1.0\n")]);
        let v = NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn toolchain_falls_back_to_node_version_file() {
        let tmp = tmp_with(&[
            (".node-version", "20.5.1\n"),
            ("package.json", r#"{"engines":{"node":"18"}}"#),
        ]);
        let v = NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "20.5.1");
    }

    #[test]
    fn toolchain_falls_back_to_engines_node() {
        let tmp = tmp_with(&[(
            "package.json",
            r#"{"name":"x","engines":{"node":"^22.0.0"}}"#,
        )]);
        let v = NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "^22.0.0");
    }

    #[test]
    fn toolchain_returns_none_when_no_pin() {
        let tmp = tmp_with(&[("package.json", r#"{"name":"x"}"#)]);
        assert!(NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_returns_none_when_engines_has_no_node() {
        let tmp = tmp_with(&[("package.json", r#"{"engines":{"npm":"10"}}"#)]);
        assert!(NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_returns_none_with_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_errors_on_malformed_package_json() {
        let tmp = tmp_with(&[("package.json", "{not valid json")]);
        let err = NodeNpmAdapter.required_toolchain(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("parsing"), "got: {err}");
    }

    #[test]
    fn toolchain_returns_none_for_empty_nvmrc() {
        let tmp = tmp_with(&[(".nvmrc", "\n")]);
        assert!(NodeNpmAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn default_tasks_include_build_test_lint() {
        let tasks = NodeNpmAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "npm run build");
        assert_eq!(tasks[1].run, "npm test");
        assert_eq!(tasks[2].run, "npm run lint");
    }

    #[test]
    fn build_task_declares_dist_outputs() {
        let tasks = NodeNpmAdapter.default_tasks();
        let outputs = tasks[0].outputs.as_ref().unwrap();
        assert!(outputs.contains(&"dist/**".to_string()));
    }

    #[test]
    fn lint_task_includes_eslint_config_in_inputs() {
        let tasks = NodeNpmAdapter.default_tasks();
        let inputs = tasks[2].inputs.as_ref().unwrap();
        assert!(inputs.iter().any(|i| i == ".eslintrc*"));
        assert!(inputs.iter().any(|i| i == "eslint.config.*"));
    }

    #[test]
    fn detected_tasks_uses_npm_run_prefix() {
        let tmp = tmp_with(&[(
            "package.json",
            r#"{"scripts":{"build":"vite","test":"vitest"}}"#,
        )]);
        let tasks = NodeNpmAdapter.detected_tasks(tmp.path()).unwrap();
        let build = tasks.iter().find(|t| t.name == "build").unwrap();
        assert_eq!(build.run, "npm run build");
    }

    #[test]
    fn install_probe_missing_when_no_node_modules() {
        let tmp = tmp_with(&[("package.json", r#"{}"#), ("package-lock.json", "{}")]);
        assert!(matches!(
            NodeNpmAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_missing_when_node_modules_lacks_sentinel() {
        let tmp = tmp_with(&[("package.json", r#"{}"#), ("package-lock.json", "{}")]);
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        // Has a populated node_modules but no `.package-lock.json` sentinel
        // (torn install / manually nuked). Should still probe Missing.
        std::fs::create_dir_all(tmp.path().join("node_modules/foo")).unwrap();
        assert!(matches!(
            NodeNpmAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_ready_when_sentinel_present() {
        let tmp = tmp_with(&[("package.json", r#"{}"#), ("package-lock.json", "{}")]);
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        std::fs::write(tmp.path().join("node_modules/.package-lock.json"), "{}").unwrap();
        assert_eq!(
            NodeNpmAdapter.install_probe(tmp.path()),
            InstallProbe::Ready
        );
    }

    #[test]
    fn install_scope_returns_workspace_root_for_workspace_member() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let leaf = tmp.path().join("packages/lib");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(
            NodeNpmAdapter.install_scope(&leaf).canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn install_scope_falls_back_to_dish_dir_when_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            NodeNpmAdapter.install_scope(tmp.path()),
            tmp.path().to_path_buf()
        );
    }
}
