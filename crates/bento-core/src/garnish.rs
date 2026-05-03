//! Garnish payload — the structured JSON that gets piped to
//! [`IntegrationTaskKind::Notify`] tasks' stdin after each Deploy
//! task completes.
//!
//! Integration Notify tasks (Slack posts, Linear status flips,
//! PagerDuty triggers) chain automatically off every Deploy task in
//! the same dish, fan out in parallel, and receive their triggering
//! task's outcome as a single JSON object on stdin — never as env
//! vars (shell-visible) and never as CLI args (ps-visible).
//!
//! The schema is published via `bento schema garnish-payload` so
//! agent-authored notify scripts can validate their inputs.
//!
//! [`IntegrationTaskKind::Notify`]: bento_adapters::IntegrationTaskKind::Notify

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Current `GarnishPayload.schema_version`. Bump only on a change that
/// existing notify consumers couldn't handle transparently.
pub const GARNISH_PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// The structured JSON object delivered to Notify-kind tasks on stdin.
/// Shape is stable across patch/minor releases within the same
/// `schema_version`; the version bumps whenever a breaking change
/// would force agent-authored notify scripts to adapt.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct GarnishPayload {
    /// Starts at 1. Bumped on breaking shape changes only; additive
    /// field introductions (e.g. a new optional trigger field) do not
    /// bump it because serde-ignorant consumers tolerate extra keys.
    pub schema_version: u32,
    /// The bento CLI version emitting the payload. Lets notify scripts
    /// branch conditionally on feature availability — e.g. `jq '.bento_version'`.
    pub bento_version: String,
    /// Value of `bento deploy --env <name>` when the deploy fired.
    /// `None` if `--env` wasn't passed (ad-hoc `--secret-from` flow or
    /// bare deploy against raw env vars). Lets notify tasks format
    /// messages differently for staging vs prod without re-reading
    /// the CLI args.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The completed task that triggered this garnish.
    pub trigger: GarnishPayloadTrigger,
}

/// Identity and outcome of the task that caused a garnish to fire.
/// Everything a notify script might want to template into a message
/// or use for deduping/idempotence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct GarnishPayloadTrigger {
    /// Task name as declared by the integration / user — e.g.
    /// `railway:deploy`, `railway:deploy:landing-page`, `vercel:deploy`.
    /// Pick this over `integration_kind` when you want to present a
    /// human-readable source for the notification.
    pub task_name: String,
    /// Dish the task ran in (`DishConfig.name`).
    pub dish_name: String,
    /// Bento the dish belongs to. When a dish is in multiple bentos,
    /// this is the bento that was being executed — useful for
    /// "staging" vs "prod" bento splits.
    pub bento_name: String,
    /// Tagged-enum outcome as a lowercase string. One of `built`,
    /// `cache_hit`, `failed`, `skipped`. Matches
    /// [`crate::TaskOutcome`]'s serialisation.
    pub outcome: String,
    /// Process exit code. 0 on `built` / `cache_hit`; the real exit
    /// code (often non-zero) on `failed`; `-1` when the process
    /// never got to exec (missing env var, preflight fail).
    pub exit_code: i32,
    /// Wall-clock duration of the triggering task in milliseconds.
    pub duration_ms: u64,
    /// The task's content-cache key. Useful for deduping notifications
    /// on retries — two deploys of the same SHA have the same key for
    /// their deploy inputs (though `no_cache = true` on deploy tasks
    /// means the cache isn't consulted for hits).
    pub cache_key: String,
    /// The integration kind as a lowercase string: `deploy`,
    /// `deploy_preview`, `rollback`, `notify`, `release`, `other`.
    /// Notify tasks can switch on this to format differently for
    /// preview vs prod, for example.
    pub integration_kind: String,
    /// Captured stdout/stderr from the triggering task. For
    /// `railway:deploy`, this is where the Build Logs URL lives.
    /// Tail-truncated to 4KB; empty string when nothing was
    /// captured (cache hit, skipped).
    pub output_excerpt: String,
    /// Short stderr excerpt when the task failed. `None` on success.
    /// The full task's stderr lives in the cache bundle — use
    /// `bento why <cache_key>` to fetch the whole thing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_excerpt: Option<String>,
}

impl GarnishPayload {
    /// Serialise to a newline-terminated JSON line — the shape
    /// notify tasks receive on stdin. Newline is intentional: agent-
    /// authored scripts can choose to `while read line` for line-
    /// oriented parsing even though we only ever emit one record.
    pub fn to_ndjson_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("GarnishPayload always serialises");
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_serde() {
        let p = GarnishPayload {
            schema_version: GARNISH_PAYLOAD_SCHEMA_VERSION,
            bento_version: "0.1.0".into(),
            environment: Some("staging".into()),
            trigger: GarnishPayloadTrigger {
                task_name: "railway:deploy".into(),
                dish_name: "admin".into(),
                bento_name: "prod".into(),
                outcome: "built".into(),
                exit_code: 0,
                duration_ms: 4272,
                cache_key: "abcdef1234567890".into(),
                integration_kind: "deploy".into(),
                output_excerpt: "Build Logs: https://railway.com/...\n".into(),
                stderr_excerpt: None,
            },
        };
        let line = p.to_ndjson_line();
        assert!(line.ends_with('\n'));
        let parsed: GarnishPayload = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn omits_none_fields_from_json() {
        let p = GarnishPayload {
            schema_version: GARNISH_PAYLOAD_SCHEMA_VERSION,
            bento_version: "0.1.0".into(),
            environment: None,
            trigger: GarnishPayloadTrigger {
                task_name: "x:deploy".into(),
                dish_name: "d".into(),
                bento_name: "b".into(),
                outcome: "built".into(),
                exit_code: 0,
                duration_ms: 100,
                cache_key: "key".into(),
                integration_kind: "deploy".into(),
                output_excerpt: String::new(),
                stderr_excerpt: None,
            },
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("\"environment\""));
        assert!(!json.contains("\"stderr_excerpt\""));
    }

    #[test]
    fn schema_is_valid_json_schema() {
        // Just confirm schemars derives something serialisable —
        // the actual contract is exercised by `bento schema` and by
        // consumers that validate against it.
        let schema = schemars::schema_for!(GarnishPayload);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(json.contains("schema_version"));
        assert!(json.contains("trigger"));
    }
}
