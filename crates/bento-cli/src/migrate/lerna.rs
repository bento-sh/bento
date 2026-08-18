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
//! Without an explicit `npmClient` the package manager is detected the
//! same way the node adapters detect it (corepack `packageManager`,
//! then the nearest lockfile).
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

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{
    discover_workspace_packages, node_pm, node_pm_by_name, parse_jsonc_file, render_task,
    short_name, DiscoveredPackage, Emitter, MigrationReport, NodePm, NoteKind, PackageJson,
    TaskBlock,
};

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
    /// `"npm"`, `"pnpm"`, `"yarn"`, or `"bun"`. Drives the adapter id +
    /// the `run` command in emitted dish.toml task blocks. Absent →
    /// detected from the workspace.
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

// ── Public entry point ─────────────────────────────────────────────

pub fn run(mut e: Emitter) -> Result<MigrationReport> {
    let root = e.root().to_path_buf();

    // 1. Load lerna.json.
    let lerna_path = root.join("lerna.json");
    let lerna: LernaJson = parse_jsonc_file(&lerna_path)
        .with_context(|| format!("reading {}", lerna_path.display()))?;

    // 2. Surface command.* config + useNx as notes (informational; not ported).
    if let Some(obj) = lerna.command.as_ref().and_then(|c| c.as_object()) {
        if !obj.is_empty() {
            let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
            e.note(
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
    if lerna.use_nx == Some(true) {
        e.note(
            NoteKind::Skipped,
            "lerna.json has `useNx: true` — this is a hybrid lerna+nx repo. \
             Re-run `bento migrate nx` to capture the nx task graph; the lerna \
             migrator only ports scripts from package.json.",
        );
    }
    if lerna.version.as_deref() == Some("independent") {
        e.note(
            NoteKind::Inferred,
            "lerna.json `version: \"independent\"` — bento doesn't manage package \
             versions; release tooling (`bento release` or `changesets`) lives outside \
             the migrator's scope.",
        );
    }

    // 3. Resolve workspace globs. Order of precedence:
    //    a) `useWorkspaces: true` → read root `package.json` `workspaces`
    //    b) `packages: [...]` in lerna.json
    //    c) Implicit fallback: lerna 7+ removed `useWorkspaces` (the
    //       repo-wide migration to internal nx delegation made it the
    //       default). When `packages` is absent AND `useWorkspaces` is
    //       absent, defer to root `package.json` `workspaces` — the
    //       lerna 7+ canonical shape.
    let use_workspaces = lerna.use_workspaces.unwrap_or(false);
    let lerna7_implicit = lerna.use_workspaces.is_none() && lerna.packages.is_none();

    let workspace_globs: Vec<String> = if use_workspaces || lerna7_implicit {
        let root_pkg_path = root.join("package.json");
        let root_pkg: PackageJson = parse_jsonc_file(&root_pkg_path)
            .with_context(|| format!("reading {}", root_pkg_path.display()))?;
        let globs = root_pkg.workspace_globs();
        if globs.is_empty() {
            let detail = if lerna7_implicit {
                "lerna.json has no `packages` field (lerna 7+ delegates to package.json \
                 `workspaces`) but root package.json also has no `workspaces` field — \
                 nothing to migrate."
            } else {
                "lerna.json sets useWorkspaces: true but root package.json has no \
                 `workspaces` field — nothing to migrate."
            };
            e.note(NoteKind::Skipped, detail);
            return e.finish();
        }
        if lerna7_implicit {
            e.note(
                NoteKind::Inferred,
                "lerna.json has no explicit `packages` or `useWorkspaces` field — \
                 inferred lerna 7+ shape (defers to root package.json `workspaces`).",
            );
        }
        globs
    } else {
        lerna.packages.clone().unwrap_or_default()
    };

    if workspace_globs.is_empty() {
        e.note(
            NoteKind::Skipped,
            "lerna.json declares no `packages` globs — nothing to migrate.",
        );
        return e.finish();
    }

    // 4. Discover packages.
    let packages = discover_workspace_packages(&root, &workspace_globs)?;
    if packages.is_empty() {
        e.note(
            NoteKind::Skipped,
            format!("no packages matched lerna globs: {workspace_globs:?} — nothing to write"),
        );
        return e.finish();
    }

    // 5. Inferred note: lerna doesn't model task-level cross-package edges.
    e.note(
        NoteKind::Inferred,
        "lerna doesn't model task dependencies between packages — bento derives \
         ordering from the dish graph. If your build needs upstream dishes built first, \
         wire `depends_on = [\"<dish>\"]` at the dish.toml top level by hand.",
    );

    // 6. Emit per-package dish.toml.
    for pkg in &packages {
        let pm = match lerna.npm_client.as_deref() {
            Some(client) => node_pm_by_name(client),
            None => node_pm(&pkg.dir, &root),
        };
        let body = render_dish_toml(pkg, pm);
        e.dish(&pkg.dir, &body)?;
    }

    e.finish()
}

// ── dish.toml renderer ─────────────────────────────────────────────

fn render_dish_toml(pkg: &DiscoveredPackage, pm: NodePm) -> String {
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

    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{lang}\"\n\
         \n\
         # Migrated from lerna. Each [tasks.<name>] mirrors the package.json\n\
         # script with the same name. Lerna doesn't model task-level deps —\n\
         # add `depends_on = [\"<dish>\"]` at the dish top level by hand if\n\
         # this dish needs another built first.\n",
        lang = pm.language,
    );

    // package.json scripts are raw shell entry points, not a declared
    // CI pipeline — a mirrored `dev` / `start` must stay `bento run`-only.
    for name in pkg.pkg.scripts.keys() {
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
        let report = migrate(tmp.path(), false, false);

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
    fn refuses_to_overwrite_without_force_but_keeps_the_dish() {
        let tmp = fixture();
        let preexisting = "name = \"hand-written\"\n";
        std::fs::write(tmp.path().join("packages/a/dish.toml"), preexisting).unwrap();

        let report = migrate(tmp.path(), false, false);

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

        // A skipped dish.toml is still a dish in the bento.
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("packages/a"), "{prod}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = fixture();
        let report = migrate(tmp.path(), true, false);

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

        migrate(tmp.path(), false, false);

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
    fn detects_the_package_manager_when_npm_client_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lerna.json"),
            r#"{ "packages": ["packages/*"] }"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{ "name": "root", "packageManager": "bun@1.2.0" }"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/x")).unwrap();
        std::fs::write(
            tmp.path().join("packages/x/package.json"),
            r#"{ "name": "x", "scripts": { "build": "tsc" } }"#,
        )
        .unwrap();

        migrate(tmp.path(), false, false);

        let dish = std::fs::read_to_string(tmp.path().join("packages/x/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "bun""#), "{dish}");
        assert!(dish.contains(r#"run = "bun run build""#), "{dish}");
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

        let report = migrate(tmp.path(), false, false);

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

        let report = migrate(tmp.path(), true, false);

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

        migrate(tmp.path(), false, false);

        let dish = std::fs::read_to_string(tmp.path().join("pkg/a/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-yarn""#));
        assert!(dish.contains(r#"run = "yarn run build""#));
    }
}
