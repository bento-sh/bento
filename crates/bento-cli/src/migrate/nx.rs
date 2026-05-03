//! Nx → bento migrator.
//!
//! Reads root `nx.json` (`targetDefaults`, `namedInputs`,
//! `workspaceLayout`) plus per-project `project.json` files.
//! Discovers projects by walking the workspace for `project.json`
//! (capped to `appsDir` + `libsDir` when `workspaceLayout` is
//! declared, otherwise the whole tree minus `node_modules` /
//! `.nx`). Emits per-project `dish.toml`, root `bento.toml`, and a
//! starter `bentos/prod.toml`.
//!
//! ## What translates cleanly
//!
//! | Nx                                   | Bento                                                            |
//! |--------------------------------------|------------------------------------------------------------------|
//! | `targets.<name>.outputs`             | `dish.toml [tasks.<name>] outputs = ...` (substitutions stripped) |
//! | `targets.<name>.inputs` (literals)   | `dish.toml [tasks.<name>] inputs = ...`                           |
//! | `executor: nx:run-commands`          | `run = options.command` (or `commands[]` joined with `&&`)        |
//! | Common `@nx/...` build/test executors| canonical CLI invocation (`vite build`, `jest`, `tsc`, …)         |
//!
//! ## What gets a note instead
//!
//! - `dependsOn` arrays — bento derives task ordering from the dish
//!   graph. `^build` ≈ bento's automatic upstream rebuild via
//!   `dish.depends_on`, which the user wires by hand.
//! - `cache: false` — bento has no per-task no-cache flag; surfaced
//!   as a note. Use the global `bento --no-cache` for ad-hoc bypass.
//! - Persistent dev executors (`@nx/vite:dev-server`, `@nx/webpack:
//!   dev-server`, `@nx/next:server`) — modelled as the dish-level
//!   `[serve]` block in dish.toml, not `[tasks.<name>]`.
//! - Unknown executors — emitted as `nx run <project>:<target>`
//!   shims with an `Inferred` note recommending hand-port.
//! - Named-input references (`production`, `default`, …) — resolved
//!   from `nx.json.namedInputs` when possible; cross-project
//!   `^production` is noted but not auto-derived.
//! - `configurations` — only the default config is migrated; other
//!   configurations get a `NotYetImplemented` note.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::init::{toml_basic_string, toml_table_key};

use super::{MigrationReport, NoteKind};

// ── nx.json (subset) ───────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NxJson {
    #[serde(rename = "targetDefaults")]
    target_defaults: BTreeMap<String, NxTarget>,
    #[serde(rename = "namedInputs")]
    named_inputs: BTreeMap<String, Vec<NxInput>>,
    #[serde(rename = "workspaceLayout")]
    workspace_layout: Option<WorkspaceLayout>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkspaceLayout {
    #[serde(rename = "appsDir")]
    apps_dir: Option<String>,
    #[serde(rename = "libsDir")]
    libs_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NxTarget {
    executor: Option<String>,
    options: BTreeMap<String, serde_json::Value>,
    outputs: Vec<String>,
    inputs: Vec<NxInput>,
    #[serde(rename = "dependsOn")]
    depends_on: Vec<NxDependsOn>,
    cache: Option<bool>,
    configurations: BTreeMap<String, serde_json::Value>,
    /// Some Nx configs put a top-level `command` on a target rather
    /// than nesting it under `options`. We accept both shapes.
    command: Option<String>,
    commands: Option<Vec<NxCommand>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NxInput {
    Pattern(String),
    External {
        #[serde(rename = "externalDependencies")]
        external_dependencies: Vec<String>,
    },
    Env {
        env: String,
    },
    DependentTasks {
        #[serde(rename = "dependentTasksOutputFiles")]
        dependent_tasks_output_files: String,
    },
    Other(serde_json::Value),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NxDependsOn {
    Simple(String),
    Detailed { target: String },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NxCommand {
    Simple(String),
    Detailed { command: String },
}

// ── project.json (subset) ──────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectJson {
    name: Option<String>,
    #[serde(rename = "projectType")]
    project_type: Option<String>,
    targets: BTreeMap<String, NxTarget>,
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

    // 1. Load nx.json — present in nearly every Nx workspace, but
    //    inferred-only workspaces can omit it. Treat absence as an
    //    empty config rather than a hard error.
    let nx_path = opts.root.join("nx.json");
    let nx: NxJson = if nx_path.is_file() {
        parse_json_file(&nx_path).with_context(|| format!("reading {}", nx_path.display()))?
    } else {
        report.push_note(
            NoteKind::Inferred,
            "no nx.json at workspace root — proceeding with empty defaults",
        );
        NxJson::default()
    };

    // 2. Discover projects by walking for project.json. Cap the walk
    //    to the declared apps/libs dirs when workspaceLayout is set.
    let scan_dirs = scan_dirs(&opts.root, nx.workspace_layout.as_ref());
    let projects = discover_projects(&opts.root, &scan_dirs)?;
    if projects.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            format!(
                "no project.json files found under {:?} — nothing to write",
                scan_dirs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
            ),
        );
        return Ok(report);
    }

    // 3. Per-project dish.toml.
    let mut dish_rels: Vec<String> = Vec::new();
    for proj in &projects {
        let dish_path = proj.dir.join("dish.toml");
        if dish_path.exists() && !opts.force {
            report.push_note(
                NoteKind::Conflict,
                format!(
                    "{} already exists — skipped (re-run with --force to overwrite)",
                    relative(&dish_path, &opts.root).display()
                ),
            );
            continue;
        }
        let body = render_dish_toml(proj, &nx, &mut report);
        write_or_simulate(&dish_path, &body, opts.dry_run, &mut report)?;
        dish_rels.push(relative(&proj.dir, &opts.root).display().to_string());
    }

    // 4. Workspace bento.toml — placeholder shape; user fills in
    //    cache + toolchain pins later.
    let bento_path = opts.root.join("bento.toml");
    if bento_path.exists() && !opts.force {
        report.push_note(
            NoteKind::Conflict,
            "bento.toml already exists — skipped (re-run with --force to overwrite)",
        );
    } else {
        let body = crate::init::render_bento_toml(&BTreeMap::new());
        write_or_simulate(&bento_path, &body, opts.dry_run, &mut report)?;
    }

    // 5. bentos/prod.toml — list every dish the migrator created.
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
        let body = crate::init::render_prod_toml(&dish_rels);
        write_or_simulate(&prod_path, &body, opts.dry_run, &mut report)?;
    }

    Ok(report)
}

