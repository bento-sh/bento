//! Hidden `bento _slack-post` subcommand — reads a GarnishPayload on
//! stdin, formats a Slack message, POSTs to an Incoming Webhook.
//!
//! Built-in counterpart to the `SlackIntegration` in `bento-adapters`:
//! keeps the shipped integration zero-dependency (no curl / jq on
//! host). The subcommand name is `_`-prefixed so it doesn't crowd the
//! user-facing verb list — it's an internal tool the integration
//! invokes, not something humans run directly.

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bento_core::GarnishPayload;

/// Wire shape of the Slack Incoming Webhook body we POST. Only the
/// fields Slack actually reads are included — everything else is
/// ignored server-side, so we keep it tight.
#[derive(serde::Serialize)]
struct SlackMessage<'a> {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon_emoji: Option<&'a str>,
}

/// Entry point called from `main.rs` when the `_slack-post` arg is
/// parsed. `webhook_env` is the host env-var name holding the URL;
/// `channel` and `username` are optional overrides flowed through
/// from the integration's dish config.
pub fn run(webhook_env: &str, channel: Option<&str>, username: Option<&str>) -> Result<i32> {
    let webhook_url = std::env::var(webhook_env).with_context(|| {
        format!("reading Slack webhook URL from ${webhook_env} — is the env var exported?")
    })?;
    if webhook_url.trim().is_empty() {
        return Err(anyhow!(
            "${webhook_env} is set but empty — a Slack Incoming Webhook URL is required"
        ));
    }

    let payload = read_payload_from_stdin()?;
    let body = build_slack_message(&payload, channel, username);
    post_to_slack(&webhook_url, &body)?;
    Ok(0)
}

fn read_payload_from_stdin() -> Result<GarnishPayload> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading garnish payload from stdin")?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        anyhow::bail!("no garnish payload on stdin — was this invoked by bento's notify fan-out?");
    }
    serde_json::from_str(trimmed).with_context(|| {
        format!(
            "parsing garnish payload JSON (first 200 chars: {})",
            &trimmed.chars().take(200).collect::<String>()
        )
    })
}

/// Shape the GarnishPayload into Slack-friendly text. Outcome drives
/// the leading emoji so a quick channel scroll reads at-a-glance:
/// rocket = success, rotating_light = failure, package = cache hit
/// (rare for Deploy), ghost = skipped.
fn build_slack_message<'a>(
    p: &GarnishPayload,
    channel: Option<&'a str>,
    username: Option<&'a str>,
) -> SlackMessage<'a> {
    let (emoji, verb) = match p.trigger.outcome.as_str() {
        "built" => (":rocket:", "deployed"),
        "failed" => (":rotating_light:", "deploy FAILED"),
        "cache_hit" => (":package:", "cache-hit (unusual for a deploy)"),
        "skipped" => (":ghost:", "skipped"),
        _ => (":bento:", "ran"),
    };
    let env = p.environment.as_deref().unwrap_or("no-env");
    let dur_s = (p.trigger.duration_ms as f64) / 1000.0;
    let url = extract_url(&p.trigger.output_excerpt);

    // Plain-text, Slack-markdown-aware format. Keeps the integration
    // opinionated-but-minimal; users wanting blocks / attachments /
    // threading write their own Notify-kind integration or shell out.
    let mut text = format!(
        "{emoji} *{dish}* {verb} → *{env}* in {dur_s:.1}s (task `{task}`)",
        dish = p.trigger.dish_name,
        task = p.trigger.task_name,
    );
    if let Some(u) = url {
        text.push_str(&format!("\n<{u}|Build logs>"));
    }
    if let Some(err) = p.trigger.stderr_excerpt.as_deref() {
        if !err.is_empty() {
            // Slack renders triple-backtick as a monospaced code block.
            // Cap at 2KB so a pathological backtrace doesn't blow the
            // 40KB Slack message limit.
            let snippet: String = err.chars().take(2_000).collect();
            text.push_str(&format!("\n```\n{snippet}\n```"));
        }
    }

    SlackMessage {
        text,
        channel,
        username,
        // The colour of the bento icon — cute branding, zero cost.
        icon_emoji: Some(":bento:"),
    }
}

/// Pluck an `https://…` URL from the deploy's captured output. Most
/// deploy CLIs (railway, vercel, fly) print a canonical "Build Logs:"
/// or "Preview: https://…" line near the tail; grabbing the last one
/// handles both prefixed and bare forms without pinning to specific
/// CLIs.
fn extract_url(excerpt: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for raw in excerpt.split_whitespace() {
        if raw.starts_with("https://") || raw.starts_with("http://") {
            // Strip trailing punctuation commonly attached by log
            // formatters — `,`, `.`, `)`, etc.
            let cleaned: String = raw
                .trim_end_matches(['.', ',', ')', '>', ']', '"', '\''])
                .to_string();
            last = Some(cleaned);
        }
    }
    last
}

