//! Hidden `bento _linear-notify` subcommand — reads a GarnishPayload
//! on stdin, extracts issue identifiers from the deploy context, and
//! transitions referenced Linear issues to a target state via
//! Linear's GraphQL API.
//!
//! Behaviour:
//!   - Extract issue IDs matching Linear's `[A-Z]{2,}-\d+` pattern
//!     from the payload's captured output + task name.
//!   - For each matched issue, POST an `issueUpdate` mutation
//!     transitioning it to the target state.
//!   - If no issues matched and a `--fallback-issue-id` is set, post
//!     a deploy comment there instead so release visibility isn't
//!     lost silently.
//!   - On the Linear side: unknown state names fail clearly; unknown
//!     issues log a warning but don't fail the command (partial
//!     success is still useful — one missing issue shouldn't block
//!     the others).

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bento_core::GarnishPayload;

const LINEAR_API: &str = "https://api.linear.app/graphql";

pub fn run(
    api_key_env: &str,
    target_state: &str,
    fallback_issue_id: Option<&str>,
    team: Option<&str>,
) -> Result<i32> {
    let api_key = std::env::var(api_key_env).with_context(|| {
        format!("reading Linear API key from ${api_key_env} — is the env var exported?")
    })?;
    if api_key.trim().is_empty() {
        return Err(anyhow!(
            "${api_key_env} is set but empty — a Linear Personal API key is required"
        ));
    }

    let payload = read_payload_from_stdin()?;

    // Don't transition issues on a failed deploy — that would
    // advertise a broken release as done. A `fallback_issue_id`
    // notify, if set, still fires so operators see the failure.
    if payload.trigger.outcome == "failed" {
        if let Some(issue) = fallback_issue_id {
            post_deploy_comment(&api_key, issue, &payload)?;
        } else {
            let _ = writeln!(
                std::io::stdout(),
                "linear: deploy failed + no fallback_issue_id; skipping issue transitions"
            );
        }
        return Ok(0);
    }

    let issue_refs = discover_issue_refs(&payload);
    if issue_refs.is_empty() {
        if let Some(issue) = fallback_issue_id {
            post_deploy_comment(&api_key, issue, &payload)?;
        } else {
            let _ = writeln!(
                std::io::stdout(),
                "linear: no issue IDs in deploy output; nothing to transition"
            );
        }
        return Ok(0);
    }

    let state_id = find_state_id(&api_key, target_state, team)?;
    let mut failed = 0usize;
    for issue_ref in &issue_refs {
        match transition_issue(&api_key, issue_ref, &state_id) {
            Ok(()) => {
                let _ = writeln!(std::io::stdout(), "linear: {issue_ref} → {target_state}");
            }
            Err(e) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "linear: failed to transition {issue_ref}: {e:#}"
                );
                failed += 1;
            }
        }
    }

    // Partial success is fine — if one issue is garbage-collected
    // Linear-side, the others still deserve the update. Only fail
    // hard if nothing at all succeeded.
    if failed == issue_refs.len() {
        anyhow::bail!("linear: every issue transition failed ({failed}/{failed})");
    }
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
    serde_json::from_str(trimmed).context("parsing garnish payload JSON")
}

