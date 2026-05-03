//! Slack integration — Notify-kind garnish that posts to an
//! Incoming Webhook after each Deploy.
//!
//! - Detects: `[integrations.slack]` present in `dish.toml`. Unlike
//!   Vercel / Railway there's no platform-side config file to sniff —
//!   a dish is Slack-enabled iff it explicitly opts in.
//! - Required env: `SLACK_WEBHOOK_URL` (the full `https://hooks.slack.
//!   com/...` URL) by default. Overridable per-dish via
//!   `webhook_url_env = "SLACK_WEBHOOK_STAGING"`.
//! - Required CLI: `bento` itself — the emitted task invokes the
//!   hidden `bento _slack-post` sub-command that reads the garnish
//!   payload on stdin and POSTs. Keeps the integration zero-dependency
//!   (no jq / curl needed on the host).
//! - Emits: `slack:notify` (Notify kind).

use std::path::Path;

use crate::integration::{CliRequirement, Integration, IntegrationTask, IntegrationTaskKind};

pub struct SlackIntegration;

/// Default env-var name holding the webhook URL. Overridable per-dish
/// via `[integrations.slack] webhook_url_env = "..."`.
const DEFAULT_WEBHOOK_ENV: &str = "SLACK_WEBHOOK_URL";

impl Integration for SlackIntegration {
    fn id(&self) -> &str {
        "slack"
    }

    fn display_name(&self) -> &str {
        "Slack"
    }

    fn detect(&self, _dir: &Path) -> bool {
        // No platform-side file to detect. A dish opts in by setting
        // `[integrations.slack]` in dish.toml — the workspace loader
        // already routes config to our `detected_tasks` when that
        // block exists, so returning false here keeps the integration
        // strictly config-driven.
        false
    }

    fn required_env(&self) -> Vec<String> {
        vec![DEFAULT_WEBHOOK_ENV.into()]
    }

    fn required_cli(&self) -> Vec<CliRequirement> {
        // `bento` itself — the emitted run command shells back into
        // this binary. Every sane install puts bento on PATH so this
        // rarely trips, but declaring it surfaces a clear preflight
        // error if someone invokes `bento deploy` from a weird shell
        // without PATH set up.
        vec![CliRequirement::new(
            "bento",
            "https://github.com/bento-sh/bento (`curl -fsSL .../install.sh | sh`)",
        )]
    }

    fn detected_tasks(&self, _dir: &Path, config: &toml::Table) -> Vec<IntegrationTask> {
        // Per-dish override: a custom env var name for the webhook
        // URL. Lets `[environments.<name>] secrets.SLACK_WEBHOOK_URL
        // = "SLACK_WEBHOOK_STAGING"` flow through uniformly, while
        // also supporting ad-hoc shapes where the URL lives under a
        // non-standard name.
        let webhook_env = config
            .get("webhook_url_env")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_WEBHOOK_ENV)
            .to_string();

        // Optional human-visible channel override — threaded into the
        // `bento _slack-post` sub-command via a flag. Incoming webhook
        // URLs pin a channel at creation time, so this only takes
        // effect for webhooks created without a channel binding.
        let channel = config
            .get("channel")
            .and_then(|v| v.as_str())
            .map(|s| format!(" --channel {}", shell_quote(s)))
            .unwrap_or_default();

        // Optional username override — same flag pass-through shape
        // as channel. Templates / emoji are fixed for v1; agents who
        // want full custom formatting can write their own Notify-kind
        // integration.
        let username = config
            .get("username")
            .and_then(|v| v.as_str())
            .map(|s| format!(" --username {}", shell_quote(s)))
            .unwrap_or_default();

        let run = format!("bento _slack-post --webhook-env {webhook_env}{channel}{username}");

        vec![IntegrationTask {
            name: "slack:notify".into(),
            kind: IntegrationTaskKind::Notify,
            run,
            depends_on: Vec::new(),
            env_vars: vec![webhook_env],
            no_cache: true,
            outputs: Vec::new(),
        }]
    }
}

/// Minimal POSIX-shell single-quoting. Wraps `s` in single quotes and
/// escapes embedded single quotes via the standard `'\''` dance. Good
/// enough for `sh -c` which is what bento uses to run task commands.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_and_required_env() {
        assert_eq!(SlackIntegration.id(), "slack");
        assert_eq!(SlackIntegration.display_name(), "Slack");
        assert_eq!(SlackIntegration.required_env(), vec![DEFAULT_WEBHOOK_ENV]);
    }

    #[test]
    fn detect_is_false_without_explicit_opt_in() {
        // Slack is config-driven: file-based detection always returns
        // false so the workspace loader consults `[integrations.slack]`
        // in dish.toml as the sole opt-in signal.
        let tmp = tempfile::tempdir().unwrap();
        assert!(!SlackIntegration.detect(tmp.path()));
    }

    #[test]
    fn emits_one_notify_task_with_default_webhook_env() {
        let tmp = tempfile::tempdir().unwrap();
        let config = toml::Table::new();
        let tasks = SlackIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.name, "slack:notify");
        assert_eq!(t.kind, IntegrationTaskKind::Notify);
        assert!(t.no_cache);
        assert_eq!(t.env_vars, vec![DEFAULT_WEBHOOK_ENV]);
        assert!(
            t.run
                .contains(&format!("--webhook-env {DEFAULT_WEBHOOK_ENV}")),
            "run: {}",
            t.run
        );
    }

    #[test]
    fn webhook_url_env_override_flows_to_run_command_and_env_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = toml::Table::new();
        config.insert(
            "webhook_url_env".into(),
            toml::Value::String("SLACK_WEBHOOK_STAGING".into()),
        );
        let tasks = SlackIntegration.detected_tasks(tmp.path(), &config);
        let t = &tasks[0];
        assert!(
            t.run.contains("--webhook-env SLACK_WEBHOOK_STAGING"),
            "run: {}",
            t.run
        );
        // The overridden name must be in the task's env allowlist so
        // the executor passes it through to the child process.
        assert_eq!(t.env_vars, vec!["SLACK_WEBHOOK_STAGING"]);
    }

    #[test]
    fn channel_and_username_overrides_are_shell_quoted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = toml::Table::new();
        config.insert("channel".into(), toml::Value::String("#deploys".into()));
        config.insert("username".into(), toml::Value::String("Bento Bot".into()));
        let tasks = SlackIntegration.detected_tasks(tmp.path(), &config);
        let run = &tasks[0].run;
        assert!(run.contains("--channel '#deploys'"), "run: {run}");
        assert!(run.contains("--username 'Bento Bot'"), "run: {run}");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        // Defensive: a username like `O'Brien Deploy` must survive
        // round-tripping through `sh -c` without mangling.
        assert_eq!(shell_quote("O'Brien"), "'O'\\''Brien'");
    }
}
