//! `bento init` — workspace bootstrap with optional dish auto-detection.
//!
//! When run in a non-empty monorepo, walks subdirectories looking for
//! languages bento knows about and **adopts** each as a dish (writes only
//! `dish.toml`; sources untouched). Toolchain versions reported by each
//! adapter's `required_toolchain()` are merged into the top-level
//! `bento.toml [toolchain]` block.
//!
//! Detection uses the in-process registry only (no plugins) because there
//! is no `bento.toml` to read `[plugins]` filters from yet — chicken-and-egg.
//! Plugin languages can be adopted via `bento dish add` after init.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bento_adapters::DetectedTask;
use bento_core::AdapterRegistry;

/// One detected dish — what `run_init` will write a `dish.toml` for and
/// add to the bentos/prod.toml dishes list.
#[derive(Debug)]
pub struct DetectedDish {
    /// Path relative to the workspace root (forward-slash, stable key).
    pub rel: String,
    /// Absolute path on disk.
    pub abs: PathBuf,
    /// Dish name — last component of `rel`.
    pub name: String,
    /// Adapter id from `LanguageAdapter::id()`.
    pub language: String,
    /// Toolchain pin reported by the adapter, if any.
    pub toolchain: Option<(String, String)>,
}

/// Subdirectory names that are never worth recursing into. Build outputs,
/// vendored deps, hidden tooling state. Conservative — better to miss a
/// weird layout than walk a 100k-file `node_modules`.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".github",
    ".idea",
    ".vscode",
    ".bento",
    ".turbo",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".cache",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    "out",
    "__pycache__",
    "venv",
    ".venv",
    "env",
    ".tox",
    "coverage",
];

/// Maximum depth (in path components below `root`) to walk. `apps/api`
/// is depth 2, `services/foo/bar` is depth 3. Four is enough for every
/// monorepo layout we've seen and short-circuits pathological trees.
const MAX_DEPTH: usize = 4;

/// Walk `root`'s subdirectories looking for adapter matches. The root
/// itself is intentionally NOT considered — single-app repos can run
/// `bento dish add .` separately. Returns dishes in deterministic order
/// (sorted by relative path) so generated configs are stable across
/// machines.
pub fn detect_dishes(root: &Path, registry: &AdapterRegistry) -> Vec<DetectedDish> {
    let mut out: Vec<DetectedDish> = Vec::new();
    walk(root, root, 1, registry, &mut out);
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    registry: &AdapterRegistry,
    out: &mut Vec<DetectedDish>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    // Collect + sort for deterministic walk order.
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| !SKIP_DIRS.contains(&n) && !n.starts_with('.'))
                .unwrap_or(false)
        })
        .collect();
    subdirs.sort();

    for sub in subdirs {
        let Some(adapter) = registry.detect(&sub) else {
            walk(root, &sub, depth + 1, registry, out);
            continue;
        };

        let rel_path = sub.strip_prefix(root).unwrap_or(&sub).to_path_buf();
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        let Some(name) = rel_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let toolchain = adapter
            .required_toolchain(&sub)
            .ok()
            .flatten()
            .map(|tv| (tv.tool, tv.version));

        out.push(DetectedDish {
            rel,
            abs: sub,
            name: name.to_string(),
            language: adapter.id().to_string(),
            toolchain,
        });
        // Don't recurse into a matched dir — the dish IS this directory.
    }
}

/// Reduce per-dish toolchain pins to one pin per tool. Conflicting
/// versions (two dishes pinning different versions of the same tool)
/// keep the first-seen and surface a note for the caller to print.
pub struct ToolchainSummary {
    pub pins: BTreeMap<String, String>,
    pub conflicts: Vec<String>,
}

pub fn merge_toolchains(dishes: &[DetectedDish]) -> ToolchainSummary {
    let mut pins: BTreeMap<String, String> = BTreeMap::new();
    let mut conflicts: Vec<String> = Vec::new();

    for d in dishes {
        let Some((tool, version)) = &d.toolchain else {
            continue;
        };
        match pins.get(tool) {
            Some(existing) if existing != version => {
                conflicts.push(format!(
                    "tool '{tool}': dish '{}' wants {version}, kept earlier pin {existing} \
                     — override per-dish via [toolchain] in {}/dish.toml if needed",
                    d.name, d.rel
                ));
            }
            None => {
                pins.insert(tool.clone(), version.clone());
            }
            _ => {}
        }
    }
    ToolchainSummary { pins, conflicts }
}

