//! Which tool is driving this bento run — `claude-code`, `codex`,
//! `github-actions`, `human`, …
//!
//! Sent as `bento_cas_protocol::CLIENT_HEADER` on every request the CLI
//! makes to a `bento://` cache server, and carried on the build report,
//! so a dashboard can answer "how much of this repo's build traffic is
//! agents?" without any per-user identity.
//!
//! # Privacy
//!
//! Detection reads environment-variable *names* only — never values.
//! The output is one of the ten strings in
//! [`bento_cas_protocol::CLIENT_KINDS`], so the wire carries a category,
//! not a fingerprint.
//!
//! Lives in `bento-cache` rather than `bento-core` because the bearer
//! remote needs it and `bento-cache` can't depend on `bento-core`.
//! `bento_core::client_id` re-exports this module.

/// `(matcher, kind)`, checked in order — first match wins. A matcher
/// ending in `_` matches any variable with that prefix (agents namespace
/// their env, and the specific names churn); anything else must match a
/// variable name exactly. `CI` in particular must not be a prefix, or
/// `CIRCLE_*` / `CI_COMMIT_*` would swallow more specific kinds below.
const RULES: &[(&str, &str)] = &[
    ("CLAUDECODE", "claude-code"),
    ("CLAUDE_CODE_", "claude-code"),
    ("CODEX_", "codex"),
    ("CURSOR_", "cursor"),
    ("AIDER_", "aider"),
    ("COPILOT_", "copilot"),
    ("GITHUB_ACTIONS", "github-actions"),
    ("CI", "ci"),
];

/// Coarse client kind for this process. Always one of
/// [`bento_cas_protocol::CLIENT_KINDS`]; `"human"` when nothing matches.
pub fn detect() -> &'static str {
    let names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    classify(&names)
}

/// `bento/<version> (<client>)` — the User-Agent every CLI→cloud request
/// carries, so the kind survives even where the header doesn't (proxy
/// logs, self-hosted endpoints that only record UAs).
pub fn user_agent() -> String {
    format!("bento/{} ({})", env!("CARGO_PKG_VERSION"), detect())
}

fn classify<S: AsRef<str>>(names: &[S]) -> &'static str {
    for (matcher, kind) in RULES {
        let hit = names.iter().any(|n| match matcher.strip_suffix('_') {
            Some(_) => n.as_ref().starts_with(matcher),
            None => n.as_ref() == *matcher,
        });
        if hit {
            return kind;
        }
    }
    "human"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_env_is_human() {
        assert_eq!(classify::<&str>(&[]), "human");
        assert_eq!(classify(&["HOME", "PATH", "SHELL"]), "human");
    }

    #[test]
    fn detects_each_kind() {
        for (var, kind) in [
            ("CLAUDECODE", "claude-code"),
            ("CLAUDE_CODE_ENTRYPOINT", "claude-code"),
            ("CODEX_SANDBOX", "codex"),
            ("CODEX_HOME", "codex"),
            ("CURSOR_TRACE_ID", "cursor"),
            ("AIDER_MODEL", "aider"),
            ("COPILOT_AGENT_ID", "copilot"),
            ("GITHUB_ACTIONS", "github-actions"),
            ("CI", "ci"),
        ] {
            assert_eq!(classify(&[var]), kind, "{var}");
        }
    }

    #[test]
    fn agents_win_over_ci() {
        // An agent running inside CI is still an agent — the whole point
        // of the ordering. CI is the fallback for "automation we can't
        // name", not a label that outranks a named tool.
        assert_eq!(classify(&["CI", "GITHUB_ACTIONS"]), "github-actions");
        assert_eq!(
            classify(&["CI", "GITHUB_ACTIONS", "CLAUDECODE"]),
            "claude-code"
        );
        assert_eq!(classify(&["CI", "CODEX_SANDBOX"]), "codex");
        assert_eq!(classify(&["GITHUB_ACTIONS", "CURSOR_TRACE_ID"]), "cursor");
    }

    #[test]
    fn ci_matches_exactly_not_by_prefix() {
        // GitLab / CircleCI set a pile of CI_* and CIRCLE* vars without
        // setting bare CI in every context; matching CI as a prefix
        // would make every one of them outrank a named agent.
        assert_eq!(classify(&["CI_COMMIT_SHA", "CIRCLECI"]), "human");
        assert_eq!(classify(&["CI_COMMIT_SHA", "CLAUDECODE"]), "claude-code");
    }

    #[test]
    fn every_kind_is_in_the_protocol_allowlist() {
        for (_, kind) in RULES {
            assert!(
                bento_cas_protocol::CLIENT_KINDS.contains(kind),
                "{kind} missing from CLIENT_KINDS"
            );
        }
        assert!(bento_cas_protocol::CLIENT_KINDS.contains(&"human"));
    }

    // `detect()` reads process-global env, so this one test (the only
    // one that touches it) holds a lock. Everything above exercises the
    // pure `classify` and needs no serialization.
    #[test]
    fn detect_reads_the_real_environment() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "CURSOR_TRACE_ID";
        let prior = std::env::var_os(var);
        std::env::set_var(var, "x");
        let got = detect();
        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        // A real agent env (this suite often runs under one) can outrank
        // cursor, so assert the weaker invariant that always holds:
        // detect() returns a valid kind and is never "human" here.
        assert!(bento_cas_protocol::CLIENT_KINDS.contains(&got));
        assert_ne!(got, "human");
    }

    #[test]
    fn user_agent_shape() {
        let ua = user_agent();
        assert!(ua.starts_with("bento/"), "{ua}");
        assert!(ua.ends_with(&format!(" ({})", detect())), "{ua}");
    }
}
