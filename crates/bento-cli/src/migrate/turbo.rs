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
//! | top-level `tasks.build`     | per-package `[tasks.build]` with the     |
//! |                             | matching `package.json` `scripts.build`  |
//!
//! ## What gets a note instead
//!
//! - `dependsOn` arrays — bento derives task ordering from the dish
//!   graph rather than per-task `dependsOn`. Cross-package `^build`
//!   maps to bento's automatic upstream rebuild via `dish.depends_on`,
//!   which the user wires by hand (we don't auto-derive from
//!   `package.json` `dependencies` yet).
//! - `cache: false` — bento doesn't have a per-task no-cache flag;
//!   surfaced as a note so the user can decide.
//! - `persistent: true` — usually `dev` / `serve` tasks; surfaced as
//!   a note recommending the dish-level `[serve]` block instead.
//! - Per-package `turbo.json` overrides — detected, listed, but the
//!   per-package overrides aren't merged in (rare in practice).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::init::{toml_basic_string, toml_table_key};

use super::{MigrationReport, NoteKind};

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

// ── Package.json (subset) ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PackageJson {
    #[serde(default)]
    name: Option<String>,
    /// Either an array of globs (`["packages/*"]`) or an object with
    /// `{packages: [...]}` (yarn classic). Both shapes flatten to a
    /// list of glob strings via `WorkspacesField`.
    #[serde(default)]
    workspaces: Option<WorkspacesField>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkspacesField {
    Array(Vec<String>),
    Object {
        #[serde(default)]
        packages: Vec<String>,
    },
}

