//! yarn adapter.
//!
//! - Detects: `yarn.lock` at the dish root.
//! - Fingerprints: `package.json`, `yarn.lock`, `.yarnrc.yml`, `.nvmrc`,
//!   `.node-version`.
//! - Toolchain pin (priority): `.nvmrc` > `.node-version` > `engines.node`.
//! - Install: `yarn install --immutable` (Berry) or `--frozen-lockfile`
//!   (classic v1), picked from the lockfile header; bare `yarn install`
//!   when there's no lockfile yet.
//! - Default tasks: `build`, `test`, `lint` via `yarn <task>`.

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
        crate::node_common::detect_node_pm(dir) == Some("yarn")
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        resolve_node_version(dir, "node")
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        // Frozen install requires a lockfile. Fall back to a plain
        // `yarn install` on cold projects so it gets generated on the
        // first run.
        let lock = ctx.dish_dir.join("yarn.lock");
        let mut cmd = Command::new("yarn");
        cmd.arg("install");
        if lock.is_file() {
            cmd.arg(if is_classic_lockfile(&lock) {
                "--frozen-lockfile"
            } else {
                "--immutable"
            });
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
        if crate::node_common::package_has_nothing_to_install(dir) {
            return InstallProbe::Ready;
        }
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

    fn install_scope(&self, dir: &Path) -> PathBuf {
        find_node_workspace_root(dir).unwrap_or_else(|| dir.to_path_buf())
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        let node = crate::probe::memoised("node", &["--version"]);
        let yarn = crate::probe::memoised("yarn", &["--version"]);
        crate::node_common::combine_probes(&[("node", node), ("yarn", yarn)])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        node_eslint_hook(task, "yarn")
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        detected_npm_scripts(dir, "yarn")
    }

    fn derived_paths(&self) -> Vec<String> {
        crate::node_common::node_derived_paths()
    }

    fn default_tasks(&self, _dir: &Path) -> Vec<DefaultTask> {
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

/// Yarn 1 accepts `--immutable` and ignores it — the install re-resolves
/// and rewrites `yarn.lock` anyway, so a lockfile drift lands in CI green.
/// Its equivalent is `--frozen-lockfile`, which Berry in turn rejects, so
/// the flag has to follow the project. The lockfile header is the
/// authoritative discriminator and costs one small read: classic writes
/// `# yarn lockfile v1`, Berry writes a `__metadata:` block.
fn is_classic_lockfile(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 256];
    let Ok(n) = f.read(&mut head) else {
        return false;
    };
    String::from_utf8_lossy(&head[..n]).contains("yarn lockfile v1")
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
        let tasks = YarnAdapter.default_tasks(Path::new("."));
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
    fn classic_and_berry_lockfiles_are_told_apart() {
        let classic = tmp_with(&[(
            "yarn.lock",
            "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n\
             # yarn lockfile v1\n\n\nleft-pad@^1.3.0:\n  version \"1.3.0\"\n",
        )]);
        assert!(is_classic_lockfile(&classic.path().join("yarn.lock")));

        let berry = tmp_with(&[(
            "yarn.lock",
            "# This file is generated by running \"yarn install\" inside your project.\n\
             # Manual changes might be lost - proceed with caution!\n\n\
             __metadata:\n  version: 8\n",
        )]);
        assert!(!is_classic_lockfile(&berry.path().join("yarn.lock")));
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

    #[test]
    fn install_scope_returns_workspace_root_for_workspace_member() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let leaf = tmp.path().join("packages/api");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(
            YarnAdapter.install_scope(&leaf).canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn install_scope_falls_back_to_dish_dir_when_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            YarnAdapter.install_scope(tmp.path()),
            tmp.path().to_path_buf()
        );
    }
}
