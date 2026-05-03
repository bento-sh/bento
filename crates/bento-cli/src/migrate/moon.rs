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
//! - `tasks.<name>.options.cache: false` — bento has no per-task no-cache
//!   flag; surfaced as a `Skipped` note.
//! - Toolchain blocks (`node:`, `rust:`, `python:` at workspace.yml top
//!   level) — bento has its own `[toolchain]` block in `bento.toml`;
//!   surfaced as `Inferred` so the user can copy versions across.
//! - Unknown languages — `language = "node-npm"` placeholder + a note.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::init::{toml_basic_string, toml_table_key};

use super::{MigrationReport, NoteKind};

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

    // 1. Load .moon/workspace.yml.
    let ws_path = opts.root.join(".moon").join("workspace.yml");
    let ws_text =
        fs::read_to_string(&ws_path).with_context(|| format!("opening {}", ws_path.display()))?;
    let ws_doc =
        parse_yaml(&ws_text).with_context(|| format!("parsing {} as YAML", ws_path.display()))?;

    // 2. Surface toolchain blocks as `Inferred` notes — bento has its
    //    own [toolchain] block in bento.toml; we don't auto-port versions.
    for tool in TOOLCHAIN_KEYS {
        if let Some(YamlValue::Map(m)) = ws_doc.get(tool) {
            // Try to surface the version if it's a simple `version: "..."`
            // — purely for the note message; we don't write it anywhere.
            let version = m.get("version").and_then(|v| match v {
                YamlValue::Scalar(s) => Some(s.as_str()),
                _ => None,
            });
            let extra = match version {
                Some(v) => format!(" (version: {v})"),
                None => String::new(),
            };
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "workspace.yml has a `{tool}:` toolchain block{extra} — bento uses its \
                     own `[toolchain]` block in bento.toml; copy the version across by hand."
                ),
            );
        }
    }

    // 3. Resolve `projects:` field — array of globs or object map.
    let project_dirs = match ws_doc.get("projects") {
        Some(YamlValue::Array(globs)) => {
            let globs: Vec<String> = globs
                .iter()
                .filter_map(|v| match v {
                    YamlValue::Scalar(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            discover_via_globs(&opts.root, &globs)?
        }
        Some(YamlValue::Map(map)) => {
            let mut out: Vec<PathBuf> = Vec::new();
            for (_id, val) in map {
                if let YamlValue::Scalar(rel) = val {
                    let dir = opts.root.join(rel);
                    if dir.is_dir() {
                        out.push(dir);
                    }
                }
            }
            out.sort();
            out
        }
        _ => Vec::new(),
    };

    if project_dirs.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            "workspace.yml has no `projects:` entries (or none resolved to a dir) — \
             nothing to migrate",
        );
        return Ok(report);
    }

    // 4. For each project dir with a moon.yml, parse + emit a dish.toml.
    let mut dish_rels: Vec<String> = Vec::new();
    for dir in &project_dirs {
        let moon_yml = dir.join("moon.yml");
        if !moon_yml.exists() {
            continue;
        }
        let body_text = fs::read_to_string(&moon_yml)
            .with_context(|| format!("opening {}", moon_yml.display()))?;
        let doc = parse_yaml(&body_text)
            .with_context(|| format!("parsing {} as YAML", moon_yml.display()))?;

        let dish_toml_path = dir.join("dish.toml");
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
        let body = render_dish_toml(dir, &doc, &mut report);
        write_or_simulate(&dish_toml_path, &body, opts.dry_run, &mut report)?;
        dish_rels.push(relative(dir, &opts.root).display().to_string());
    }

    // 5. Workspace bento.toml — placeholder shape; user fills in cache
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

    // 6. bentos/prod.toml — list every dish the migrator created.
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

/// Resolve one `projects:` glob. Same shape as the turbo migrator's
/// helper — supports `<seg>/*`, `<seg>/**`, and literal paths.
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
        // Treat the same as /* — Moon's recursive globs are rare in
        // practice and the user can list nested entries explicitly.
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

fn render_dish_toml(dir: &Path, doc: &YamlMap, report: &mut MigrationReport) -> String {
    let dish_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dish")
        .to_string();

    let language_raw = match doc.get("language") {
        Some(YamlValue::Scalar(s)) => Some(s.as_str()),
        _ => None,
    };
    let language_id = match language_raw {
        Some("typescript") | Some("javascript") | Some("node") => {
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{} → language `{}` mapped to `node-npm` — switch to `node-pnpm`, \
                     `node-yarn`, or `node-bun` if you use a different package manager.",
                    relative(dir, dir.parent().unwrap_or(dir)).display(),
                    language_raw.unwrap_or("node")
                ),
            );
            "node-npm"
        }
        Some("rust") => "cargo",
        Some("go") => "go",
        Some("python") => "python",
        Some("ruby") => "ruby",
        Some("php") => "php",
        Some(other) => {
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{} → unknown moon language `{other}` — defaulted dish.toml `language = \
                     \"node-npm\"`; edit by hand if your project uses a different toolchain.",
                    dish_name
                ),
            );
            "node-npm"
        }
        None => {
            report.push_note(
                NoteKind::Inferred,
                format!(
                    "{} → moon.yml has no `language` field — defaulted to `node-npm`; edit \
                     by hand to match your project's toolchain.",
                    dish_name
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

    if let Some(YamlValue::Map(tasks)) = doc.get("tasks") {
        for (task_name, task_val) in tasks {
            let YamlValue::Map(task) = task_val else {
                continue;
            };

            // command + args → "run" string
            let cmd = scalar_or_array_joined(task.get("command"));
            let args = scalar_or_array_joined(task.get("args"));
            let run_str = match (cmd.is_empty(), args.is_empty()) {
                (true, true) => continue, // nothing to run, skip silently
                (true, false) => args,
                (false, true) => cmd,
                (false, false) => format!("{cmd} {args}"),
            };

            // options.cache: false → Skipped note
            if let Some(YamlValue::Map(opts)) = task.get("options") {
                if matches!(opts.get("cache"), Some(YamlValue::Scalar(s)) if s == "false") {
                    report.push_note(
                        NoteKind::Skipped,
                        format!(
                            "task `{task_name}` has `options.cache: false` — bento has no \
                             per-task no-cache flag; the task still runs but its output WILL \
                             be cached. Use `bento --no-cache` for ad-hoc bypass."
                        ),
                    );
                }
            }

            // deps → Inferred note
            if let Some(YamlValue::Array(deps)) = task.get("deps") {
                let dep_strs: Vec<String> = deps
                    .iter()
                    .filter_map(|v| match v {
                        YamlValue::Scalar(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if !dep_strs.is_empty() {
                    report.push_note(
                        NoteKind::Inferred,
                        format!(
                            "task `{task_name}` had deps = {dep_strs:?} — bento derives task \
                             ordering from the dish graph; cross-project refs (`^:<task>`, \
                             `<project>:<task>`) map to dish.toml `depends_on` between dishes \
                             (wire by hand)."
                        ),
                    );
                }
            }

            body.push('\n');
            body.push_str(&format!("[tasks.{}]\n", toml_table_key(task_name)));
            body.push_str(&format!("run = {}\n", toml_basic_string(&run_str)));

            if let Some(YamlValue::Array(inputs)) = task.get("inputs") {
                let xs: Vec<String> = inputs
                    .iter()
                    .filter_map(|v| match v {
                        YamlValue::Scalar(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if !xs.is_empty() {
                    body.push_str(&format!("inputs = {}\n", render_string_array(&xs)));
                }
            }
            if let Some(YamlValue::Array(outputs)) = task.get("outputs") {
                let xs: Vec<String> = outputs
                    .iter()
                    .filter_map(|v| match v {
                        YamlValue::Scalar(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if !xs.is_empty() {
                    body.push_str(&format!("outputs = {}\n", render_string_array(&xs)));
                }
            }
        }
    }

    body
}

fn scalar_or_array_joined(v: Option<&YamlValue>) -> String {
    match v {
        Some(YamlValue::Scalar(s)) => s.clone(),
        Some(YamlValue::Array(xs)) => xs
            .iter()
            .filter_map(|v| match v {
                YamlValue::Scalar(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
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

// ── Tiny YAML parser (Moon subset) ─────────────────────────────────
//
// Supports exactly what Moon configs need:
//   - 2-space-indented block-style maps
//   - block-style arrays (`- item`)
//   - flow-style arrays (`[a, b, c]`)
//   - scalar strings (bare, single-quoted, double-quoted)
//   - `# comments` to end of line
//
// Does NOT support: YAML anchors, tags, multi-line strings (`|` / `>`),
// nested flow maps, multiple documents, complex types. If a real-world
// Moon config exercises something more exotic, we either accept the
// degraded output or grow the parser then.

#[derive(Debug, Clone, PartialEq)]
enum YamlValue {
    Scalar(String),
    Array(Vec<YamlValue>),
    Map(YamlMap),
}

type YamlMap = Vec<(String, YamlValue)>;

trait YamlMapExt {
    fn get(&self, key: &str) -> Option<&YamlValue>;
}

impl YamlMapExt for YamlMap {
    fn get(&self, key: &str) -> Option<&YamlValue> {
        self.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

fn parse_yaml(text: &str) -> Result<YamlMap> {
    // Pre-pass: strip comments, blank lines, normalise tabs (rare but
    // tolerable — we treat a leading tab as 2 spaces). Track line numbers
    // for error context.
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(_, l)| !is_blank_or_comment(l))
        .collect();

    let mut idx = 0;
    parse_map(&lines, &mut idx, 0)
}

fn is_blank_or_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// Strip a trailing `# comment` (only when the `#` is preceded by
/// whitespace — a `#` inside a quoted string is fine for our subset
/// because Moon configs don't use unquoted strings containing `#`).
fn strip_trailing_comment(s: &str) -> &str {
    // Find a `#` preceded by whitespace, outside quotes.
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                return s[..i].trim_end();
            }
            _ => {}
        }
    }
    s.trim_end()
}

fn indent_of(line: &str) -> usize {
    let mut n = 0;
    for c in line.chars() {
        match c {
            ' ' => n += 1,
            '\t' => n += 2,
            _ => break,
        }
    }
    n
}

fn parse_map(lines: &[(usize, &str)], idx: &mut usize, base_indent: usize) -> Result<YamlMap> {
    let mut out: YamlMap = Vec::new();
    while *idx < lines.len() {
        let (lineno, raw) = lines[*idx];
        let indent = indent_of(raw);
        if indent < base_indent {
            break;
        }
        let stripped = strip_trailing_comment(raw);
        let content = stripped.trim_start();
        if content.is_empty() {
            *idx += 1;
            continue;
        }
        if content.starts_with('-') {
            // List item at this indent — caller should be parse_array,
            // not parse_map. Bail back up.
            break;
        }

        // Expect `key: <maybe value>`.
        let colon = content
            .find(':')
            .with_context(|| format!("line {lineno}: expected `key: value`, got `{content}`"))?;
        let key = unquote(content[..colon].trim()).to_string();
        let rest = content[colon + 1..].trim();

        *idx += 1;

        if rest.is_empty() {
            // Nested map or array — peek next non-blank line.
            if *idx >= lines.len() {
                out.push((key, YamlValue::Scalar(String::new())));
                continue;
            }
            let (_, next_raw) = lines[*idx];
            let next_indent = indent_of(next_raw);
            if next_indent <= indent {
                // Empty value: `key:` with nothing nested.
                out.push((key, YamlValue::Scalar(String::new())));
                continue;
            }
            let next_stripped = strip_trailing_comment(next_raw).trim_start();
            if next_stripped.starts_with('-') {
                let arr = parse_array(lines, idx, next_indent)?;
                out.push((key, YamlValue::Array(arr)));
            } else {
                let map = parse_map(lines, idx, next_indent)?;
                out.push((key, YamlValue::Map(map)));
            }
        } else {
            // Inline scalar or flow-array.
            out.push((key, parse_inline_value(rest)?));
        }
    }
    Ok(out)
}

fn parse_array(
    lines: &[(usize, &str)],
    idx: &mut usize,
    base_indent: usize,
) -> Result<Vec<YamlValue>> {
    let mut out: Vec<YamlValue> = Vec::new();
    while *idx < lines.len() {
        let (lineno, raw) = lines[*idx];
        let indent = indent_of(raw);
        if indent < base_indent {
            break;
        }
        let stripped = strip_trailing_comment(raw);
        let content = stripped.trim_start();
        if !content.starts_with('-') {
            break;
        }
        let after_dash = content[1..].trim_start();
        *idx += 1;

        if after_dash.is_empty() {
            // Nested map/array under this dash — parse from the next line.
            if *idx >= lines.len() {
                out.push(YamlValue::Scalar(String::new()));
                continue;
            }
            let (_, next_raw) = lines[*idx];
            let next_indent = indent_of(next_raw);
            if next_indent <= indent {
                out.push(YamlValue::Scalar(String::new()));
                continue;
            }
            let next_stripped = strip_trailing_comment(next_raw).trim_start();
            if next_stripped.starts_with('-') {
                let arr = parse_array(lines, idx, next_indent)?;
                out.push(YamlValue::Array(arr));
            } else {
                let map = parse_map(lines, idx, next_indent)?;
                out.push(YamlValue::Map(map));
            }
        } else if after_dash.contains(':') && !is_quoted(after_dash) {
            // Inline map: `- key: value` — bento doesn't use this for
            // the moon shapes we care about, but tolerate it. We treat
            // the whole rest as a single-entry map and look for further
            // entries on subsequent more-indented lines.
            let _ = lineno;
            let colon = after_dash.find(':').unwrap();
            let key = unquote(after_dash[..colon].trim()).to_string();
            let rest = after_dash[colon + 1..].trim();
            let mut m: YamlMap = Vec::new();
            if rest.is_empty() {
                if *idx < lines.len() {
                    let (_, peek) = lines[*idx];
                    let peek_indent = indent_of(peek);
                    if peek_indent > indent {
                        let peek_stripped = strip_trailing_comment(peek).trim_start();
                        let val = if peek_stripped.starts_with('-') {
                            YamlValue::Array(parse_array(lines, idx, peek_indent)?)
                        } else {
                            YamlValue::Map(parse_map(lines, idx, peek_indent)?)
                        };
                        m.push((key, val));
                    } else {
                        m.push((key, YamlValue::Scalar(String::new())));
                    }
                } else {
                    m.push((key, YamlValue::Scalar(String::new())));
                }
            } else {
                m.push((key, parse_inline_value(rest)?));
            }
            // Pick up additional keys at the same indent as the dash's
            // child (which is `indent + 2` typically). parse_map walks
            // every key at that indent in one shot, so we only need to
            // call it once — no loop required.
            let child_indent = indent + 2;
            if *idx < lines.len() {
                let (_, peek) = lines[*idx];
                let peek_indent = indent_of(peek);
                let peek_stripped = strip_trailing_comment(peek).trim_start();
                if peek_indent >= child_indent && !peek_stripped.starts_with('-') {
                    let extras = parse_map(lines, idx, child_indent)?;
                    m.extend(extras);
                }
            }
            out.push(YamlValue::Map(m));
        } else {
            out.push(parse_inline_value(after_dash)?);
        }
    }
    Ok(out)
}

fn is_quoted(s: &str) -> bool {
    (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))
}

fn parse_inline_value(s: &str) -> Result<YamlValue> {
    let s = s.trim();
    if s.starts_with('[') && s.ends_with(']') {
        return Ok(YamlValue::Array(parse_flow_array(&s[1..s.len() - 1])?));
    }
    if s.starts_with('{') && s.ends_with('}') {
        // Flow-style maps are rare in Moon configs but show up in
        // the toolchain test fixture: `node: { version: "20.0.0" }`.
        return Ok(YamlValue::Map(parse_flow_map(&s[1..s.len() - 1])?));
    }
    Ok(YamlValue::Scalar(unquote(s).to_string()))
}

fn parse_flow_array(s: &str) -> Result<Vec<YamlValue>> {
    let mut out = Vec::new();
    for piece in split_flow(s, ',') {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(parse_inline_value(trimmed)?);
    }
    Ok(out)
}

fn parse_flow_map(s: &str) -> Result<YamlMap> {
    let mut out: YamlMap = Vec::new();
    for piece in split_flow(s, ',') {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        let colon = trimmed
            .find(':')
            .with_context(|| format!("flow map entry missing `:` — `{trimmed}`"))?;
        let key = unquote(trimmed[..colon].trim()).to_string();
        let val = parse_inline_value(trimmed[colon + 1..].trim())?;
        out.push((key, val));
    }
    Ok(out)
}

/// Split `s` on `sep` while honouring quotes and bracket nesting. So
/// `a, [b, c], d` splits into 3 not 5.
fn split_flow(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth_sq = 0i32;
    let mut depth_br = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(c);
            }
            '[' if !in_single && !in_double => {
                depth_sq += 1;
                cur.push(c);
            }
            ']' if !in_single && !in_double => {
                depth_sq -= 1;
                cur.push(c);
            }
            '{' if !in_single && !in_double => {
                depth_br += 1;
                cur.push(c);
            }
            '}' if !in_single && !in_double => {
                depth_br -= 1;
                cur.push(c);
            }
            c if c == sep && !in_single && !in_double && depth_sq == 0 && depth_br == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
             \x20\x20\x20\x20command: vitest run\n",
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
    fn refuses_to_overwrite_without_force() {
        let tmp = fixture_two_projects();
        // Pre-create one of the dish.tomls so the migrator hits a conflict.
        std::fs::write(
            tmp.path().join("apps/web/dish.toml"),
            "name = \"existing\"\n",
        )
        .unwrap();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(report.has_conflicts());
        // The existing dish.toml stays untouched.
        let body = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert_eq!(body, "name = \"existing\"\n");
        // The fresh project still gets written.
        assert!(tmp.path().join("apps/api/dish.toml").exists());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = fixture_two_projects();
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
    fn maps_language_typescript_to_node_npm() {
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
        let _ = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let dish = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-npm""#));
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
        let _ = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let has_inferred = report
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Inferred && n.message.contains("^:build"));
        assert!(
            has_inferred,
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let has_toolchain_note = report.notes.iter().any(|n| {
            n.kind == NoteKind::Inferred
                && n.message.contains("node:")
                && n.message.contains("[toolchain]")
        });
        assert!(
            has_toolchain_note,
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
            "language: typescript\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: tsc\n",
        );
        write_moon_yml(
            tmp.path(),
            "apps/api",
            "language: rust\n\
             tasks:\n\
             \x20\x20build:\n\
             \x20\x20\x20\x20command: cargo build\n",
        );
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
    }

    // ── parser sanity ──────────────────────────────────────────────

    #[test]
    fn parser_handles_array_and_map_mix() {
        let body = "projects:\n  - \"apps/*\"\n  - \"packages/*\"\nvcs:\n  manager: git\n";
        let doc = parse_yaml(body).unwrap();
        match doc.get("projects") {
            Some(YamlValue::Array(xs)) => {
                assert_eq!(xs.len(), 2);
                assert!(matches!(&xs[0], YamlValue::Scalar(s) if s == "apps/*"));
            }
            other => panic!("expected array, got {other:?}"),
        }
        match doc.get("vcs") {
            Some(YamlValue::Map(m)) => {
                assert!(matches!(m.get("manager"), Some(YamlValue::Scalar(s)) if s == "git"))
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn parser_handles_flow_array() {
        let body = "projects: [\"apps/*\", \"packages/*\"]\n";
        let doc = parse_yaml(body).unwrap();
        match doc.get("projects") {
            Some(YamlValue::Array(xs)) => {
                assert_eq!(xs.len(), 2);
                assert!(matches!(&xs[1], YamlValue::Scalar(s) if s == "packages/*"));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn parser_strips_comments() {
        let body = "# top comment\nprojects:\n  - \"apps/*\" # inline\n";
        let doc = parse_yaml(body).unwrap();
        match doc.get("projects") {
            Some(YamlValue::Array(xs)) => {
                assert_eq!(xs.len(), 1);
                assert!(matches!(&xs[0], YamlValue::Scalar(s) if s == "apps/*"));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn parser_handles_command_as_array() {
        let body = "tasks:\n  build:\n    command:\n      - vitest\n      - run\n";
        let doc = parse_yaml(body).unwrap();
        let tasks = match doc.get("tasks") {
            Some(YamlValue::Map(m)) => m,
            other => panic!("{other:?}"),
        };
        let build = match tasks.get("build") {
            Some(YamlValue::Map(m)) => m,
            other => panic!("{other:?}"),
        };
        let joined = scalar_or_array_joined(build.get("command"));
        assert_eq!(joined, "vitest run");
    }
}
