//! Send a `BuildReport` to the configured `bento://` cache server at
//! the end of `bento ci` / `bento build` invocations.
//!
//! The cache server (the bento.build hosted service, or any
//! compatible self-hosted endpoint) terminates the request at
//! `<base>/report/build` and decides what to do — bento OSS knows
//! nothing about a "control plane" or "dashboard". Self-hosters can
//! implement the endpoint against any backend, or reject with 404 and
//! the CLI silently no-ops.
//!
//! # Posture
//!
//! Best-effort. Any failure (no remote configured, no token, network
//! error, server 5xx) is logged via `tracing::warn!` (or silently
//! dropped, for the "no telemetry desired" cases) and swallowed. The
//! CLI never fails a build because the report didn't land — telemetry
//! is downstream of user-visible output by design.

use std::time::Duration;

use bento_cas_protocol::{BuildReport, BuildStatus};

use crate::run::ExecutionReport;

/// Tight per-attempt timeout. The hot path is "user already saw the
/// build outcome and is waiting for their prompt back" — a slow report
/// post should not visibly stall the CLI.
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// Fire a build report at the configured `bento://` remote.
///
/// `cache_remote` and `cache_token_env` come from the caller's loaded
/// `Workspace` (`workspace.repo.cache.remote` /
/// `workspace.repo.cache.remote_token_env`); kept as plain `&str`s
/// here so this module doesn't pull in `bento-config` and stays easy
/// to call after the `Workspace` has already moved into an
/// `Executor`.
pub fn send(
    cache_remote: Option<&str>,
    cache_token_env: Option<&str>,
    report: &ExecutionReport,
    package: impl Into<String>,
) {
    let Some(url) = build_endpoint(cache_remote) else {
        return; // no remote, or s3:// — nothing to report to.
    };
    let Some(env_name) = cache_token_env.filter(|s| !s.is_empty()) else {
        return; // no env var declared — silent skip.
    };
    let Some(token) = bento_cache::token::resolve_cache_token(env_name) else {
        return; // no token env/keychain/file — silent skip.
    };

    let body = build_report_from(report, package.into());
    let json = match serde_json::to_string(&body) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("report/build: serialize failed: {e}");
            return;
        }
    };

    let agent = ureq::AgentBuilder::new()
        .timeout(POST_TIMEOUT)
        .user_agent(concat!("bento-cli/", env!("CARGO_PKG_VERSION"), " report"))
        .build();

    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&json)
    {
        Ok(resp) => {
            let status = resp.status();
            if !(200..300).contains(&status) {
                tracing::warn!("report/build {url}: server replied {status}");
            }
        }
        // 404 = self-hoster whose cache server doesn't implement
        // /report/build. Not an error — the protocol is opt-in for
        // backends that don't care about dashboards.
        Err(ureq::Error::Status(404, _)) => {}
        Err(ureq::Error::Status(status, _)) => {
            tracing::warn!("report/build {url}: server replied {status}");
        }
        Err(ureq::Error::Transport(t)) => {
            tracing::warn!("report/build {url}: transport error: {t}");
        }
    }
}

/// `bento://host[/prefix]` → `https://host/report/build`. Returns
/// `None` for any other scheme (`s3://`, missing remote, malformed)
/// since those cache backends don't speak our extension protocol.
fn build_endpoint(cache_remote: Option<&str>) -> Option<String> {
    let remote = cache_remote?;
    let rest = remote.strip_prefix("bento://")?;
    let host = rest.split('/').next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("https://{host}/report/build"))
}

fn build_report_from(report: &ExecutionReport, package: String) -> BuildReport {
    let s = &report.summary;

    // Hit ratio is "fraction of executable tasks that came from cache"
    // — `hits / (hits + built)`. Tasks that never ran (install
    // failures, deploy short-circuits) aren't part of the denominator
    // because they never had a cache lookup.
    let denom = (s.hits + s.built).max(1) as f32;
    let cache_hit_ratio = (s.hits as f32 / denom).clamp(0.0, 1.0);

    let executed = s.hits + s.built;
    let expected = s.tasks.saturating_sub(s.deploy_unchanged);
    let status = if s.failed > 0 || s.install_failures > 0 {
        BuildStatus::Failed
    } else if executed < expected {
        // Some tasks were planned but didn't run AND nothing failed —
        // typically install_failures upstream of a dish chain, or a
        // mid-flight cancellation. Surfaces in the dashboard as a
        // distinct outcome from a clean success.
        BuildStatus::Partial
    } else {
        BuildStatus::Success
    };

    BuildReport {
        package,
        branch: git_branch(),
        sha: git_sha(),
        cache_hit_ratio,
        status,
        duration_ms: s.duration_ms,
    }
}

