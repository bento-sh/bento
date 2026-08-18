//! `bento mcp install` — register `bento-mcp` as an MCP server in the
//! various agent clients' config files so a fresh agent picks up the
//! bento verb surface as typed tool calls without the user editing
//! JSON by hand.
//!
//! Supported clients (config paths):
//!
//! - **Claude Code**: `~/.claude.json` (user — single dotfile that
//!   holds all Claude Code state, including `mcpServers`) or
//!   `.mcp.json` at the project root (project-scoped, with `--local`).
//! - **Claude Desktop**: `~/Library/Application Support/Claude/
//!   claude_desktop_config.json` (macOS) / `~/.config/Claude/
//!   claude_desktop_config.json` (Linux).
//! - **Cursor**: `~/.cursor/mcp.json` (user) or `.cursor/mcp.json`
//!   (project, with `--local`).
//! - **Windsurf**: `~/.codeium/windsurf/mcp_config.json` (no
//!   project-local variant in current Windsurf).
//! - **Codex CLI**: `~/.codex/config.toml` (user) or
//!   `.codex/config.toml` at the project root (with `--local`).
//!   TOML — entries land under `[mcp_servers.<name>]`.
//! - **OpenCode**: `~/.config/opencode/opencode.json` (user) or
//!   `opencode.json` at the project root (with `--local`). Top-level
//!   key is `mcp`; entries carry a `type: "local"` discriminator and
//!   the `command` field is a single array (binary + args together).
//! - **Zed**: `~/.config/zed/settings.json` (user) or
//!   `.zed/settings.json` (with `--local`). Top-level key is
//!   `context_servers` (otherwise the same shape as `mcpServers`).
//!
//! Most clients accept the `{ "mcpServers": { "<key>": { "command":
//! "...", "args": [...] } } }` JSON shape; Zed swaps the wrapper key
//! to `context_servers`, OpenCode flattens `command` into a single
//! array under a top-level `mcp` key, and Codex uses TOML.
//! Re-running `bento mcp install` updates the existing record rather
//! than creating a duplicate.
//!
//! Writes are atomic (tmp file in the same dir, then rename) and
//! inherit the target's mode. Pre-existing user content under other
//! server keys is preserved; `//` and `/* */` comments (Zed's
//! settings.json ships with them) are tolerated on read, and a
//! commented file is spliced rather than re-serialised when we're
//! adding a top-level key. Claude Code's `~/.claude.json` is written
//! through `claude mcp add` whenever that CLI is on PATH — it's the
//! owner of that file and a live session rewrites it constantly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::cli::McpClient;
use crate::style;

/// One client's resolved config path + whether the file existed
/// before this run. Returned to callers for human/JSON output.
#[derive(Debug, Clone)]
pub struct InstallResult {
    pub client: McpClient,
    pub path: PathBuf,
    pub existed_before: bool,
    pub action: InstallAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    /// Created a new config file.
    Created,
    /// Added the bento server entry to an existing config.
    Added,
    /// Updated an existing bento entry (different command/args).
    Updated,
    /// Entry already matched — no-op.
    Unchanged,
}

impl InstallAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
        }
    }
}