// ── Project discovery ──────────────────────────────────────────────

#[derive(Debug)]
struct DiscoveredProject {
    /// Absolute path to the project directory.
    dir: PathBuf,
    /// Path relative to workspace root.
    rel_dir: PathBuf,
    /// Parsed project.json contents.
    project: ProjectJson,
}

/// Determine which directories to walk for project.json files. With
/// no workspaceLayout, scan from the root. With workspaceLayout, scan
/// only the declared `appsDir` + `libsDir`.
fn scan_dirs(root: &Path, layout: Option<&WorkspaceLayout>) -> Vec<PathBuf> {
    let Some(layout) = layout else {
        return vec![root.to_path_buf()];
    };
    let mut out = Vec::new();
    if let Some(apps) = &layout.apps_dir {
        out.push(root.join(apps));
    }
    if let Some(libs) = &layout.libs_dir {
        out.push(root.join(libs));
    }
    if out.is_empty() {
        out.push(root.to_path_buf());
    }
    out
}

fn discover_projects(root: &Path, scan_dirs: &[PathBuf]) -> Result<Vec<DiscoveredProject>> {
    let mut out: Vec<DiscoveredProject> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in scan_dirs {
        if !dir.is_dir() {
            continue;
        }
        walk_for_project_json(dir, &mut |project_json| {
            let proj_dir = project_json.parent().unwrap().to_path_buf();
            if !seen.insert(proj_dir.clone()) {
                return Ok(());
            }
            let project: ProjectJson = parse_json_file(project_json)
                .with_context(|| format!("reading {}", project_json.display()))?;
            let rel_dir = proj_dir
                .strip_prefix(root)
                .unwrap_or(&proj_dir)
                .to_path_buf();
            out.push(DiscoveredProject {
                dir: proj_dir,
                rel_dir,
                project,
            });
            Ok(())
        })?;
    }
    out.sort_by(|a, b| a.rel_dir.cmp(&b.rel_dir));
    Ok(out)
}

/// Recursively walk `dir`, calling `cb` for every `project.json` we
/// find. Skips `node_modules`, `.nx`, `.git`, and `dist` by name —
/// the standard noise sinks where a stray `project.json` is never a
/// real Nx project.
fn walk_for_project_json(dir: &Path, cb: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if matches!(
                name_str.as_ref(),
                "node_modules" | ".nx" | ".git" | "dist" | "build" | "tmp" | ".cache"
            ) {
                continue;
            }
            walk_for_project_json(&path, cb)?;
        } else if file_type.is_file() && entry.file_name() == "project.json" {
            cb(&path)?;
        }
    }
    Ok(())
}

// ── dish.toml renderer ─────────────────────────────────────────────

