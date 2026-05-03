//! Lerna → bento migrator.
//!
//! Reads root `lerna.json` to discover packages either via lerna's own
//! `packages` glob array, or — when `useWorkspaces: true` — via the
//! root `package.json`'s `workspaces` field. Walks every discovered
//! package, mirrors its `package.json` `scripts` map into per-package
//! `dish.toml` `[tasks.<name>]` blocks, and emits a starter workspace
//! `bento.toml` + `bentos/prod.toml`.
//!
//! ## What translates cleanly
//!
//! | Lerna                                  | Bento                                      |
//! |----------------------------------------|--------------------------------------------|
//! | `packages: ["packages/*"]`             | per-package `dish.toml`                    |
//! | `package.json` `scripts.<name>`        | `[tasks.<name>]` with matching `run`       |
//! | `npmClient: "pnpm" \| "yarn" \| "bun"` | `language = "node-pnpm" \| "node-yarn" \| "bun"` and `run = "<client> run <task>"` |
//! | `useWorkspaces: true`                  | reads globs from root `package.json`'s `workspaces` |
//!
//! ## What gets a note instead
//!
//! - **Cross-package dependencies.** Lerna doesn't model task-level
//!   dependencies between packages — it relies on topological ordering
//!   from `package.json` `dependencies`. Surfaced as `Inferred`: the
//!   user wires `dish.depends_on` by hand.
//! - **`command.publish.*` / `command.bootstrap.*` / `command.version.*`.**
//!   Lerna's command-specific config (registry, conventional commits,
//!   ignore globs, hoisting) doesn't map to bento — surfaced as
//!   `Skipped` listing the unported subkeys.
//! - **`useNx: true`.** Hybrid lerna+nx repos should run the nx
//!   migrator instead — surfaced as `Skipped` with a pointer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::init::{toml_basic_string, toml_table_key};

use super::{MigrationReport, NoteKind};

// ── Lerna config (subset we care about) ────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LernaJson {
    /// Glob array of package directories. Required unless
    /// `useWorkspaces: true`, in which case packages globs come from
    /// root `package.json` `workspaces`.
    packages: Option<Vec<String>>,
    /// When true, defer to root `package.json`'s `workspaces` field.
    #[serde(rename = "useWorkspaces")]
    use_workspaces: Option<bool>,
    /// `"npm"` (default), `"pnpm"`, `"yarn"`, or `"bun"`. Drives the
    /// adapter id + the `run` command in emitted dish.toml task blocks.
    #[serde(rename = "npmClient")]
    npm_client: Option<String>,
    /// Lerna's own version (or `"independent"`). Informational only.
    version: Option<String>,
    /// Hybrid lerna+nx — informational; user should run `bento migrate nx`.
    #[serde(rename = "useNx")]
    use_nx: Option<bool>,
    /// `command.publish.*`, `command.bootstrap.*`, etc. Surfaced as
    /// `Skipped` notes since none of it maps to bento.
    command: Option<serde_json::Value>,
}