/// Resolve the absolute config path for `client` + scope (`local` ⇒
/// project-relative, otherwise user-global). Returns `None` when the
/// requested combination isn't supported on this platform — the
/// caller surfaces a friendly error.
pub fn config_path(client: McpClient, local: bool, cwd: &Path) -> Result<Option<PathBuf>> {
    let home =
        || -> Result<PathBuf> { dirs::home_dir().context("could not resolve user home directory") };
    Ok(match client {
        McpClient::Auto => None, // resolved by the caller via expand_auto.
        McpClient::ClaudeCode => Some(if local {
            // Project-scoped MCP servers live in `.mcp.json` at the
            // repo root (Claude Code reads this at session start when
            // it's checked in to the project).
            cwd.join(".mcp.json")
        } else {
            // User-scoped state — including `mcpServers` — lives in a
            // single dotfile, NOT under `~/.claude/`. The `~/.claude/`
            // directory is for `settings.json`, `skills/`, etc.;
            // `~/.claude.json` is the authoritative MCP-server source.
            home()?.join(".claude.json")
        }),
        McpClient::Cursor => Some(if local {
            cwd.join(".cursor").join("mcp.json")
        } else {
            home()?.join(".cursor").join("mcp.json")
        }),
        McpClient::Windsurf => {
            if local {
                anyhow::bail!(
                    "Windsurf doesn't support project-local MCP config — \
                     drop `--local` to write the user-global config at \
                     `~/.codeium/windsurf/mcp_config.json`."
                );
            }
            Some(
                home()?
                    .join(".codeium")
                    .join("windsurf")
                    .join("mcp_config.json"),
            )
        }
        McpClient::Codex => Some(if local {
            // Codex CLI honours `.codex/config.toml` at the repo root
            // for trusted projects.
            cwd.join(".codex").join("config.toml")
        } else {
            home()?.join(".codex").join("config.toml")
        }),
        McpClient::Opencode => Some(if local {
            // OpenCode walks up to the nearest git root to find this.
            cwd.join("opencode.json")
        } else {
            // XDG-style (`$HOME/.config/opencode/`). OpenCode honours
            // `OPENCODE_CONFIG_DIR` overrides at runtime; we don't
            // chase those — `bento mcp install` is for the canonical
            // path.
            home()?
                .join(".config")
                .join("opencode")
                .join("opencode.json")
        }),
        McpClient::Zed => Some(if local {
            cwd.join(".zed").join("settings.json")
        } else {
            home()?.join(".config").join("zed").join("settings.json")
        }),
        McpClient::ClaudeDesktop => {
            if local {
                anyhow::bail!(
                    "Claude Desktop doesn't support project-local MCP config — \
                     drop `--local` to write the user-global config."
                );
            }
            // Per-OS path. Windows omitted: not a supported install
            // target for the bento installer (install.sh is Linux +
            // macOS only). If a Windows user reaches this verb they
            // can still pass `bento mcp install cursor` / `bento mcp
            // install claude-code` (positional) which both have
            // OS-agnostic paths.
            #[cfg(target_os = "macos")]
            let p = home()?
                .join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json");
            #[cfg(all(unix, not(target_os = "macos")))]
            let p = home()?
                .join(".config")
                .join("Claude")
                .join("claude_desktop_config.json");
            #[cfg(not(unix))]
            let p: PathBuf = anyhow::bail!(
                "Claude Desktop config path on this OS isn't supported by `bento mcp install` \
                 yet — open the MCP settings in Claude Desktop and add a server entry by hand."
            );
            Some(p)
        }
    })
}

/// Expand `Auto` to every client whose user-global presence we can
/// detect. Per-client because each client leaves a different marker
/// (Cursor a `~/.cursor/` dir, Claude Code a `~/.claude.json` *or*
/// `~/.claude/` dir, etc.). The earlier "parent dir exists" heuristic
/// breaks for `~/.claude.json` since its parent is `$HOME` and would
/// always match. Honours `--local` for the per-project case.
pub fn expand_auto(local: bool, cwd: &Path) -> Result<Vec<McpClient>> {
    let candidates = [
        McpClient::ClaudeCode,
        McpClient::Cursor,
        McpClient::Windsurf,
        McpClient::ClaudeDesktop,
        McpClient::Codex,
        McpClient::Opencode,
        McpClient::Zed,
    ];
    let home = dirs::home_dir();
    let mut out = Vec::new();
    for c in candidates {
        // Skip clients that fail config_path (e.g. local-only-not-supported).
        let path = match config_path(c, local, cwd) {
            Ok(Some(p)) => p,
            Ok(None) | Err(_) => continue,
        };
        let installed = home.as_deref().is_some_and(|h| client_installed(c, h));
        if path.is_file() || installed {
            out.push(c);
        }
    }
    if out.is_empty() {
        anyhow::bail!(
            "no agent clients detected — pass an explicit client (e.g. \
             `bento mcp install claude-code`) to register without auto-detection."
        );
    }
    Ok(out)
}

