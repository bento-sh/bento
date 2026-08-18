//! Moonrepo → bento migrator.
//!
//! Reads root `.moon/workspace.yml` to discover project directories,
//! then walks each project's `moon.yml` for its `language` and `tasks`.
//! Emits a starter bento config the user can iterate on.
//!
//! ## What translates cleanly
//!
//! | Moon                                | Bento                                          |
//! |-------------------------------------|------------------------------------------------|
//! | `tasks.build.command` (+ `args`)    | `dish.toml [tasks.build] run = "<cmd> <args>"` |
//! | `tasks.build.inputs`                | `dish.toml [tasks.build] inputs = [...]`       |
//! | `tasks.build.outputs`               | `dish.toml [tasks.build] outputs = [...]`      |
//! | `tasks.build.options.cache: false`  | `dish.toml [tasks.build] cache = false`        |
//! | top-level `language: typescript`    | `dish.toml language = "node-npm"` (+ note)     |
//! | top-level `language: rust`          | `dish.toml language = "cargo"`                 |
//! | `projects:` array of globs          | recursive walk of each glob                    |
//! | `projects:` object map (id → path)  | direct-path lookup of each value               |
//!
//! ## What gets a note instead
//!
//! - `tasks.<name>.deps` arrays — bento derives ordering from the dish
//!   graph (`dish.depends_on`), not per-task within a dish; `^:build` /
//!   cross-project refs surface as `Inferred` notes.
//! - Toolchain blocks (`node:`, `rust:`, `python:` at workspace.yml top
//!   level) — bento has its own `[toolchain]` block in `bento.toml`;
//!   surfaced as `Inferred` so the user can copy versions across.
//! - Unknown languages — `language` detected from the package manager
//!   for node-family projects, `node-npm` placeholder otherwise + a note.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{node_pm, render_task, resolve_glob, Emitter, MigrationReport, NoteKind, TaskBlock};