/// Scan a payload for Linear issue identifiers. Matches the canonical
/// team-prefixed form (`ENG-1234`, `OPS-42`) which is what shows up
/// in commit messages, branch names, and PR titles that most deploys
/// print. Duplicates collapsed; order preserved by first-seen so the
/// report has predictable ordering.
fn discover_issue_refs(p: &GarnishPayload) -> Vec<String> {
    let haystack = format!(
        "{} {} {}",
        p.trigger.task_name, p.trigger.dish_name, p.trigger.output_excerpt
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for token in tokenize_for_issue_ids(&haystack) {
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

/// Split on whitespace + punctuation, keep tokens matching Linear's
/// `^[A-Z]{2,}-\d+$` shape. Conservative — requires at least two
/// uppercase letters in the team prefix so common English capitalised
/// words (`I-95`, `A-1`) don't false-positive.
fn tokenize_for_issue_ids(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter_map(|raw| {
            // token must have a single dash separating uppercase-prefix
            // from digit-suffix.
            let (prefix, suffix) = raw.split_once('-')?;
            if prefix.len() < 2
                || !prefix.chars().all(|c| c.is_ascii_uppercase())
                || suffix.is_empty()
                || !suffix.chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            Some(format!("{prefix}-{suffix}"))
        })
}

fn linear_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build()
}

/// Look up the workflow-state ID by name (optionally scoped to a team).
/// Linear's mutations take a UUID, not a human name — so every run
/// makes this GraphQL round-trip once. Cached per-process would be
/// nice but isn't worth the complexity for a notify that runs a
/// handful of times.
fn find_state_id(api_key: &str, name: &str, team: Option<&str>) -> Result<String> {
    let query = r#"query($name:String!){workflowStates(filter:{name:{eq:$name}}){nodes{id name team{key}}}}"#;
    let resp: serde_json::Value = graphql(api_key, query, serde_json::json!({ "name": name }))?;
    let nodes = resp
        .pointer("/data/workflowStates/nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("malformed workflowStates response: {resp}"))?;
    let matched: Vec<&serde_json::Value> = nodes
        .iter()
        .filter(|n| {
            team.map_or(true, |t| {
                n.pointer("/team/key").and_then(|k| k.as_str()) == Some(t)
            })
        })
        .collect();
    match matched.len() {
        0 => Err(anyhow!(
            "no Linear workflow state named '{name}'{} — check target_state in dish.toml",
            team.map(|t| format!(" on team '{t}'")).unwrap_or_default()
        )),
        1 => Ok(matched[0]
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("state has no id"))?
            .to_string()),
        _ => Err(anyhow!(
            "workflow state '{name}' is ambiguous across {} teams — set `team = \"...\"` in \
             [integrations.linear] to disambiguate",
            matched.len()
        )),
    }
}

fn transition_issue(api_key: &str, issue_identifier: &str, state_id: &str) -> Result<()> {
    // Linear's mutations accept the human identifier directly when
    // passed as the `issueId` arg to `issueUpdate` via a lookup
    // alias. We use the `issue` query to resolve identifier → id
    // first so the mutation is unambiguous regardless of workspace
    // shape.
    let lookup_query = r#"query($id:String!){issue(id:$id){id}}"#;
    let lookup = graphql(
        api_key,
        lookup_query,
        serde_json::json!({ "id": issue_identifier }),
    )?;
    let issue_id = lookup
        .pointer("/data/issue/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("issue '{issue_identifier}' not found (or API key lacks access)"))?
        .to_string();

    let update = r#"mutation($id:String!,$state:String!){issueUpdate(id:$id,input:{stateId:$state}){success}}"#;
    let resp = graphql(
        api_key,
        update,
        serde_json::json!({ "id": issue_id, "state": state_id }),
    )?;
    let success = resp
        .pointer("/data/issueUpdate/success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !success {
        anyhow::bail!("issueUpdate returned success=false: {resp}");
    }
    Ok(())
}

fn post_deploy_comment(api_key: &str, issue_identifier: &str, p: &GarnishPayload) -> Result<()> {
    let lookup_query = r#"query($id:String!){issue(id:$id){id}}"#;
    let lookup = graphql(
        api_key,
        lookup_query,
        serde_json::json!({ "id": issue_identifier }),
    )?;
    let issue_id = lookup
        .pointer("/data/issue/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("fallback issue '{issue_identifier}' not found"))?
        .to_string();

    let env = p.environment.as_deref().unwrap_or("unknown env");
    let body = format!(
        "Deploy: **{dish}** → **{env}** — outcome: `{outcome}`. Task: `{task}`.",
        dish = p.trigger.dish_name,
        outcome = p.trigger.outcome,
        task = p.trigger.task_name,
    );
    let mutation = r#"mutation($issueId:String!,$body:String!){commentCreate(input:{issueId:$issueId,body:$body}){success}}"#;
    let resp = graphql(
        api_key,
        mutation,
        serde_json::json!({ "issueId": issue_id, "body": body }),
    )?;
    let success = resp
        .pointer("/data/commentCreate/success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !success {
        anyhow::bail!("commentCreate returned success=false: {resp}");
    }
    let _ = writeln!(std::io::stdout(), "linear: commented on {issue_identifier}");
    Ok(())
}

fn graphql(api_key: &str, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
    let body = serde_json::json!({ "query": query, "variables": variables });
    let resp = linear_agent()
        .post(LINEAR_API)
        .set("authorization", api_key)
        .set("content-type", "application/json")
        .send_string(&serde_json::to_string(&body)?);
    match resp {
        Ok(r) => {
            let body = r.into_string().context("reading Linear response body")?;
            let v: serde_json::Value =
                serde_json::from_str(&body).context("parsing Linear response JSON")?;
            if let Some(errors) = v.get("errors") {
                anyhow::bail!("Linear GraphQL errors: {errors}");
            }
            Ok(v)
        }
        Err(ureq::Error::Status(status, r)) => {
            let body = r.into_string().unwrap_or_default();
            anyhow::bail!("Linear HTTP {status}: {body}")
        }
        Err(ureq::Error::Transport(e)) => anyhow::bail!("Linear transport error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bento_core::{GarnishPayload, GarnishPayloadTrigger, GARNISH_PAYLOAD_SCHEMA_VERSION};

    fn payload_with(output: &str, task_name: &str) -> GarnishPayload {
        GarnishPayload {
            schema_version: GARNISH_PAYLOAD_SCHEMA_VERSION,
            bento_version: "test".into(),
            environment: Some("prod".into()),
            trigger: GarnishPayloadTrigger {
                task_name: task_name.into(),
                dish_name: "admin".into(),
                bento_name: "prod".into(),
                outcome: "built".into(),
                exit_code: 0,
                duration_ms: 0,
                cache_key: "k".into(),
                integration_kind: "deploy".into(),
                output_excerpt: output.into(),
                stderr_excerpt: None,
            },
        }
    }

    #[test]
    fn discovers_single_issue_ref_from_output() {
        let p = payload_with("Built ENG-1234 successfully", "railway:deploy");
        let refs = discover_issue_refs(&p);
        assert_eq!(refs, vec!["ENG-1234"]);
    }

    #[test]
    fn discovers_multiple_issues_across_output_and_task_name() {
        let p = payload_with(
            "ENG-1 fixed, ops-2 ignored (lowercase), OPS-42 closes issue.",
            "ENG-99:deploy",
        );
        let refs = discover_issue_refs(&p);
        // Preserves first-seen order (task name scanned first in the
        // haystack), dedupes, skips lowercase `ops-2`.
        assert_eq!(refs, vec!["ENG-99", "ENG-1", "OPS-42"]);
    }

    #[test]
    fn skips_false_positives_like_single_letter_prefix() {
        // Single-letter prefix like `I-95` (highway), `A-1` (grade) —
        // Linear requires ≥2 uppercase letters.
        let p = payload_with("Released on I-95 via A-1 road.", "deploy");
        let refs = discover_issue_refs(&p);
        assert!(refs.is_empty(), "got: {refs:?}");
    }

    #[test]
    fn skips_lowercase_prefixes() {
        let p = payload_with("Closes pr-42 and task-7", "deploy");
        let refs = discover_issue_refs(&p);
        assert!(
            refs.is_empty(),
            "lowercase prefixes must not match: {refs:?}"
        );
    }

    #[test]
    fn dedupes_repeated_issue_references() {
        let p = payload_with("ENG-1 ENG-1 eng-1 ENG-1.", "deploy");
        let refs = discover_issue_refs(&p);
        assert_eq!(refs, vec!["ENG-1"]);
    }

    #[test]
    fn punctuation_around_id_is_stripped() {
        let p = payload_with("Fixed (ENG-99), shipped [ENG-100]! See ENG-1,2", "deploy");
        let refs = discover_issue_refs(&p);
        // ENG-1,2 → tokenizer splits at `,` → "ENG-1" then "2"; "2" fails prefix rule.
        assert_eq!(refs, vec!["ENG-99", "ENG-100", "ENG-1"]);
    }
}
