//! `bento migrate <tool>` — convert a competing monorepo tool's
//! workspace config into bento config.
//!
//! Each migrator reads the source tool's manifests (`turbo.json`,
//! `nx.json`, `moon/workspace.yml`, …), walks the package layout,
//! and emits the equivalent bento config: workspace `bento.toml`,
//! per-package `dish.toml`s, and a starter `bentos/prod.toml`.
//!
//! Migrators are intentionally non-destructive: by default they refuse
//! to overwrite any existing bento file. `--force` opts in to clobber
//! the files a migrator can regenerate (`dish.toml`, `bentos/prod.toml`);
//! an existing `bento.toml` is *never* overwritten — it carries
//! hand-written toolchain pins, cache config, and deploy targets no
//! migrator can reconstruct. `--dry-run` prints the report without
//! touching the filesystem.
//!
//! The output is a *starting point* the user reviews and tweaks — not
//! a perfect 1:1 translation. Notes are included in the report for
//! anything the migrator couldn't faithfully translate (per-package
//! overrides, persistent dev tasks, custom cache settings).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::init::{toml_basic_string, toml_table_key};

pub mod lerna;
pub mod make;
pub mod moon;
pub mod nx;
pub mod rush;
pub mod turbo;

/// Source tool `bento migrate` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MigrateTool {
    Turbo,
    Nx,
    Lerna,
    Make,
    Moon,
    Rush,
}

pub struct Options {
    pub root: PathBuf,
    pub dry_run: bool,
    pub force: bool,
}

pub fn run(tool: MigrateTool, opts: Options) -> Result<MigrationReport> {
    let emitter = Emitter::new(opts);
    match tool {
        MigrateTool::Turbo => turbo::run(emitter),
        MigrateTool::Nx => nx::run(emitter),
        MigrateTool::Lerna => lerna::run(emitter),
        MigrateTool::Make => make::run(emitter),
        MigrateTool::Moon => moon::run(emitter),
        MigrateTool::Rush => rush::run(emitter),
    }
}

/// Common report shape across all migrators. Printed to the user (with
/// human formatting) and serialised to `--json` mode.
#[derive(Debug, Default, serde::Serialize)]
pub struct MigrationReport {
    /// Files the migrator wrote (or *would* have written under `--dry-run`).
    pub files_written: Vec<WrittenFile>,
    /// Things the migrator skipped or couldn't translate. Each note has
    /// a stable kind so agents can filter, plus a human message.
    pub notes: Vec<MigrationNote>,
    /// Did we actually touch the filesystem? `false` under `--dry-run`.
    pub applied: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct WrittenFile {
    pub path: PathBuf,
    pub bytes: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct MigrationNote {
    pub kind: NoteKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// We chose not to translate something — usually because the source
    /// concept doesn't exist in bento (turbo's `cache: false`, persistent
    /// dev tasks).
    Skipped,
    /// A heuristic guess that the user should review (e.g. inferring
    /// `outputs` from a missing turbo declaration).
    Inferred,
    /// Source feature we recognise but haven't implemented yet.
    NotYetImplemented,
    /// Refused to overwrite an existing file (the user must `--force`).
    Conflict,
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoteKind::Skipped => f.write_str("skipped"),
            NoteKind::Inferred => f.write_str("inferred"),
            NoteKind::NotYetImplemented => f.write_str("not-yet-impl"),
            NoteKind::Conflict => f.write_str("conflict"),
        }
    }
}

impl MigrationReport {
    pub fn push_file(&mut self, path: PathBuf, bytes: usize) {
        self.files_written.push(WrittenFile { path, bytes });
    }
    pub fn push_note(&mut self, kind: NoteKind, message: impl Into<String>) {
        self.notes.push(MigrationNote {
            kind,
            message: message.into(),
        });
    }
    /// True iff the migrator hit any conflict the user must resolve.
    pub fn has_conflicts(&self) -> bool {
        self.notes.iter().any(|n| n.kind == NoteKind::Conflict)
    }
}

