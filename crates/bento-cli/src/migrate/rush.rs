//! Rush.js → bento migrator.
//!
//! Reads root `rush.json` (the workspace manifest) plus each project's
//! `package.json` to emit a starter bento config. Optionally cross-checks
//! `common/config/rush/command-line.json` for custom bulk/global commands
//! and surfaces them as notes (no auto-translation — they don't have a
//! one-shot bento equivalent).
//!
//! ## What translates cleanly
//!
//! | Rush                                           | Bento                                              |
//! |------------------------------------------------|----------------------------------------------------|
//! | `rush.json` `projects[]` → `projectFolder`     | per-project `dish.toml` at that path               |
//! | `rush.json` `pnpmVersion` / `npmVersion` / …   | `dish.toml` `language = "node-pnpm"` (etc.)        |
//! | each project's `package.json` `scripts.<name>` | `dish.toml` `[tasks.<name>] run = "<pm> run …"`    |
//! | `packageName` `@scope/foo`                     | dish name `foo`                                    |
//!
//! ## What gets a note instead
//!
//! - Custom commands in `common/config/rush/command-line.json`
//!   (`commandKind: "bulk" | "global"`) — surfaced with an Inferred note
//!   suggesting either a top-level `[tasks.<name>]` in `bento.toml` or
//!   a workflow-style fan-out, depending on the user's intent.
//! - Rush phased builds (`phases:` in `command-line.json`) — surfaced
//!   as NotYetImplemented; bento has no direct phase concept.
//!
//! Rush manifests are documented as JSON but ship as JSONC — the shared
//! `parse_jsonc_file` tolerates both comment styles.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{
    node_pm, node_pm_by_name, parse_jsonc_file, render_task, short_name, Emitter, MigrationReport,
    NodePm, NoteKind, PackageJson, TaskBlock,
};

// ── rush.json (subset we care about) ───────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RushJson {
    /// Exactly one of npmVersion / pnpmVersion / yarnVersion is set in
    /// any well-formed rush.json. Whichever is present picks the adapter.
    #[serde(rename = "npmVersion")]
    npm_version: Option<String>,
    #[serde(rename = "pnpmVersion")]
    pnpm_version: Option<String>,
    #[serde(rename = "yarnVersion")]
    yarn_version: Option<String>,
    projects: Vec<RushProject>,
}

#[derive(Debug, Deserialize)]
struct RushProject {
    #[serde(rename = "packageName")]
    package_name: String,
    #[serde(rename = "projectFolder")]
    project_folder: String,
}

// ── command-line.json (subset) ─────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommandLineJson {
    commands: Vec<CommandLineCommand>,
    /// Phased-build descriptors (Rush 5.7+). We don't translate them;
    /// we just surface their presence as a NotYetImplemented note.
    phases: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CommandLineCommand {
    #[serde(rename = "commandKind")]
    command_kind: Option<String>,
    name: String,
    #[serde(default)]
    summary: Option<String>,
}

// ── Public entry point ─────────────────────────────────────────────

