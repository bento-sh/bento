//! Cloudflare Pages deploy integration.
//!
//! - Detects: nothing — Pages projects rarely ship a `wrangler.toml`
//!   at the dish root (project settings live in the CF dashboard),
//!   so this integration only fires via an explicit
//!   `[integrations.cloudflare_pages]` block in `dish.toml`. Same
//!   opt-in shape as Slack / Linear.
//! - Required env: none for the OAuth-logged-in path; CI flows set
//!   `CLOUDFLARE_API_TOKEN` and we forward it.
//! - Emits: `cloudflare_pages:deploy` running
//!   `wrangler pages deploy <dist> --project-name <project>
//!    --branch <branch> --commit-dirty=true`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cloudflare_worker::{capture_cli_stdout, run_cli_with_stdin};
use crate::integration::{CliRequirement, Integration, IntegrationTask, IntegrationTaskKind};

pub struct CloudflarePagesIntegration;

impl Integration for CloudflarePagesIntegration {
    fn id(&self) -> &str {
        "cloudflare_pages"
    }

    fn display_name(&self) -> &str {
        "Cloudflare Pages"
    }

    fn detect(&self, _dir: &Path) -> bool {
        // Config-only opt-in. Pages projects are usually managed via
        // the CF dashboard + wrangler pages CLI; nothing on disk
        // reliably identifies a "this dish is a Pages project"
        // pattern. The explicit `[integrations.cloudflare_pages]`
        // block in dish.toml is the unambiguous signal.
        false
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
        let project = pages_project(config)?;
        let mut cmd = Command::new("wrangler");
        cmd.current_dir(cwd)
            .arg("pages")
            .arg("secret")
            .arg("put")
            .arg(name)
            .arg("--project-name")
            .arg(&project);
        run_cli_with_stdin(&mut cmd, value.as_bytes(), "wrangler pages secret put")
    }

    fn list_secrets(&self, cwd: &Path, config: &toml::Table) -> Result<Vec<String>> {
        let project = pages_project(config)?;
        let mut cmd = Command::new("wrangler");
        cmd.current_dir(cwd)
            .arg("pages")
            .arg("secret")
            .arg("list")
            .arg("--project-name")
            .arg(&project);
        let stdout = capture_cli_stdout(&mut cmd, "wrangler pages secret list")?;
        parse_pages_secret_list(&stdout)
    }

    fn delete_secret(&self, cwd: &Path, config: &toml::Table, name: &str) -> Result<()> {
        let project = pages_project(config)?;
        let mut cmd = Command::new("wrangler");
        cmd.current_dir(cwd)
            .arg("pages")
            .arg("secret")
            .arg("delete")
            .arg(name)
            .arg("--project-name")
            .arg(&project);
        // Wrangler prompts the same "Are you sure?" confirmation on
        // Pages delete as it does on Worker delete.
        run_cli_with_stdin(&mut cmd, b"y\n", "wrangler pages secret delete")
    }

