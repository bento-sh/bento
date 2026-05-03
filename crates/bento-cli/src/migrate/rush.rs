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
//! ## JSONC handling
//!
//! Rush's `rush.json` is documented as JSON but in practice ships with
//! `// …` line comments (JSONC). We try strict `serde_json` first, and
//! on failure fall back to a lightweight comment-stripper that only
//! removes `//` line comments outside of string literals.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::init::{toml_basic_string, toml_table_key};

use super::{MigrationReport, NoteKind};

// ── rush.json (subset we care about) ───────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RushJson {
    /// Ignored — informational only.
    #[serde(rename = "rushVersion")]
    rush_version: Option<String>,
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
    /// Ignored — Rush metadata for review-policy enforcement.
    #[serde(rename = "reviewCategory", default)]
    _review_category: Option<String>,
    /// Ignored — Rush metadata for `rush publish`.
    #[serde(rename = "shouldPublish", default)]
    _should_publish: Option<bool>,
}

// ── package.json (subset) ──────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PackageJson {
    name: Option<String>,
    scripts: BTreeMap<String, String>,
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

// ── npm-family adapter picker ──────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct NpmFamily {
    /// Adapter id used in dish.toml `language = …`.
    adapter_id: &'static str,
    /// Binary name used in `[tasks.<name>] run = "<pm> run <task>"`.
    pm: &'static str,
}

const NODE_NPM: NpmFamily = NpmFamily {
    adapter_id: "node-npm",
    pm: "npm",
};
const NODE_PNPM: NpmFamily = NpmFamily {
    adapter_id: "node-pnpm",
    pm: "pnpm",
};
const NODE_YARN: NpmFamily = NpmFamily {
    adapter_id: "node-yarn",
    pm: "yarn",
};

fn pick_npm_family(rush: &RushJson) -> NpmFamily {
    if rush.pnpm_version.is_some() {
        NODE_PNPM
    } else if rush.yarn_version.is_some() {
        NODE_YARN
    } else if rush.npm_version.is_some() {
        NODE_NPM
    } else {
        // Spec: default to node-npm when nothing's set.
        NODE_NPM
    }
}

// ── Public entry point ─────────────────────────────────────────────

pub struct Options {
    pub root: PathBuf,
    pub dry_run: bool,
    pub force: bool,
}

pub fn run(opts: Options) -> Result<MigrationReport> {
    let mut report = MigrationReport {
        applied: !opts.dry_run,
        ..Default::default()
    };

    // 1. Load rush.json (JSONC-tolerant).
    let rush_path = opts.root.join("rush.json");
    let rush: RushJson =
        parse_jsonc_file(&rush_path).with_context(|| format!("reading {}", rush_path.display()))?;

    if rush.projects.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            "rush.json has no projects — nothing to migrate",
        );
        return Ok(report);
    }

    let family = pick_npm_family(&rush);

    // 2. Optional: command-line.json for custom commands + phases.
    let cli_path = opts
        .root
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
            report.push_note(
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
            report.push_note(
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
    let mut dish_rels: Vec<String> = Vec::new();
    for proj in &rush.projects {
        let proj_dir = opts.root.join(&proj.project_folder);
        let pkg_json_path = proj_dir.join("package.json");
        let pkg: PackageJson = if pkg_json_path.exists() {
            parse_jsonc_file(&pkg_json_path)
                .with_context(|| format!("reading {}", pkg_json_path.display()))?
        } else {
            report.push_note(
                NoteKind::Skipped,
                format!(
                    "{} has no package.json — emitted dish.toml without [tasks.<name>] blocks",
                    proj.project_folder
                ),
            );
            PackageJson::default()
        };

        let dish_toml_path = proj_dir.join("dish.toml");
        if dish_toml_path.exists() && !opts.force {
            report.push_note(
                NoteKind::Conflict,
                format!(
                    "{} already exists — skipped (re-run with --force to overwrite)",
                    relative(&dish_toml_path, &opts.root).display()
                ),
            );
            // Still record the project in prod.toml — its config is
            // intentionally left alone, but the dish itself exists.
            dish_rels.push(forward_slash(&proj.project_folder));
            continue;
        }

        let dish_name = infer_short_name(&proj.package_name);
        let body = render_dish_toml(&dish_name, family, &pkg.scripts);
        if !opts.dry_run {
            fs::create_dir_all(&proj_dir)
                .with_context(|| format!("creating {}", proj_dir.display()))?;
        }
        write_or_simulate(&dish_toml_path, &body, opts.dry_run, &mut report)?;
        dish_rels.push(forward_slash(&proj.project_folder));
    }

    // 4. Workspace bento.toml — placeholder. Same shape as turbo.
    let bento_toml_path = opts.root.join("bento.toml");
    if bento_toml_path.exists() && !opts.force {
        report.push_note(
            NoteKind::Conflict,
            "bento.toml already exists — skipped (re-run with --force to overwrite)",
        );
    } else {
        let bento_body = crate::init::render_bento_toml(&BTreeMap::new());
        write_or_simulate(&bento_toml_path, &bento_body, opts.dry_run, &mut report)?;
    }

    // 5. bentos/prod.toml — list every project.
    let prod_path = opts.root.join("bentos").join("prod.toml");
    if prod_path.exists() && !opts.force {
        report.push_note(
            NoteKind::Conflict,
            "bentos/prod.toml already exists — skipped (re-run with --force to overwrite)",
        );
    } else {
        if !opts.dry_run {
            fs::create_dir_all(prod_path.parent().unwrap()).context("creating bentos/")?;
        }
        let prod_body = crate::init::render_prod_toml(&dish_rels);
        write_or_simulate(&prod_path, &prod_body, opts.dry_run, &mut report)?;
    }

    Ok(report)
}

// ── dish.toml renderer (rush-aware) ────────────────────────────────

fn render_dish_toml(
    dish_name: &str,
    family: NpmFamily,
    scripts: &BTreeMap<String, String>,
) -> String {
    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{lang}\"\n\
         \n\
         # Migrated from rush.json. Each [tasks.<name>] mirrors the\n\
         # corresponding `package.json` script via `{pm} run <name>`.\n",
        lang = family.adapter_id,
        pm = family.pm,
    );

    // Stable order — package.json scripts come from BTreeMap so already sorted.
    for task_name in scripts.keys() {
        body.push('\n');
        body.push_str(&format!("[tasks.{}]\n", toml_table_key(task_name)));
        body.push_str(&format!(
            "run = {}\n",
            toml_basic_string(&format!("{pm} run {task_name}", pm = family.pm))
        ));
    }

    body
}