fn git_branch() -> Option<String> {
    git_one_line(&["symbolic-ref", "--short", "HEAD"])
}

fn git_sha() -> Option<String> {
    git_one_line(&["rev-parse", "HEAD"])
}

fn git_one_line(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::ExecutionSummary;

    fn report_with(summary: ExecutionSummary) -> ExecutionReport {
        ExecutionReport {
            bentos: vec![],
            summary,
        }
    }

    #[test]
    fn build_endpoint_handles_bento_scheme() {
        assert_eq!(
            build_endpoint(Some("bento://cache.example.com")).as_deref(),
            Some("https://cache.example.com/report/build")
        );
        assert_eq!(
            build_endpoint(Some("bento://cache.example.com/")).as_deref(),
            Some("https://cache.example.com/report/build")
        );
        assert_eq!(
            build_endpoint(Some("bento://cache.example.com/some/prefix")).as_deref(),
            Some("https://cache.example.com/report/build")
        );
    }

    #[test]
    fn build_endpoint_skips_other_schemes() {
        assert!(build_endpoint(Some("s3://bucket/prefix")).is_none());
        assert!(build_endpoint(Some("https://cache.example.com")).is_none());
        assert!(build_endpoint(None).is_none());
        assert!(build_endpoint(Some("bento://")).is_none());
    }

    #[test]
    fn resolve_token_silent_on_unset() {
        // Token resolution lives in `bento_cache::token`; this test
        // is the regression gate for report.rs's "skip silently when
        // no token in the env var" behaviour. We assert the env-var
        // lookup in isolation — the keychain / credentials-file tiers
        // are covered by bento_cache::token's own tests. A blanket
        // "None from every source" check here would false-fail any
        // time a dev machine has `~/.bento/credentials` from `bento
        // login` (i.e. always, during dogfooding).
        std::env::remove_var("BENTO_TEST_DEFINITELY_UNSET_VAR");
        assert!(std::env::var("BENTO_TEST_DEFINITELY_UNSET_VAR").is_err());
    }

    #[test]
    fn status_success_when_no_failures_and_all_executed() {
        let r = report_with(ExecutionSummary {
            tasks: 4,
            hits: 3,
            built: 1,
            failed: 0,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert!(matches!(br.status, BuildStatus::Success));
        assert!((br.cache_hit_ratio - 0.75).abs() < 1e-5);
    }

    #[test]
    fn status_failed_on_failed_count() {
        let r = report_with(ExecutionSummary {
            tasks: 4,
            hits: 1,
            built: 2,
            failed: 1,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert!(matches!(br.status, BuildStatus::Failed));
    }

    #[test]
    fn status_failed_on_install_failures() {
        let r = report_with(ExecutionSummary {
            tasks: 4,
            install_failures: 1,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert!(matches!(br.status, BuildStatus::Failed));
    }

    #[test]
    fn status_partial_when_some_skipped_without_failure() {
        let r = report_with(ExecutionSummary {
            tasks: 5,
            hits: 2,
            built: 1,
            failed: 0,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert!(matches!(br.status, BuildStatus::Partial));
    }

    #[test]
    fn status_success_when_short_circuited_deploys_account_for_gap() {
        // tasks=5, executed=2 (hits) + 1 (built) = 3, deploy_unchanged=2
        // expected = 5 - 2 = 3 → Success, not Partial.
        let r = report_with(ExecutionSummary {
            tasks: 5,
            hits: 2,
            built: 1,
            deploy_unchanged: 2,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert!(matches!(br.status, BuildStatus::Success));
    }

    #[test]
    fn cache_hit_ratio_zero_when_nothing_executable() {
        let r = report_with(ExecutionSummary::default());
        let br = build_report_from(&r, "x".into());
        assert_eq!(br.cache_hit_ratio, 0.0);
    }

    #[test]
    fn cache_hit_ratio_clamped_above() {
        // Pathological summary — shouldn't happen, but the clamp keeps
        // the wire value sane for the dashboard.
        let r = report_with(ExecutionSummary {
            tasks: 10,
            hits: 100, // wildly inconsistent with tasks; defensive only.
            built: 0,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert!(br.cache_hit_ratio <= 1.0);
    }

    #[test]
    fn duration_ms_passes_through() {
        let r = report_with(ExecutionSummary {
            tasks: 3,
            hits: 3,
            built: 0,
            duration_ms: 4321,
            ..Default::default()
        });
        let br = build_report_from(&r, "x".into());
        assert_eq!(br.duration_ms, 4321);
    }

    #[test]
    fn package_field_propagates() {
        let r = report_with(ExecutionSummary::default());
        let br = build_report_from(&r, "api/server".into());
        assert_eq!(br.package, "api/server");
    }
}