    fn detected_tasks(&self, _dir: &Path, config: &toml::Table) -> Vec<IntegrationTask> {
        // Config shape:
        //   [integrations.cloudflare_pages]
        //   project = "my-pages-project"   # required
        //   dist    = "dist"               # default "dist"
        //   branch  = "main"               # default "main"
        //
        // `--commit-dirty=true` is always on — bento's executor runs
        // with the dish dir as cwd, and Wrangler's default git check
        // gets noisy on monorepos where build artefacts (dist/*) are
        // rebuilt fresh each invocation.
        let Some(project) = config.get("project").and_then(|v| v.as_str()) else {
            // No project → no task. We could error loudly at detect
            // time instead; leaving it silent matches the existing
            // integrations' tolerance for partial config (Slack skips
            // a deploy notify when no webhook is configured, etc).
            return Vec::new();
        };
        let dist = config
            .get("dist")
            .and_then(|v| v.as_str())
            .unwrap_or("dist");
        let branch = config
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or("main");

        let cmd = format!(
            "wrangler pages deploy {} --project-name {} --branch {} --commit-dirty=true",
            shell_quote(dist),
            shell_quote(project),
            shell_quote(branch),
        );

        vec![IntegrationTask {
            name: "cloudflare_pages:deploy".to_string(),
            kind: IntegrationTaskKind::Deploy,
            run: cmd,
            depends_on: vec!["build".into()],
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
/// metachars — same rules the Railway / CF-Worker integrations use.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Pull the required `project = "..."` from a cloudflare_pages config
/// block. Errors when absent because every Pages-secret command needs
/// it — there's no sensible default.
fn pages_project(config: &toml::Table) -> Result<String> {
    config
        .get("project")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[integrations.cloudflare_pages] project = \"...\" is required for secret management"
            )
        })
}

/// Parse `wrangler pages secret list` output. The CLI is less
/// structured than the worker variant: it emits a plain-text table.
/// The first column is the secret name; we split on whitespace and
/// skip the header row.
fn parse_pages_secret_list(stdout: &[u8]) -> Result<Vec<String>> {
    let s = std::str::from_utf8(stdout).context("wrangler pages list not valid UTF-8")?;
    // Wrangler's pages secret list output shape (2026):
    //   Name          Type
    //   ────────────  ───────────
    //   MY_SECRET     secret_text
    //
    // Some wrangler versions instead emit JSON when no TTY is detected.
    // Probe for either shape.
    if let Some(arr_start) = s.find('[') {
        #[derive(serde::Deserialize)]
        struct Entry {
            name: String,
        }
        let rest = &s[arr_start..];
        let arr_end = rest
            .rfind(']')
            .ok_or_else(|| anyhow::anyhow!("pages secret list: array never closes"))?;
        let entries: Vec<Entry> = serde_json::from_str(&rest[..=arr_end])
            .context("parsing wrangler pages secret list JSON")?;
        return Ok(entries.into_iter().map(|e| e.name).collect());
    }
    let mut out = Vec::new();
    for line in s.lines().skip(2) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(first) = line.split_whitespace().next() {
            out.push(first.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(CloudflarePagesIntegration.id(), "cloudflare_pages");
        assert_eq!(
            CloudflarePagesIntegration.display_name(),
            "Cloudflare Pages"
        );
    }

    #[test]
    fn detect_is_always_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("wrangler.toml"), "").unwrap();
        // Even with a wrangler.toml — Pages is config-only.
        assert!(!CloudflarePagesIntegration.detect(tmp.path()));
    }

    #[test]
    fn emits_nothing_when_project_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflarePagesIntegration.detected_tasks(tmp.path(), &toml::Table::new());
        assert!(tasks.is_empty());
    }

    #[test]
    fn emits_deploy_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflarePagesIntegration
            .detected_tasks(tmp.path(), &cfg(&[("project", s("my-site"))]));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "cloudflare_pages:deploy");
        assert_eq!(tasks[0].kind, IntegrationTaskKind::Deploy);
        assert!(tasks[0].no_cache);
        assert_eq!(
            tasks[0].run,
            "wrangler pages deploy dist --project-name my-site --branch main --commit-dirty=true"
        );
    }

    #[test]
    fn respects_dist_and_branch_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflarePagesIntegration.detected_tasks(
            tmp.path(),
            &cfg(&[
                ("project", s("my-site")),
                ("dist", s("build")),
                ("branch", s("preview")),
            ]),
        );
        assert_eq!(
            tasks[0].run,
            "wrangler pages deploy build --project-name my-site --branch preview --commit-dirty=true"
        );
    }

    #[test]
    fn quotes_project_with_special_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflarePagesIntegration
            .detected_tasks(tmp.path(), &cfg(&[("project", s("my site"))]));
        assert!(tasks[0].run.contains("--project-name 'my site'"));
    }

    #[test]
    fn dist_path_with_slashes_unquoted() {
        // Paths like "build/static" are common; `/` is safe in shell
        // quoting rules so we keep them bare for readability.
        let tmp = tempfile::tempdir().unwrap();
        let tasks = CloudflarePagesIntegration.detected_tasks(
            tmp.path(),
            &cfg(&[("project", s("my-site")), ("dist", s("build/static"))]),
        );
        assert!(tasks[0].run.contains("wrangler pages deploy build/static "));
    }
}