fn render_dish_toml(proj: &DiscoveredProject, nx: &NxJson, report: &mut MigrationReport) -> String {
    let dish_name = proj
        .project
        .name
        .as_deref()
        .map(infer_short_name)
        .or_else(|| {
            proj.dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "dish".to_string());

    // Heuristic: every Nx project ultimately runs through node, so
    // `node-npm` is the safest starter language. User picks the right
    // manager (npm/pnpm/bun/yarn) by editing the line.
    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"node-npm\"\n\
         \n\
         # Migrated from project.json. Each [tasks.<name>] mirrors the\n\
         # Nx target with the same name. Review run / outputs / inputs\n\
         # against your build artefacts.\n",
    );

    let rel_project_root = proj.rel_dir.to_string_lossy().to_string();
    let mut serve_block: Option<(String, String)> = None;

    for (target_name, target) in &proj.project.targets {
        // Merge targetDefaults onto the per-project target so
        // outputs/inputs/cache fall through correctly.
        let merged = merge_with_defaults(target, target_name, &nx.target_defaults);

        if merged.cache == Some(false) {
            report.push_note(
                NoteKind::Skipped,
                format!(
                    "{}: target `{target_name}` has `cache: false` — bento has no \
                     per-task no-cache flag; the task still runs but its output \
                     WILL be cached. Use `bento --no-cache` for ad-hoc bypass.",
                    rel_project_root
                ),
            );
        }
        if !merged.depends_on.is_empty() {
            let names: Vec<String> = merged
                .depends_on
                .iter()
                .map(|d| match d {
                    NxDependsOn::Simple(s) => s.clone(),
                    NxDependsOn::Detailed { target } => target.clone(),
                })
                .collect();
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{}: target `{target_name}` had dependsOn = {names:?} — bento derives \
                     task ordering from the dish graph; cross-project `^build` maps to \
                     dish.toml `depends_on` between dishes (not auto-derived from \
                     project.json — wire by hand).",
                    rel_project_root
                ),
            );
        }
        if !merged.configurations.is_empty() {
            let names: Vec<&String> = merged.configurations.keys().collect();
            report.push_note(
                NoteKind::NotYetImplemented,
                format!(
                    "{}: target `{target_name}` declares configurations {names:?} — only \
                     the default configuration was migrated; other configurations need \
                     dedicated bento tasks.",
                    rel_project_root
                ),
            );
        }

        let mapping = map_executor(target_name, &merged);
        if mapping.persistent {
            serve_block = Some((target_name.clone(), mapping.run.clone()));
            continue;
        }
        if mapping.unknown_executor {
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{}: target `{target_name}` uses executor {:?} — emitted as a \
                     `nx run` shim; replace with the underlying CLI invocation \
                     for the cache-key benefit.",
                    rel_project_root, mapping.executor_label,
                ),
            );
        }

        body.push('\n');
        body.push_str(&format!("[tasks.{}]\n", toml_table_key(target_name)));
        body.push_str(&format!("run = {}\n", toml_basic_string(&mapping.run)));

        let outputs = translate_outputs(&merged.outputs, &merged.options, &rel_project_root);
        if !outputs.literals.is_empty() {
            body.push_str(&format!(
                "outputs = {}\n",
                render_string_array(&outputs.literals)
            ));
        }
        for note in outputs.notes {
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{}: target `{target_name}` outputs: {note}",
                    rel_project_root
                ),
            );
        }

        let inputs = translate_inputs(&merged.inputs, &nx.named_inputs, &rel_project_root);
        if !inputs.literals.is_empty() {
            body.push_str(&format!(
                "inputs = {}\n",
                render_string_array(&inputs.literals)
            ));
        }
        for note in inputs.notes {
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{}: target `{target_name}` inputs: {note}",
                    rel_project_root
                ),
            );
        }
    }

    if let Some((target_name, run)) = serve_block {
        body.push('\n');
        body.push_str("# Persistent target migrated from Nx. Bento models long-running\n");
        body.push_str("# servers as the dish-level [serve] block instead of [tasks.<name>].\n");
        body.push_str("# [serve]\n");
        body.push_str(&format!(
            "# run = \"{run}\"  # was: nx target `{target_name}`\n"
        ));
    }

    body
}

/// Merge per-project target with workspace targetDefaults. The
/// per-project value wins; defaults only fill in unset fields.
fn merge_with_defaults(
    target: &NxTarget,
    target_name: &str,
    defaults: &BTreeMap<String, NxTarget>,
) -> NxTarget {
    let by_name = defaults.get(target_name);
    let by_executor = target.executor.as_deref().and_then(|e| defaults.get(e));

    NxTarget {
        executor: target.executor.clone().or_else(|| {
            by_name
                .and_then(|d| d.executor.clone())
                .or_else(|| by_executor.and_then(|d| d.executor.clone()))
        }),
        options: if target.options.is_empty() {
            by_name
                .map(|d| d.options.clone())
                .or_else(|| by_executor.map(|d| d.options.clone()))
                .unwrap_or_default()
        } else {
            target.options.clone()
        },
        outputs: if target.outputs.is_empty() {
            by_name
                .map(|d| d.outputs.clone())
                .or_else(|| by_executor.map(|d| d.outputs.clone()))
                .unwrap_or_default()
        } else {
            target.outputs.clone()
        },
        inputs: if target.inputs.is_empty() {
            by_name
                .map(|d| clone_inputs(&d.inputs))
                .or_else(|| by_executor.map(|d| clone_inputs(&d.inputs)))
                .unwrap_or_default()
        } else {
            clone_inputs(&target.inputs)
        },
        depends_on: if target.depends_on.is_empty() {
            by_name
                .map(|d| clone_depends_on(&d.depends_on))
                .or_else(|| by_executor.map(|d| clone_depends_on(&d.depends_on)))
                .unwrap_or_default()
        } else {
            clone_depends_on(&target.depends_on)
        },
        cache: target.cache.or_else(|| {
            by_name
                .and_then(|d| d.cache)
                .or_else(|| by_executor.and_then(|d| d.cache))
        }),
        configurations: target.configurations.clone(),
        command: target.command.clone(),
        commands: target.commands.as_ref().map(|cs| {
            cs.iter()
                .map(|c| match c {
                    NxCommand::Simple(s) => NxCommand::Simple(s.clone()),
                    NxCommand::Detailed { command } => NxCommand::Detailed {
                        command: command.clone(),
                    },
                })
                .collect()
        }),
    }
}