/// Strip a leading `@scope/` from a Rush packageName so the dish name
/// reads as a clean identifier. `@org/web` → `web`. Multi-slash names
/// (rare) keep the rightmost segment.
fn infer_short_name(pkg_name: &str) -> String {
    pkg_name
        .rsplit_once('/')
        .map(|(_, last)| last.to_string())
        .unwrap_or_else(|| pkg_name.to_string())
}

/// Normalise a relative project folder to forward-slash form so the
/// emitted bentos/prod.toml is stable across platforms.
fn forward_slash(p: &str) -> String {
    p.replace('\\', "/")
}

// ── Helpers ────────────────────────────────────────────────────────

/// Parse a JSON file, falling back to a JSONC-style line-comment strip
/// if strict parsing fails. Rush manifests are documented as JSON but
/// in practice ship with `// …` comments.
fn parse_jsonc_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let body = fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))?;
    match serde_json::from_str::<T>(&body) {
        Ok(parsed) => Ok(parsed),
        Err(_) => {
            let stripped = strip_line_comments(&body);
            serde_json::from_str(&stripped)
                .with_context(|| format!("parsing {} as JSON (after JSONC strip)", path.display()))
        }
    }
}

/// Strip `// …` line comments outside of string literals. Only handles
/// what Rush ships in practice; doesn't try to be a full JSONC parser
/// (no `/* */` block comments, no trailing-comma fixup).
fn strip_line_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c as char);
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // skip to end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn write_or_simulate(
    path: &Path,
    body: &str,
    dry_run: bool,
    report: &mut MigrationReport,
) -> Result<()> {
    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    report.push_file(path.to_path_buf(), body.len());
    Ok(())
}

fn relative<'a>(p: &'a Path, root: &'a Path) -> &'a Path {
    p.strip_prefix(root).unwrap_or(p)
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("rush.json"),
            r#"{
                "rushVersion": "5.100.0",
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();

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

        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("apps/api"));
        assert!(prod.contains("apps/web"));
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let tmp = fixture();
        std::fs::write(tmp.path().join("bento.toml"), "name = \"existing\"\n").unwrap();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(report.has_conflicts());
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(!report.applied);
        assert!(!report.files_written.is_empty());
        assert!(!tmp.path().join("apps/web/dish.toml").exists());
        assert!(!tmp.path().join("bento.toml").exists());
        assert!(!tmp.path().join("bentos/prod.toml").exists());
    }

    #[test]
    fn picks_pnpm_adapter_when_pnpm_version_set() {
        let tmp = fixture(); // already uses pnpmVersion
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let _ = report;
        let dish = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-pnpm""#));
        assert!(dish.contains(r#"run = "pnpm run build""#));
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
        let _ = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("app/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-yarn""#));
        assert!(dish.contains(r#"run = "yarn run build""#));
    }

    #[test]
    fn defaults_to_node_npm_when_no_pm_set() {
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
        let _ = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("app/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-npm""#));
        assert!(dish.contains(r#"run = "npm run build""#));
    }

    #[test]
    fn surfaces_command_line_json_bulk_commands_as_notes() {
        let tmp = fixture();
        let cli_dir = tmp.path().join("common/config/rush");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("command-line.json"),
            r#"{
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::NotYetImplemented && n.message.contains("phase")),
            "expected a NotYetImplemented note mentioning phases, got {:?}",
            report.notes,
        );
    }

    #[test]
    fn strips_scope_from_package_name() {
        assert_eq!(infer_short_name("@org/web"), "web");
        assert_eq!(infer_short_name("just-a-name"), "just-a-name");
        assert_eq!(infer_short_name("@org/very/deep"), "deep");
    }

    #[test]
    fn jsonc_line_comments_are_tolerated() {
        // Defensive — Rush's rush.json commonly has `//` comments.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("rush.json"),
            r#"{
                // top-level comment
                "rushVersion": "5.100.0", // trailing comment
                "pnpmVersion": "8.10.0",
                "projects": [
                    // a project
                    { "packageName": "c", "projectFolder": "c" }
                ]
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("c")).unwrap();
        std::fs::write(
            tmp.path().join("c/package.json"),
            r#"{ "name": "c", "scripts": { "build": "echo c" } }"#,
        )
        .unwrap();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(report.applied);
        assert!(tmp.path().join("c/dish.toml").exists());
    }

    #[test]
    fn strip_line_comments_preserves_strings() {
        // A `//` inside a string literal must NOT be stripped.
        let s = r#"{ "url": "https://example.com//x" }"#;
        let stripped = strip_line_comments(s);
        assert_eq!(stripped, s);
    }
}
