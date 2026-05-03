//! Railway integration.
//!
//! - Detects: `railway.toml`, `railway.json`, or a `.railway/`
//!   directory at the dish root.
//! - Required env: `RAILWAY_TOKEN` (project-scoped token; user-
//!   scoped tokens work too). Service selection is usually handled
//!   via `railway link` beforehand — if `$RAILWAY_SERVICE` is set
//!   it's passed along, but not required.
//! - Emits: `railway:deploy`. Railway supports preview/PR
//!   deployments via PR environments, not a separate CLI subcommand,
//!   so there's no `railway:preview` — it's a config concern.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::cloudflare_worker::{capture_cli_stdout, run_cli_with_stdin};
use crate::integration::{CliRequirement, Integration, IntegrationTask, IntegrationTaskKind};

pub struct RailwayIntegration;

impl Integration for RailwayIntegration {
    fn id(&self) -> &str {
        "railway"
    }

    fn display_name(&self) -> &str {
        "Railway"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("railway.toml").is_file()
            || dir.join("railway.json").is_file()
            || dir.join(".railway").is_dir()
    }

    fn required_env(&self) -> Vec<String> {
        // Intentionally empty. Railway CLI authenticates two ways:
        //   1. `RAILWAY_TOKEN` env var (CI / project tokens).
        //   2. OAuth session in `~/.railway/config.json` after
        //      `railway login` (local dev).
        // We can't tell from inside the preflight check which mode
        // the user is in, so we don't fail-fast on a missing env var;
        // `railway up`'s own error is clear enough when neither auth
        // mode is available. `env_vars` below still forwards
        // RAILWAY_TOKEN through to the task when it IS set.
        Vec::new()
    }

    fn required_cli(&self) -> Vec<CliRequirement> {
        vec![CliRequirement::new(
            "railway",
            "npm install -g @railway/cli  (or brew install railway)",
        )]
    }

    fn supports_secrets(&self) -> bool {
        true
    }

    fn put_secret(&self, cwd: &Path, config: &toml::Table, name: &str, value: &str) -> Result<()> {
        // Railway's CLI doesn't have a true "secrets" concept — every
        // env var lives in `railway variables`. We treat `put` as an
        // upsert; `--set "NAME=value"` overwrites existing entries.
        let mut cmd = Command::new("railway");
        cmd.current_dir(cwd)
            .arg("variables")
            .arg("--set")
            .arg(format!("{name}={value}"));
        for s in collect_services(config) {
            cmd.arg("--service").arg(s);
        }
        // `--skip-deploys` keeps put from triggering a redeploy on
        // every secret change — callers can run `bento deploy` when
        // they actually want new values live.
        cmd.arg("--skip-deploys");
        run_cli_with_stdin(&mut cmd, b"", "railway variables --set")
    }

    fn list_secrets(&self, cwd: &Path, config: &toml::Table) -> Result<Vec<String>> {
        // `railway variables --kv` prints `NAME=VALUE` pairs, one per
        // line. We parse only the LHS so values never enter bento's
        // surface. `--json` is cleaner but relies on a wrapped stdout
        // redirect the CLI refuses outside --ci; --kv is stable across
        // modes.
        let mut cmd = Command::new("railway");
        cmd.current_dir(cwd).arg("variables").arg("--kv");
        for s in collect_services(config) {
            cmd.arg("--service").arg(s);
        }
        let stdout = capture_cli_stdout(&mut cmd, "railway variables --kv")?;
        parse_railway_kv(&stdout)
    }

    fn delete_secret(&self, cwd: &Path, config: &toml::Table, name: &str) -> Result<()> {
        let mut cmd = Command::new("railway");
        cmd.current_dir(cwd)
            .arg("variables")
            .arg("--remove")
            .arg(name);
        for s in collect_services(config) {
            cmd.arg("--service").arg(s);
        }
        cmd.arg("--skip-deploys");
        run_cli_with_stdin(&mut cmd, b"", "railway variables --remove")
    }