impl WorkspacesField {
    fn into_globs(self) -> Vec<String> {
        match self {
            WorkspacesField::Array(v) => v,
            WorkspacesField::Object { packages } => packages,
        }
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

    // 1. Load root turbo.json.
    let turbo_path = opts.root.join("turbo.json");
    let turbo: TurboJson = parse_json_file(&turbo_path)
        .with_context(|| format!("reading {}", turbo_path.display()))?;
    let turbo_tasks = turbo.tasks.or(turbo.pipeline).unwrap_or_default();
    if turbo_tasks.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            "turbo.json has no tasks/pipeline — nothing to migrate",
        );
        return Ok(report);
    }

    // 2. Annotate any tasks whose semantics we won't faithfully port.
    for (name, t) in &turbo_tasks {
        if t.cache == Some(false) {
            report.push_note(
                NoteKind::Skipped,
                format!(
                    "task `{name}` has `cache: false` — bento has no per-task no-cache flag; \
                     the task still runs but its output WILL be cached. Use `bento --no-cache` \
                     for ad-hoc bypass."
                ),
            );
        }
        if t.persistent == Some(true) {
            report.push_note(
                NoteKind::Skipped,
                format!(
                    "task `{name}` is persistent (likely a dev server) — model this as the \
                     dish-level `[serve]` block in dish.toml instead of `[tasks.{name}]`."
                ),
            );
        }
        if !t.depends_on.is_empty() {
            report.push_note(
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
    let root_pkg_path = opts.root.join("package.json");
    let mut root_pkg: PackageJson = parse_json_file(&root_pkg_path)
        .with_context(|| format!("reading {}", root_pkg_path.display()))?;

    let workspace_globs = root_pkg
        .workspaces
        .take()
        .map(|w| w.into_globs())
        .unwrap_or_default();

    let packages = if workspace_globs.is_empty() {
        // Single-package repo. The root IS the only package.
        vec![DiscoveredPackage {
            dir: opts.root.clone(),
            rel_dir: PathBuf::from("."),
            pkg: root_pkg,
        }]
    } else {
        discover_workspace_packages(&opts.root, &workspace_globs)?
    };

    if packages.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            format!(
                "no packages matched workspaces globs: {:?} — nothing to write",
                workspace_globs
            ),
        );
        return Ok(report);
    }

    // 4. Detect per-package turbo.json overrides (informational only).
    for p in &packages {
        if p.dir.join("turbo.json").exists() && p.rel_dir != Path::new(".") {
            report.push_note(
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
    let mut dish_rels: Vec<String> = Vec::new();
    for pkg in &packages {
        let dish_toml_path = pkg.dir.join("dish.toml");
        if dish_toml_path.exists() && !opts.force {
            report.push_note(
                NoteKind::Conflict,
                format!(
                    "{} already exists — skipped (re-run with --force to overwrite)",
                    relative(&dish_toml_path, &opts.root).display()
                ),
            );
            continue;
        }
        let body = render_dish_toml(pkg, &turbo_tasks);
        write_or_simulate(&dish_toml_path, &body, opts.dry_run, &mut report)?;
        dish_rels.push(relative(&pkg.dir, &opts.root).display().to_string());
    }

    // 6. Workspace bento.toml — placeholder shape; user fills in cache
    //    + toolchain pins later.
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

    // 7. bentos/prod.toml — list every dish the migrator created.
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

// ── Workspace discovery ────────────────────────────────────────────

struct DiscoveredPackage {
    dir: PathBuf,
    rel_dir: PathBuf,
    pkg: PackageJson,
}

/// Resolve npm-style workspace globs to a list of package directories.
/// Only `<segment>/*` and `<segment>/**` are supported; deeper glob
/// metacharacters fall back to a literal-path interpretation. Good
/// enough for the ~95% case (`packages/*`, `apps/*`, `services/*`).
fn discover_workspace_packages(root: &Path, globs: &[String]) -> Result<Vec<DiscoveredPackage>> {
    let mut out = Vec::new();
    for g in globs {
        for dir in resolve_glob(root, g)? {
            let pkg_json = dir.join("package.json");
            if !pkg_json.exists() {
                continue;
            }
            let pkg: PackageJson = parse_json_file(&pkg_json)
                .with_context(|| format!("reading {}", pkg_json.display()))?;
            let rel_dir = dir.strip_prefix(root).unwrap_or(&dir).to_path_buf();
            out.push(DiscoveredPackage { dir, rel_dir, pkg });
        }
    }
    out.sort_by(|a, b| a.rel_dir.cmp(&b.rel_dir));
    Ok(out)
}

/// Resolve one workspaces glob. Supports the common forms:
///   - "packages/*"   — direct children of packages/ that are dirs
///   - "packages/**"  — every descendant dir (1 level deep ok in practice)
///   - "apps/web"     — literal path
fn resolve_glob(root: &Path, glob: &str) -> Result<Vec<PathBuf>> {
    if let Some(prefix) = glob.strip_suffix("/*") {
        let dir = root.join(prefix);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out: Vec<PathBuf> = fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect();
        out.sort();
        Ok(out)
    } else if let Some(prefix) = glob.strip_suffix("/**") {
        // Treat the same as /* for now — recursing deeper is unusual
        // for npm workspaces and the user can add nested entries
        // explicitly if needed.
        resolve_glob(root, &format!("{prefix}/*"))
    } else {
        let p = root.join(glob);
        if p.is_dir() {
            Ok(vec![p])
        } else {
            Ok(Vec::new())
        }
    }
}

// ── dish.toml renderer (turbo-aware) ───────────────────────────────

fn render_dish_toml(pkg: &DiscoveredPackage, turbo_tasks: &BTreeMap<String, TurboTask>) -> String {
    let dish_name = pkg
        .pkg
        .name
        .as_deref()
        .map(infer_short_name)
        .or_else(|| {
            pkg.dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "dish".to_string());

    // Heuristic: any package with package.json gets `language = "node-npm"`
    // as a starter — user picks the right manager (npm/pnpm/bun/yarn) by
    // editing the line. Same default `bento init` uses for unknown.
    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"node-npm\"\n\
         \n\
         # Migrated from turbo.json. Each [tasks.<name>] mirrors the\n\
         # turbo task with the same name + the matching package.json\n\
         # script. Review outputs / inputs against your build artefacts.\n",
    );

    // Emit a [tasks.<name>] for every turbo task whose name matches a
    // package.json script. Skip persistent tasks (they belong in
    // [serve], surfaced as a note in the report).
    for (task_name, turbo_task) in turbo_tasks {
        if turbo_task.persistent == Some(true) {
            continue;
        }
        let Some(script) = pkg.pkg.scripts.get(task_name) else {
            continue;
        };
        body.push('\n');
        body.push_str(&format!("[tasks.{}]\n", toml_table_key(task_name)));
        body.push_str(&format!(
            "run = {}\n",
            toml_basic_string(&format!("npm run {task_name}"))
        ));
        let _ = script; // captured into the comment below for context
        if !turbo_task.outputs.is_empty() {
            body.push_str(&format!(
                "outputs = {}\n",
                render_string_array(&turbo_task.outputs)
            ));
        }
        if !turbo_task.inputs.is_empty() {
            body.push_str(&format!(
                "inputs = {}\n",
                render_string_array(&turbo_task.inputs)
            ));
        }
    }

    // If the package has a persistent task (e.g. `dev`), drop a
    // commented [serve] template so the user knows where it goes.
    if let Some((dev_name, dev_script)) = persistent_dev(pkg, turbo_tasks) {
        body.push('\n');
        body.push_str("# Persistent task migrated from turbo. bento models long-running\n");
        body.push_str("# servers as the dish-level [serve] block instead of [tasks.<name>].\n");
        body.push_str("# [serve]\n");
        body.push_str(&format!(
            "# run = \"npm run {dev_name}\"  # was: {dev_script}\n"
        ));
    }

    body
}

/// Strip a leading `@scope/` from a package.json name so the dish name
/// reads as a clean identifier. `@acme/web` → `web`.
fn infer_short_name(pkg_name: &str) -> String {
    pkg_name
        .rsplit_once('/')
        .map(|(_, last)| last.to_string())
        .unwrap_or_else(|| pkg_name.to_string())
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

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("turbo.json"),
            r#"{
                "$schema": "https://turbo.build/schema.json",
                "tasks": {
                    "build": {
                        "dependsOn": ["^build"],
                        "outputs": ["dist/**"]
                    },
                    "test": {
                        "dependsOn": ["build"]
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
        assert!(written.contains(&PathBuf::from("packages/web/dish.toml")));
        assert!(written.contains(&PathBuf::from("packages/api/dish.toml")));
        assert!(written.contains(&PathBuf::from("bento.toml")));
        assert!(written.contains(&PathBuf::from("bentos/prod.toml")));
        assert!(report.applied);

        let web_dish = std::fs::read_to_string(tmp.path().join("packages/web/dish.toml")).unwrap();
        assert!(web_dish.contains(r#"name = "web""#));
        assert!(web_dish.contains("[tasks.build]"));
        assert!(web_dish.contains(r#"run = "npm run build""#));
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
        // But the dish.tomls in fresh dirs still get written.
        let written: Vec<_> = report
            .files_written
            .iter()
            .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
            .collect();
        assert!(written.contains(&PathBuf::from("packages/web/dish.toml")));
        // bento.toml stays untouched.
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("[tasks.build]"));
        assert!(dish.contains(r#"outputs = ["build/**"]"#));
        let _ = report;
    }

    #[test]
    fn surfaces_dependson_and_cache_false_as_notes() {
        let tmp = fixture();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let kinds: std::collections::BTreeSet<_> = report.notes.iter().map(|n| n.kind).collect();
        assert!(
            kinds.contains(&NoteKind::Inferred),
            "dependsOn should produce Inferred notes"
        );
        assert!(
            kinds.contains(&NoteKind::Skipped),
            "cache:false / persistent should produce Skipped notes"
        );
    }

    #[test]
    fn infer_short_name_strips_scope() {
        assert_eq!(infer_short_name("@acme/web"), "web");
        assert_eq!(infer_short_name("just-a-name"), "just-a-name");
        assert_eq!(infer_short_name("@acme/very/deep"), "deep");
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
        std::fs::create_dir_all(tmp.path().join("pkg/a")).unwrap();
        std::fs::write(
            tmp.path().join("pkg/a/package.json"),
            r#"{ "name": "a", "scripts": { "build": "echo a" } }"#,
        )
        .unwrap();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(tmp.path().join("pkg/a/dish.toml").exists());
        let _ = report;
    }
}