// ── Package.json (subset) ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PackageJson {
    #[serde(default)]
    name: Option<String>,
    /// Either an array of globs (`["packages/*"]`) or an object with
    /// `{packages: [...]}` (yarn classic). Both shapes flatten via
    /// `WorkspacesField`.
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

    // 1. Load lerna.json.
    let lerna_path = opts.root.join("lerna.json");
    let lerna: LernaJson = parse_json_file(&lerna_path)
        .with_context(|| format!("reading {}", lerna_path.display()))?;

    // 2. Pick the npm client → adapter + run-prefix.
    let npm_client = lerna.npm_client.as_deref().unwrap_or("npm");
    let (language_id, run_prefix) = match npm_client {
        "pnpm" => ("node-pnpm", "pnpm run"),
        "yarn" => ("node-yarn", "yarn run"),
        "bun" => ("bun", "bun run"),
        // Any unknown / unset / "npm" falls through to npm.
        _ => ("node-npm", "npm run"),
    };

    // 3. Surface command.* config + useNx as notes (informational; not ported).
    if let Some(cmd) = &lerna.command {
        if let Some(obj) = cmd.as_object() {
            if !obj.is_empty() {
                let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
                report.push_note(
                    NoteKind::Skipped,
                    format!(
                        "lerna.json `command.*` config not ported: {} — these (publish, \
                         bootstrap, version, etc.) are lerna-specific commands with no \
                         direct bento equivalent. If you used `lerna publish`, model release \
                         flow via `bento release`; for `lerna bootstrap`, the matching npm \
                         client install (e.g. `bento install`) handles workspaces natively.",
                        keys.join(", ")
                    ),
                );
            }
        }
    }
    if lerna.use_nx == Some(true) {
        report.push_note(
            NoteKind::Skipped,
            "lerna.json has `useNx: true` — this is a hybrid lerna+nx repo. \
             Re-run `bento migrate nx` to capture the nx task graph; the lerna \
             migrator only ports scripts from package.json.",
        );
    }
    if let Some(version) = &lerna.version {
        if version == "independent" {
            report.push_note(
                NoteKind::Inferred,
                "lerna.json `version: \"independent\"` — bento doesn't manage package \
                 versions; release tooling (`bento release` or `changesets`) lives outside \
                 the migrator's scope.",
            );
        }
    }

    // 4. Resolve workspace globs. Order of precedence:
    //    a) `useWorkspaces: true` → read root `package.json` `workspaces`
    //    b) `packages: [...]` in lerna.json
    //    c) Implicit fallback: lerna 7+ removed `useWorkspaces` (the
    //       repo-wide migration to internal nx delegation made it the
    //       default). When `packages` is absent AND `useWorkspaces` is
    //       absent, defer to root `package.json` `workspaces` — the
    //       lerna 7+ canonical shape.
    let use_workspaces = lerna.use_workspaces.unwrap_or(false);
    let lerna7_implicit = lerna.use_workspaces.is_none() && lerna.packages.is_none();
    let root_pkg_path = opts.root.join("package.json");
    let mut root_pkg_loaded: Option<PackageJson> = None;

    let workspace_globs: Vec<String> = if use_workspaces || lerna7_implicit {
        let root_pkg: PackageJson = parse_json_file(&root_pkg_path)
            .with_context(|| format!("reading {}", root_pkg_path.display()))?;
        let globs = root_pkg
            .workspaces
            .as_ref()
            .map(|w| match w {
                WorkspacesField::Array(v) => v.clone(),
                WorkspacesField::Object { packages } => packages.clone(),
            })
            .unwrap_or_default();
        if globs.is_empty() {
            let detail = if lerna7_implicit {
                "lerna.json has no `packages` field (lerna 7+ delegates to package.json \
                 `workspaces`) but root package.json also has no `workspaces` field — \
                 nothing to migrate."
            } else {
                "lerna.json sets useWorkspaces: true but root package.json has no \
                 `workspaces` field — nothing to migrate."
            };
            report.push_note(NoteKind::Skipped, detail);
            return Ok(report);
        }
        if lerna7_implicit {
            report.push_note(
                NoteKind::Inferred,
                "lerna.json has no explicit `packages` or `useWorkspaces` field — \
                 inferred lerna 7+ shape (defers to root package.json `workspaces`).",
            );
        }
        root_pkg_loaded = Some(root_pkg);
        globs
    } else {
        lerna.packages.clone().unwrap_or_default()
    };

    if workspace_globs.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            "lerna.json declares no `packages` globs — nothing to migrate.",
        );
        return Ok(report);
    }

    // 5. Discover packages.
    let packages = discover_workspace_packages(&opts.root, &workspace_globs)?;
    if packages.is_empty() {
        // Maybe the root itself has scripts and the user expected single-package mode.
        if root_pkg_loaded.is_none() && root_pkg_path.exists() {
            let root_pkg: PackageJson = parse_json_file(&root_pkg_path)
                .with_context(|| format!("reading {}", root_pkg_path.display()))?;
            root_pkg_loaded = Some(root_pkg);
        }
        report.push_note(
            NoteKind::Skipped,
            format!("no packages matched lerna globs: {workspace_globs:?} — nothing to write"),
        );
        let _ = root_pkg_loaded;
        return Ok(report);
    }

    // 6. Inferred note: lerna doesn't model task-level cross-package edges.
    report.push_note(
        NoteKind::Inferred,
        "lerna doesn't model task dependencies between packages — bento derives \
         ordering from the dish graph. If your build needs upstream dishes built first, \
         wire `depends_on = [\"<dish>\"]` at the dish.toml top level by hand.",
    );

    // 7. Emit per-package dish.toml.
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
        let body = render_dish_toml(pkg, language_id, run_prefix);
        write_or_simulate(&dish_toml_path, &body, opts.dry_run, &mut report)?;
        dish_rels.push(relative(&pkg.dir, &opts.root).display().to_string());
    }

    // 8. Workspace bento.toml — same starter shape as turbo migrator.
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

    // 9. bentos/prod.toml — list every dish.
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
/// Mirrors the turbo migrator's resolver: `<segment>/*` and
/// `<segment>/**` plus literal paths.
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