fn clone_inputs(xs: &[NxInput]) -> Vec<NxInput> {
    xs.iter()
        .map(|i| match i {
            NxInput::Pattern(s) => NxInput::Pattern(s.clone()),
            NxInput::External {
                external_dependencies,
            } => NxInput::External {
                external_dependencies: external_dependencies.clone(),
            },
            NxInput::Env { env } => NxInput::Env { env: env.clone() },
            NxInput::DependentTasks {
                dependent_tasks_output_files,
            } => NxInput::DependentTasks {
                dependent_tasks_output_files: dependent_tasks_output_files.clone(),
            },
            NxInput::Other(v) => NxInput::Other(v.clone()),
        })
        .collect()
}

fn clone_depends_on(xs: &[NxDependsOn]) -> Vec<NxDependsOn> {
    xs.iter()
        .map(|d| match d {
            NxDependsOn::Simple(s) => NxDependsOn::Simple(s.clone()),
            NxDependsOn::Detailed { target } => NxDependsOn::Detailed {
                target: target.clone(),
            },
        })
        .collect()
}

// ── Executor → run command ─────────────────────────────────────────

struct ExecutorMapping {
    run: String,
    persistent: bool,
    unknown_executor: bool,
    executor_label: Option<String>,
}

/// Map an Nx target's executor to a shell command bento can run.
/// Falls back to `nx run <project>:<target>` for unknown executors so
/// the migrated config stays runnable; the user can replace the shim
/// with the native CLI invocation afterwards.
fn map_executor(target_name: &str, target: &NxTarget) -> ExecutorMapping {
    // Strip the legacy @nrwl/ scope so the match table doesn't double up.
    let raw = target.executor.as_deref().unwrap_or("");
    let executor = raw.strip_prefix("@nrwl/").map(|rest| format!("@nx/{rest}"));
    let exec_str: &str = executor.as_deref().unwrap_or(raw);

    match exec_str {
        // Generic command runners — pull the actual command out of options.
        "nx:run-commands" | "@nx/workspace:run-commands" => {
            let run = run_commands_to_shell(target);
            ExecutorMapping {
                run,
                persistent: false,
                unknown_executor: false,
                executor_label: Some(exec_str.to_string()),
            }
        }
        "nx:run-script" | "@nx/workspace:run-script" => {
            let script = target
                .options
                .get("script")
                .and_then(|v| v.as_str())
                .unwrap_or(target_name);
            ExecutorMapping {
                run: format!("npm run {script}"),
                persistent: false,
                unknown_executor: false,
                executor_label: Some(exec_str.to_string()),
            }
        }

        // Build / test / lint executors with a known canonical CLI.
        "@nx/js:tsc" => ExecutorMapping {
            run: "tsc".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/js:swc" => ExecutorMapping {
            run: "swc src -d dist".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/jest:jest" => ExecutorMapping {
            run: "jest".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/vite:build" => ExecutorMapping {
            run: "vite build".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/vite:test" => ExecutorMapping {
            run: "vitest run".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/vite:preview-server" | "@nx/vite:dev-server" => ExecutorMapping {
            run: "vite".to_string(),
            persistent: true,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/webpack:webpack" => ExecutorMapping {
            run: "webpack".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/webpack:dev-server" => ExecutorMapping {
            run: "webpack serve".to_string(),
            persistent: true,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/eslint:lint" | "@nx/linter:eslint" => ExecutorMapping {
            run: "eslint .".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/storybook:build" => ExecutorMapping {
            run: "storybook build".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/storybook:storybook" => ExecutorMapping {
            run: "storybook dev".to_string(),
            persistent: true,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/cypress:cypress" => ExecutorMapping {
            run: "cypress run".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/playwright:playwright" => ExecutorMapping {
            run: "playwright test".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/next:build" => ExecutorMapping {
            run: "next build".to_string(),
            persistent: false,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "@nx/next:server" | "@nx/next:start" => ExecutorMapping {
            run: "next dev".to_string(),
            persistent: true,
            unknown_executor: false,
            executor_label: Some(exec_str.to_string()),
        },
        "" => {
            // No executor declared — Nx requires one in normal use, but
            // some inferred-target shapes carry only `command` /
            // `commands`. Try those before giving up.
            if target.command.is_some() || target.commands.is_some() {
                ExecutorMapping {
                    run: run_commands_to_shell(target),
                    persistent: false,
                    unknown_executor: false,
                    executor_label: None,
                }
            } else {
                ExecutorMapping {
                    run: format!("nx run {{project}}:{target_name}"),
                    persistent: false,
                    unknown_executor: true,
                    executor_label: None,
                }
            }
        }
        other => ExecutorMapping {
            run: format!("nx run {{project}}:{target_name}"),
            persistent: false,
            unknown_executor: true,
            executor_label: Some(other.to_string()),
        },
    }
}

/// Pull the shell command(s) out of an `nx:run-commands` target.
/// Honours the four shapes Nx accepts:
///   { command: "..." }
///   { commands: ["a", "b"] }
///   { commands: [{ command: "a" }, { command: "b" }] }
///   command at top level (rare; kept for older configs)
fn run_commands_to_shell(target: &NxTarget) -> String {
    if let Some(cmd) = target.command.as_deref() {
        return cmd.to_string();
    }
    if let Some(cmd) = target.options.get("command").and_then(|v| v.as_str()) {
        return cmd.to_string();
    }
    if let Some(commands) = &target.commands {
        return join_commands(commands);
    }
    if let Some(commands) = target.options.get("commands").and_then(|v| v.as_array()) {
        let parts: Vec<String> = commands
            .iter()
            .filter_map(|item| {
                item.as_str().map(|s| s.to_string()).or_else(|| {
                    item.get("command")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string())
                })
            })
            .collect();
        if !parts.is_empty() {
            return parts.join(" && ");
        }
    }
    // Nothing usable — leave a placeholder the user must fix.
    "echo 'TODO: migrate Nx run-commands target'".to_string()
}