/// Per-client "is this client installed on this machine" check. Used
/// by `expand_auto` so that we don't register `bento-mcp` against
/// agents the user doesn't have. Each client leaves a different
/// marker on disk; we rely on whichever the client itself owns.
fn client_installed(client: McpClient, home: &Path) -> bool {
    match client {
        McpClient::Auto => false,
        // Claude Code: the dotfile `~/.claude.json` is the MCP source
        // of truth, but the directory `~/.claude/` (settings.json,
        // skills/) is also a strong "installed" signal — and the
        // dotfile may not exist on a fresh install yet.
        McpClient::ClaudeCode => {
            home.join(".claude.json").is_file() || home.join(".claude").is_dir()
        }
        McpClient::Cursor => home.join(".cursor").is_dir(),
        McpClient::Windsurf => home.join(".codeium").join("windsurf").is_dir(),
        McpClient::Codex => home.join(".codex").is_dir(),
        McpClient::Opencode => home.join(".config").join("opencode").is_dir(),
        McpClient::Zed => home.join(".config").join("zed").is_dir(),
        McpClient::ClaudeDesktop => {
            #[cfg(target_os = "macos")]
            {
                home.join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .is_dir()
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                home.join(".config").join("Claude").is_dir()
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

/// Install for a single resolved client + path. Idempotent: returns
/// `Unchanged` when the existing entry matches what we'd write.
pub fn install_one(
    client: McpClient,
    path: &Path,
    server_name: &str,
    workspace: Option<&Path>,
) -> Result<InstallResult> {
    let existed_before = path.is_file();
    let action = match client {
        // Auto is resolved up-stack into a concrete client; reaching
        // install_one with Auto is a programming error.
        McpClient::Auto => anyhow::bail!("install_one called with Auto — caller must expand"),
        McpClient::ClaudeCode => {
            let entry = build_entry(workspace);
            if json_entry_matches(path, "mcpServers", server_name, &entry, existed_before)? {
                InstallAction::Unchanged
            } else if let Some(action) = (!cfg!(test))
                // `claude mcp add` writes the real ~/.claude.json —
                // never from a unit test's tempdir fixture.
                .then(|| claude_cli_install(path, server_name, workspace, existed_before))
                .flatten()
            {
                action
            } else {
                install_json_object(path, "mcpServers", server_name, entry, existed_before)?
            }
        }
        McpClient::ClaudeDesktop | McpClient::Cursor | McpClient::Windsurf => install_json_object(
            path,
            "mcpServers",
            server_name,
            build_entry(workspace),
            existed_before,
        )?,
        McpClient::Zed => install_json_object(
            path,
            "context_servers",
            server_name,
            build_entry(workspace),
            existed_before,
        )?,
        // OpenCode's top-level key is `mcp` and `command` is a single
        // array (binary + args) with a `type` discriminator.
        McpClient::Opencode => install_json_object(
            path,
            "mcp",
            server_name,
            opencode_entry(workspace),
            existed_before,
        )?,
        McpClient::Codex => install_codex(path, server_name, workspace, existed_before)?,
    };

    Ok(InstallResult {
        client,
        path: path.to_path_buf(),
        existed_before,
        action,
    })
}

/// True when the config already carries exactly the entry we'd write
/// — i.e. the install is a no-op. Read-only, so it's safe to call
/// before delegating the write to another process.
fn json_entry_matches(
    path: &Path,
    top_key: &str,
    server_name: &str,
    entry: &Value,
    existed_before: bool,
) -> Result<bool> {
    let loaded = read_json_or_empty(path, existed_before)?;
    Ok(loaded.root.get(top_key).and_then(|v| v.get(server_name)) == Some(entry))
}

/// Hand the write to `claude mcp add` when the Claude Code CLI is on
/// PATH. `~/.claude.json` is Claude Code's whole user-state file and
/// a live session rewrites it continuously, so a read-modify-write
/// from here can drop whatever the session flushed in between; the
/// CLI owns that file and serialises against itself. `None` ⇒ no
/// usable `claude` on PATH, caller falls back to the file writer.
fn claude_cli_install(
    path: &Path,
    server_name: &str,
    workspace: Option<&Path>,
    existed_before: bool,
) -> Option<InstallAction> {
    let scope = if path.file_name().is_some_and(|n| n == ".mcp.json") {
        "project"
    } else {
        "user"
    };
    let mut add: Vec<String> = [
        "mcp",
        "add",
        "--scope",
        scope,
        server_name,
        "--",
        "bento-mcp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(ws) = workspace {
        add.push("--workspace".into());
        add.push(ws.display().to_string());
    }
    let claude = |args: &[String]| {
        std::process::Command::new("claude")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };

    if claude(&add).is_some() {
        return Some(if existed_before {
            InstallAction::Added
        } else {
            InstallAction::Created
        });
    }
    // `claude mcp add` refuses to shadow an existing name ("already
    // exists in <scope> config"), so an update is remove-then-add.
    let remove: Vec<String> = ["mcp", "remove", "--scope", scope, server_name]
        .iter()
        .map(|s| s.to_string())
        .collect();
    claude(&remove)?;
    claude(&add).map(|_| InstallAction::Updated)
}

/// JSON writer for clients shaped `{ "<top_key>": { "<server_name>":
/// <entry> } }`. Covers Claude Desktop, Cursor, Windsurf
/// (`mcpServers`), Zed (`context_servers`), OpenCode (`mcp`), and
/// Claude Code when its CLI isn't on PATH.
fn install_json_object(
    path: &Path,
    top_key: &str,
    server_name: &str,
    entry: Value,
    existed_before: bool,
) -> Result<InstallAction> {
    let loaded = read_json_or_empty(path, existed_before)?;
    let Some(root_obj) = loaded.root.as_object() else {
        anyhow::bail!(
            "expected a JSON object at the root of {} (got: {})",
            path.display(),
            kind_of(&loaded.root),
        );
    };
    if let Some(servers) = root_obj.get(top_key) {
        if !servers.is_object() {
            anyhow::bail!(
                "expected `{}` to be an object in {} (got: {})",
                top_key,
                path.display(),
                kind_of(servers),
            );
        }
    }

    let prior = root_obj.get(top_key).and_then(|v| v.get(server_name));
    let action = decide_action(prior, &entry, existed_before);
    if action == InstallAction::Unchanged {
        return Ok(action);
    }

    ensure_parent(path)?;

    // A commented config (Zed ships one; `.mcp.json` gets hand-edited)
    // survives the round-trip only if we never re-serialise it: splice
    // the new top-level key in as text instead. When the key already
    // exists we can't splice safely, so the user's comments are the
    // cost of the update — take a copy first so nothing is lost.
    // ponytail: a span-aware JSONC editor would cover the second case
    // too; not worth it until someone hits it.
    if loaded.had_comments {
        if root_obj.get(top_key).is_none() {
            let spliced = splice_top_level(
                &loaded.raw,
                top_key,
                &json!({ server_name: entry }),
                root_obj.is_empty(),
            )?;
            write_text_atomic(path, &spliced)?;
            return Ok(action);
        }
        let backup = path.with_file_name(format!(
            "{}.bento-backup",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("config"),
        ));
        std::fs::copy(path, &backup)
            .with_context(|| format!("backing up {} → {}", path.display(), backup.display()))?;
        eprintln!(
            "warning: {} contains comments that this update drops — original copied to {}",
            path.display(),
            backup.display(),
        );
    }

    let mut root = loaded.root;
    root.as_object_mut()
        .expect("checked above: root is an object")
        .entry(top_key.to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("checked above: top_key is an object or absent")
        .insert(server_name.to_string(), entry);

    write_json_atomic(path, &root)?;
    Ok(action)
}

/// Splice `{ "<top_key>": <value> }` into `raw` right after the root
/// object's `{`, leaving every other byte (comments included) alone.
/// Only valid when `top_key` is absent from the document.
fn splice_top_level(raw: &str, top_key: &str, value: &Value, root_empty: bool) -> Result<String> {
    let brace = crate::jsonc::strip_comments(raw)
        .find('{')
        .context("no root `{` found")?;
    let block = serde_json::to_string_pretty(&json!({ top_key: value }))
        .context("serialising MCP config to JSON")?;
    // `{\n  "<top_key>": …\n}` → the inner lines, without the braces.
    let inner = block[1..block.len() - 1].trim_end();
    Ok(format!(
        "{}{}{}{}",
        &raw[..=brace],
        inner,
        if root_empty { "" } else { "," },
        &raw[brace + 1..],
    ))
}

/// TOML writer for Codex CLI. Entries land at
/// `[mcp_servers.<server_name>]` with `command = "bento-mcp"` and
/// optional `args`. Uses `toml_edit` so existing comments + ordering
/// in `~/.codex/config.toml` survive a round-trip.
fn install_codex(
    path: &Path,
    server_name: &str,
    workspace: Option<&Path>,
    existed_before: bool,
) -> Result<InstallAction> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};

    let body = if existed_before {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = body
        .parse()
        .with_context(|| format!("parsing TOML in {}", path.display()))?;

    let mcp_servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "expected `mcp_servers` to be a TOML table in {}",
                path.display()
            )
        })?;
    mcp_servers.set_implicit(true);

    // Build the desired entry.
    let mut want = Table::new();
    want["command"] = value("bento-mcp");
    if let Some(ws) = workspace {
        let mut arr = Array::new();
        arr.push("--workspace");
        arr.push(ws.display().to_string());
        want["args"] = value(arr);
    }

    let prior = mcp_servers.get(server_name);
    let action = match prior {
        Some(Item::Table(t)) if tables_equivalent(t, &want) => InstallAction::Unchanged,
        Some(_) => InstallAction::Updated,
        None if existed_before => InstallAction::Added,
        None => InstallAction::Created,
    };
    if action == InstallAction::Unchanged {
        return Ok(action);
    }

    mcp_servers.insert(server_name, Item::Table(want));

    ensure_parent(path)?;
    write_text_atomic(path, &doc.to_string())?;
    Ok(action)
}

/// Compare two `[mcp_servers.<name>]` tables ignoring decorations
/// (whitespace, comments, key ordering). We only care about the
/// semantic fields we write — `command` and optionally `args`.
fn tables_equivalent(a: &toml_edit::Table, b: &toml_edit::Table) -> bool {
    fn norm(t: &toml_edit::Table) -> (Option<String>, Option<Vec<String>>) {
        let cmd = t.get("command").and_then(|v| v.as_str()).map(String::from);
        let args = t.get("args").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .map(|x| x.as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        });
        (cmd, args)
    }
    norm(a) == norm(b)
}