// ── moon config (subset we care about) ─────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkspaceYml {
    projects: Option<Projects>,
    /// Everything else at the top level — scanned for toolchain blocks.
    #[serde(flatten)]
    rest: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Projects {
    Globs(Vec<String>),
    Map(BTreeMap<String, String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MoonYml {
    language: Option<String>,
    tasks: BTreeMap<String, MoonTask>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MoonTask {
    command: Option<StringOrList>,
    args: Option<StringOrList>,
    deps: Vec<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    options: MoonTaskOptions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MoonTaskOptions {
    cache: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn joined(v: &Option<Self>) -> String {
        match v {
            Some(StringOrList::One(s)) => s.clone(),
            Some(StringOrList::Many(xs)) => xs.join(" "),
            None => String::new(),
        }
    }
}

const TOOLCHAIN_KEYS: &[&str] = &[
    "node",
    "rust",
    "python",
    "deno",
    "bun",
    "go",
    "ruby",
    "php",
    "typescript",
];

// ── Public entry point ─────────────────────────────────────────────

pub fn run(mut e: Emitter) -> Result<MigrationReport> {
    let root = e.root().to_path_buf();

    // 1. Load .moon/workspace.yml.
    let ws_path = root.join(".moon").join("workspace.yml");
    let ws: WorkspaceYml = parse_yaml_file(&ws_path)?;

    // 2. Surface toolchain blocks as `Inferred` notes — bento has its
    //    own [toolchain] block in bento.toml; we don't auto-port versions.
    for tool in TOOLCHAIN_KEYS {
        let Some(block) = ws.rest.get(*tool) else {
            continue;
        };
        let extra = match block.get("version").and_then(|v| v.as_str()) {
            Some(v) => format!(" (version: {v})"),
            None => String::new(),
        };
        e.note(
            NoteKind::Inferred,
            format!(
                "workspace.yml has a `{tool}:` toolchain block{extra} — bento uses its \
                 own `[toolchain]` block in bento.toml; copy the version across by hand."
            ),
        );
    }

    // 3. Resolve `projects:` — array of globs or object map.
    let project_dirs = match &ws.projects {
        Some(Projects::Globs(globs)) => discover_via_globs(&root, globs)?,
        Some(Projects::Map(map)) => {
            let mut out: Vec<PathBuf> = map
                .values()
                .map(|rel| root.join(rel))
                .filter(|dir| dir.is_dir())
                .collect();
            out.sort();
            out
        }
        None => Vec::new(),
    };

    if project_dirs.is_empty() {
        e.note(
            NoteKind::Skipped,
            "workspace.yml has no `projects:` entries (or none resolved to a dir) — \
             nothing to migrate",
        );
        return e.finish();
    }

    // 4. For each project dir with a moon.yml, emit a dish.toml.
    for dir in &project_dirs {
        let moon_yml = dir.join("moon.yml");
        if !moon_yml.exists() {
            continue;
        }
        let project: MoonYml = parse_yaml_file(&moon_yml)?;
        let body = render_dish_toml(dir, &root, &project, &mut e);
        e.dish(dir, &body)?;
    }

    e.finish()
}

fn parse_yaml_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing {} as YAML", path.display()))
}

// ── Project discovery ──────────────────────────────────────────────

fn discover_via_globs(root: &Path, globs: &[String]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for g in globs {
        for dir in resolve_glob(root, g)? {
            if dir.join("moon.yml").exists() {
                out.push(dir);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

// ── dish.toml renderer ─────────────────────────────────────────────

fn render_dish_toml(dir: &Path, root: &Path, project: &MoonYml, e: &mut Emitter) -> String {
    let dish_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dish")
        .to_string();
    let rel = e.rel(dir);

    let language_id = match project.language.as_deref() {
        Some("typescript") | Some("javascript") | Some("node") => node_pm(dir, root).language,
        Some("rust") => "cargo",
        Some("go") => "go",
        Some("python") => "python",
        Some("ruby") => "ruby",
        Some("php") => "php",
        Some(other) => {
            e.note(
                NoteKind::Inferred,
                format!(
                    "{rel} → unknown moon language `{other}` — defaulted dish.toml `language = \
                     \"node-npm\"`; edit by hand if your project uses a different toolchain."
                ),
            );
            "node-npm"
        }
        None => {
            e.note(
                NoteKind::Inferred,
                format!(
                    "{rel} → moon.yml has no `language` field — defaulted to `node-npm`; edit \
                     by hand to match your project's toolchain."
                ),
            );
            "node-npm"
        }
    };

    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{language_id}\"\n\
         \n\
         # Migrated from moon.yml. Each [tasks.<name>] mirrors the moon\n\
         # task with the same name. Review inputs / outputs against your\n\
         # build artefacts.\n",
    );

    for (task_name, task) in &project.tasks {
        let cmd = StringOrList::joined(&task.command);
        let args = StringOrList::joined(&task.args);
        let run_str = match (cmd.is_empty(), args.is_empty()) {
            (true, true) => continue, // nothing to run, skip silently
            (true, false) => args,
            (false, true) => cmd,
            (false, false) => format!("{cmd} {args}"),
        };

        if !task.deps.is_empty() {
            e.note(
                NoteKind::Inferred,
                format!(
                    "{rel}: task `{task_name}` had deps = {:?} — bento derives task \
                     ordering from the dish graph; cross-project refs (`^:<task>`, \
                     `<project>:<task>`) map to dish.toml `depends_on` between dishes \
                     (wire by hand).",
                    task.deps
                ),
            );
        }

        body.push_str(&render_task(
            task_name,
            &TaskBlock {
                run: &run_str,
                inputs: &task.inputs,
                outputs: &task.outputs,
                // A moon task IS the repo's declared CI surface; a
                // custom name would otherwise be `bento run`-only.
                ci: true,
                no_cache: task.options.cache == Some(false),
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

    fn migrate(root: &Path, dry_run: bool, force: bool) -> MigrationReport {
        run(Emitter::new(Options {
            root: root.to_path_buf(),
            dry_run,
            force,
        }))
        .unwrap()
    }

    fn write_workspace(root: &Path, body: &str) {
        let dir = root.join(".moon");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workspace.yml"), body).unwrap();
    }

    fn write_moon_yml(root: &Path, rel: &str, body: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("moon.yml"), body).unwrap();
    }

    fn fixture_two_projects() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_workspace(root, "projects:\n  - \"apps/*\"\n");
        write_moon_yml(
            root,
            "apps/web",
            "language: typescript\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: vite build\n\
             \x20\x20\x20\x20outputs:\n\
             \x20\x20\x20\x20\x20\x20- \"dist/**\"\n\
             \x20\x20test:\n\
             \x20\x20\x20\x20command: vitest run\n\
             \x20\x20e2e:\n\
             \x20\x20\x20\x20command: playwright test\n\
             \x20\x20\x20\x20options:\n\
             \x20\x20\x20\x20\x20\x20cache: false\n",
        );
        write_moon_yml(
            root,
            "apps/api",
            "language: rust\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: cargo build\n\
             \x20\x20test:\n\
             \x20\x20\x20\x20command: cargo test\n",
        );
        tmp
    }

    #[test]
    fn migrates_workspace_with_two_projects() {
        let tmp = fixture_two_projects();
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
        assert!(web_dish.contains(r#"language = "node-npm""#));
        assert!(web_dish.contains("[tasks.build]"));
        assert!(web_dish.contains(r#"run = "vite build""#));
        assert!(web_dish.contains(r#"outputs = ["dist/**"]"#));
        assert!(web_dish.contains("[tasks.test]"));
        assert!(web_dish.contains(r#"run = "vitest run""#));

        let api_dish = std::fs::read_to_string(tmp.path().join("apps/api/dish.toml")).unwrap();
        assert!(api_dish.contains(r#"language = "cargo""#));
        assert!(api_dish.contains(r#"run = "cargo build""#));

        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("apps/api"));
        assert!(prod.contains("apps/web"));
    }

    #[test]
    fn translates_options_cache_false_and_marks_custom_tasks_for_ci() {
        let tmp = fixture_two_projects();
        migrate(tmp.path(), false, false);
        let web = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        let e2e = web.split("[tasks.e2e]").nth(1).unwrap();
        assert!(e2e.contains("cache = false"), "{web}");
        assert!(e2e.contains("ci = true"), "{web}");
        let build = web.split("[tasks.build]").nth(1).unwrap();
        assert!(!build.split("[tasks.").next().unwrap().contains("ci = true"));
    }

    #[test]
    fn conflicting_dish_toml_keeps_the_project_in_prod_toml() {
        let tmp = fixture_two_projects();
        std::fs::write(
            tmp.path().join("apps/web/dish.toml"),
            "name = \"existing\"\n",
        )
        .unwrap();
        let report = migrate(tmp.path(), false, false);
        assert!(report.has_conflicts());
        // The existing dish.toml stays untouched.
        let body = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert_eq!(body, "name = \"existing\"\n");
        // The fresh project still gets written.
        assert!(tmp.path().join("apps/api/dish.toml").exists());
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("apps/web"), "{prod}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = fixture_two_projects();
        let report = migrate(tmp.path(), true, false);
        assert!(!report.applied);
        assert!(!report.files_written.is_empty());
        assert!(!tmp.path().join("apps/web/dish.toml").exists());
        assert!(!tmp.path().join("bento.toml").exists());
    }

    #[test]
    fn node_language_follows_the_workspace_package_manager() {
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(tmp.path(), "projects:\n  - \"apps/*\"\n");
        write_moon_yml(
            tmp.path(),
            "apps/web",
            "language: typescript\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: tsc\n",
        );
        std::fs::write(
            tmp.path().join("apps/web/package.json"),
            r#"{ "name": "web", "packageManager": "pnpm@8.10.0" }"#,
        )
        .unwrap();
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-pnpm""#), "{dish}");
    }

    #[test]
    fn maps_language_rust_to_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(tmp.path(), "projects:\n  - \"crates/*\"\n");
        write_moon_yml(
            tmp.path(),
            "crates/core",
            "language: rust\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: cargo build\n",
        );
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("crates/core/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "cargo""#));
    }

    #[test]
    fn surfaces_cross_project_deps_as_notes() {
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(tmp.path(), "projects:\n  - \"apps/*\"\n");
        write_moon_yml(
            tmp.path(),
            "apps/web",
            "language: typescript\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: vite build\n\
             \x20\x20\x20\x20deps:\n\
             \x20\x20\x20\x20\x20\x20- \"^:build\"\n",
        );
        let report = migrate(tmp.path(), true, false);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Inferred && n.message.contains("^:build")),
            "expected an Inferred note about cross-project deps"
        );
    }

    #[test]
    fn surfaces_toolchain_blocks_as_notes() {
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(
            tmp.path(),
            "projects:\n  - \"apps/*\"\n\
             node:\n  version: \"20.0.0\"\n",
        );
        write_moon_yml(
            tmp.path(),
            "apps/web",
            "language: typescript\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: tsc\n",
        );
        let report = migrate(tmp.path(), true, false);
        assert!(
            report.notes.iter().any(|n| n.kind == NoteKind::Inferred
                && n.message.contains("node:")
                && n.message.contains("20.0.0")
                && n.message.contains("[toolchain]")),
            "expected an Inferred note pointing at [toolchain]; got {:?}",
            report.notes
        );
    }

    #[test]
    fn parses_object_form_projects_field() {
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(
            tmp.path(),
            "projects:\n\
             \x20\x20web: apps/web\n\
             \x20\x20api: apps/api\n",
        );
        write_moon_yml(
            tmp.path(),
            "apps/web",
            "language: typescript\ntasks:\n  build:\n    command: tsc\n",
        );
        write_moon_yml(
            tmp.path(),
            "apps/api",
            "language: rust\ntasks:\n  build:\n    command: cargo build\n",
        );
        let report = migrate(tmp.path(), false, false);
        let written: Vec<_> = report
            .files_written
            .iter()
            .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
            .collect();
        assert!(written.contains(&PathBuf::from("apps/web/dish.toml")));
        assert!(written.contains(&PathBuf::from("apps/api/dish.toml")));
    }

    #[test]
    fn handles_yaml_shapes_the_hand_rolled_parser_could_not() {
        // Anchors, block scalars, and flow maps are plain YAML that the
        // previous bespoke parser rejected or mangled.
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(
            tmp.path(),
            "projects: [\"apps/*\"]\nnode: { version: \"20\" }\n",
        );
        write_moon_yml(
            tmp.path(),
            "apps/web",
            "language: typescript\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command:\n\
             \x20\x20\x20\x20\x20\x20- vitest\n\
             \x20\x20\x20\x20\x20\x20- run\n\
             \x20\x20\x20\x20inputs: [\"src/**/*\", \"package.json\"]\n",
        );
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"run = "vitest run""#), "{dish}");
        assert!(
            dish.contains(r#"inputs = ["src/**/*", "package.json"]"#),
            "{dish}"
        );
    }
}
