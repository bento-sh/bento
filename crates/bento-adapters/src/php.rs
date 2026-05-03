//! PHP adapter (composer).
//!
//! - Detects: `composer.json` at the dish root.
//! - Fingerprints: `composer.json`, `composer.lock`, `.php-version`.
//! - Toolchain pin (priority): `.php-version` > `require.php` constraint
//!   in `composer.json` (stringly cached — we don't resolve a Composer
//!   semver spec, we just hash the raw string into the cache key).
//! - Install: `composer install`.
//! - Default tasks: `composer install` (build — PHP is interpreted),
//!   `vendor/bin/phpunit` (test), `vendor/bin/phpstan analyse` (lint).
//!
//! Lint tool choice: phpstan is the most-deployed static analyser; users
//! on psalm or php-cs-fixer override the `lint` task in dish.toml.
//! Standard convention beats configurability.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{
    DefaultTask, DetectedTask, InstallProbe, LanguageAdapter, TaskContext, ToolVersion,
};

pub struct PhpAdapter;

const FINGERPRINT: &[&str] = &[
    "composer.json",
    "composer.lock",
    ".php-version",
    ".tool-versions",
];

impl LanguageAdapter for PhpAdapter {
    fn id(&self) -> &str {
        "php"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("composer.json").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // 1. `.php-version` — phpenv / asdf-php convention.
        let dot = dir.join(".php-version");
        if dot.is_file() {
            let raw = std::fs::read_to_string(&dot)
                .with_context(|| format!("reading {}", dot.display()))?;
            let line = raw.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                return Ok(Some(ToolVersion {
                    tool: "php".into(),
                    version: line.to_string(),
                }));
            }
        }
        // 2. `require.php` constraint in composer.json.
        let composer = dir.join("composer.json");
        if composer.is_file() {
            let raw = std::fs::read_to_string(&composer)
                .with_context(|| format!("reading {}", composer.display()))?;
            if let Some(version) = parse_require_php(&raw) {
                return Ok(Some(ToolVersion {
                    tool: "php".into(),
                    version,
                }));
            }
        }
        // 3. .tool-versions (asdf/mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["php"])? {
            return Ok(Some(ToolVersion {
                tool: "php".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let mut cmd = Command::new("composer");
        cmd.arg("install");
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "composer install")
    }

    fn install_probe(&self, dir: &Path) -> InstallProbe {
        // Composer writes `vendor/autoload.php` as the canonical entry
        // point — every non-global install has it. If it's gone,
        // `vendor/bin/phpunit` and the like will fail with a useless
        // "file not found", so re-run composer install.
        if dir.join("vendor").join("autoload.php").is_file() {
            InstallProbe::Ready
        } else {
            InstallProbe::missing("vendor/autoload.php absent")
        }
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("php", &["--version"])
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        detected_composer_scripts(dir)
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = vec![
            "**/*.php".into(),
            "composer.json".into(),
            "composer.lock".into(),
        ];

        vec![
            DefaultTask {
                // PHP is interpreted; install IS the build. Users with
                // an asset-pipeline / production-optimisation flow
                // override (e.g. `composer install --no-dev --optimize-autoloader`).
                name: "build".into(),
                run: "composer install".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["vendor/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "vendor/bin/phpunit".into(),
                inputs: Some({
                    let mut v = inputs.clone();
                    v.push("phpunit.xml".into());
                    v.push("phpunit.xml.dist".into());
                    v.push("tests/**".into());
                    v
                }),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "vendor/bin/phpstan analyse".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push("phpstan.neon".into());
                    v.push("phpstan.neon.dist".into());
                    v.push("phpstan.dist.neon".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

/// Parse the `scripts` block of a composer.json into [`DetectedTask`]s.
/// Composer scripts run via `composer <name>` (composer dispatches to the
/// underlying binary or callback). Returns `None` when composer.json is
/// absent / unparseable; `Some(vec![])` when it exists but declares no
/// scripts.
fn detected_composer_scripts(dir: &Path) -> Option<Vec<DetectedTask>> {
    let composer = dir.join("composer.json");
    let raw = std::fs::read_to_string(&composer).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let scripts = value.get("scripts").and_then(|v| v.as_object())?;
    let tasks = scripts
        .iter()
        .filter_map(|(name, _)| {
            let n = name.trim();
            if n.is_empty() {
                None
            } else {
                Some(DetectedTask {
                    name: n.to_string(),
                    run: format!("composer {n}"),
                })
            }
        })
        .collect();
    Some(tasks)
}

/// Pull `require.php` out of a composer.json. Returns the raw constraint
/// string (`^8.2`, `>=8.0`, `8.2.*`, ...) — we don't resolve, just hash.
fn parse_require_php(s: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    let php = value.get("require")?.get("php")?.as_str()?.trim();
    if php.is_empty() {
        None
    } else {
        Some(php.to_string())
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
        let a = PhpAdapter;
        assert_eq!(a.id(), "php");
        let fp = a.fingerprint_files();
        for f in ["composer.json", "composer.lock", ".php-version"] {
            assert!(fp.iter().any(|s| s == f));
        }
    }

    #[test]
    fn detect_finds_composer_json() {
        let tmp = tmp_with(&[("composer.json", r#"{"name":"acme/app"}"#)]);
        assert!(PhpAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_non_php() {
        let tmp = tmp_with(&[("package.json", "{}")]);
        assert!(!PhpAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_dot_php_version() {
        let tmp = tmp_with(&[
            (".php-version", "8.2.10\n"),
            ("composer.json", r#"{"require":{"php":"^8.0"}}"#),
        ]);
        let v = PhpAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "php");
        assert_eq!(v.version, "8.2.10");
    }

    #[test]
    fn toolchain_falls_back_to_require_php() {
        let tmp = tmp_with(&[(
            "composer.json",
            r#"{"name":"acme/app","require":{"php":"^8.2","ext-mbstring":"*"}}"#,
        )]);
        let v = PhpAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "^8.2");
    }

    #[test]
    fn toolchain_returns_none_when_unpinned() {
        let tmp = tmp_with(&[(
            "composer.json",
            r#"{"name":"acme/app","require":{"ext-json":"*"}}"#,
        )]);
        assert!(PhpAdapter.required_toolchain(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn toolchain_returns_none_when_composer_unparseable() {
        // Malformed JSON should not crash — parse_require_php returns None.
        let tmp = tmp_with(&[("composer.json", "{ this is not json")]);
        assert!(PhpAdapter.required_toolchain(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn parse_require_php_extracts_constraint() {
        assert_eq!(
            parse_require_php(r#"{"require":{"php":">=8.1"}}"#),
            Some(">=8.1".into())
        );
    }

    #[test]
    fn parse_require_php_returns_none_when_php_absent() {
        assert_eq!(parse_require_php(r#"{"require":{"foo/bar":"1.0"}}"#), None);
    }

    #[test]
    fn default_tasks_use_composer_phpunit_phpstan() {
        let tasks = PhpAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "composer install");
        assert_eq!(tasks[1].run, "vendor/bin/phpunit");
        assert_eq!(tasks[2].run, "vendor/bin/phpstan analyse");
    }

    #[test]
    fn detected_tasks_mirrors_composer_scripts() {
        let tmp = tmp_with(&[(
            "composer.json",
            r#"{"scripts":{"test":"phpunit","cs-fix":"php-cs-fixer fix"}}"#,
        )]);
        let tasks = PhpAdapter.detected_tasks(tmp.path()).unwrap();
        assert_eq!(tasks.len(), 2);
        let test = tasks.iter().find(|t| t.name == "test").unwrap();
        assert_eq!(test.run, "composer test");
        let csfix = tasks.iter().find(|t| t.name == "cs-fix").unwrap();
        assert_eq!(csfix.run, "composer cs-fix");
    }

    #[test]
    fn detected_tasks_returns_none_when_composer_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(PhpAdapter.detected_tasks(tmp.path()).is_none());
    }

    #[test]
    fn detected_tasks_returns_none_when_no_scripts_block() {
        let tmp = tmp_with(&[("composer.json", r#"{"name":"acme/app"}"#)]);
        assert!(PhpAdapter.detected_tasks(tmp.path()).is_none());
    }

    #[test]
    fn install_probe_missing_without_vendor() {
        let tmp = tmp_with(&[("composer.json", r#"{"name":"x"}"#)]);
        assert!(matches!(
            PhpAdapter.install_probe(tmp.path()),
            InstallProbe::Missing { .. }
        ));
    }

    #[test]
    fn install_probe_ready_with_autoload() {
        let tmp = tmp_with(&[("composer.json", r#"{"name":"x"}"#)]);
        std::fs::create_dir(tmp.path().join("vendor")).unwrap();
        std::fs::write(
            tmp.path().join("vendor/autoload.php"),
            "<?php require_once __DIR__.'/composer/autoload_real.php';",
        )
        .unwrap();
        assert_eq!(PhpAdapter.install_probe(tmp.path()), InstallProbe::Ready);
    }
}