/// A client config as read from disk. `raw` is the untouched file
/// text (empty when the file is new) — kept so a commented document
/// can be updated without re-serialising it.
struct LoadedJson {
    root: Value,
    raw: String,
    had_comments: bool,
}

fn read_json_or_empty(path: &Path, existed_before: bool) -> Result<LoadedJson> {
    let raw = if existed_before {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    if raw.trim().is_empty() {
        return Ok(LoadedJson {
            root: json!({}),
            raw,
            had_comments: false,
        });
    }
    // Zed's settings.json ships with `//` comments and `.mcp.json`
    // gets hand-annotated; serde_json rejects both outright.
    let stripped = crate::jsonc::strip_comments(&raw);
    let root = serde_json::from_str(&stripped)
        .with_context(|| format!("parsing JSON in {}", path.display()))?;
    Ok(LoadedJson {
        had_comments: stripped != raw,
        root,
        raw,
    })
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

fn decide_action(prior: Option<&Value>, want: &Value, existed_before: bool) -> InstallAction {
    match prior {
        Some(p) if p == want => InstallAction::Unchanged,
        Some(_) => InstallAction::Updated,
        None if existed_before => InstallAction::Added,
        None => InstallAction::Created,
    }
}

fn opencode_entry(workspace: Option<&Path>) -> Value {
    let mut command: Vec<String> = vec!["bento-mcp".into()];
    if let Some(ws) = workspace {
        command.push("--workspace".into());
        command.push(ws.display().to_string());
    }
    json!({
        "type": "local",
        "command": command,
        "enabled": true,
    })
}

fn build_entry(workspace: Option<&Path>) -> Value {
    let mut args: Vec<String> = Vec::new();
    if let Some(ws) = workspace {
        args.push("--workspace".into());
        args.push(ws.display().to_string());
    }
    json!({
        "command": "bento-mcp",
        "args": args,
    })
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let body = serde_json::to_string_pretty(value).context("serialising MCP config to JSON")?;
    write_text_atomic(path, &body)
}

fn write_text_atomic(path: &Path, body: &str) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.bento-mcp.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config")
    ));
    write_private(&tmp_path, body).with_context(|| format!("writing {}", tmp_path.display()))?;
    // Inherit the config's own mode: `~/.claude.json` is 0600 and
    // holds session credentials — a default-mode rewrite would widen
    // it to 0644.
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp_path, meta.permissions());
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Create (or truncate) `path` owner-readable only, then write `body`.
/// The mode is set at open time so the temp file is never briefly
/// world-readable.
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(body.as_bytes())
}