fn join_commands(commands: &[NxCommand]) -> String {
    commands
        .iter()
        .map(|c| match c {
            NxCommand::Simple(s) => s.clone(),
            NxCommand::Detailed { command } => command.clone(),
        })
        .collect::<Vec<_>>()
        .join(" && ")
}

// ── outputs / inputs translation ───────────────────────────────────

#[derive(Default)]
struct Translated {
    literals: Vec<String>,
    notes: Vec<String>,
}

/// Translate Nx output globs into bento dish-relative outputs.
///
/// Substitutions handled:
///   - `{projectRoot}/X`           → `X` (dish-relative)
///   - `{workspaceRoot}/X/{projectRoot}` → relative path when project
///     prefix is known; else surfaced as a note.
///   - `{options.<key>}`           → resolved from target options when
///     the value is a string; else a note.
fn translate_outputs(
    outputs: &[String],
    options: &BTreeMap<String, serde_json::Value>,
    project_rel: &str,
) -> Translated {
    let mut out = Translated::default();
    for raw in outputs {
        let substituted = substitute_options(raw, options);
        let final_path = strip_project_root(&substituted, project_rel);
        match final_path {
            Ok(p) => out.literals.push(p),
            Err(reason) => out.notes.push(format!("dropped output `{raw}` — {reason}")),
        }
    }
    out
}

/// Translate Nx input set into bento dish-relative input globs.
fn translate_inputs(
    inputs: &[NxInput],
    named_inputs: &BTreeMap<String, Vec<NxInput>>,
    project_rel: &str,
) -> Translated {
    let mut out = Translated::default();
    let mut seen_named: BTreeSet<String> = BTreeSet::new();
    expand_inputs(inputs, named_inputs, project_rel, &mut out, &mut seen_named);
    // Dedupe + preserve order.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    out.literals.retain(|p| seen.insert(p.clone()));
    out
}

fn expand_inputs(
    inputs: &[NxInput],
    named_inputs: &BTreeMap<String, Vec<NxInput>>,
    project_rel: &str,
    out: &mut Translated,
    seen_named: &mut BTreeSet<String>,
) {
    for input in inputs {
        match input {
            NxInput::Pattern(raw) => {
                if let Some(named) = raw.strip_prefix('^') {
                    out.notes.push(format!(
                        "dropped cross-project input `^{named}` — bento derives upstream \
                         dependencies from the dish graph; declare `dish.depends_on` \
                         between dishes if needed."
                    ));
                    continue;
                }
                if !raw.contains('{') && !raw.starts_with('!') {
                    // A bare named-input reference like "production".
                    if named_inputs.contains_key(raw) {
                        if seen_named.insert(raw.clone()) {
                            if let Some(definition) = named_inputs.get(raw) {
                                expand_inputs(
                                    definition,
                                    named_inputs,
                                    project_rel,
                                    out,
                                    seen_named,
                                );
                            }
                        }
                        continue;
                    }
                }
                let stripped = match strip_project_root(raw, project_rel) {
                    Ok(p) => p,
                    Err(reason) => {
                        out.notes.push(format!("dropped input `{raw}` — {reason}"));
                        continue;
                    }
                };
                out.literals.push(stripped);
            }
            NxInput::External {
                external_dependencies,
            } => {
                out.notes.push(format!(
                    "dropped externalDependencies = {external_dependencies:?} — bento \
                     covers npm deps via the package.json + lockfile in the dish's \
                     fingerprint; declare additional pins in dish.toml if needed."
                ));
            }
            NxInput::Env { env } => {
                out.notes.push(format!(
                    "dropped env-var input `${env}` — bento doesn't yet hash env \
                     vars per task; if this var changes the build, surface it via a \
                     `[tasks.<task>] env = [...]` declaration once supported."
                ));
            }
            NxInput::DependentTasks {
                dependent_tasks_output_files,
            } => {
                out.notes.push(format!(
                    "dropped dependentTasksOutputFiles = `{dependent_tasks_output_files}` \
                     — bento derives upstream cache invalidation from the dish graph."
                ));
            }
            NxInput::Other(value) => {
                out.notes.push(format!(
                    "dropped non-string input {value:?} — review by hand"
                ));
            }
        }
    }
}