/// Render a `bento.toml` body that includes the toolchain pins block
/// when non-empty. Matches the no-detect placeholder style.
pub fn render_bento_toml(pins: &BTreeMap<String, String>) -> String {
    let mut body = String::from(BENTO_TOML_PLACEHOLDER);
    if !pins.is_empty() {
        body.push('\n');
        body.push_str("[toolchain]\n");
        body.push_str("# Versions detected from your dishes during `bento init`. Override\n");
        body.push_str("# per-dish via [toolchain] in any dish.toml; remove use_system to\n");
        body.push_str("# install pinned toolchains under ~/.bento/tools/.\n");
        for (tool, version) in pins {
            body.push_str(&format!("{tool} = \"{version}\"\n"));
        }
    }
    body
}

/// Render a `bentos/prod.toml` body with the dish list pre-populated.
pub fn render_prod_toml(dish_rels: &[String]) -> String {
    if dish_rels.is_empty() {
        return PROD_TOML_PLACEHOLDER.to_string();
    }
    let mut body = String::from(
        "# bentos/prod.toml — your first bento (deployment unit).\n\
         #\n\
         # A dish name (derived from the directory basename) can appear in more\n\
         # than one bento. Its cache is shared across bentos.\n\
         \n\
         name = \"prod\"\n\
         dishes = [\n",
    );
    for rel in dish_rels {
        body.push_str(&format!("  \"{rel}\",\n"));
    }
    body.push_str("]\n");
    body
}

/// Adoption-style dish.toml body — name + language plus, optionally,
/// pre-populated `[tasks.<name>]` blocks reflecting scripts the adapter
/// detected in the project (e.g. `package.json` scripts). When `tasks`
/// is `None` or empty, falls back to the comment-only adapter-defaults
/// hint.
pub fn render_dish_toml(
    dish_name: &str,
    language_id: &str,
    tasks: Option<&[DetectedTask]>,
) -> String {
    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"{language_id}\"\n"
    );

    let tasks = tasks.unwrap_or(&[]);
    if tasks.is_empty() {
        body.push('\n');
        body.push_str(&format!(
            "# Adapter defaults for {language_id} cover build / test / lint.\n\
             # Override them by adding [tasks.<name>] blocks here — see\n\
             # `bento schema manifest` for the full input-manifest shape.\n"
        ));
        return body;
    }

    body.push('\n');
    body.push_str(
        "# Tasks below were detected from this project's manifest at init\n\
         # time. Edit, remove, or add new ones — `bento schema manifest`\n\
         # documents every field.\n",
    );
    for t in tasks {
        body.push('\n');
        body.push_str(&format!("[tasks.{}]\n", toml_table_key(&t.name)));
        body.push_str(&format!("run = {}\n", toml_basic_string(&t.run)));
    }
    body
}

/// Render a TOML key for a `[tasks.<key>]` table header. Bare keys
/// (ASCII alnum, `_`, `-`) pass through unquoted; anything else
/// (including `:`, `/`, dots) becomes a quoted string. Per the TOML
/// spec, dotted-table semantics apply when the key is unquoted, so
/// `test:coverage` MUST be quoted to avoid being parsed as a path.
pub(crate) fn toml_table_key(s: &str) -> String {
    let bare_ok = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare_ok {
        s.to_string()
    } else {
        toml_basic_string(s)
    }
}

