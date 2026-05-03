//! Linear integration — Notify-kind garnish that transitions Linear
//! issues after a Deploy based on the deploy's context.
//!
//! - Detects: `[integrations.linear]` in `dish.toml` (config-driven,
//!   same pattern as Slack — there's no platform file to sniff).
//! - Required env: `LINEAR_API_KEY` (Personal API key or OAuth access
//!   token). Overridable per-dish via `api_key_env = "..."`.
//! - Required CLI: `bento` itself — the emitted task invokes the
//!   hidden `bento _linear-notify` sub-command.
//! - Emits: `linear:notify` (Notify kind).
//!
//! Behaviour (v1, opinionated): on a successful deploy, transition
//! every referenced issue to a configured target state (default:
//! `Deployed`). Issue IDs are extracted from the deploy's git log /
//! task name / output via `[0-9]{1,}` match against Linear's
//! `^[A-Z]+-\d+$` pattern elsewhere. If no issues match, post a single
//! comment on a fallback issue declared via `fallback_issue_id`
//! (optional — skipped if unset).
//!
//! v1 is intentionally small: one target state, one fallback issue.
//! Deeper shaping (per-environment states, issue-scoped comments) is
//! a follow-on once the pattern has settled.

use std::path::Path;

use crate::integration::{CliRequirement, Integration, IntegrationTask, IntegrationTaskKind};

pub struct LinearIntegration;

const DEFAULT_API_KEY_ENV: &str = "LINEAR_API_KEY";

impl Integration for LinearIntegration {
    fn id(&self) -> &str {
        "linear"
    }

    fn display_name(&self) -> &str {
        "Linear"
    }

    fn detect(&self, _dir: &Path) -> bool {
        false
    }

    fn required_env(&self) -> Vec<String> {
        vec![DEFAULT_API_KEY_ENV.into()]
    }

    fn required_cli(&self) -> Vec<CliRequirement> {
        vec![CliRequirement::new(
            "bento",
            "https://github.com/bento-sh/bento (`curl -fsSL .../install.sh | sh`)",
        )]
    }

    fn detected_tasks(&self, _dir: &Path, config: &toml::Table) -> Vec<IntegrationTask> {
        let api_key_env = config
            .get("api_key_env")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_API_KEY_ENV)
            .to_string();

        // State name to transition referenced issues to on a
        // successful deploy. Defaults to "Deployed" — most Linear
        // teams have or trivially add that state.
        let target_state = config
            .get("target_state")
            .and_then(|v| v.as_str())
            .unwrap_or("Deployed");

        // Optional catch-all issue. When the deploy's context has no
        // discoverable issue IDs, the integration posts a comment on
        // this issue instead (so release visibility isn't lost).
        let fallback = config
            .get("fallback_issue_id")
            .and_then(|v| v.as_str())
            .map(|s| format!(" --fallback-issue-id {}", shell_quote(s)))
            .unwrap_or_default();

        // Team key is required for some Linear workflows but optional
        // for state transitions keyed purely by issue identifier.
        let team = config
            .get("team")
            .and_then(|v| v.as_str())
            .map(|s| format!(" --team {}", shell_quote(s)))
            .unwrap_or_default();

        let run = format!(
            "bento _linear-notify --api-key-env {api_key_env} --target-state {state}{fallback}{team}",
            state = shell_quote(target_state),
        );

        vec![IntegrationTask {
            name: "linear:notify".into(),
            kind: IntegrationTaskKind::Notify,
            run,
            depends_on: Vec::new(),
            env_vars: vec![api_key_env],
            no_cache: true,
            outputs: Vec::new(),
        }]
    }
}

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
        assert_eq!(LinearIntegration.id(), "linear");
        assert_eq!(LinearIntegration.display_name(), "Linear");
        assert_eq!(LinearIntegration.required_env(), vec![DEFAULT_API_KEY_ENV]);
    }

    #[test]
    fn detect_false_without_explicit_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!LinearIntegration.detect(tmp.path()));
    }

    #[test]
    fn defaults_produce_a_notify_task_with_deployed_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = toml::Table::new();
        let tasks = LinearIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.name, "linear:notify");
        assert_eq!(t.kind, IntegrationTaskKind::Notify);
        assert!(t.no_cache);
        assert!(
            t.run.contains("--api-key-env LINEAR_API_KEY"),
            "run: {}",
            t.run
        );
        assert!(
            t.run.contains("--target-state 'Deployed'"),
            "run: {}",
            t.run
        );
        assert_eq!(t.env_vars, vec![DEFAULT_API_KEY_ENV]);
    }

    #[test]
    fn target_state_override_is_shell_quoted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = toml::Table::new();
        config.insert(
            "target_state".into(),
            toml::Value::String("In Production".into()),
        );
        let tasks = LinearIntegration.detected_tasks(tmp.path(), &config);
        let run = &tasks[0].run;
        assert!(run.contains("--target-state 'In Production'"), "run: {run}");
    }

    #[test]
    fn fallback_issue_override_is_passed_through() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = toml::Table::new();
        config.insert(
            "fallback_issue_id".into(),
            toml::Value::String("ENG-1234".into()),
        );
        let tasks = LinearIntegration.detected_tasks(tmp.path(), &config);
        let run = &tasks[0].run;
        assert!(run.contains("--fallback-issue-id 'ENG-1234'"), "run: {run}");
    }

    #[test]
    fn api_key_env_override_flows_to_env_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = toml::Table::new();
        config.insert(
            "api_key_env".into(),
            toml::Value::String("LINEAR_KEY_PROD".into()),
        );
        let tasks = LinearIntegration.detected_tasks(tmp.path(), &config);
        let t = &tasks[0];
        assert_eq!(t.env_vars, vec!["LINEAR_KEY_PROD"]);
        assert!(
            t.run.contains("--api-key-env LINEAR_KEY_PROD"),
            "run: {}",
            t.run
        );
    }
}