/// Rewrite a single Nx path/glob to a dish-relative bento path.
/// Honours Nx's leading `!` negation by stripping it, doing the
/// rewrite, and re-attaching it.
fn strip_project_root(raw: &str, project_rel: &str) -> Result<String, String> {
    let (negation, body): (&str, &str) = if let Some(rest) = raw.strip_prefix('!') {
        ("!", rest)
    } else {
        ("", raw)
    };
    let workspace_with_project = format!("{{workspaceRoot}}/{project_rel}/");
    if let Some(rest) = body.strip_prefix(&workspace_with_project) {
        return Ok(format!("{negation}{rest}"));
    }
    if let Some(rest) = body.strip_prefix("{projectRoot}/") {
        return Ok(format!("{negation}{rest}"));
    }
    if body == "{projectRoot}" {
        return Ok(format!("{negation}."));
    }
    if let Some(rest) = body.strip_prefix("{workspaceRoot}/") {
        return Err(format!(
            "workspace-relative path `{rest}` doesn't sit inside this project; \
             bento outputs/inputs are dish-relative"
        ));
    }
    if body.contains('{') {
        return Err(format!(
            "unresolved Nx substitution; replace `{raw}` with the literal dish-relative path"
        ));
    }
    Ok(format!("{negation}{body}"))
}