    fn detected_tasks(&self, _dir: &Path, config: &toml::Table) -> Vec<IntegrationTask> {
        // `railway up` is idempotent and handles both initial deploy
        // and subsequent updates. We invoke it with `--ci` so the CLI
        // streams build logs and exits non-zero on terminal failure
        // (SUCCESS / FAILED / CRASHED). The default `railway up`
        // relies on TTY detection to decide whether to attach to the
        // log stream, and bento runs tasks via `sh -c` with piped
        // stdio — no TTY — so the default silently falls through to
        // detach-like behaviour and reports upload as "built" even
        // when Railway's server-side build later fails. `--detach` is
        // the other obvious wrong answer for the same reason.
        //
        // Service routing: `dish.toml`'s `[integrations.railway]`
        // supplies the Railway service name via either:
        //   service = "Admin"                       (one deploy task)
        //   services = ["Frontend", "Landing Page"] (N deploy tasks,
        //                                            one per entry)
        // Both honour project-scoped tokens — `--service <name>` is
        // injected per task without needing `railway link` first.
        //
        // Task naming with multiple services:
        //   singular / absent → `railway:deploy`
        //   plural            → `railway:deploy:<slug>` per service
        // The Deploy kind on every task means `bento deploy` still
        // filters correctly, and unique names let the cache / retry
        // machinery track each deploy independently.
        //
        // Upload root: Railway services configured with
        // `rootDirectory` (common in monorepos) expect the full repo
        // uploaded so Railway can resolve their scoped path. Set
        // `root = ".."` (or deeper) in `[integrations.railway]` to
        // `cd` there before `railway up` — the CLI's own positional
        // PATH argument fails with "prefix not found" on
        // sibling/parent paths.
        //
        // Quoting: service names and the cd target both get
        // shell-quoted defensively — harmless for plain names,
        // necessary for names/paths with spaces.
        let services = collect_services(config);
        let root = config.get("root").and_then(|v| v.as_str());

        let mut tasks = Vec::new();
        if services.is_empty() {
            // No service declared — emit one bare task. Railway CLI
            // picks the linked service (or errors) at runtime. `multi`
            // is trivially false here (no services to disambiguate
            // between) so the task keeps the bare `railway:deploy`
            // name.
            tasks.push(build_task(None, root, false));
        } else {
            let multiple = services.len() > 1;
            for service in &services {
                tasks.push(build_task(Some(service), root, multiple));
            }
        }
        tasks
    }
}

/// Read `services` (array) or `service` (scalar) from the Railway
/// config block. Returns `Vec<String>` — empty when neither is set.
/// `services` takes precedence when both appear; the single-scalar
/// form is sugar for `services = [value]`.
fn collect_services(config: &toml::Table) -> Vec<String> {
    if let Some(arr) = config.get("services").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    if let Some(s) = config.get("service").and_then(|v| v.as_str()) {
        return vec![s.to_string()];
    }
    Vec::new()
}

/// Build the Railway deploy task. `multi = true` switches the task
/// name to `railway:deploy:<slug>` so multiple services on one dish
/// don't collide on the single `railway:deploy` key.
///
/// Uses **`railway up --ci`** — explicit CI mode. The CLI streams
/// build logs then exits on terminal status (SUCCESS / FAILED /
/// CRASHED), so the task's exit code reflects the real Railway-side
/// deploy outcome. Plain `railway up` decides whether to block based
/// on TTY detection, and bento invokes tasks via `sh -c` with piped
/// stdio — the no-TTY path collapses to detach-like behaviour and
/// reports upload as "built" even when the server-side build later
/// crashes. `--ci` sidesteps that entirely.
fn build_task(service: Option<&str>, root: Option<&str>, multi: bool) -> IntegrationTask {
    let mut railway_cmd = String::from("railway up --ci");
    if let Some(s) = service {
        railway_cmd.push_str(&format!(" --service {}", shell_quote(s)));
    }
    let run = match root {
        Some(r) => format!("cd {} && {}", shell_quote(r), railway_cmd),
        None => railway_cmd,
    };
    let name = match service {
        Some(s) if multi => format!("railway:deploy:{}", slugify(s)),
        _ => "railway:deploy".to_string(),
    };
    IntegrationTask {
        name,
        kind: IntegrationTaskKind::Deploy,
        run,
        depends_on: vec!["build".into()],
        env_vars: vec!["RAILWAY_TOKEN".into(), "RAILWAY_SERVICE".into()],
        no_cache: true,
        outputs: Vec::new(),
    }
}