pub fn run(
    json_out: bool,
    client: McpClient,
    local: bool,
    workspace: Option<PathBuf>,
    name: String,
) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let workspace = workspace.as_deref();

    // Validate `name`: server keys flow into `mcp__bento__<verb>` tool
    // surface, so reject anything that wouldn't form a valid tool
    // prefix. Same charset as MCP server keys in published clients.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!(
            "server name must be non-empty and contain only ASCII letters, digits, '-', or '_' \
             (got {name:?})"
        );
    }

    let clients = if matches!(client, McpClient::Auto) {
        expand_auto(local, &cwd)?
    } else {
        vec![client]
    };

    let mut results = Vec::new();
    for c in clients {
        let path = config_path(c, local, &cwd)?
            .ok_or_else(|| anyhow::anyhow!("no config path for {c:?}"))?;
        let result = install_one(c, &path, &name, workspace)?;
        results.push(result);
    }

    if json_out {
        let arr: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "client": format!("{:?}", r.client).to_lowercase(),
                    "path": r.path.display().to_string(),
                    "existed_before": r.existed_before,
                    "action": r.action.as_str(),
                })
            })
            .collect();
        crate::json::emit(&Value::Array(arr))?;
    } else {
        for r in &results {
            let icon = match r.action {
                InstallAction::Created | InstallAction::Added | InstallAction::Updated => {
                    style::green("✓")
                }
                InstallAction::Unchanged => style::dim("·"),
            };
            println!(
                "{} {:<14} {} {}",
                icon,
                format!("{:?}", r.client).to_lowercase(),
                style::dim(r.action.as_str()),
                r.path.display(),
            );
        }
        if results.iter().any(|r| {
            matches!(
                r.action,
                InstallAction::Created | InstallAction::Added | InstallAction::Updated
            )
        }) {
            println!();
            println!(
                "{}",
                style::dim("restart the affected client(s) so they reload the MCP server list.")
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_json(path: &Path) -> Value {
        let bytes = std::fs::read(path).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn install_one_creates_config_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".cursor").join("mcp.json");
        let r = install_one(McpClient::Cursor, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Created);
        let v = read_json(&path);
        assert_eq!(v["mcpServers"]["bento"]["command"], "bento-mcp");
        assert_eq!(v["mcpServers"]["bento"]["args"], json!([]));
    }

    #[test]
    fn install_one_adds_to_existing_config_without_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        // Pre-existing config with another server entry.
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"other-bin"}},"otherKey":42}"#,
        )
        .unwrap();
        let r = install_one(McpClient::ClaudeCode, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Added);
        let v = read_json(&path);
        assert_eq!(v["mcpServers"]["other"]["command"], "other-bin");
        assert_eq!(v["mcpServers"]["bento"]["command"], "bento-mcp");
        assert_eq!(v["otherKey"], 42);
    }

    #[test]
    fn install_one_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        let r1 = install_one(McpClient::Cursor, &path, "bento", None).unwrap();
        assert_eq!(r1.action, InstallAction::Created);
        let r2 = install_one(McpClient::Cursor, &path, "bento", None).unwrap();
        assert_eq!(r2.action, InstallAction::Unchanged);
    }

    #[test]
    fn install_one_updates_when_workspace_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        let _r1 = install_one(McpClient::Cursor, &path, "bento", None).unwrap();
        let ws = tmp.path().join("repo");
        let r2 = install_one(McpClient::Cursor, &path, "bento", Some(&ws)).unwrap();
        assert_eq!(r2.action, InstallAction::Updated);
        let v = read_json(&path);
        let args = &v["mcpServers"]["bento"]["args"];
        assert_eq!(args[0], "--workspace");
        assert_eq!(args[1].as_str().unwrap(), ws.display().to_string());
    }

    #[test]
    fn install_one_rejects_non_object_root() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, "[]").unwrap();
        let err = install_one(McpClient::Cursor, &path, "bento", None).unwrap_err();
        assert!(format!("{err}").contains("JSON object"));
    }

    #[test]
    fn windsurf_rejects_local_scope() {
        let cwd = std::path::PathBuf::from("/tmp");
        let err = config_path(McpClient::Windsurf, true, &cwd).unwrap_err();
        assert!(format!("{err}").contains("project-local"));
    }

    #[test]
    fn claude_code_user_scope_resolves_to_dotfile() {
        // Regression: `~/.claude/mcp.json` is the wrong path —
        // Claude Code reads `~/.claude.json` (single dotfile holding
        // every user-scoped setting). v0.1.0 wrote to the wrong path
        // and bento never showed up in the MCP picker.
        let cwd = std::path::PathBuf::from("/tmp");
        let path = config_path(McpClient::ClaudeCode, false, &cwd)
            .unwrap()
            .unwrap();
        assert!(
            path.ends_with(".claude.json"),
            "user-scope claude-code path should be ~/.claude.json, got {}",
            path.display(),
        );
        // Defensively: NOT a directory-then-file pattern under `.claude/`.
        assert!(
            !path.to_string_lossy().contains("/.claude/"),
            "user-scope claude-code must not write under ~/.claude/, got {}",
            path.display(),
        );
    }

    #[test]
    fn claude_code_local_scope_uses_dot_mcp_json() {
        // Project-scoped Claude Code MCP servers live in `.mcp.json`
        // at the repo root.
        let cwd = std::path::PathBuf::from("/tmp/repo");
        let path = config_path(McpClient::ClaudeCode, true, &cwd)
            .unwrap()
            .unwrap();
        assert_eq!(path, std::path::PathBuf::from("/tmp/repo/.mcp.json"));
    }

    #[test]
    fn client_installed_requires_real_marker() {
        // Regression: the original heuristic was "parent of the
        // user-config path is a directory". For Claude Code, the
        // user config is `~/.claude.json`, so the parent is `$HOME`
        // — which always exists, so Claude Code was always
        // "detected" even on machines that had never run it. Per-
        // client detection must be specific.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Empty home → nothing detected.
        assert!(!client_installed(McpClient::ClaudeCode, home));
        assert!(!client_installed(McpClient::Cursor, home));
        assert!(!client_installed(McpClient::Windsurf, home));
        assert!(!client_installed(McpClient::ClaudeDesktop, home));

        // Claude Code: dotfile-only is enough.
        std::fs::write(home.join(".claude.json"), b"{}").unwrap();
        assert!(client_installed(McpClient::ClaudeCode, home));

        // Claude Code: directory-only is also enough (fresh install
        // before the dotfile materialises).
        let dir2 = tempfile::tempdir().unwrap();
        let home2 = dir2.path();
        std::fs::create_dir(home2.join(".claude")).unwrap();
        assert!(client_installed(McpClient::ClaudeCode, home2));

        // Cursor: requires `~/.cursor/`.
        std::fs::create_dir(home2.join(".cursor")).unwrap();
        assert!(client_installed(McpClient::Cursor, home2));

        // Windsurf: requires the nested codeium dir.
        std::fs::create_dir_all(home2.join(".codeium").join("windsurf")).unwrap();
        assert!(client_installed(McpClient::Windsurf, home2));

        // Codex: requires `~/.codex/`.
        let dir3 = tempfile::tempdir().unwrap();
        let home3 = dir3.path();
        assert!(!client_installed(McpClient::Codex, home3));
        std::fs::create_dir(home3.join(".codex")).unwrap();
        assert!(client_installed(McpClient::Codex, home3));

        // Opencode: requires `~/.config/opencode/`.
        assert!(!client_installed(McpClient::Opencode, home3));
        std::fs::create_dir_all(home3.join(".config").join("opencode")).unwrap();
        assert!(client_installed(McpClient::Opencode, home3));

        // Zed: requires `~/.config/zed/`.
        assert!(!client_installed(McpClient::Zed, home3));
        std::fs::create_dir(home3.join(".config").join("zed")).unwrap();
        assert!(client_installed(McpClient::Zed, home3));
    }

    #[test]
    fn opencode_writes_typed_mcp_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("opencode.json");
        let r = install_one(McpClient::Opencode, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Created);
        let v = read_json(&path);
        // Top-level key is `mcp`, not `mcpServers`.
        assert!(v.get("mcpServers").is_none());
        let entry = &v["mcp"]["bento"];
        assert_eq!(entry["type"], "local");
        // `command` is a single array — binary + args together.
        assert_eq!(entry["command"], json!(["bento-mcp"]));
        assert_eq!(entry["enabled"], true);

        // Idempotent.
        let r2 = install_one(McpClient::Opencode, &path, "bento", None).unwrap();
        assert_eq!(r2.action, InstallAction::Unchanged);

        // Workspace gets folded into the command array (not args).
        let ws = tmp.path().join("repo");
        let r3 = install_one(McpClient::Opencode, &path, "bento", Some(&ws)).unwrap();
        assert_eq!(r3.action, InstallAction::Updated);
        let v = read_json(&path);
        let cmd = &v["mcp"]["bento"]["command"];
        assert_eq!(cmd[0], "bento-mcp");
        assert_eq!(cmd[1], "--workspace");
        assert_eq!(cmd[2].as_str().unwrap(), ws.display().to_string());
    }

    #[test]
    fn zed_writes_under_context_servers_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Pre-existing Zed settings with unrelated keys — must not be
        // clobbered.
        std::fs::write(
            &path,
            r#"{"theme":"One Dark","context_servers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();
        let r = install_one(McpClient::Zed, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Added);
        let v = read_json(&path);
        assert_eq!(v["theme"], "One Dark");
        assert_eq!(v["context_servers"]["other"]["command"], "x");
        assert_eq!(v["context_servers"]["bento"]["command"], "bento-mcp");
        // No mcpServers key — Zed uses context_servers.
        assert!(v.get("mcpServers").is_none());
    }

    #[test]
    fn zed_settings_with_comments_are_spliced_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Real Zed settings.json: JSONC, heavily commented.
        let original = "{\n  // Theme picked by hand\n  \"theme\": \"One Dark\",\n  /* keymap\n     notes */\n  \"vim_mode\": false\n}\n";
        std::fs::write(&path, original).unwrap();

        let r = install_one(McpClient::Zed, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Added);

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("// Theme picked by hand"), "got: {body}");
        assert!(body.contains("/* keymap"), "got: {body}");
        let v: Value = serde_json::from_str(&crate::jsonc::strip_comments(&body)).unwrap();
        assert_eq!(v["theme"], "One Dark");
        assert_eq!(v["vim_mode"], false);
        assert_eq!(v["context_servers"]["bento"]["command"], "bento-mcp");

        // Idempotent even though the file is JSONC.
        let r2 = install_one(McpClient::Zed, &path, "bento", None).unwrap();
        assert_eq!(r2.action, InstallAction::Unchanged);
    }

    #[test]
    fn comment_dropping_update_leaves_a_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // The top-level key already exists, so the entry can't be
        // spliced in — the rewrite drops comments, hence the copy.
        let original =
            "{\n  // keep me\n  \"context_servers\": {\"other\": {\"command\": \"x\"}}\n}\n";
        std::fs::write(&path, original).unwrap();

        let r = install_one(McpClient::Zed, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Added);
        let backup =
            std::fs::read_to_string(tmp.path().join("settings.json.bento-backup")).unwrap();
        assert_eq!(backup, original);
        let v = read_json(&path);
        assert_eq!(v["context_servers"]["other"]["command"], "x");
        assert_eq!(v["context_servers"]["bento"]["command"], "bento-mcp");
    }

    #[test]
    fn empty_root_object_splices_without_a_stray_comma() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, "{ /* nothing here yet */ }").unwrap();
        install_one(McpClient::Cursor, &path, "bento", None).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&crate::jsonc::strip_comments(&body)).unwrap();
        assert_eq!(v["mcpServers"]["bento"]["command"], "bento-mcp");
    }

    #[cfg(unix)]
    #[test]
    fn write_preserves_the_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        install_one(McpClient::Cursor, &path, "bento", None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewrite widened the config's mode");
    }

    #[test]
    fn codex_writes_toml_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let r = install_one(McpClient::Codex, &path, "bento", None).unwrap();
        assert_eq!(r.action, InstallAction::Created);
        let body = std::fs::read_to_string(&path).unwrap();
        // The shape Codex expects: `[mcp_servers.<name>]` table with
        // `command = "..."`.
        assert!(
            body.contains("[mcp_servers.bento]"),
            "expected [mcp_servers.bento] in {body}",
        );
        assert!(
            body.contains("command = \"bento-mcp\""),
            "expected command line in {body}",
        );

        // Idempotent.
        let r2 = install_one(McpClient::Codex, &path, "bento", None).unwrap();
        assert_eq!(r2.action, InstallAction::Unchanged);

        // Round-trip preserves user-edited keys above ours.
        std::fs::write(
            &path,
            "# user comment\nmodel = \"gpt-5\"\n\n[mcp_servers.bento]\ncommand = \"bento-mcp\"\n",
        )
        .unwrap();
        let r3 = install_one(McpClient::Codex, &path, "bento", None).unwrap();
        assert_eq!(r3.action, InstallAction::Unchanged);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# user comment"));
        assert!(body.contains("model = \"gpt-5\""));
    }

    #[test]
    fn codex_updates_when_workspace_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let _r1 = install_one(McpClient::Codex, &path, "bento", None).unwrap();
        let ws = tmp.path().join("repo");
        let r2 = install_one(McpClient::Codex, &path, "bento", Some(&ws)).unwrap();
        assert_eq!(r2.action, InstallAction::Updated);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("--workspace"));
        assert!(body.contains(&ws.display().to_string()));
    }

    #[test]
    fn codex_rejects_invalid_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is = not valid = toml = at all").unwrap();
        let err = install_one(McpClient::Codex, &path, "bento", None).unwrap_err();
        assert!(format!("{err}").contains("parsing TOML"));
    }

    #[test]
    fn new_client_paths_resolve() {
        let cwd = std::path::PathBuf::from("/tmp/repo");

        // Codex
        let p = config_path(McpClient::Codex, false, &cwd).unwrap().unwrap();
        assert!(p.ends_with(".codex/config.toml"));
        let p = config_path(McpClient::Codex, true, &cwd).unwrap().unwrap();
        assert_eq!(p, std::path::PathBuf::from("/tmp/repo/.codex/config.toml"));

        // OpenCode
        let p = config_path(McpClient::Opencode, false, &cwd)
            .unwrap()
            .unwrap();
        assert!(p.ends_with(".config/opencode/opencode.json"));
        let p = config_path(McpClient::Opencode, true, &cwd)
            .unwrap()
            .unwrap();
        assert_eq!(p, std::path::PathBuf::from("/tmp/repo/opencode.json"));

        // Zed
        let p = config_path(McpClient::Zed, false, &cwd).unwrap().unwrap();
        assert!(p.ends_with(".config/zed/settings.json"));
        let p = config_path(McpClient::Zed, true, &cwd).unwrap().unwrap();
        assert_eq!(p, std::path::PathBuf::from("/tmp/repo/.zed/settings.json"));
    }
}