/// Resolve `{options.<key>}` substitutions in a single output / input
/// glob. Other substitutions (`{projectRoot}` / `{workspaceRoot}`)
/// are left intact for downstream handling in `strip_project_root`.
///
/// Nx convention: an option value that doesn't already carry an
/// explicit `{projectRoot}` or `{workspaceRoot}` prefix is treated as
/// workspace-relative (the canonical example is `outputPath:
/// "dist/apps/web"`). We re-prefix with `{workspaceRoot}/` so the
/// downstream stripper can decide whether the result actually sits
/// inside this project — and if it doesn't, surface a note rather
/// than silently emitting a workspace-shaped path under a dish.
fn substitute_options(raw: &str, options: &BTreeMap<String, serde_json::Value>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    let mut first_token = true;
    while let Some(start) = rest.find("{options.") {
        out.push_str(&rest[..start]);
        let at_path_start = first_token && start == 0;
        let after = &rest[start + "{options.".len()..];
        if let Some(end) = after.find('}') {
            let key = &after[..end];
            if let Some(value) = options.get(key).and_then(|v| v.as_str()) {
                if at_path_start
                    && !value.starts_with("{projectRoot}")
                    && !value.starts_with("{workspaceRoot}")
                {
                    out.push_str("{workspaceRoot}/");
                    out.push_str(value);
                } else {
                    out.push_str(value);
                }
            } else {
                out.push_str("{options.");
                out.push_str(key);
                out.push('}');
            }
            rest = &after[end + 1..];
            first_token = false;
        } else {
            // Malformed substitution — leave intact and bail.
            out.push_str("{options.");
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Strip a leading `@scope/` from a project name so the dish name
/// reads as a clean identifier. `@acme/web` → `web`.
fn infer_short_name(project_name: &str) -> String {
    project_name
        .rsplit_once('/')
        .map(|(_, last)| last.to_string())
        .unwrap_or_else(|| project_name.to_string())
}

fn render_string_array(xs: &[String]) -> String {
    let mut s = String::from("[");
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&toml_basic_string(x));
    }
    s.push(']');
    s
}

// ── Helpers ────────────────────────────────────────────────────────

fn parse_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let body = fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))?;
    let parsed = serde_json::from_str(&body)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    Ok(parsed)
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

    fn write(path: PathBuf, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            root.join("nx.json"),
            r#"{
                "targetDefaults": {
                    "build": {
                        "cache": true,
                        "dependsOn": ["^build"],
                        "inputs": ["production", "^production"],
                        "outputs": ["{options.outputPath}"]
                    },
                    "@nx/jest:jest": {
                        "cache": true,
                        "inputs": ["default", "^production"]
                    }
                },
                "namedInputs": {
                    "default": ["{projectRoot}/**/*"],
                    "production": ["default", "!{projectRoot}/**/*.spec.ts"],
                    "sharedGlobals": []
                },
                "workspaceLayout": { "appsDir": "apps", "libsDir": "libs" }
            }"#,
        );
        write(
            root.join("apps/web/project.json"),
            r#"{
                "name": "web",
                "projectType": "application",
                "targets": {
                    "build": {
                        "executor": "@nx/vite:build",
                        "options": { "outputPath": "dist/apps/web" }
                    },
                    "serve": {
                        "executor": "@nx/vite:dev-server",
                        "options": { "buildTarget": "web:build" }
                    },
                    "test": {
                        "executor": "@nx/jest:jest",
                        "outputs": ["{workspaceRoot}/coverage/apps/web"],
                        "options": { "jestConfig": "apps/web/jest.config.ts" }
                    },
                    "custom": {
                        "executor": "nx:run-commands",
                        "options": { "command": "echo hello && echo bye" }
                    }
                }
            }"#,
        );
        write(
            root.join("libs/util/project.json"),
            r#"{
                "name": "@acme/util",
                "projectType": "library",
                "targets": {
                    "build": {
                        "executor": "@nx/js:tsc",
                        "options": { "outputPath": "{projectRoot}/dist" }
                    },
                    "lint": {
                        "executor": "@nx/eslint:lint",
                        "configurations": {
                            "production": { "fix": false }
                        }
                    }
                }
            }"#,
        );
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
        assert!(written.contains(&PathBuf::from("libs/util/dish.toml")));
        assert!(written.contains(&PathBuf::from("bento.toml")));
        assert!(written.contains(&PathBuf::from("bentos/prod.toml")));
        assert!(report.applied);

        let web = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(web.contains(r#"name = "web""#));
        assert!(web.contains("[tasks.build]"));
        assert!(web.contains(r#"run = "vite build""#));
        // web.build's outputPath ("dist/apps/web") is workspace-relative
        // per Nx convention; bento outputs are dish-relative so it
        // surfaces as a note (no `outputs = ` line under web.build).
        assert!(!web.contains("outputs = "));
        // serve target is persistent → no [tasks.serve], serve template instead.
        assert!(!web.contains("[tasks.serve]"));
        assert!(web.contains("[serve]"));
        // Custom run-commands target with options.command is honoured.
        assert!(web.contains("[tasks.custom]"));
        assert!(web.contains(r#"run = "echo hello && echo bye""#));
        // jest target landed.
        assert!(web.contains("[tasks.test]"));
        assert!(web.contains(r#"run = "jest""#));

        let util = std::fs::read_to_string(tmp.path().join("libs/util/dish.toml")).unwrap();
        assert!(util.contains(r#"name = "util""#));
        assert!(util.contains("[tasks.build]"));
        assert!(util.contains(r#"run = "tsc""#));
        // util.build uses {projectRoot}/dist — translates cleanly.
        assert!(util.contains(r#"outputs = ["dist"]"#));
        assert!(util.contains("[tasks.lint]"));

        // Report should carry an Inferred note about web.build's
        // workspace-relative outputPath that didn't translate.
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Inferred && n.message.contains("dist/apps/web")),
            "expected an Inferred note about web.build's workspace-relative output; \
             got {:?}",
            report.notes
        );

        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("apps/web"));
        assert!(prod.contains("libs/util"));
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
    }

    #[test]
    fn surfaces_dependson_cache_false_and_configurations_as_notes() {
        let tmp = fixture();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let kinds: BTreeSet<_> = report.notes.iter().map(|n| n.kind).collect();
        // dependsOn from targetDefaults.build → Inferred note.
        assert!(
            kinds.contains(&NoteKind::Inferred),
            "dependsOn should produce Inferred notes; got {kinds:?}"
        );
        // libs/util.lint has configurations → NotYetImplemented note.
        assert!(
            kinds.contains(&NoteKind::NotYetImplemented),
            "configurations should produce NotYetImplemented notes; got {kinds:?}"
        );
    }

    #[test]
    fn missing_nx_json_is_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path().join("apps/web/project.json"),
            r#"{
                "name": "web",
                "targets": {
                    "build": {
                        "executor": "nx:run-commands",
                        "options": { "command": "echo hi" }
                    }
                }
            }"#,
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(tmp.path().join("apps/web/dish.toml").exists());
        let kinds: BTreeSet<_> = report.notes.iter().map(|n| n.kind).collect();
        assert!(
            kinds.contains(&NoteKind::Inferred),
            "missing nx.json should produce an Inferred note"
        );
    }

    #[test]
    fn unknown_executor_falls_back_to_nx_run_shim() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path().join("nx.json"), r#"{}"#);
        write(
            tmp.path().join("apps/special/project.json"),
            r#"{
                "name": "special",
                "targets": {
                    "package": {
                        "executor": "@my-org/some-plugin:bundle"
                    }
                }
            }"#,
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("apps/special/dish.toml")).unwrap();
        assert!(dish.contains("[tasks.package]"));
        assert!(dish.contains(r#"run = "nx run {project}:package""#));
        let inferred: Vec<_> = report
            .notes
            .iter()
            .filter(|n| n.kind == NoteKind::Inferred)
            .collect();
        assert!(
            inferred
                .iter()
                .any(|n| n.message.contains("@my-org/some-plugin:bundle")),
            "should warn about the unknown executor; got {:?}",
            report.notes
        );
    }

    #[test]
    fn nrwl_legacy_scope_maps_like_nx() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path().join("nx.json"), r#"{}"#);
        write(
            tmp.path().join("apps/legacy/project.json"),
            r#"{
                "name": "legacy",
                "targets": {
                    "test": { "executor": "@nrwl/jest:jest" }
                }
            }"#,
        );
        run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("apps/legacy/dish.toml")).unwrap();
        assert!(dish.contains(r#"run = "jest""#));
    }

    #[test]
    fn run_commands_with_array_form_joins_with_amp_amp() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path().join("nx.json"), r#"{}"#);
        write(
            tmp.path().join("apps/multi/project.json"),
            r#"{
                "name": "multi",
                "targets": {
                    "release": {
                        "executor": "nx:run-commands",
                        "options": { "commands": ["bump", "publish"] }
                    }
                }
            }"#,
        );
        run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("apps/multi/dish.toml")).unwrap();
        assert!(dish.contains(r#"run = "bump && publish""#));
    }

    #[test]
    fn workspace_layout_caps_discovery() {
        // project.json under a non-listed directory must NOT be picked up.
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path().join("nx.json"),
            r#"{ "workspaceLayout": { "appsDir": "apps", "libsDir": "libs" } }"#,
        );
        write(
            tmp.path().join("apps/web/project.json"),
            r#"{ "name": "web", "targets": { "build": { "executor": "@nx/js:tsc" } } }"#,
        );
        write(
            tmp.path().join("packages/extra/project.json"),
            r#"{ "name": "extra", "targets": {} }"#,
        );
        run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(tmp.path().join("apps/web/dish.toml").exists());
        assert!(!tmp.path().join("packages/extra/dish.toml").exists());
    }

    #[test]
    fn skips_node_modules_when_walking() {
        // A nested package under node_modules with a project.json must
        // not be treated as a real project.
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path().join("nx.json"), r#"{}"#);
        write(
            tmp.path().join("apps/web/project.json"),
            r#"{ "name": "web", "targets": {} }"#,
        );
        write(
            tmp.path().join("node_modules/some-pkg/nested/project.json"),
            r#"{ "name": "nope", "targets": {} }"#,
        );
        run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(tmp.path().join("apps/web/dish.toml").exists());
        assert!(!tmp
            .path()
            .join("node_modules/some-pkg/nested/dish.toml")
            .exists());
    }

    #[test]
    fn options_substitution_resolves_output_path() {
        let mut options = BTreeMap::new();
        options.insert(
            "outputPath".to_string(),
            serde_json::Value::String("dist/apps/web".to_string()),
        );
        let translated =
            translate_outputs(&["{options.outputPath}".to_string()], &options, "apps/web");
        // dist/apps/web is not under apps/web/ so it can't be made
        // dish-relative — should surface as a note instead of a literal.
        assert!(translated.literals.is_empty());
        assert_eq!(translated.notes.len(), 1);
        assert!(translated.notes[0].contains("dist/apps/web"));
    }

    #[test]
    fn options_substitution_keeps_project_relative_outputs() {
        let mut options = BTreeMap::new();
        options.insert(
            "outputPath".to_string(),
            serde_json::Value::String("{projectRoot}/dist".to_string()),
        );
        let translated =
            translate_outputs(&["{options.outputPath}".to_string()], &options, "apps/web");
        assert_eq!(translated.literals, vec!["dist".to_string()]);
        assert!(translated.notes.is_empty());
    }

    #[test]
    fn named_inputs_expand_to_globs() {
        let mut named: BTreeMap<String, Vec<NxInput>> = BTreeMap::new();
        named.insert(
            "default".to_string(),
            vec![NxInput::Pattern("{projectRoot}/**/*".to_string())],
        );
        named.insert(
            "production".to_string(),
            vec![
                NxInput::Pattern("default".to_string()),
                NxInput::Pattern("!{projectRoot}/**/*.spec.ts".to_string()),
            ],
        );
        let translated = translate_inputs(
            &[NxInput::Pattern("production".to_string())],
            &named,
            "apps/web",
        );
        assert!(translated.literals.iter().any(|s| s == "**/*"));
        assert!(translated.literals.iter().any(|s| s == "!**/*.spec.ts"));
    }

    #[test]
    fn cross_project_input_marker_is_dropped_with_note() {
        let translated = translate_inputs(
            &[NxInput::Pattern("^production".to_string())],
            &BTreeMap::new(),
            "apps/web",
        );
        assert!(translated.literals.is_empty());
        assert_eq!(translated.notes.len(), 1);
        assert!(translated.notes[0].contains("^production"));
    }

    #[test]
    fn target_defaults_inputs_inherit_when_target_has_none() {
        let mut defaults: BTreeMap<String, NxTarget> = BTreeMap::new();
        let build_default = NxTarget {
            outputs: vec!["{projectRoot}/dist".to_string()],
            ..Default::default()
        };
        defaults.insert("build".to_string(), build_default);

        let target = NxTarget {
            executor: Some("@nx/js:tsc".to_string()),
            ..Default::default()
        };
        let merged = merge_with_defaults(&target, "build", &defaults);
        assert_eq!(merged.outputs, vec!["{projectRoot}/dist".to_string()]);
    }

    #[test]
    fn infer_short_name_strips_scope() {
        assert_eq!(infer_short_name("@acme/web"), "web");
        assert_eq!(infer_short_name("just-a-name"), "just-a-name");
        assert_eq!(infer_short_name("@acme/very/deep"), "deep");
    }

    #[test]
    fn substitute_options_handles_repeated_keys() {
        let mut options = BTreeMap::new();
        options.insert(
            "outputPath".to_string(),
            serde_json::Value::String("dist".to_string()),
        );
        options.insert(
            "name".to_string(),
            serde_json::Value::String("web".to_string()),
        );
        let out = substitute_options(
            "{options.outputPath}/{options.name}/{options.outputPath}",
            &options,
        );
        // Only the leading {options.X} reference is treated as a path
        // root and gets the {workspaceRoot}/ prefix; subsequent
        // substitutions are mid-path components.
        assert_eq!(out, "{workspaceRoot}/dist/web/dist");
    }

    #[test]
    fn substitute_options_at_path_start_rooted_value_is_left_alone() {
        // When the resolved value already carries an explicit
        // {projectRoot}/{workspaceRoot} prefix, no re-prefixing.
        let mut options = BTreeMap::new();
        options.insert(
            "outputPath".to_string(),
            serde_json::Value::String("{projectRoot}/dist".to_string()),
        );
        let out = substitute_options("{options.outputPath}", &options);
        assert_eq!(out, "{projectRoot}/dist");
    }

    #[test]
    fn substitute_options_leaves_unknown_keys_intact() {
        let options = BTreeMap::new();
        let out = substitute_options("{options.missingKey}", &options);
        assert_eq!(out, "{options.missingKey}");
    }
}
