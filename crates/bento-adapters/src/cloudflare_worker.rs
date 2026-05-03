//! Cloudflare Worker deploy integration.
//!
//! - Detects: `wrangler.toml` or `wrangler.jsonc` at the dish root.
//! - Required env: none by default (Wrangler's OAuth login is the
//!   usual local path). `CLOUDFLARE_API_TOKEN` is forwarded if set,
//!   which is how CI auths without the OAuth dance.
//! - Emits: `cloudflare_worker:deploy` running `wrangler deploy`.
//!   Wrangler's own deploy command is idempotent and blocks on the
//!   edge's terminal status — no `--ci` / TTY quirks like Railway.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::integration::{CliRequirement, Integration, IntegrationTask, IntegrationTaskKind};

pub struct CloudflareWorkerIntegration;

impl Integration for CloudflareWorkerIntegration {
    fn id(&self) -> &str {
        "cloudflare_worker"
    }

    fn display_name(&self) -> &str {
        "Cloudflare Worker"
    }

    fn detect(&self, dir: &Path) -> bool {
        // wrangler.toml / wrangler.jsonc is the universal worker
        // discriminator. Pages projects with their own wrangler.toml
        // are the exception; those opt in via an explicit
        // [integrations.cloudflare_pages] block in dish.toml, which
        // the config-union path picks up regardless of what detect()
        // returns here.
        dir.join("wrangler.toml").is_file() || dir.join("wrangler.jsonc").is_file()
    }

    fn required_env(&self) -> Vec<String> {
        // Wrangler's OAuth session (`wrangler login`) covers local
        // dev; CI sets `CLOUDFLARE_API_TOKEN` instead. Neither is
        // strictly required if Wrangler is already authenticated via
        // the OAuth cache, so we pass the token through when present
        // and let Wrangler decide — no preflight fail on missing.
        Vec::new()
    }

    fn required_cli(&self) -> Vec<CliRequirement> {
        vec![CliRequirement::new(
            "wrangler",
            "npm install -g wrangler  (or  bun add -g wrangler)",
        )]
    }

    fn supports_secrets(&self) -> bool {
        true
    }

    fn put_secret(&self, cwd: &Path, config: &toml::Table, name: &str, value: &str) -> Result<()> {
        // `wrangler secret put <name>` reads the value from stdin in
        // non-interactive mode — exactly what we want. `--env` maps to
        // wrangler.toml's per-env blocks (same shape as the deploy task).
        let mut cmd = Command::new("wrangler");
        cmd.current_dir(cwd).arg("secret").arg("put").arg(name);
        if let Some(env) = env_flag(config) {
            cmd.arg("--env").arg(env);
        }
        run_cli_with_stdin(&mut cmd, value.as_bytes(), "wrangler secret put")
    }

    fn list_secrets(&self, cwd: &Path, config: &toml::Table) -> Result<Vec<String>> {
        // `wrangler secret list` returns a JSON array: `[{"name":"X","type":"secret_text"}]`.
        // `--env` is the same per-env toggle as put/delete.
        let mut cmd = Command::new("wrangler");
        cmd.current_dir(cwd).arg("secret").arg("list");
        if let Some(env) = env_flag(config) {
            cmd.arg("--env").arg(env);
        }
        let stdout = capture_cli_stdout(&mut cmd, "wrangler secret list")?;
        parse_wrangler_secret_list(&stdout)
    }

    fn delete_secret(&self, cwd: &Path, config: &toml::Table, name: &str) -> Result<()> {
        // Wrangler prompts "Are you sure...? (y/N)" on delete; feed "y\n"
        // on stdin to confirm without a TTY.
        let mut cmd = Command::new("wrangler");
        cmd.current_dir(cwd).arg("secret").arg("delete").arg(name);
        if let Some(env) = env_flag(config) {
            cmd.arg("--env").arg(env);
        }
        run_cli_with_stdin(&mut cmd, b"y\n", "wrangler secret delete")
    }

    fn detected_tasks(&self, _dir: &Path, config: &toml::Table) -> Vec<IntegrationTask> {
        // Config shape:
        //   [integrations.cloudflare_worker]
        //   env = "production"   # optional; adds --env <name>
        //
        // `--env` maps to Wrangler's per-environment config blocks
        // (`[env.production]` in wrangler.toml). Omit for the default
        // environment.
        let env = config.get("env").and_then(|v| v.as_str());
        let mut cmd = String::from("wrangler deploy");
        if let Some(e) = env {
            cmd.push_str(&format!(" --env {}", shell_quote(e)));
        }
        vec![IntegrationTask {
            name: "cloudflare_worker:deploy".to_string(),
            kind: IntegrationTaskKind::Deploy,
            run: cmd,
            depends_on: vec!["build".into()],
            // Forward whichever auth mode is in use. Wrangler picks the
            // first one it finds. `CI` suppresses Wrangler's update
            // banner in non-interactive contexts (no prompt to stall a
            // pipeline on).
            env_vars: vec![
                "CLOUDFLARE_API_TOKEN".into(),
                "CLOUDFLARE_ACCOUNT_ID".into(),
                "CI".into(),
            ],
            no_cache: true,
            outputs: Vec::new(),
        }]
    }
}

