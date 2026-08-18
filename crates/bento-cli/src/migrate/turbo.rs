//! Turborepo → bento migrator.
//!
//! Reads root `turbo.json` (v2 `tasks` or v1 `pipeline`) plus the root
//! `package.json` to discover packages via `workspaces` (npm/yarn/pnpm
//! glob syntax). Emits a starter bento config the user can iterate on.
//!
//! ## What translates cleanly
//!
//! | Turbo                       | Bento                                    |
//! |-----------------------------|------------------------------------------|
//! | `tasks.build.outputs`       | `dish.toml [tasks.build] outputs = ...`  |
//! | `tasks.build.inputs`        | `dish.toml [tasks.build] inputs = ...`   |
//! | `tasks.build.cache: false`  | `dish.toml [tasks.build] cache = false`  |
//! | top-level `tasks.build`     | per-package `[tasks.build]` with the     |
//! |                             | matching `package.json` script, plus     |
//! |                             | `ci = true` for non-lifecycle names      |
//!
//! ## What gets a note instead
//!
//! - `dependsOn` arrays — bento derives task ordering from the dish
//!   graph rather than per-task `dependsOn`. Cross-package `^build`
//!   maps to bento's automatic upstream rebuild via `dish.depends_on`,
//!   which the user wires by hand (we don't auto-derive from
//!   `package.json` `dependencies` yet).
//! - `persistent: true` — usually `dev` / `serve` tasks; surfaced as
//!   a note recommending the dish-level `[serve]` block instead.
//! - Per-package `turbo.json` overrides — detected, listed, but the
//!   per-package overrides aren't merged in (rare in practice).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{
    discover_workspace_packages, node_pm, parse_jsonc_file, render_task, short_name,
    DiscoveredPackage, Emitter, MigrationReport, NoteKind, PackageJson, TaskBlock,
};

// ── Turbo config (subset we care about) ────────────────────────────

