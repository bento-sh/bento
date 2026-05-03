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
//! Writes are atomic (tmp file in the same dir, then rename). Pre-
//! existing user content under other server keys is preserved.

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
        McpClient::ClaudeCode
        | McpClient::ClaudeDesktop
        | McpClient::Cursor
        | McpClient::Windsurf => {
            install_json_object(path, "mcpServers", server_name, workspace, existed_before)?
        }
        McpClient::Zed => install_json_object(
            path,
            "context_servers",
            server_name,
            workspace,
            existed_before,
        )?,
        McpClient::Opencode => install_opencode(path, server_name, workspace, existed_before)?,
        McpClient::Codex => install_codex(path, server_name, workspace, existed_before)?,
    };

    Ok(InstallResult {
        client,
        path: path.to_path_buf(),
        existed_before,
        action,
    })
}

/// JSON writer for clients shaped `{ "<top_key>": { "<server_name>":
/// { "command": "bento-mcp", "args": [...] } } }`. Covers Claude
/// Code, Claude Desktop, Cursor, Windsurf (`mcpServers`), and Zed
/// (`context_servers`).
fn install_json_object(
    path: &Path,
    top_key: &str,
    server_name: &str,
    workspace: Option<&Path>,
    existed_before: bool,
) -> Result<InstallAction> {
    let mut root: Value = read_json_or_empty(path, existed_before)?;
    if !root.is_object() {
        anyhow::bail!(
            "expected a JSON object at the root of {} (got: {})",
            path.display(),
            kind_of(&root),
        );
    }

    let entry = build_entry(workspace);
    let prior = root.get(top_key).and_then(|v| v.get(server_name)).cloned();

    let action = decide_action(prior.as_ref(), &entry, existed_before);
    if action == InstallAction::Unchanged {
        return Ok(action);
    }

    {
        let obj = root
            .as_object_mut()
            .expect("checked above: root is an object");
        let servers = obj.entry(top_key.to_string()).or_insert_with(|| json!({}));
        if !servers.is_object() {
            anyhow::bail!(
                "expected `{}` to be an object in {} (got: {})",
                top_key,
                path.display(),
                kind_of(servers),
            );
        }
        servers
            .as_object_mut()
            .expect("checked above")
            .insert(server_name.to_string(), entry);
    }

    ensure_parent(path)?;
    write_json_atomic(path, &root)?;
    Ok(action)
}

/// JSON writer for OpenCode. Top-level key is `mcp` (not
/// `mcpServers`), `command` is a single array (binary + args), env
/// is `environment`, entries carry `type: "local"`.
fn install_opencode(
    path: &Path,
    server_name: &str,
    workspace: Option<&Path>,
    existed_before: bool,
) -> Result<InstallAction> {
    let mut root: Value = read_json_or_empty(path, existed_before)?;
    if !root.is_object() {
        anyhow::bail!(
            "expected a JSON object at the root of {} (got: {})",
            path.display(),
            kind_of(&root),
        );
    }

    let mut command: Vec<String> = vec!["bento-mcp".into()];
    if let Some(ws) = workspace {
        command.push("--workspace".into());
        command.push(ws.display().to_string());
    }
    let entry = json!({
        "type": "local",
        "command": command,
        "enabled": true,
    });

    let prior = root.get("mcp").and_then(|v| v.get(server_name)).cloned();
    let action = decide_action(prior.as_ref(), &entry, existed_before);
    if action == InstallAction::Unchanged {
        return Ok(action);
    }

    {
        let obj = root.as_object_mut().expect("root is an object");
        let servers = obj.entry("mcp".to_string()).or_insert_with(|| json!({}));
        if !servers.is_object() {
            anyhow::bail!(
                "expected `mcp` to be an object in {} (got: {})",
                path.display(),
                kind_of(servers),
            );
        }
        servers
            .as_object_mut()
            .expect("checked above")
            .insert(server_name.to_string(), entry);
    }

    ensure_parent(path)?;
    write_json_atomic(path, &root)?;
    Ok(action)
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

fn read_json_or_empty(path: &Path, existed_before: bool) -> Result<Value> {
    if !existed_before {
        return Ok(json!({}));
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("parsing JSON in {}", path.display()))
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
    std::fs::write(&tmp_path, body.as_bytes())
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))?;
    Ok(())
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
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
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
