//! pnpm adapter.
//!
//! - Detects: `pnpm-lock.yaml` at the dish root.
//! - Fingerprints: `package.json`, `pnpm-lock.yaml`, `.npmrc`, `.nvmrc`,
//!   `.node-version`.
//! - Toolchain pin (priority): `.nvmrc` > `.node-version` > `engines.node`.
//! - Install: `pnpm install --frozen-lockfile`.
//! - Default tasks: `build`, `test`, `lint` via `pnpm run <task>`.

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

pub struct PnpmAdapter;

const FINGERPRINT: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    ".npmrc",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
];

impl LanguageAdapter for PnpmAdapter {
    fn id(&self) -> &str {
        "node-pnpm"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("pnpm-lock.yaml").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        resolve_node_version(dir, "node")
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        // --frozen-lockfile is strict — requires a lockfile. Fall back
        // to a regular `pnpm install` on cold projects so the lockfile
        // gets generated on first run.
        let mut cmd = Command::new("pnpm");
        if ctx.dish_dir.join("pnpm-lock.yaml").is_file() {
            cmd.args(["install", "--frozen-lockfile"]);
        } else {
            cmd.arg("install");
        }
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "pnpm install")
    }

    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        let mut cmd = Command::new("pnpm");
        cmd.arg("add");
        if opts.dev {
            cmd.arg("-D");
        }
        for p in packages {
            cmd.arg(p);
        }
        ctx.apply_env(&mut cmd);
        let label = if opts.dev { "pnpm add -D" } else { "pnpm add" };
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
        // pnpm writes `node_modules/.modules.yaml` on every install; it
        // records the store path + hoisting config, and its absence
        // reliably indicates an incomplete install.
        if dir.join("node_modules").join(".modules.yaml").is_file() {
            InstallProbe::Ready
        } else {
            InstallProbe::missing("node_modules/.modules.yaml absent")
        }
    }

    fn install_scope(&self, dir: &Path) -> PathBuf {
        find_node_workspace_root(dir).unwrap_or_else(|| dir.to_path_buf())
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        let node = crate::probe::memoised("node", &["--version"]);
        let pnpm = crate::probe::memoised("pnpm", &["--version"]);
        crate::node_common::combine_probes(&[("node", node), ("pnpm", pnpm)])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        node_eslint_hook(task)
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        detected_npm_scripts(dir, "pnpm run")
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = base_inputs("pnpm-lock.yaml");
        vec![
            DefaultTask {
                name: "build".into(),
                run: "pnpm run build".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into(), "build/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "pnpm test".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "pnpm run lint".into(),
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
        let a = PnpmAdapter;
        assert_eq!(a.id(), "node-pnpm");
        let fp = a.fingerprint_files();
        assert!(fp.iter().any(|s| s == "pnpm-lock.yaml"));
        assert!(fp.iter().any(|s| s == ".npmrc"));
    }

    #[test]
    fn detect_finds_pnpm_project() {
        let tmp = tmp_with(&[
            ("package.json", r#"{"name":"x"}"#),
            ("pnpm-lock.yaml", "lockfileVersion: '6.0'\n"),
        ]);
        assert!(PnpmAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_ignores_npm_only_project() {
        let tmp = tmp_with(&[
            ("package.json", r#"{"name":"x"}"#),
            ("package-lock.json", "{}"),
        ]);
        assert!(!PnpmAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_reads_nvmrc() {
        let tmp = tmp_with(&[(".nvmrc", "v20.10.0\n"), ("pnpm-lock.yaml", "")]);
        let v = PnpmAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "node");
        assert_eq!(v.version, "20.10.0");
    }

    #[test]
    fn default_tasks_use_pnpm_run() {
        let tasks = PnpmAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert!(tasks[0].run.starts_with("pnpm run"));
        assert!(tasks[1].run.starts_with("pnpm test"));
        assert!(tasks[2].run.starts_with("pnpm run lint"));
    }

    #[test]
    fn build_inputs_include_lockfile() {
        let tasks = PnpmAdapter.default_tasks();
        let inputs = tasks[0].inputs.as_ref().unwrap();
        assert!(inputs.iter().any(|i| i == "pnpm-lock.yaml"));
    }

    #[test]
    fn detected_tasks_uses_pnpm_run_prefix() {
        let tmp = tmp_with(&[("package.json", r#"{"scripts":{"dev":"vite"}}"#)]);
        let tasks = PnpmAdapter.detected_tasks(tmp.path()).unwrap();
        assert_eq!(tasks[0].name, "dev");
        assert_eq!(tasks[0].run, "pnpm run dev");
    }

    #[test]
    fn install_probe_missing_without_modules_yaml() {
        let tmp = tmp_with(&[("pnpm-lock.yaml", "")]);
        assert!(matches!(
            PnpmAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_ready_with_modules_yaml() {
        let tmp = tmp_with(&[("pnpm-lock.yaml", "")]);
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        std::fs::write(tmp.path().join("node_modules/.modules.yaml"), "").unwrap();
        assert_eq!(PnpmAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }

    #[test]
    fn install_scope_returns_workspace_root_via_pnpm_workspace_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        let leaf = tmp.path().join("packages/web");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(
            PnpmAdapter.install_scope(&leaf).canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn install_scope_falls_back_to_dish_dir_when_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            PnpmAdapter.install_scope(tmp.path()),
            tmp.path().to_path_buf()
        );
    }
}