pub fn run(mut e: Emitter) -> Result<MigrationReport> {
    let root = e.root().to_path_buf();

    // 1. Load rush.json (JSONC-tolerant).
    let rush_path = root.join("rush.json");
    let rush: RushJson =
        parse_jsonc_file(&rush_path).with_context(|| format!("reading {}", rush_path.display()))?;

    if rush.projects.is_empty() {
        e.note(
            NoteKind::Skipped,
            "rush.json has no projects — nothing to migrate",
        );
        return e.finish();
    }

    let declared_pm = if rush.pnpm_version.is_some() {
        Some("pnpm")
    } else if rush.yarn_version.is_some() {
        Some("yarn")
    } else if rush.npm_version.is_some() {
        Some("npm")
    } else {
        None
    };

    // 2. Optional: command-line.json for custom commands + phases.
    let cli_path = root
        .join("common")
        .join("config")
        .join("rush")
        .join("command-line.json");
    if cli_path.exists() {
        let cli: CommandLineJson = parse_jsonc_file(&cli_path)
            .with_context(|| format!("reading {}", cli_path.display()))?;
        for cmd in &cli.commands {
            let kind = cmd.command_kind.as_deref().unwrap_or("custom");
            let summary = cmd
                .summary
                .as_deref()
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            e.note(
                NoteKind::Inferred,
                format!(
                    "Rush {kind} command `{name}` not auto-translated{summary}; consider \
                     modelling as a top-level `[tasks.{name}]` in bento.toml or a workflow \
                     that fans out across dishes.",
                    name = cmd.name,
                ),
            );
        }
        if !cli.phases.is_empty() {
            e.note(
                NoteKind::NotYetImplemented,
                format!(
                    "command-line.json declares {n} phased-build entr{ies} — bento has no \
                     direct phase model yet; review and hand-port the ordering as task \
                     dependencies between dishes.",
                    n = cli.phases.len(),
                    ies = if cli.phases.len() == 1 { "y" } else { "ies" },
                ),
            );
        }
    }

    // 3. Emit per-project dish.toml.
    for proj in &rush.projects {
        let proj_dir = root.join(&proj.project_folder);
        let pkg_json_path = proj_dir.join("package.json");
        let pkg: PackageJson = if pkg_json_path.exists() {
            parse_jsonc_file(&pkg_json_path)
                .with_context(|| format!("reading {}", pkg_json_path.display()))?
        } else {
            e.note(
                NoteKind::Skipped,
                format!(
                    "{} has no package.json — emitted dish.toml without [tasks.<name>] blocks",
                    proj.project_folder
                ),
            );
            PackageJson::default()
        };

        let pm = match declared_pm {
            Some(name) => node_pm_by_name(name),
            None => node_pm(&proj_dir, &root),
        };
        let body = render_dish_toml(&short_name(&proj.package_name), pm, &pkg.scripts);
        e.dish(&proj_dir, &body)?;
    }

    e.finish()
}

// ── dish.toml renderer (rush-aware) ────────────────────────────────

