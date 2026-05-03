//! yarn adapter.
//!
//! - Detects: `yarn.lock` at the dish root.
//! - Fingerprints: `package.json`, `yarn.lock`, `.yarnrc.yml`, `.nvmrc`,
//!   `.node-version`.
//! - Toolchain pin (priority): `.nvmrc` > `.node-version` > `engines.node`.
//! - Install: `yarn install --immutable`  (v2+ Berry style; still works
//!   on classic v1 where `--immutable` maps to `--frozen-lockfile`).
//! - Default tasks: `build`, `test`, `lint` via `yarn <task>`.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::adapter::{
    AddOptions, Added, DefaultTask, DetectedTask, InstallProbe, LanguageAdapter, TaskContext,
    ToolVersion,
};
use crate::diagnostic::DiagnosticHook;
use crate::node_common::node_eslint_hook;
use crate::node_common::{base_inputs, detected_npm_scripts, resolve_node_version};

pub struct YarnAdapter;

const FINGERPRINT: &[&str] = &[
    "package.json",
    "yarn.lock",
    ".yarnrc.yml",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
];

impl LanguageAdapter for YarnAdapter {
    fn id(&self) -> &str {
        "node-yarn"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("yarn.lock").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        resolve_node_version(dir, "node")
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        // --immutable is strict — requires a lockfile. Fall back to a
        // plain `yarn install` on cold projects so the lockfile gets
        // generated on first run.
        let mut cmd = Command::new("yarn");
        if ctx.dish_dir.join("yarn.lock").is_file() {
            cmd.args(["install", "--immutable"]);
        } else {
            cmd.arg("install");
        }
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "yarn install")
    }

    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        let mut cmd = Command::new("yarn");
        cmd.arg("add");
        if opts.dev {
            cmd.arg("--dev");
        }
        for p in packages {
            cmd.arg(p);
        }
        ctx.apply_env(&mut cmd);
        let label = if opts.dev {
            "yarn add --dev"
        } else {
            "yarn add"
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
        // Yarn v2+ with PnP skips `node_modules` entirely and writes
        // `.pnp.cjs` at the project root — that counts as Ready.
        // Classic v1 (`node_modules/.yarn-integrity`) and Berry's
        // `node-modules` linker (`node_modules/.yarn-state.yml`) are
        // the two remaining shapes.
        if dir.join(".pnp.cjs").is_file()
            || dir.join("node_modules").join(".yarn-state.yml").is_file()
            || dir.join("node_modules").join(".yarn-integrity").is_file()
        {
            InstallProbe::Ready
        } else {
            InstallProbe::missing("no .pnp.cjs, .yarn-state.yml, or .yarn-integrity found")
        }
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        let node = crate::probe::memoised("node", &["--version"]);
        let yarn = crate::probe::memoised("yarn", &["--version"]);
        crate::node_common::combine_probes(&[("node", node), ("yarn", yarn)])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        node_eslint_hook(task)
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        detected_npm_scripts(dir, "yarn")
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = base_inputs("yarn.lock");
        vec![
            DefaultTask {
                name: "build".into(),
                run: "yarn build".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into(), "build/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "yarn test".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "yarn lint".into(),
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
        let a = YarnAdapter;
        assert_eq!(a.id(), "node-yarn");
        let fp = a.fingerprint_files();
        assert!(fp.iter().any(|s| s == "yarn.lock"));
        assert!(fp.iter().any(|s| s == ".yarnrc.yml"));
    }

    #[test]
    fn detect_finds_yarn_project() {
        let tmp = tmp_with(&[
            ("package.json", r#"{"name":"x"}"#),
            ("yarn.lock", "# yarn lock\n"),
        ]);
        assert!(YarnAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_ignores_non_yarn_project() {
        let tmp = tmp_with(&[("package.json", r#"{}"#), ("pnpm-lock.yaml", "")]);
        assert!(!YarnAdapter.detect(tmp.path()));
    }

    #[test]
    fn default_tasks_use_yarn() {
        let tasks = YarnAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "yarn build");
        assert_eq!(tasks[1].run, "yarn test");
        assert_eq!(tasks[2].run, "yarn lint");
    }

    #[test]
    fn toolchain_reads_nvmrc() {
        let tmp = tmp_with(&[(".nvmrc", "20.10.0"), ("yarn.lock", "")]);
        let v = YarnAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "node");
        assert_eq!(v.version, "20.10.0");
    }

    #[test]
    fn detected_tasks_uses_yarn_prefix() {
        let tmp = tmp_with(&[("package.json", r#"{"scripts":{"build":"vite"}}"#)]);
        let tasks = YarnAdapter.detected_tasks(tmp.path()).unwrap();
        assert_eq!(tasks[0].run, "yarn build");
    }

    #[test]
    fn install_probe_missing_without_any_marker() {
        let tmp = tmp_with(&[("yarn.lock", "")]);
        assert!(matches!(
            YarnAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_ready_with_pnp_cjs() {
        // Yarn v2+ with PnP skips node_modules; `.pnp.cjs` at the root is
        // the canonical "install succeeded" marker.
        let tmp = tmp_with(&[("yarn.lock", ""), (".pnp.cjs", "module.exports = {};")]);
        assert_eq!(YarnAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }

    #[test]
    fn install_probe_ready_with_yarn_integrity() {
        let tmp = tmp_with(&[("yarn.lock", "")]);
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        std::fs::write(tmp.path().join("node_modules/.yarn-integrity"), "").unwrap();
        assert_eq!(YarnAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }
}