/// Filesystem side of every migrator: conflict policy, `--dry-run`
/// simulation, the dish list that becomes `bentos/prod.toml`, and the
/// report. Migrators parse their source tool and render TOML; the
/// Emitter decides what actually lands on disk.
pub struct Emitter {
    root: PathBuf,
    dry_run: bool,
    force: bool,
    report: MigrationReport,
    dish_rels: Vec<String>,
}

impl Emitter {
    pub fn new(opts: Options) -> Self {
        Self {
            root: opts.root,
            dry_run: opts.dry_run,
            force: opts.force,
            report: MigrationReport {
                applied: !opts.dry_run,
                ..Default::default()
            },
            dish_rels: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn note(&mut self, kind: NoteKind, message: impl Into<String>) {
        self.report.push_note(kind, message);
    }

    /// Workspace-relative, forward-slashed path string. The workspace
    /// root itself is `.` — an empty string is not a dish reference.
    pub fn rel(&self, p: &Path) -> String {
        let rel = p.strip_prefix(&self.root).unwrap_or(p);
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            ".".to_string()
        } else {
            s
        }
    }

    /// Write `dir/dish.toml` and register the dish in `bentos/prod.toml`.
    ///
    /// A pre-existing `dish.toml` is left alone (without `--force`) but
    /// the dish is still listed: the package belongs to the bento
    /// whether or not the migrator authored its config.
    pub fn dish(&mut self, dir: &Path, body: &str) -> Result<()> {
        let rel = self.rel(dir);
        if !self.dish_rels.contains(&rel) {
            self.dish_rels.push(rel);
        }
        let path = dir.join("dish.toml");
        if path.exists() && !self.force {
            let shown = self.rel(&path);
            self.note(
                NoteKind::Conflict,
                format!("{shown} already exists — skipped (re-run with --force to overwrite)"),
            );
            return Ok(());
        }
        self.write(&path, body)
    }

    /// Emit the workspace `bento.toml` (only when absent — see the
    /// module docs) plus `bentos/prod.toml`, and hand back the report.
    pub fn finish(mut self) -> Result<MigrationReport> {
        let bento_toml = self.root.join("bento.toml");
        if bento_toml.exists() {
            self.note(
                NoteKind::Skipped,
                "bento.toml already exists — left untouched. Migrators never rewrite it \
                 (not even with --force); it holds toolchain pins and cache config they \
                 can't reconstruct. Delete it first if you want a fresh one.",
            );
        } else {
            let body = crate::init::render_bento_toml(&BTreeMap::new());
            self.write(&bento_toml, &body)?;
        }

        let prod = self.root.join("bentos").join("prod.toml");
        if prod.exists() && !self.force {
            self.note(
                NoteKind::Conflict,
                "bentos/prod.toml already exists — skipped (re-run with --force to overwrite)",
            );
        } else {
            let body = crate::init::render_prod_toml(&self.dish_rels);
            self.write(&prod, &body)?;
        }
        Ok(self.report)
    }

    fn write(&mut self, path: &Path, body: &str) -> Result<()> {
        if !self.dry_run {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        }
        self.report.push_file(path.to_path_buf(), body.len());
        Ok(())
    }
}

// ── Shared manifest parsing ────────────────────────────────────────

/// Parse a JSON file, retrying with comments stripped when strict
/// parsing fails. Every manifest the migrators read (`turbo.json`,
/// `nx.json`, `rush.json`, and Rush's `command-line.json`) is JSONC in
/// practice — Rush's own template ships `/* */` banners.
pub fn parse_jsonc_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let body = fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))?;
    match serde_json::from_str::<T>(&body) {
        Ok(parsed) => Ok(parsed),
        Err(strict_err) => serde_json::from_str(&strip_jsonc_comments(&body))
            .with_context(|| format!("parsing {} as JSON ({strict_err})", path.display())),
    }
}