fn post_to_slack(url: &str, msg: &SlackMessage<'_>) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let resp = agent
        .post(url)
        .set("content-type", "application/json")
        .send_bytes(&body);
    match resp {
        Ok(r) => {
            // Slack returns 200 with body "ok" on success. Anything
            // else is a problem — surface the body so the operator
            // can see the real failure.
            let status = r.status();
            let mut body = String::new();
            let _ = r.into_reader().take(4 * 1024).read_to_string(&mut body);
            if !(200..300).contains(&status) {
                anyhow::bail!("Slack POST returned {status}: {body}");
            }
            // Success path — Slack's `ok` body is unremarkable; print
            // it so the ExecutedTask output_excerpt has something to
            // surface in the report.
            let _ = writeln!(std::io::stdout(), "slack: {body}");
            Ok(())
        }
        Err(ureq::Error::Status(status, r)) => {
            let mut body = String::new();
            let _ = r.into_reader().take(4 * 1024).read_to_string(&mut body);
            anyhow::bail!("Slack POST returned {status}: {body}")
        }
        Err(ureq::Error::Transport(e)) => {
            anyhow::bail!("Slack POST transport error: {e}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bento_core::{GarnishPayload, GarnishPayloadTrigger, GARNISH_PAYLOAD_SCHEMA_VERSION};

    fn sample_payload(outcome: &str, output: &str, stderr: Option<&str>) -> GarnishPayload {
        GarnishPayload {
            schema_version: GARNISH_PAYLOAD_SCHEMA_VERSION,
            bento_version: "test".into(),
            environment: Some("staging".into()),
            trigger: GarnishPayloadTrigger {
                task_name: "railway:deploy".into(),
                dish_name: "admin".into(),
                bento_name: "prod".into(),
                outcome: outcome.into(),
                exit_code: if outcome == "failed" { 7 } else { 0 },
                duration_ms: 4272,
                cache_key: "abcdef".into(),
                integration_kind: "deploy".into(),
                output_excerpt: output.into(),
                stderr_excerpt: stderr.map(str::to_string),
            },
        }
    }

    #[test]
    fn built_outcome_renders_rocket() {
        let p = sample_payload("built", "Build Logs: https://x/abc", None);
        let m = build_slack_message(&p, None, None);
        assert!(m.text.starts_with(":rocket:"), "text: {}", m.text);
        assert!(m.text.contains("*admin* deployed"), "text: {}", m.text);
        assert!(m.text.contains("*staging*"), "text: {}", m.text);
        assert!(m.text.contains("4.3s"), "duration formatted: {}", m.text);
        assert!(
            m.text.contains("<https://x/abc|Build logs>"),
            "text: {}",
            m.text
        );
    }

    #[test]
    fn failed_outcome_renders_alert_and_stderr() {
        let p = sample_payload("failed", "", Some("boom: config parse error"));
        let m = build_slack_message(&p, None, None);
        assert!(m.text.starts_with(":rotating_light:"), "text: {}", m.text);
        assert!(m.text.contains("deploy FAILED"), "text: {}", m.text);
        assert!(
            m.text.contains("boom: config parse error"),
            "text: {}",
            m.text
        );
        // Stderr should land in a Slack code block.
        assert!(m.text.contains("```"), "text: {}", m.text);
    }

    #[test]
    fn missing_environment_falls_back_to_placeholder() {
        let mut p = sample_payload("built", "", None);
        p.environment = None;
        let m = build_slack_message(&p, None, None);
        assert!(m.text.contains("*no-env*"), "text: {}", m.text);
    }

    #[test]
    fn extract_url_grabs_last_url_and_trims_trailing_punctuation() {
        let ex = "Indexing...\nUploading files.\n  Build Logs: https://railway.com/project/x/deploy/abc.\n";
        assert_eq!(
            extract_url(ex).as_deref(),
            Some("https://railway.com/project/x/deploy/abc")
        );
    }

    #[test]
    fn extract_url_returns_none_when_no_url() {
        assert!(extract_url("nothing to see here").is_none());
    }

    #[test]
    fn channel_and_username_are_passed_through() {
        let p = sample_payload("built", "", None);
        let m = build_slack_message(&p, Some("#alerts"), Some("DeployBot"));
        assert_eq!(m.channel, Some("#alerts"));
        assert_eq!(m.username, Some("DeployBot"));
    }

    #[test]
    fn stderr_is_tail_capped_to_avoid_slack_overflow() {
        // Slack's per-message cap is ~40KB; we cap stderr at 2KB.
        let huge = "e".repeat(10_000);
        let p = sample_payload("failed", "", Some(&huge));
        let m = build_slack_message(&p, None, None);
        // The message should contain a bounded number of 'e's.
        let e_count = m.text.chars().filter(|c| *c == 'e').count();
        // Some 'e's come from other words ("deploy FAILED", etc.) so
        // we use a loose upper bound well below the 10k source.
        assert!(
            e_count < 2_100,
            "stderr snippet not capped (e_count={e_count})"
        );
    }
}