/// Render a TOML basic string with the minimal escaping needed for the
/// strings we generate (script bodies + task names). Escapes `\` and `"`;
/// strings containing control characters fall back to a JSON-like escape
/// for the few we care about (newline, tab) — package.json scripts in
/// practice are single-line so this is defensive.
pub(crate) fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write `dish.toml` files for each detected dish. Skips any dir that
/// already has one (idempotent — previous partial init won't trip us).
/// Returns the relative paths that were actually written.
///
/// The `registry` is used to ask each detected dish's adapter for its
/// `detected_tasks(dir)` so the generated `dish.toml` can pre-populate
/// `[tasks.<name>]` blocks (e.g. mirroring `package.json` scripts).
pub fn write_dish_tomls(
    dishes: &[DetectedDish],
    registry: &AdapterRegistry,
) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for d in dishes {
        let dish_toml = d.abs.join("dish.toml");
        if dish_toml.exists() {
            continue;
        }
        let detected = registry
            .by_id(&d.language)
            .and_then(|a| a.detected_tasks(&d.abs));
        std::fs::write(
            &dish_toml,
            render_dish_toml(&d.name, &d.language, detected.as_deref()),
        )
        .with_context(|| format!("writing {}", dish_toml.display()))?;
        written.push(dish_toml);
    }
    Ok(written)
}

const BENTO_TOML_PLACEHOLDER: &str = r#"# bento.toml — repo-level defaults.
# Every field here is optional; the values shown match the built-in defaults.

[defaults]
# Max dishes to run in parallel within a single level of the dep graph.
# Omit to auto-size to `std::thread::available_parallelism()`.
# parallelism = 4

# Abort at the next level boundary on the first failed dish.
fail_fast = true

[cache]
# Local content-addressed cache at ~/.bento/cache.
local = true

# GitHub Actions cache tier. "auto" = on when running inside a workflow.
# gha = "auto"

# S3-compatible remote cache. Works with AWS S3, Cloudflare R2, MinIO, etc.
# Credentials from AWS env chain (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY).
# remote = "s3://my-bucket/bento-cache"
# remote_region = "us-east-1"
# remote_endpoint = "https://<account>.r2.cloudflarestorage.com"  # non-AWS only

[telemetry]
# Anonymous usage metrics. Set to false to opt out.
enabled = true
"#;

const PROD_TOML_PLACEHOLDER: &str = r#"# bentos/prod.toml — your first bento (deployment unit).
#
# A dish name (derived from the directory basename) can appear in more
# than one bento. Its cache is shared across bentos.

name = "prod"
dishes = []
"#;

/// HTML-comment markers wrapping the bento-managed section inside
/// AGENTS.md / CLAUDE.md. Looking for these on a re-run lets bento
/// upgrade the snippet idempotently without clobbering user prose.
const BENTO_BEGIN: &str = "<!-- bento:agent-instructions:BEGIN -->";
const BENTO_END: &str = "<!-- bento:agent-instructions:END -->";

/// What `merge_agent_file` did to the file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFileAction {
    /// File didn't exist; created with the bento section.
    Created,
    /// File existed without our markers; bento section appended,
    /// user prose preserved.
    Appended,
    /// File existed with our markers; bento section replaced in
    /// place, surrounding content preserved.
    Updated,
    /// File existed with our markers and they already matched the
    /// snippet exactly — no write.
    Unchanged,
}

impl AgentFileAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Appended => "appended",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
        }
    }
}

/// Header used only when bento creates a fresh `AGENTS.md`. Existing
/// files (with their own title) are left alone — only the marker
/// block gets added/updated.
pub const AGENTS_MD_HEADER: &str = "# AGENTS.md\n\n";

/// Header used only when bento creates a fresh `CLAUDE.md`.
pub const CLAUDE_MD_HEADER: &str = concat!(
    "# CLAUDE.md\n\n",
    "Project-level guidance for Claude Code. The canonical bento-uses-bento\n",
    "instructions live in [AGENTS.md](./AGENTS.md) — read those first.\n\n",
);

/// Bento snippet for `AGENTS.md` — what goes inside the marker block.
/// Header content above the markers (`# AGENTS.md` etc.) is supplied
/// by [`AGENTS_MD_HEADER`] and only used on first create.
pub fn render_agents_snippet() -> &'static str {
    AGENTS_MD_SNIPPET
}

/// Bento snippet for `CLAUDE.md`. Just the `@AGENTS.md` import,
/// wrapped in markers so re-running init doesn't duplicate it.
pub fn render_claude_snippet() -> &'static str {
    CLAUDE_MD_SNIPPET
}

