//! Bun adapter.
//!
//! - Detects: `bun.lock` or `bun.lockb` at the dish root.
//! - Fingerprints: `package.json`, `bun.lock`, `bun.lockb`, `bunfig.toml`,
//!   `.bun-version`, `.nvmrc`, `.node-version`.
//! - Toolchain pin (priority): `.bun-version` > Node fallbacks
//!   (`.nvmrc` > `.node-version` > `engines.node`) — a dish that pins
//!   Node but runs Bun is valid (Bun honours the Node target).
//! - Install: `bun install --frozen-lockfile`.
//! - Default tasks: `bun run build` / `bun test` / `bun run lint`.

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
    base_inputs, detected_npm_scripts, find_node_workspace_root, read_version_file,
    resolve_node_version, tool_version,
};

pub struct BunAdapter;

const FINGERPRINT: &[&str] = &[
    "package.json",
    "bun.lock",
    "bun.lockb",
    "bunfig.toml",
    ".bun-version",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
];

impl LanguageAdapter for BunAdapter {
    fn id(&self) -> &str {
        "bun"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("bun.lock").is_file() || dir.join("bun.lockb").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        if let Some(v) = read_version_file(&dir.join(".bun-version"))? {
            return Ok(Some(tool_version("bun", v)));
        }
        // .tool-versions can pin bun directly (asdf-bun / mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["bun"])? {
            return Ok(Some(tool_version("bun", v)));
        }
        // Fall back to a Node version pin — bun runs Node code and a
        // pinned Node version is still a useful cache-key input.
        resolve_node_version(dir, "node")
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        // --frozen-lockfile is strict — requires a lockfile. Fall back
        // to a plain `bun install` on cold projects.
        let has_lockfile =
            ctx.dish_dir.join("bun.lock").is_file() || ctx.dish_dir.join("bun.lockb").is_file();
        let mut cmd = Command::new("bun");
        if has_lockfile {
            cmd.args(["install", "--frozen-lockfile"]);
        } else {
            cmd.arg("install");
        }
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "bun install")
    }

    fn add(&self, ctx: &TaskContext, packages: &[&str], opts: AddOptions) -> Result<Vec<Added>> {
        let mut cmd = Command::new("bun");
        cmd.arg("add");
        if opts.dev {
            cmd.arg("-d");
        }
        for p in packages {
            cmd.arg(p);
        }
        ctx.apply_env(&mut cmd);
        let label = if opts.dev { "bun add -d" } else { "bun add" };
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
        // Bun doesn't write a canonical install-sentinel file, so we
        // fall back to checking `node_modules/` exists and has at
        // least one child entry. An empty or missing directory means
        // `bun install --frozen-lockfile` hasn't run successfully.
        let nm = dir.join("node_modules");
        let populated = std::fs::read_dir(&nm)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if populated {
            InstallProbe::Ready
        } else {
            InstallProbe::missing("node_modules missing or empty")
        }
    }

    fn install_scope(&self, dir: &Path) -> PathBuf {
        find_node_workspace_root(dir).unwrap_or_else(|| dir.to_path_buf())
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("bun", &["--version"])
    }

    fn diagnostic_hook(&self, task: &str) -> Option<DiagnosticHook> {
        node_eslint_hook(task)
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        detected_npm_scripts(dir, "bun run")
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let mut inputs = base_inputs("bun.lock");
        inputs.push("bun.lockb".into());
        inputs.push("bunfig.toml".into());

        vec![
            DefaultTask {
                name: "build".into(),
                run: "bun run build".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into(), "build/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                // Bun has a built-in test runner; prefer it over `bun run test`.
                run: "bun test".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "bun run lint".into(),
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
        let a = BunAdapter;
        assert_eq!(a.id(), "bun");
        let fp = a.fingerprint_files();
        for f in [
            "package.json",
            "bun.lock",
            "bun.lockb",
            "bunfig.toml",
            ".bun-version",
        ] {
            assert!(fp.iter().any(|s| s == f), "fingerprint missing: {f}");
        }
    }

    #[test]
    fn detect_prefers_bun_lock_text() {
        let tmp = tmp_with(&[("package.json", r#"{}"#), ("bun.lock", "")]);
        assert!(BunAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_also_accepts_legacy_bun_lockb() {
        let tmp = tmp_with(&[("package.json", r#"{}"#), ("bun.lockb", "")]);
        assert!(BunAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_bun_version_over_nvmrc() {
        let tmp = tmp_with(&[
            (".bun-version", "1.1.0\n"),
            (".nvmrc", "22.1.0\n"),
            ("bun.lock", ""),
        ]);
        let v = BunAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "bun");
        assert_eq!(v.version, "1.1.0");
    }

    #[test]
    fn toolchain_falls_back_to_node_pin_when_bun_unpinned() {
        let tmp = tmp_with(&[(".nvmrc", "22.1.0\n"), ("bun.lock", "")]);
        let v = BunAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "node");
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn default_tasks_use_bun_test() {
        let tasks = BunAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[1].run, "bun test");
    }

    #[test]
    fn detected_tasks_uses_bun_run_prefix() {
        let tmp = tmp_with(&[
            (
                "package.json",
                r#"{"scripts":{"build":"bun build src/index.ts"}}"#,
            ),
            ("bun.lock", ""),
        ]);
        let tasks = BunAdapter.detected_tasks(tmp.path()).unwrap();
        assert_eq!(tasks[0].run, "bun run build");
    }

    #[test]
    fn install_probe_missing_without_node_modules() {
        let tmp = tmp_with(&[("bun.lock", "")]);
        assert!(matches!(
            BunAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_missing_when_node_modules_empty() {
        let tmp = tmp_with(&[("bun.lock", "")]);
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        assert!(matches!(
            BunAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_ready_when_node_modules_populated() {
        let tmp = tmp_with(&[("bun.lock", "")]);
        std::fs::create_dir_all(tmp.path().join("node_modules/foo")).unwrap();
        assert_eq!(BunAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }

    #[test]
    fn install_scope_returns_workspace_root_for_workspace_member() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let leaf = tmp.path().join("packages/web");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(
            BunAdapter.install_scope(&leaf).canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn install_scope_falls_back_to_dish_dir_when_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            BunAdapter.install_scope(tmp.path()),
            tmp.path().to_path_buf()
        );
    }
}