/// Strip `//` line and `/* */` block comments outside string literals.
/// Newlines inside block comments are preserved so error line numbers
/// still line up with the source file.
fn strip_jsonc_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    let mut in_str = false;
    let mut escape = false;
    while let Some((_, c)) = chars.next() {
        if in_str {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if matches!(chars.peek(), Some((_, '/'))) => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if matches!(chars.peek(), Some((_, '*'))) => {
                chars.next();
                let mut prev = '\0';
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// `package.json` subset every node-family migrator needs.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PackageJson {
    pub name: Option<String>,
    /// Array of globs (`["packages/*"]`) or yarn-classic's
    /// `{packages: [...]}` object.
    pub workspaces: Option<WorkspacesField>,
    pub scripts: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum WorkspacesField {
    Array(Vec<String>),
    Object {
        #[serde(default)]
        packages: Vec<String>,
    },
}

impl PackageJson {
    pub fn workspace_globs(&self) -> Vec<String> {
        match &self.workspaces {
            Some(WorkspacesField::Array(v)) => v.clone(),
            Some(WorkspacesField::Object { packages }) => packages.clone(),
            None => Vec::new(),
        }
    }
}

pub struct DiscoveredPackage {
    pub dir: PathBuf,
    pub rel_dir: PathBuf,
    pub pkg: PackageJson,
}

/// Resolve npm-style workspace globs to package directories. Only
/// `<segment>/*` and `<segment>/**` are supported; deeper glob
/// metacharacters fall back to a literal-path interpretation. Good
/// enough for the ~95% case (`packages/*`, `apps/*`, `services/*`).
pub fn discover_workspace_packages(
    root: &Path,
    globs: &[String],
) -> Result<Vec<DiscoveredPackage>> {
    let mut out = Vec::new();
    for g in globs {
        for dir in resolve_glob(root, g)? {
            let pkg_json = dir.join("package.json");
            if !pkg_json.exists() {
                continue;
            }
            let pkg: PackageJson = parse_jsonc_file(&pkg_json)
                .with_context(|| format!("reading {}", pkg_json.display()))?;
            let rel_dir = dir.strip_prefix(root).unwrap_or(&dir).to_path_buf();
            out.push(DiscoveredPackage { dir, rel_dir, pkg });
        }
    }
    out.sort_by(|a, b| a.rel_dir.cmp(&b.rel_dir));
    Ok(out)
}

pub fn resolve_glob(root: &Path, glob: &str) -> Result<Vec<PathBuf>> {
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
        // Treat the same as /* — recursing deeper is unusual for npm
        // workspaces and the user can add nested entries explicitly.
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

// ── Node package-manager pick ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodePm {
    /// dish.toml `language = …` adapter id.
    pub language: &'static str,
    /// Prefix for `run = "<prefix> <script>"`.
    pub run_prefix: &'static str,
}

pub const NODE_NPM: NodePm = NodePm {
    language: "node-npm",
    run_prefix: "npm run",
};

/// Which package manager owns `dir`, from the same signals the node
/// adapters detect with (corepack `packageManager`, then the nearest
/// lockfile). A migrator that hard-codes npm emits config whose very
/// first `bento install` runs the wrong installer against someone
/// else's lockfile. Falls back to the workspace root, then npm.
pub fn node_pm(dir: &Path, root: &Path) -> NodePm {
    let detected =
        bento_adapters::detect_node_pm(dir).or_else(|| bento_adapters::detect_node_pm(root));
    node_pm_by_name(detected.unwrap_or("npm"))
}

/// Map a package-manager name (`pnpm`, `yarn`, `bun`, `npm` — or a
/// source tool's own declaration) onto the bento adapter id.
pub fn node_pm_by_name(name: &str) -> NodePm {
    match name {
        "pnpm" => NodePm {
            language: "node-pnpm",
            run_prefix: "pnpm run",
        },
        "yarn" => NodePm {
            language: "node-yarn",
            run_prefix: "yarn run",
        },
        "bun" => NodePm {
            language: "bun",
            run_prefix: "bun run",
        },
        _ => NODE_NPM,
    }
}

// ── TOML rendering ─────────────────────────────────────────────────

pub fn toml_string_array(xs: &[String]) -> String {
    let rendered: Vec<String> = xs.iter().map(|x| toml_basic_string(x)).collect();
    format!("[{}]", rendered.join(", "))
}

/// Strip a leading `@scope/` so a package name reads as a dish
/// identifier. `@acme/web` → `web`.
pub fn short_name(pkg_name: &str) -> String {
    pkg_name
        .rsplit_once('/')
        .map(|(_, last)| last.to_string())
        .unwrap_or_else(|| pkg_name.to_string())
}

#[derive(Default)]
pub struct TaskBlock<'a> {
    pub run: &'a str,
    pub inputs: &'a [String],
    pub outputs: &'a [String],
    /// Include a custom-named task in `bento ci`. Lifecycle names
    /// (`build`/`check`/`test`/`lint`) always run, so the field is
    /// only emitted where it changes behaviour.
    pub ci: bool,
    /// Source tool declared `cache: false` — always run, never restore.
    pub no_cache: bool,
}

pub fn render_task(name: &str, t: &TaskBlock) -> String {
    let mut body = format!(
        "\n[tasks.{}]\nrun = {}\n",
        toml_table_key(name),
        toml_basic_string(t.run)
    );
    if !t.outputs.is_empty() {
        body.push_str(&format!("outputs = {}\n", toml_string_array(t.outputs)));
    }
    if !t.inputs.is_empty() {
        body.push_str(&format!("inputs = {}\n", toml_string_array(t.inputs)));
    }
    if t.ci && !bento_core::plan::is_lifecycle_task(name) {
        body.push_str("ci = true\n");
    }
    if t.no_cache {
        body.push_str("cache = false\n");
    }
    body
}

pub fn print_human(report: &MigrationReport) {
    use crate::style;
    if report.files_written.is_empty() && !report.applied {
        println!("{}", style::dim("(nothing to write)"));
    }
    for f in &report.files_written {
        let prefix = if report.applied {
            style::green("✓ wrote ")
        } else {
            style::dim("· would write ")
        };
        println!("{prefix}{} ({} bytes)", f.path.display(), f.bytes);
    }
    if !report.notes.is_empty() {
        println!();
        for n in &report.notes {
            let tag = match n.kind {
                NoteKind::Conflict => style::red(&format!("[{}]", n.kind)),
                NoteKind::Skipped => style::dim(&format!("[{}]", n.kind)),
                NoteKind::Inferred => style::yellow(&format!("[{}]", n.kind)),
                NoteKind::NotYetImplemented => style::yellow(&format!("[{}]", n.kind)),
            };
            println!("  {tag}  {msg}", msg = n.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_strips_scope() {
        assert_eq!(short_name("@acme/web"), "web");
        assert_eq!(short_name("just-a-name"), "just-a-name");
        assert_eq!(short_name("@acme/very/deep"), "deep");
    }

    #[test]
    fn jsonc_strips_line_and_block_comments() {
        // Rush's stock rush.json opens with a /* */ banner; the old
        // line-comment-only stripper rejected every real Rush repo.
        let src = r#"{
            /* banner
             * spanning lines
             */
            "a": 1, // trailing
            "b": "https://example.com//x", /* inline */ "c": "/* not a comment */"
        }"#;
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc_comments(src)).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "https://example.com//x");
        assert_eq!(v["c"], "/* not a comment */");
    }

    #[test]
    fn jsonc_keeps_line_numbers_across_block_comments() {
        let stripped = strip_jsonc_comments("{\n/* a\nb\nc */\n}");
        assert_eq!(stripped.lines().count(), 5);
    }

    #[test]
    fn task_block_omits_defaults() {
        let rendered = render_task(
            "build",
            &TaskBlock {
                run: "make build",
                ..Default::default()
            },
        );
        assert_eq!(rendered, "\n[tasks.build]\nrun = \"make build\"\n");
    }

    #[test]
    fn task_block_emits_ci_only_for_custom_names() {
        let lifecycle = render_task(
            "test",
            &TaskBlock {
                run: "x",
                ci: true,
                ..Default::default()
            },
        );
        assert!(!lifecycle.contains("ci = true"));
        let custom = render_task(
            "e2e",
            &TaskBlock {
                run: "x",
                ci: true,
                no_cache: true,
                ..Default::default()
            },
        );
        assert!(custom.contains("ci = true"));
        assert!(custom.contains("cache = false"));
    }
}