#[derive(Debug, Deserialize)]
struct TurboJson {
    /// v2 schema (`turbo.json` >= 2.0).
    #[serde(default)]
    tasks: Option<BTreeMap<String, TurboTask>>,
    /// v1 schema. Same shape; named `pipeline` instead of `tasks`.
    #[serde(default)]
    pipeline: Option<BTreeMap<String, TurboTask>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TurboTask {
    #[serde(rename = "dependsOn")]
    depends_on: Vec<String>,
    outputs: Vec<String>,
    inputs: Vec<String>,
    cache: Option<bool>,
    persistent: Option<bool>,
}

// ── Public entry point ─────────────────────────────────────────────

pub fn run(mut e: Emitter) -> Result<MigrationReport> {
    let root = e.root().to_path_buf();

    // 1. Load root turbo.json.
    let turbo_path = root.join("turbo.json");
    let turbo: TurboJson = parse_jsonc_file(&turbo_path)
        .with_context(|| format!("reading {}", turbo_path.display()))?;
    let turbo_tasks = turbo.tasks.or(turbo.pipeline).unwrap_or_default();
    if turbo_tasks.is_empty() {
        e.note(
            NoteKind::Skipped,
            "turbo.json has no tasks/pipeline — nothing to migrate",
        );
        return e.finish();
    }

    // 2. Annotate any tasks whose semantics we won't faithfully port.
    for (name, t) in &turbo_tasks {
        if t.persistent == Some(true) {
            e.note(
                NoteKind::Skipped,
                format!(
                    "task `{name}` is persistent (likely a dev server) — model this as the \
                     dish-level `[serve]` block in dish.toml instead of `[tasks.{name}]`."
                ),
            );
        }
        if !t.depends_on.is_empty() {
            e.note(
                NoteKind::Inferred,
                format!(
                    "task `{name}` had dependsOn = {:?} — bento derives task ordering from the \
                     dish graph; cross-package `^build` maps to dish.toml `depends_on` between \
                     dishes (not auto-derived from package.json — wire by hand).",
                    t.depends_on,
                ),
            );
        }
    }

    // 3. Load root package.json + discover packages.
    let root_pkg_path = root.join("package.json");
    let root_pkg: PackageJson = parse_jsonc_file(&root_pkg_path)
        .with_context(|| format!("reading {}", root_pkg_path.display()))?;

    let workspace_globs = root_pkg.workspace_globs();
    let packages = if workspace_globs.is_empty() {
        // Single-package repo. The root IS the only package.
        vec![DiscoveredPackage {
            dir: root.clone(),
            rel_dir: PathBuf::from("."),
            pkg: root_pkg,
        }]
    } else {
        discover_workspace_packages(&root, &workspace_globs)?
    };

    if packages.is_empty() {
        e.note(
            NoteKind::Skipped,
            format!("no packages matched workspaces globs: {workspace_globs:?} — nothing to write"),
        );
        return e.finish();
    }

    // 4. Detect per-package turbo.json overrides (informational only).
    for p in &packages {
        if p.dir.join("turbo.json").exists() && p.rel_dir != Path::new(".") {
            e.note(
                NoteKind::NotYetImplemented,
                format!(
                    "{} has its own turbo.json — per-package overrides aren't merged \
                     yet; review and hand-port any task tweaks.",
                    p.rel_dir.display()
                ),
            );
        }
    }

    // 5. Emit per-package dish.toml.
    for pkg in &packages {
        let body = render_dish_toml(pkg, &root, &turbo_tasks);
        e.dish(&pkg.dir, &body)?;
    }

    e.finish()
}

// ── dish.toml renderer (turbo-aware) ───────────────────────────────

fn render_dish_toml(
    pkg: &DiscoveredPackage,
    root: &Path,
    turbo_tasks: &BTreeMap<String, TurboTask>,
) -> String {
    let dish_name = pkg
        .pkg
        .name
        .as_deref()
        .map(short_name)
        .or_else(|| {
            pkg.dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "dish".to_string());
    let pm = node_pm(&pkg.dir, root);

    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{lang}\"\n\
         \n\
         # Migrated from turbo.json. Each [tasks.<name>] mirrors the\n\
         # turbo task with the same name + the matching package.json\n\
         # script. Review outputs / inputs against your build artefacts.\n",
        lang = pm.language,
    );

    // Emit a [tasks.<name>] for every turbo task whose name matches a
    // package.json script. Skip persistent tasks (they belong in
    // [serve], surfaced as a note in the report).
    for (task_name, turbo_task) in turbo_tasks {
        if turbo_task.persistent == Some(true) || !pkg.pkg.scripts.contains_key(task_name) {
            continue;
        }
        body.push_str(&render_task(
            task_name,
            &TaskBlock {
                run: &format!("{} {task_name}", pm.run_prefix),
                inputs: &turbo_task.inputs,
                outputs: &turbo_task.outputs,
                // A turbo pipeline entry IS the repo's CI surface; a
                // custom name (`typecheck`, `e2e`) would otherwise be
                // `bento run`-only and drop out of `bento ci` silently.
                ci: true,
                no_cache: turbo_task.cache == Some(false),
            },
        ));
    }

    // If the package has a persistent task (e.g. `dev`), drop a
    // commented [serve] template so the user knows where it goes.
    if let Some((dev_name, dev_script)) = persistent_dev(pkg, turbo_tasks) {
        body.push('\n');
        body.push_str("# Persistent task migrated from turbo. bento models long-running\n");
        body.push_str("# servers as the dish-level [serve] block instead of [tasks.<name>].\n");
        body.push_str("# [serve]\n");
        body.push_str(&format!(
            "# run = \"{} {dev_name}\"  # was: {dev_script}\n",
            pm.run_prefix
        ));
    }

    body
}

fn persistent_dev<'a>(
    pkg: &'a DiscoveredPackage,
    turbo_tasks: &'a BTreeMap<String, TurboTask>,
) -> Option<(&'a str, &'a str)> {
    for (name, t) in turbo_tasks {
        if t.persistent == Some(true) {
            if let Some(script) = pkg.pkg.scripts.get(name) {
                return Some((name.as_str(), script.as_str()));
            }
        }
    }
    None
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

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("turbo.json"),
            r#"{
                // v2 schema, with the JSONC comments turbo.json officially allows.
                "$schema": "https://turbo.build/schema.json",
                "tasks": {
                    "build": {
                        "dependsOn": ["^build"],
                        "outputs": ["dist/**"]
                    },
                    "test": {
                        "dependsOn": ["build"]
                    },
                    "typecheck": {
                        "cache": false
                    },
                    "dev": {
                        "cache": false,
                        "persistent": true
                    }
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{
                "name": "monorepo",
                "private": true,
                "packageManager": "pnpm@8.10.0",
                "workspaces": ["packages/*"]
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/web")).unwrap();
        std::fs::write(
            root.join("packages/web/package.json"),
            r#"{
                "name": "@acme/web",
                "scripts": {
                    "build": "vite build",
                    "test": "vitest run",
                    "typecheck": "tsc --noEmit",
                    "dev": "vite"
                }
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/api")).unwrap();
        std::fs::write(
            root.join("packages/api/package.json"),
            r#"{
                "name": "@acme/api",
                "scripts": {
                    "build": "tsc",
                    "test": "jest"
                }
            }"#,
        )
        .unwrap();
        tmp
    }

    #[test]
    fn migrates_workspace_with_two_packages() {
        let tmp = fixture();
        let report = migrate(tmp.path(), false, false);

        let written: Vec<_> = report
            .files_written
            .iter()
            .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
            .collect();
        assert!(written.contains(&PathBuf::from("packages/web/dish.toml")));
        assert!(written.contains(&PathBuf::from("packages/api/dish.toml")));
        assert!(written.contains(&PathBuf::from("bento.toml")));
        assert!(written.contains(&PathBuf::from("bentos/prod.toml")));
        assert!(report.applied);

        let web_dish = std::fs::read_to_string(tmp.path().join("packages/web/dish.toml")).unwrap();
        assert!(web_dish.contains(r#"name = "web""#));
        assert!(web_dish.contains("[tasks.build]"));
        assert!(web_dish.contains(r#"run = "pnpm run build""#));
        assert!(web_dish.contains(r#"outputs = ["dist/**"]"#));
        assert!(web_dish.contains("[tasks.test]"));
        // dev is persistent — should NOT be a [tasks.dev] block, but
        // SHOULD have the [serve] template comment.
        assert!(!web_dish.contains("[tasks.dev]"));
        assert!(web_dish.contains("[serve]"));

        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("packages/api"));
        assert!(prod.contains("packages/web"));
    }

    #[test]
    fn honours_package_manager_field() {
        // `packageManager: pnpm@…` at the root must pick the pnpm
        // adapter — a hard-coded node-npm makes `bento install` run
        // `npm ci` against a pnpm lockfile.
        let tmp = fixture();
        migrate(tmp.path(), false, false);
        for pkg in ["web", "api"] {
            let dish =
                std::fs::read_to_string(tmp.path().join(format!("packages/{pkg}/dish.toml")))
                    .unwrap();
            assert!(
                dish.contains(r#"language = "node-pnpm""#),
                "{pkg} dish.toml:\n{dish}"
            );
        }
    }

    #[test]
    fn translates_cache_false_and_non_lifecycle_ci() {
        let tmp = fixture();
        migrate(tmp.path(), false, false);
        let web = std::fs::read_to_string(tmp.path().join("packages/web/dish.toml")).unwrap();
        let typecheck = web.split("[tasks.typecheck]").nth(1).unwrap();
        assert!(typecheck.contains("cache = false"), "{web}");
        assert!(typecheck.contains("ci = true"), "{web}");
        // Lifecycle names always run in `bento ci` — no redundant flag.
        let build = web.split("[tasks.build]").nth(1).unwrap();
        assert!(!build.split("[tasks.").next().unwrap().contains("ci = true"));
    }

    #[test]
    fn never_overwrites_bento_toml_even_with_force() {
        let tmp = fixture();
        std::fs::write(tmp.path().join("bento.toml"), "name = \"existing\"\n").unwrap();
        for force in [false, true] {
            let report = migrate(tmp.path(), false, force);
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("bento.toml")).unwrap(),
                "name = \"existing\"\n",
            );
            // The dish.tomls in fresh dirs still get written.
            let written: Vec<_> = report
                .files_written
                .iter()
                .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
                .collect();
            assert!(written.contains(&PathBuf::from("packages/web/dish.toml")));
            assert!(!written.contains(&PathBuf::from("bento.toml")));
        }
    }

    #[test]
    fn dish_conflict_still_lists_the_package_in_prod_toml() {
        let tmp = fixture();
        std::fs::write(
            tmp.path().join("packages/web/dish.toml"),
            "name = \"web\"\nlanguage = \"node-pnpm\"\n",
        )
        .unwrap();
        let report = migrate(tmp.path(), false, false);
        assert!(report.has_conflicts());
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("packages/web"), "{prod}");
        assert!(prod.contains("packages/api"), "{prod}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = fixture();
        let report = migrate(tmp.path(), true, false);
        assert!(!report.applied);
        assert!(!report.files_written.is_empty());
        assert!(!tmp.path().join("packages/web/dish.toml").exists());
        assert!(!tmp.path().join("bento.toml").exists());
    }

    #[test]
    fn supports_v1_pipeline_key() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("turbo.json"),
            r#"{ "pipeline": { "build": { "outputs": ["build/**"] } } }"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{ "name": "single", "scripts": { "build": "echo built" } }"#,
        )
        .unwrap();
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("[tasks.build]"));
        assert!(dish.contains(r#"outputs = ["build/**"]"#));
        // A single-package repo lists the root as `.`, not an empty
        // string the planner can't resolve.
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("\".\""), "{prod}");
    }

    #[test]
    fn surfaces_dependson_and_persistent_as_notes() {
        let tmp = fixture();
        let report = migrate(tmp.path(), true, false);
        let kinds: std::collections::BTreeSet<_> = report.notes.iter().map(|n| n.kind).collect();
        assert!(
            kinds.contains(&NoteKind::Inferred),
            "dependsOn should produce Inferred notes"
        );
        assert!(
            kinds.contains(&NoteKind::Skipped),
            "persistent should produce a Skipped note"
        );
    }

    #[test]
    fn yarn_classic_workspaces_object_form() {
        // Yarn classic: workspaces = { packages: [...] } instead of just [...].
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("turbo.json"),
            r#"{ "tasks": { "build": {} } }"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{
                "name": "yarn-monorepo",
                "workspaces": { "packages": ["pkg/*"] }
            }"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("yarn.lock"), "# yarn lockfile v1\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("pkg/a")).unwrap();
        std::fs::write(
            tmp.path().join("pkg/a/package.json"),
            r#"{ "name": "a", "scripts": { "build": "echo a" } }"#,
        )
        .unwrap();
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("pkg/a/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-yarn""#), "{dish}");
        assert!(dish.contains(r#"run = "yarn run build""#), "{dish}");
    }
}