/// Lowercase ASCII + collapse anything non-alphanumeric to `-`.
/// "Landing Page" → "landing-page"; "API v2" → "api-v2". Used for
/// task-name suffixes on multi-service deploys so the names stay
/// readable in reports and on the CLI.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = true; // suppress leading dashes
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Single-quote a shell argument that may contain spaces or other
/// metachars. Replaces embedded single quotes with `'\''` — the
/// standard Bourne-shell escape. Sufficient for service names which
/// in practice are ASCII-ish label strings.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        // Plain — no quoting needed; keeps the common case readable.
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Parse `railway variables --kv` stdout: lines of `NAME=VALUE`. We
/// keep NAME only; values never enter bento's surface (the whole point
/// of list-returns-names-only). A `=` in the value is fine — we split
/// on the first `=` per line.
fn parse_railway_kv(stdout: &[u8]) -> Result<Vec<String>> {
    let s = std::str::from_utf8(stdout)
        .map_err(|e| anyhow::anyhow!("railway variables not valid UTF-8: {e}"))?;
    let mut out = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq) = trimmed.find('=') {
            out.push(trimmed[..eq].to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with_dir(name: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(name)).unwrap();
        tmp
    }

    fn tmp_with_file(name: &str, content: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(name), content).unwrap();
        tmp
    }

    #[test]
    fn id_and_required_env() {
        assert_eq!(RailwayIntegration.id(), "railway");
        assert_eq!(RailwayIntegration.display_name(), "Railway");
        // Empty: local-dev OAuth-session auth is supported alongside
        // env-var tokens. See comment on required_env().
        assert!(RailwayIntegration.required_env().is_empty());
    }

    #[test]
    fn detect_matches_railway_toml() {
        let tmp = tmp_with_file("railway.toml", "");
        assert!(RailwayIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_matches_railway_json() {
        let tmp = tmp_with_file("railway.json", "{}");
        assert!(RailwayIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_matches_dot_railway_dir() {
        let tmp = tmp_with_dir(".railway");
        assert!(RailwayIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_false_for_unrelated_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert!(!RailwayIntegration.detect(tmp.path()));
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
    fn emits_deploy_task_without_service_when_config_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let config = toml::Table::new();
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "railway:deploy");
        assert_eq!(tasks[0].kind, IntegrationTaskKind::Deploy);
        assert!(tasks[0].no_cache);
        assert_eq!(tasks[0].run, "railway up --ci");
    }

    #[test]
    fn emits_service_flag_when_config_names_one() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[("service", s("Backend"))]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks[0].run, "railway up --ci --service Backend");
    }

    #[test]
    fn quotes_service_names_with_spaces() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[("service", s("Landing Page"))]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks[0].run, "railway up --ci --service 'Landing Page'");
    }

    #[test]
    fn root_config_prepends_cd_to_railway_up() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[("service", s("Admin")), ("root", s(".."))]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks[0].run, "cd .. && railway up --ci --service Admin");
    }

    #[test]
    fn root_config_quotes_paths_with_spaces() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[("root", s("../my project"))]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks[0].run, "cd '../my project' && railway up --ci");
    }

    #[test]
    fn services_array_fans_out_to_one_task_per_service() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[
            (
                "services",
                toml::Value::Array(vec![s("Frontend"), s("Landing Page")]),
            ),
            ("root", s("..")),
        ]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "railway:deploy:frontend");
        assert_eq!(tasks[1].name, "railway:deploy:landing-page");
        assert_eq!(tasks[0].run, "cd .. && railway up --ci --service Frontend");
        assert_eq!(
            tasks[1].run,
            "cd .. && railway up --ci --service 'Landing Page'"
        );
        // Both are Deploy-kind so `bento deploy` catches both.
        assert_eq!(tasks[0].kind, IntegrationTaskKind::Deploy);
        assert_eq!(tasks[1].kind, IntegrationTaskKind::Deploy);
    }

    #[test]
    fn single_service_via_services_array_keeps_unsuffixed_task_name() {
        // A one-element `services` array is equivalent to `service =
        // "..."` — the task name should stay bare so the normal
        // single-service flow works identically either way.
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[("services", toml::Value::Array(vec![s("Backend")]))]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "railway:deploy");
    }

    #[test]
    fn services_takes_precedence_over_singular_service() {
        // Tolerant but deterministic: if a dish.toml somehow has both
        // fields, plural wins (a list of one matches the singular
        // anyway). Avoids silent surprises.
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(&[
            ("service", s("Ignored")),
            ("services", toml::Value::Array(vec![s("Actual")])),
        ]);
        let tasks = RailwayIntegration.detected_tasks(tmp.path(), &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].run.contains("--service Actual"));
        assert!(!tasks[0].run.contains("Ignored"));
    }

    #[test]
    fn shell_quote_preserves_plain_strings() {
        assert_eq!(shell_quote("Backend"), "Backend");
        assert_eq!(shell_quote("frontend-prod"), "frontend-prod");
        assert_eq!(shell_quote("api_v2"), "api_v2");
        assert_eq!(shell_quote("release.1"), "release.1");
    }

    #[test]
    fn shell_quote_wraps_names_with_spaces() {
        assert_eq!(shell_quote("Landing Page"), "'Landing Page'");
    }

    #[test]
    fn shell_quote_escapes_embedded_quotes() {
        assert_eq!(shell_quote("it's fine"), r"'it'\''s fine'");
    }

    #[test]
    fn slugify_lowercases_and_dasherises() {
        assert_eq!(slugify("Landing Page"), "landing-page");
        assert_eq!(slugify("API v2"), "api-v2");
        assert_eq!(slugify("  Admin  "), "admin");
        assert_eq!(slugify("!!!weird!!!"), "weird");
    }
}