/// Single-quote a shell argument that may contain spaces or other
/// metachars — same rules the Railway integration uses. Plain names
/// pass through unquoted for readability.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Pull the optional `env = "..."` from a cloudflare_worker config
/// block. Shared by deploy + secret commands so they target the same
/// per-env configuration Wrangler knows about.
fn env_flag(config: &toml::Table) -> Option<&str> {
    config.get("env").and_then(|v| v.as_str())
}

/// Run `cmd`, piping `input` to stdin, returning an error that
/// includes `op` (e.g. "wrangler secret put") and captured stderr when
/// the child exits non-zero. Used by secret put/delete.
pub(crate) fn run_cli_with_stdin(cmd: &mut Command, input: &[u8], op: &str) -> Result<()> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawning {op}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .with_context(|| format!("writing stdin to {op}"))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting on {op}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{op} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }
    Ok(())
}

/// Run `cmd`, capture stdout on success, bail with stderr on non-zero
/// exit. Used by secret list.
pub(crate) fn capture_cli_stdout(cmd: &mut Command, op: &str) -> Result<Vec<u8>> {
    let output = cmd.output().with_context(|| format!("running {op}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{op} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }
    Ok(output.stdout)
}

/// Parse `wrangler secret list` JSON output — a top-level array of
/// `{"name": "...", "type": "secret_text"}`. Resilient to wrangler
/// appending a trailing banner / other noise: we find the first `[`
/// and parse from there.
fn parse_wrangler_secret_list(stdout: &[u8]) -> Result<Vec<String>> {
    let s = std::str::from_utf8(stdout).context("wrangler list output not valid UTF-8")?;
    let start = s
        .find('[')
        .ok_or_else(|| anyhow::anyhow!("wrangler secret list: no JSON array in output"))?;
    let rest = &s[start..];
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
    }
    // Trim anything after the outer `]` in case wrangler prints more.
    let end = rest
        .rfind(']')
        .ok_or_else(|| anyhow::anyhow!("wrangler secret list: array never closes"))?;
    let json = &rest[..=end];
    let entries: Vec<Entry> =
        serde_json::from_str(json).context("parsing wrangler secret list JSON")?;
    Ok(entries.into_iter().map(|e| e.name).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with_file(name: &str, content: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(name), content).unwrap();
        tmp
    }

    fn cfg(pairs: &[(&str, toml::Value)]) -> toml::Table {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn s(v: &str) -> toml::Value {
        toml::Value::String(v.to_string())
    }

    #[test]
    fn id_and_display_name() {
        assert_eq!(CloudflareWorkerIntegration.id(), "cloudflare_worker");
        assert_eq!(
            CloudflareWorkerIntegration.display_name(),
            "Cloudflare Worker"
        );
        assert!(CloudflareWorkerIntegration.required_env().is_empty());
    }

    #[test]
    fn detect_matches_wrangler_toml() {
        let tmp = tmp_with_file("wrangler.toml", "name = \"w\"\nmain = \"x\"\n");
        assert!(CloudflareWorkerIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_matches_wrangler_jsonc() {
        let tmp = tmp_with_file("wrangler.jsonc", "{}");
        assert!(CloudflareWorkerIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_false_for_unrelated_project() {
        let tmp = tmp_with_file("package.json", "{}");
        assert!(!CloudflareWorkerIntegration.detect(tmp.path()));
    }

    #[test]
    fn emits_plain_deploy_without_env() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflareWorkerIntegration.detected_tasks(tmp.path(), &toml::Table::new());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "cloudflare_worker:deploy");
        assert_eq!(tasks[0].kind, IntegrationTaskKind::Deploy);
        assert!(tasks[0].no_cache);
        assert_eq!(tasks[0].run, "wrangler deploy");
    }

    #[test]
    fn emits_env_flag_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflareWorkerIntegration
            .detected_tasks(tmp.path(), &cfg(&[("env", s("production"))]));
        assert_eq!(tasks[0].run, "wrangler deploy --env production");
    }

    #[test]
    fn quotes_env_with_special_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflareWorkerIntegration
            .detected_tasks(tmp.path(), &cfg(&[("env", s("staging branch"))]));
        assert_eq!(tasks[0].run, "wrangler deploy --env 'staging branch'");
    }
}