/// Convenience: install/update the AGENTS.md at `path`.
pub fn install_agents_md(path: &Path) -> Result<AgentFileAction> {
    merge_agent_file(path, AGENTS_MD_HEADER, render_agents_snippet())
}

/// Convenience: install/update the CLAUDE.md at `path`.
pub fn install_claude_md(path: &Path) -> Result<AgentFileAction> {
    merge_agent_file(path, CLAUDE_MD_HEADER, render_claude_snippet())
}

/// Idempotent merge: on a fresh repo write `body`; on a repo that
/// already has the file, append a marker-delimited bento block
/// (preserving user prose); on re-run, replace the marker block in
/// place. Unchanged when the existing block already matches the
/// snippet bento would write.
///
/// `header` is prepended only when creating from scratch — it gives
/// the new file a sensible markdown title (e.g. `# AGENTS.md\n\n`).
pub fn merge_agent_file(path: &Path, header: &str, snippet: &str) -> Result<AgentFileAction> {
    let block = format!("{BENTO_BEGIN}\n{}\n{BENTO_END}\n", snippet.trim_end());
    if !path.exists() {
        let body = format!("{header}{block}");
        std::fs::write(path, body.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(AgentFileAction::Created);
    }
    let existing =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if let (Some(begin), Some(end_after_begin)) = (
        existing.find(BENTO_BEGIN),
        existing[existing
            .find(BENTO_BEGIN)
            .map(|i| i + BENTO_BEGIN.len())
            .unwrap_or(0)..]
            .find(BENTO_END)
            .map(|rel| {
                existing
                    .find(BENTO_BEGIN)
                    .map(|i| i + BENTO_BEGIN.len())
                    .unwrap_or(0)
                    + rel
                    + BENTO_END.len()
            }),
    ) {
        // Replace the existing block. Strip the block's own trailing
        // newline so we don't double-up at the boundary — the file's
        // existing post-block content already starts with whatever
        // separator the user (or our prior write) had there.
        let mut new = String::with_capacity(existing.len() + block.len());
        new.push_str(&existing[..begin]);
        new.push_str(block.trim_end_matches('\n'));
        new.push_str(&existing[end_after_begin..]);
        if new == existing {
            return Ok(AgentFileAction::Unchanged);
        }
        std::fs::write(path, new.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(AgentFileAction::Updated);
    }
    // No markers present — append at end with one blank-line separator.
    let mut new = existing;
    if !new.ends_with('\n') {
        new.push('\n');
    }
    new.push('\n');
    new.push_str(&block);
    std::fs::write(path, new.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(AgentFileAction::Appended)
}

const AGENTS_MD_SNIPPET: &str = r#"> **This repo is managed by [bento](https://bento.build) — a polyglot monorepo orchestrator. Always prefer `bento` verbs over native package managers (`npm`, `pnpm`, `cargo`, `go`, `pip`, `composer`, …). Bento routes each dish to the right tool, content-hashes results into a shared cache, and pins toolchains. Start every fresh session with `bento prime`.**

## Verb reference

| Task                                                      | Command                                                |
|---|---|
| Orient yourself in a fresh session                        | `bento prime`                                          |
| Install every dish's deps                                 | `bento install`                                        |
| Single dish only                                          | `bento install <dish>`                                 |
| Full CI pass (build + check + test + lint)                | `bento ci`                                             |
| Build / fast type-check / test / lint one target          | `bento <build\|check\|test\|lint> [bento-or-dish]`     |
| Add a dependency to a dish                                | `bento add <pkg>... --dish <d> [--dev]`                |
| Run an ad-hoc `[tasks.<name>]` block                      | `bento run <dish> <task> -- <args>`                    |
| Deploy to an environment                                  | `bento deploy --env <env>`                             |
| Re-send Slack / Linear notifications without re-deploying | `bento notify --env <env> [target]`                    |
| Explain a cache decision                                  | `bento why <cache-key-prefix>`                         |
| Health check (config + toolchains + integrations)         | `bento doctor`                                         |
| Show what would run without running it                    | `bento plan`                                           |
| Show resolved artifact paths per dish                     | `bento artifacts --json`                               |

## Hot tips

- **`bento prime` first.** Workspace inventory, cache state, plan preview, recommended next verb. Schema-stable JSON via `bento prime --json`.
- **Pass `--json` when reasoning about output.** Every reporting command emits structured JSON; `bento schema <target>` for the shape.
- **Don't parse stderr to decide what went wrong.** `bento ci --json` returns `executedTask.outcome` (tagged union — `kind: "failed"` carries `exit_code` and `stderr_excerpt`), plus structured `diagnostics[]` for compiler / linter errors.
- **Cache surprises are explicable.** `bento why <hash>` returns the full input manifest behind any cache key — adapter, toolchain, env-var names, every hashed file's blake3 digest.
- **Before `bento deploy`, run `bento doctor --env <env>`** — preflight fails fast with structured check names (`integration.railway.env`, …) so the right knob is named.
- **Never pass secret values on the CLI.** Use `[environments.<name>]` profiles in `bento.toml` for saved aliases, or `--secret-from DECLARED=SOURCE` for ad-hoc.

## When NOT to use bento

- Filesystem exploration (`ls`, `cat`, `grep`).
- One-off debugging (`psql`, `curl`, `dig`).
- Git — bento doesn't wrap git.

If a verb you need isn't listed above, ask before reaching for the native tool. The catalogue is `bento --help`.

## More

- **MCP server**: `bento-mcp` exposes every verb as a typed Model Context Protocol tool. Wire it into Claude Desktop / Claude Code / Cursor / Windsurf via `bento mcp install`.
- **Schemas**: `bento schema [plan|report|why|scaffold|doctor|manifest|error|diagnostics|garnish-payload|prime]`.
- **Skill bundle**: `~/.claude/skills/bento/` (installed by the bento installer) ships a PreToolUse hook that blocks native package managers with a hint.
"#;

const CLAUDE_MD_SNIPPET: &str = "@AGENTS.md\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
    }

    #[test]
    fn detect_finds_go_and_node_in_apps_dirs() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "apps/api/go.mod", "module x\n\ngo 1.22\n");
        write(root.path(), "apps/web/package.json", r#"{"name":"web"}"#);
        write(root.path(), "apps/web/package-lock.json", "{}");

        let registry = AdapterRegistry::builtin();
        let detected = detect_dishes(root.path(), &registry);
        let langs: Vec<_> = detected
            .iter()
            .map(|d| (d.rel.as_str(), d.language.as_str()))
            .collect();
        assert_eq!(langs, vec![("apps/api", "go"), ("apps/web", "node-npm")]);
    }

    #[test]
    fn detect_skips_node_modules_and_vendor() {
        let root = tempfile::tempdir().unwrap();
        // A real project at apps/api.
        write(root.path(), "apps/api/go.mod", "module x\n\ngo 1.22\n");
        // Decoys inside ignored dirs.
        write(
            root.path(),
            "node_modules/foo/package.json",
            r#"{"name":"foo"}"#,
        );
        write(root.path(), "node_modules/foo/package-lock.json", "{}");
        write(root.path(), "apps/api/vendor/bar/go.mod", "module bar\n");

        let detected = detect_dishes(root.path(), &AdapterRegistry::builtin());
        let rels: Vec<_> = detected.iter().map(|d| d.rel.as_str()).collect();
        assert_eq!(rels, vec!["apps/api"]);
    }

    #[test]
    fn detect_does_not_recurse_into_matched_dir() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "apps/api/go.mod", "module x\n\ngo 1.22\n");
        // A nested package.json inside the Go dish — should NOT be detected
        // as a separate npm dish; the dish IS apps/api.
        write(root.path(), "apps/api/web/package.json", r#"{"name":"x"}"#);
        write(root.path(), "apps/api/web/package-lock.json", "{}");

        let detected = detect_dishes(root.path(), &AdapterRegistry::builtin());
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].rel, "apps/api");
    }

    #[test]
    fn detect_skips_root_itself() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "go.mod", "module x\n\ngo 1.22\n");

        let detected = detect_dishes(root.path(), &AdapterRegistry::builtin());
        // Root-level go.mod isn't auto-detected — user runs `bento dish add .`.
        assert!(detected.is_empty());
    }

    #[test]
    fn detect_captures_toolchain_pin() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "apps/api/go.mod", "module x\n\ngo 1.22.3\n");
        let detected = detect_dishes(root.path(), &AdapterRegistry::builtin());
        assert_eq!(detected[0].toolchain, Some(("go".into(), "1.22.3".into())));
    }

    #[test]
    fn merge_toolchains_dedupes_agreeing_pins() {
        let dishes = vec![
            DetectedDish {
                rel: "a".into(),
                abs: PathBuf::new(),
                name: "a".into(),
                language: "go".into(),
                toolchain: Some(("go".into(), "1.22.3".into())),
            },
            DetectedDish {
                rel: "b".into(),
                abs: PathBuf::new(),
                name: "b".into(),
                language: "go".into(),
                toolchain: Some(("go".into(), "1.22.3".into())),
            },
        ];
        let summary = merge_toolchains(&dishes);
        assert_eq!(summary.pins.get("go"), Some(&"1.22.3".into()));
        assert!(summary.conflicts.is_empty());
    }

    #[test]
    fn merge_toolchains_reports_conflicts_keeps_first() {
        let dishes = vec![
            DetectedDish {
                rel: "a".into(),
                abs: PathBuf::new(),
                name: "a".into(),
                language: "node-npm".into(),
                toolchain: Some(("node".into(), "20".into())),
            },
            DetectedDish {
                rel: "b".into(),
                abs: PathBuf::new(),
                name: "b".into(),
                language: "node-npm".into(),
                toolchain: Some(("node".into(), "22".into())),
            },
        ];
        let summary = merge_toolchains(&dishes);
        assert_eq!(summary.pins.get("node"), Some(&"20".into()));
        assert_eq!(summary.conflicts.len(), 1);
        assert!(summary.conflicts[0].contains("node"));
    }

    #[test]
    fn render_bento_toml_includes_toolchain_when_present() {
        let mut pins = BTreeMap::new();
        pins.insert("go".into(), "1.22.3".into());
        pins.insert("node".into(), "22.1.0".into());
        let body = render_bento_toml(&pins);
        assert!(body.contains("[toolchain]"));
        assert!(body.contains("go = \"1.22.3\""));
        assert!(body.contains("node = \"22.1.0\""));
    }

    #[test]
    fn render_bento_toml_omits_toolchain_when_empty() {
        let body = render_bento_toml(&BTreeMap::new());
        assert!(!body.contains("[toolchain]"));
    }

    #[test]
    fn render_prod_toml_lists_dishes() {
        let body = render_prod_toml(&["apps/api".into(), "apps/web".into()]);
        assert!(body.contains("\"apps/api\""));
        assert!(body.contains("\"apps/web\""));
        assert!(!body.contains("dishes = []"));
    }

    #[test]
    fn render_prod_toml_falls_back_to_placeholder_when_empty() {
        let body = render_prod_toml(&[]);
        assert!(body.contains("dishes = []"));
    }

    #[test]
    fn render_agents_snippet_carries_canonical_directive() {
        let body = render_agents_snippet();
        // Headline directive — first thing the agent reads.
        assert!(
            body.contains("Always prefer `bento` verbs"),
            "AGENTS.md snippet must carry the prefer-bento directive"
        );
        for verb in ["bento prime", "bento install", "bento ci", "bento deploy"] {
            assert!(body.contains(verb), "AGENTS.md snippet must mention {verb}");
        }
        assert!(
            body.contains("MCP server"),
            "AGENTS.md snippet should point at the MCP server install path"
        );
        // Snippet must NOT carry its own h1 — the file header takes
        // care of that on first create, and we don't want a stray
        // `# AGENTS.md` injected into a user's existing file mid-doc.
        assert!(
            !body.starts_with("# AGENTS.md"),
            "snippet should not include the file header"
        );
    }

    #[test]
    fn render_claude_snippet_imports_agents_md() {
        let body = render_claude_snippet();
        assert!(body.contains("@AGENTS.md"));
        assert!(body.lines().count() < 5);
    }

    #[test]
    fn merge_agent_file_creates_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let action = merge_agent_file(&path, AGENTS_MD_HEADER, "snippet body").unwrap();
        assert_eq!(action, AgentFileAction::Created);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("# AGENTS.md"));
        assert!(body.contains(BENTO_BEGIN));
        assert!(body.contains("snippet body"));
        assert!(body.contains(BENTO_END));
    }

    #[test]
    fn merge_agent_file_appends_when_no_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let user_prose = "# My Project\n\nUser-curated agent guidance.\n";
        std::fs::write(&path, user_prose).unwrap();
        let action = merge_agent_file(&path, AGENTS_MD_HEADER, "bento body").unwrap();
        assert_eq!(action, AgentFileAction::Appended);
        let body = std::fs::read_to_string(&path).unwrap();
        // User prose intact at top.
        assert!(body.starts_with(user_prose));
        // Bento section appended.
        assert!(body.contains(BENTO_BEGIN));
        assert!(body.contains("bento body"));
        assert!(body.contains(BENTO_END));
    }

    #[test]
    fn merge_agent_file_replaces_existing_block_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let initial = format!(
            "# AGENTS.md\n\nUser intro.\n\n{BENTO_BEGIN}\nold body\n{BENTO_END}\n\nUser outro.\n"
        );
        std::fs::write(&path, &initial).unwrap();
        let action = merge_agent_file(&path, AGENTS_MD_HEADER, "new body").unwrap();
        assert_eq!(action, AgentFileAction::Updated);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("User intro."), "intro preserved");
        assert!(body.contains("User outro."), "outro preserved");
        assert!(body.contains("new body"));
        assert!(!body.contains("old body"), "old content gone");
        assert_eq!(body.matches(BENTO_BEGIN).count(), 1);
        assert_eq!(body.matches(BENTO_END).count(), 1);
    }

    #[test]
    fn merge_agent_file_is_unchanged_on_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let _ = merge_agent_file(&path, AGENTS_MD_HEADER, "stable body").unwrap();
        let action = merge_agent_file(&path, AGENTS_MD_HEADER, "stable body").unwrap();
        assert_eq!(action, AgentFileAction::Unchanged);
    }

    #[test]
    fn install_handles_claude_symlink_to_agents() {
        // Real-world repo convention: AGENTS.md is canonical,
        // CLAUDE.md is a symlink to it (so Claude Code reads the same
        // file other tools do). `bento init` must not write the
        // CLAUDE.md snippet through the symlink and overwrite the
        // canonical AGENTS.md content. Reproduces the bug seen on
        // ~/workspace/sift in v0.1.0.
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let agents = tmp.path().join("AGENTS.md");
            let claude = tmp.path().join("CLAUDE.md");
            std::fs::write(&agents, "# AGENTS.md\n\nUser prose.\n").unwrap();
            std::os::unix::fs::symlink("AGENTS.md", &claude).unwrap();

            // Mirror main.rs's run_init: install AGENTS first, skip
            // CLAUDE if it canonicalizes to the same file.
            let same = std::fs::canonicalize(&agents)
                .ok()
                .zip(std::fs::canonicalize(&claude).ok())
                .map(|(a, c)| a == c)
                .unwrap_or(false);
            assert!(same, "test setup: symlink should canonicalize to AGENTS");

            install_agents_md(&agents).unwrap();
            // The symlink-aware caller in main.rs would NOT call
            // install_claude_md here. We verify the AGENTS.md content
            // is intact (canonical snippet, not @AGENTS.md).
            let body = std::fs::read_to_string(&agents).unwrap();
            assert!(
                body.contains("Always prefer `bento` verbs"),
                "AGENTS.md must carry the canonical bento snippet, got:\n{body}"
            );
            assert!(
                !body.contains("\n@AGENTS.md\n"),
                "AGENTS.md must NOT carry the CLAUDE.md `@AGENTS.md` import — that's the bug",
            );
            // CLAUDE.md (the symlink) reads the same content via the
            // symlink, so Claude Code naturally picks up AGENTS.md.
            let claude_body = std::fs::read_to_string(&claude).unwrap();
            assert_eq!(body, claude_body, "symlink should serve identical bytes");
        }
    }

    #[test]
    fn merge_agent_file_orphan_begin_marker_falls_back_to_append() {
        // Defensive: if a user deleted the END marker we still want a
        // recoverable state — append a fresh full block at the end
        // rather than overwriting past the orphan begin.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        let initial = format!("# Project\n\n{BENTO_BEGIN}\nstray\n");
        std::fs::write(&path, &initial).unwrap();
        let action = merge_agent_file(&path, AGENTS_MD_HEADER, "new body").unwrap();
        assert_eq!(action, AgentFileAction::Appended);
        let body = std::fs::read_to_string(&path).unwrap();
        // Orphan begin marker preserved; new full block appended at end.
        assert_eq!(body.matches(BENTO_BEGIN).count(), 2);
        assert!(body.contains(BENTO_END));
        assert!(body.contains("new body"));
    }

    #[test]
    fn render_dish_toml_minimal() {
        let body = render_dish_toml("api", "go", None);
        assert!(body.contains("name = \"api\""));
        assert!(body.contains("language = \"go\""));
        // Adoption-mode dish.toml without detected tasks MUST NOT define
        // its own tasks — it relies on adapter defaults. The string
        // "[tasks." may appear in the comment text only.
        assert!(!body.contains("\n[tasks."));
    }

    #[test]
    fn render_dish_toml_with_empty_tasks_falls_back_to_defaults_hint() {
        let body = render_dish_toml("api", "node-npm", Some(&[]));
        assert!(!body.contains("\n[tasks."));
        assert!(body.contains("Adapter defaults"));
    }

    #[test]
    fn render_dish_toml_emits_blocks_for_detected_tasks() {
        let tasks = vec![
            DetectedTask {
                name: "build".into(),
                run: "npm run build".into(),
            },
            DetectedTask {
                name: "test".into(),
                run: "npm test".into(),
            },
        ];
        let body = render_dish_toml("web", "node-npm", Some(&tasks));
        assert!(body.contains("[tasks.build]"));
        assert!(body.contains("run = \"npm run build\""));
        assert!(body.contains("[tasks.test]"));
        assert!(body.contains("run = \"npm test\""));
        // No defaults hint when tasks were emitted.
        assert!(!body.contains("Adapter defaults"));
    }

    #[test]
    fn render_dish_toml_quotes_keys_with_colons() {
        let tasks = vec![DetectedTask {
            name: "test:coverage".into(),
            run: "vitest --coverage".into(),
        }];
        let body = render_dish_toml("web", "node-npm", Some(&tasks));
        assert!(body.contains("[tasks.\"test:coverage\"]"));
        assert!(body.contains("run = \"vitest --coverage\""));
    }

    #[test]
    fn render_dish_toml_escapes_quotes_in_run_command() {
        let tasks = vec![DetectedTask {
            name: "greet".into(),
            run: r#"echo "hi""#.into(),
        }];
        let body = render_dish_toml("x", "node-npm", Some(&tasks));
        assert!(body.contains(r#"run = "echo \"hi\"""#));
    }

    #[test]
    fn write_dish_tomls_emits_detected_tasks_for_node() {
        let tmp = tempfile::tempdir().unwrap();
        // Real npm project so the registry's NodeNpmAdapter detects it
        // and detected_tasks returns the package.json scripts.
        write(
            tmp.path(),
            "apps/web/package.json",
            r#"{"scripts":{"build":"vite","custom":"node ./tools/x.mjs"}}"#,
        );
        write(tmp.path(), "apps/web/package-lock.json", "{}");

        let registry = AdapterRegistry::builtin();
        let detected = detect_dishes(tmp.path(), &registry);
        write_dish_tomls(&detected, &registry).unwrap();

        let dish_toml = std::fs::read_to_string(tmp.path().join("apps/web/dish.toml")).unwrap();
        assert!(dish_toml.contains("[tasks.build]"));
        assert!(dish_toml.contains("run = \"npm run build\""));
        assert!(dish_toml.contains("[tasks.custom]"));
        assert!(dish_toml.contains("run = \"npm run custom\""));
    }
}