fn render_dish_toml(dish_name: &str, pm: NodePm, scripts: &BTreeMap<String, String>) -> String {
    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{lang}\"\n\
         \n\
         # Migrated from rush.json. Each [tasks.<name>] mirrors the\n\
         # corresponding `package.json` script via `{prefix} <name>`.\n",
        lang = pm.language,
        prefix = pm.run_prefix,
    );

    // package.json scripts are raw shell entry points, not a declared
    // CI pipeline — a mirrored `dev` / `start` stays `bento run`-only.
    for name in scripts.keys() {
        body.push_str(&render_task(
            name,
            &TaskBlock {
                run: &format!("{} {name}", pm.run_prefix),
                ..Default::default()
            },
        ));
    }

    body
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::Options;
    use std::path::{Path, PathBuf};

    fn migrate(root: &Path, dry_run: bool, force: bool) -> MigrationReport {
        run(Emitter::new(Options {
            root: root.to_path_buf(),
            dry_run,
            force,
        }))
        .unwrap()
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("rush.json"),
            r#"{
                /**
                 * Rush ships this banner in its own template.
                 */
                "rushVersion": "5.100.0",
                // Workspaces use pnpm.
                "pnpmVersion": "8.10.0",
                "projects": [
                    {
                        "packageName": "@migrate-rush/web",
                        "projectFolder": "apps/web"
                    },
                    {
                        "packageName": "@migrate-rush/api",
                        "projectFolder": "apps/api",
                        "reviewCategory": "production",
                        "shouldPublish": false
                    }
                ]
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("apps/web")).unwrap();
        std::fs::write(
            root.join("apps/web/package.json"),
            r#"{
                "name": "@migrate-rush/web",
                "scripts": {
                    "build": "next build",
                    "test": "jest",
                    "lint": "eslint ."
                }
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("apps/api")).unwrap();
        std::fs::write(
            root.join("apps/api/package.json"),
            r#"{
                "name": "@migrate-rush/api",
                "scripts": {
                    "build": "tsc",
                    "test": "jest",
                    "start": "node dist/server.js"
                }
            }"#,
        )
        .unwrap();
        tmp
    }

    #[test]
    fn migrates_workspace_with_two_projects() {
        let tmp = fixture();
        let report = migrate(tmp.path(), false, false);

        let written: Vec<_> = report
            .files_written
            .iter()
            .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
            .collect();
        assert!(written.contains(&PathBuf::from("apps/web/dish.toml")));
        assert!(written.contains(&PathBuf::from("apps/api/dish.toml")));
        assert!(written.contains(&PathBuf::from("bento.toml")));
        assert!(written.contains(&PathBuf::from("bentos/prod.toml")));
        assert!(report.applied);

        let web_dish = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(web_dish.contains(r#"name = "web""#));
        assert!(web_dish.contains(r#"language = "node-pnpm""#));
        assert!(web_dish.contains("[tasks.build]"));
        assert!(web_dish.contains(r#"run = "pnpm run build""#));
        assert!(web_dish.contains("[tasks.test]"));
        assert!(web_dish.contains("[tasks.lint]"));

        let api_dish = std::fs::read_to_string(tmp.path().join("apps/api/dish.toml")).unwrap();
        assert!(api_dish.contains(r#"name = "api""#));
        assert!(api_dish.contains("[tasks.start]"));
        assert!(api_dish.contains(r#"run = "pnpm run start""#));
        // `start` is a custom name mirrored from package.json — it must
        // stay out of `bento ci`.
        assert!(!api_dish.contains("ci = true"), "{api_dish}");

        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("apps/api"));
        assert!(prod.contains("apps/web"));
    }

    #[test]
    fn never_overwrites_bento_toml() {
        let tmp = fixture();
        std::fs::write(tmp.path().join("bento.toml"), "name = \"existing\"\n").unwrap();
        let report = migrate(tmp.path(), false, true);
        let body = std::fs::read_to_string(tmp.path().join("bento.toml")).unwrap();
        assert_eq!(body, "name = \"existing\"\n");
        // Dishes still get written into fresh dirs.
        let written: Vec<_> = report
            .files_written
            .iter()
            .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
            .collect();
        assert!(written.contains(&PathBuf::from("apps/web/dish.toml")));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = fixture();
        let report = migrate(tmp.path(), true, false);
        assert!(!report.applied);
        assert!(!report.files_written.is_empty());
        assert!(!tmp.path().join("apps/web/dish.toml").exists());
        assert!(!tmp.path().join("bento.toml").exists());
        assert!(!tmp.path().join("bentos/prod.toml").exists());
    }

    #[test]
    fn picks_yarn_adapter_when_yarn_version_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("rush.json"),
            r#"{
                "rushVersion": "5.100.0",
                "yarnVersion": "1.22.19",
                "projects": [
                    { "packageName": "y-app", "projectFolder": "app" }
                ]
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        std::fs::write(
            tmp.path().join("app/package.json"),
            r#"{ "name": "y-app", "scripts": { "build": "tsc" } }"#,
        )
        .unwrap();
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("app/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-yarn""#));
        assert!(dish.contains(r#"run = "yarn run build""#));
    }

    #[test]
    fn detects_the_package_manager_when_rush_json_declares_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("rush.json"),
            r#"{
                "rushVersion": "5.100.0",
                "projects": [
                    { "packageName": "n-app", "projectFolder": "app" }
                ]
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        std::fs::write(
            tmp.path().join("app/package.json"),
            r#"{ "name": "n-app", "scripts": { "build": "echo built" } }"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("app/pnpm-lock.yaml"),
            "lockfileVersion: 6\n",
        )
        .unwrap();
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("app/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-pnpm""#), "{dish}");
        assert!(dish.contains(r#"run = "pnpm run build""#), "{dish}");
    }

    #[test]
    fn surfaces_command_line_json_bulk_commands_as_notes() {
        let tmp = fixture();
        let cli_dir = tmp.path().join("common/config/rush");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("command-line.json"),
            r#"{
                /**
                 * Rush's stock command-line.json opens with a block comment.
                 */
                "commands": [
                    {
                        "commandKind": "bulk",
                        "name": "audit",
                        "summary": "Audit every project for vulnerabilities",
                        "shellCommand": "npm audit"
                    }
                ]
            }"#,
        )
        .unwrap();
        let report = migrate(tmp.path(), true, false);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Inferred && n.message.contains("audit")),
            "expected an Inferred note mentioning the `audit` bulk command, got {:?}",
            report.notes,
        );
    }

    #[test]
    fn surfaces_phases_as_not_yet_implemented() {
        let tmp = fixture();
        let cli_dir = tmp.path().join("common/config/rush");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("command-line.json"),
            r#"{
                "phases": [
                    { "name": "_phase:build", "dependencies": { "self": [] } }
                ]
            }"#,
        )
        .unwrap();
        let report = migrate(tmp.path(), true, false);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::NotYetImplemented && n.message.contains("phase")),
            "expected a NotYetImplemented note mentioning phases, got {:?}",
            report.notes,
        );
    }
}