// ── dish.toml renderer ─────────────────────────────────────────────

fn render_dish_toml(pkg: &DiscoveredPackage, language_id: &str, run_prefix: &str) -> String {
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

    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{language_id}\"\n\
         \n\
         # Migrated from lerna. Each [tasks.<name>] mirrors the package.json\n\
         # script with the same name. Lerna doesn't model task-level deps —\n\
         # add `depends_on = [\"<dish>\"]` at the dish top level by hand if\n\
         # this dish needs another built first.\n",
    );

    // Sort scripts deterministically — BTreeMap already does this, but
    // be explicit so future map swaps don't surprise us.
    let mut script_names: Vec<&String> = pkg.pkg.scripts.keys().collect();
    script_names.sort();
    for name in script_names {
        body.push('\n');
        body.push_str(&format!("[tasks.{}]\n", toml_table_key(name)));
        body.push_str(&format!(
            "run = {}\n",
            toml_basic_string(&format!("{run_prefix} {name}"))
        ));
    }

    body
}

/// Strip a leading `@scope/` from a package.json name.
/// `@acme/web` → `web`.
fn infer_short_name(pkg_name: &str) -> String {
    pkg_name
        .rsplit_once('/')
        .map(|(_, last)| last.to_string())
        .unwrap_or_else(|| pkg_name.to_string())
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

    /// Two-package fixture using the default `npmClient: "npm"`.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("lerna.json"),
            r#"{
                "packages": ["packages/*"],
                "version": "0.0.0"
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "monorepo", "private": true }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/a")).unwrap();
        std::fs::write(
            root.join("packages/a/package.json"),
            r#"{
                "name": "@acme/a",
                "scripts": {
                    "build": "tsc",
                    "test": "jest"
                }
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/b")).unwrap();
        std::fs::write(
            root.join("packages/b/package.json"),
            r#"{
                "name": "@acme/b",
                "scripts": {
                    "build": "rollup -c",
                    "lint": "eslint ."
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
        assert!(written.contains(&PathBuf::from("packages/a/dish.toml")));
        assert!(written.contains(&PathBuf::from("packages/b/dish.toml")));
        assert!(written.contains(&PathBuf::from("bento.toml")));
        assert!(written.contains(&PathBuf::from("bentos/prod.toml")));
        assert!(report.applied);

        let a_dish = std::fs::read_to_string(tmp.path().join("packages/a/dish.toml")).unwrap();
        assert!(a_dish.contains(r#"name = "a""#));
        assert!(a_dish.contains(r#"language = "node-npm""#));
        assert!(a_dish.contains("[tasks.build]"));
        assert!(a_dish.contains(r#"run = "npm run build""#));
        assert!(a_dish.contains("[tasks.test]"));
        assert!(a_dish.contains(r#"run = "npm run test""#));

        let b_dish = std::fs::read_to_string(tmp.path().join("packages/b/dish.toml")).unwrap();
        assert!(b_dish.contains(r#"name = "b""#));
        assert!(b_dish.contains("[tasks.lint]"));
        assert!(b_dish.contains(r#"run = "npm run lint""#));

        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("packages/a"));
        assert!(prod.contains("packages/b"));

        // depends_on inference note must be present.
        assert!(report
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Inferred && n.message.contains("depends_on")));
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let tmp = fixture();
        let preexisting = "name = \"hand-written\"\n";
        std::fs::write(tmp.path().join("packages/a/dish.toml"), preexisting).unwrap();

        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();

        assert!(report.has_conflicts());
        let conflict_msgs: Vec<&str> = report
            .notes
            .iter()
            .filter(|n| n.kind == NoteKind::Conflict)
            .map(|n| n.message.as_str())
            .collect();
        assert!(
            conflict_msgs
                .iter()
                .any(|m| m.contains("packages/a/dish.toml")),
            "expected conflict note for packages/a/dish.toml, got: {conflict_msgs:?}"
        );

        // Untouched.
        let body = std::fs::read_to_string(tmp.path().join("packages/a/dish.toml")).unwrap();
        assert_eq!(body, preexisting);

        // packages/b had no preexisting file → still got migrated.
        let written: Vec<_> = report
            .files_written
            .iter()
            .map(|f| f.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
            .collect();
        assert!(written.contains(&PathBuf::from("packages/b/dish.toml")));
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
        // Nothing actually on disk afterwards.
        assert!(!tmp.path().join("packages/a/dish.toml").exists());
        assert!(!tmp.path().join("packages/b/dish.toml").exists());
        assert!(!tmp.path().join("bento.toml").exists());
        assert!(!tmp.path().join("bentos/prod.toml").exists());
    }

    #[test]
    fn picks_correct_adapter_for_npm_client_pnpm() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lerna.json"),
            r#"{
                "packages": ["packages/*"],
                "npmClient": "pnpm"
            }"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{ "name": "root" }"#).unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/x")).unwrap();
        std::fs::write(
            tmp.path().join("packages/x/package.json"),
            r#"{ "name": "x", "scripts": { "build": "tsc" } }"#,
        )
        .unwrap();

        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let _ = report;

        let dish = std::fs::read_to_string(tmp.path().join("packages/x/dish.toml")).unwrap();
        assert!(
            dish.contains(r#"language = "node-pnpm""#),
            "expected language = node-pnpm, dish:\n{dish}"
        );
        assert!(
            dish.contains(r#"run = "pnpm run build""#),
            "expected pnpm run prefix, dish:\n{dish}"
        );
    }

    #[test]
    fn falls_back_to_workspaces_when_use_workspaces_true() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lerna.json"),
            r#"{
                "useWorkspaces": true
            }"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{
                "name": "root",
                "workspaces": ["packages/*"]
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/one")).unwrap();
        std::fs::write(
            tmp.path().join("packages/one/package.json"),
            r#"{ "name": "one", "scripts": { "build": "echo 1" } }"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/two")).unwrap();
        std::fs::write(
            tmp.path().join("packages/two/package.json"),
            r#"{ "name": "two", "scripts": { "test": "echo 2" } }"#,
        )
        .unwrap();

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
        assert!(written.contains(&PathBuf::from("packages/one/dish.toml")));
        assert!(written.contains(&PathBuf::from("packages/two/dish.toml")));

        let one = std::fs::read_to_string(tmp.path().join("packages/one/dish.toml")).unwrap();
        assert!(one.contains("[tasks.build]"));
    }

    #[test]
    fn surfaces_command_config_as_skipped_note() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lerna.json"),
            r#"{
                "packages": ["packages/*"],
                "command": {
                    "publish": {
                        "conventionalCommits": true,
                        "registry": "https://npm.pkg.github.com"
                    }
                }
            }"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{ "name": "root" }"#).unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/p")).unwrap();
        std::fs::write(
            tmp.path().join("packages/p/package.json"),
            r#"{ "name": "p", "scripts": { "build": "echo p" } }"#,
        )
        .unwrap();

        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();

        let skipped: Vec<&str> = report
            .notes
            .iter()
            .filter(|n| n.kind == NoteKind::Skipped)
            .map(|n| n.message.as_str())
            .collect();
        assert!(
            skipped
                .iter()
                .any(|m| m.contains("command.*") && m.contains("publish")),
            "expected Skipped note mentioning command.* + publish, got: {skipped:?}"
        );
    }

    #[test]
    fn infer_short_name_strips_scope() {
        assert_eq!(infer_short_name("@acme/web"), "web");
        assert_eq!(infer_short_name("plain"), "plain");
        assert_eq!(infer_short_name("@a/b/c"), "c");
    }

    #[test]
    fn yarn_classic_workspaces_object_form_via_use_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lerna.json"),
            r#"{ "useWorkspaces": true, "npmClient": "yarn" }"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{
                "name": "yarn-classic",
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
        let _ = report;

        let dish = std::fs::read_to_string(tmp.path().join("pkg/a/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-yarn""#));
        assert!(dish.contains(r#"run = "yarn run build""#));
    }
}
