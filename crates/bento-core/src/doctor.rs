//! `bento doctor` — structured health checks over the workspace.
//!
//! Every check emits a [`DoctorCheck`] with a stable machine-readable
//! `name`, a [`CheckStatus`], and a human `detail` string. Agents can
//! switch on `name` to act on specific failures; humans see the same
//! information rendered as a table.
//!
//! Checks are deliberately non-destructive: nothing is installed, cached,
//! or mutated. Doctor is a read-only snapshot.

use std::path::{Path, PathBuf};
use std::process::Command;

use schemars::JsonSchema;
use serde::Serialize;

use bento_adapters::{AdapterRegistry, CliRequirement, IntegrationRegistry, LanguageAdapter};
use bento_cache::LocalCache;
use bento_config::{LoadedDish, Workspace};
use bento_toolchain::{ResolutionSource, Resolver, Store};

use crate::plan::{default_cache_root, find_workspace_root};

// ── Report types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub summary: DoctorSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorCheck {
    /// Stable, dot-namespaced machine id (e.g. `config.parse`,
    /// `toolchain.go@1.22.3`, `cache.local`, `cache.remote`,
    /// `cache.gha`, `git.repo`, `git.base_ref`).
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Check succeeded. Agents should ignore these unless surfacing them.
    Ok,
    /// Not an outright failure, but something to look at.
    Warn,
    /// Something is broken. `bento doctor` returns non-zero.
    Fail,
    /// Check did not run (prerequisite missing, not applicable).
    Skipped,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DoctorSummary {
    pub total: usize,
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
}

impl DoctorReport {
    pub fn exit_code(&self) -> i32 {
        if self.summary.fail > 0 {
            1
        } else {
            0
        }
    }
}

// ── Entry points ───────────────────────────────────────────────────

/// Run every check over the workspace rooted at `start`. Bubbles up IO
/// errors encountered while *finding* the workspace, but individual
/// check failures are reported as `Fail` entries rather than errors.
pub fn run(start: &Path) -> anyhow::Result<DoctorReport> {
    run_with_options(
        start,
        &std::collections::BTreeMap::new(),
        DoctorOptions::default(),
    )
}

/// Variant of [`run`] that threads secret aliases into the integration
/// env check. `bento doctor --env staging` uses this to verify the
/// aliased source vars (`RAILWAY_TOKEN_STAGING`) instead of the
/// declared names (`RAILWAY_TOKEN`).
pub fn run_with_aliases(
    start: &Path,
    secret_aliases: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<DoctorReport> {
    run_with_options(start, secret_aliases, DoctorOptions::default())
}

/// Opt-in extensions to the default check set. Each flag should add
/// checks that the default run deliberately skips — usually because
/// they make network calls or require credentials.
#[derive(Debug, Clone, Copy, Default)]
pub struct DoctorOptions {
    /// Add `cloud.*` checks: validate the bento:// remote-cache token,
    /// ping `cache.bento.build/health`, ping `api.bento.build/v1/healthz`.
    /// Off by default since the default doctor is non-network.
    pub cloud: bool,
}

/// Full-fat entry point. The aliases + options arguments let callers
/// extend the default behaviour without growing a new entry point per
/// flag.
pub fn run_with_options(
    start: &Path,
    secret_aliases: &std::collections::BTreeMap<String, String>,
    options: DoctorOptions,
) -> anyhow::Result<DoctorReport> {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    // Locate the workspace. If we can't, that's a terminal Fail.
    let root = match find_workspace_root(start) {
        Ok(r) => r,
        Err(e) => {
            checks.push(check_fail("workspace", e.to_string()));
            return Ok(finalize(checks));
        }
    };

    // 1. Config parses + cross-refs valid.
    let workspace = match Workspace::load(&root) {
        Ok(ws) => {
            checks.push(check_ok(
                "config",
                format!(
                    "{} bento(s), {} dish(es) loaded from {}",
                    ws.bentos.len(),
                    ws.dishes_by_name.len(),
                    root.display()
                ),
            ));
            ws
        }
        Err(e) => {
            checks.push(check_fail("config", e.to_string()));
            return Ok(finalize(checks));
        }
    };

    // 2. Toolchain pins — each explicitly-pinned (tool, version) must be
    //    installed under the store.
    checks.extend(check_toolchains(&workspace));

    // 3. Integrations — env + CLI preflight per detected integration
    //    so `bento deploy` failures surface here first.
    checks.extend(check_integrations(&workspace, secret_aliases));

    // 4. Local cache directory.
    checks.push(check_local_cache());

    // 5. Remote cache (if configured).
    checks.push(check_remote_cache(&workspace));

    // 6. GHA cache (when running inside GitHub Actions).
    checks.push(check_gha_cache());

    // 7. Git repo + base ref.
    checks.push(check_git_repo(&root));
    checks.push(check_git_base_ref(&root));

    // 8. Orphan dish.toml files — dishes on disk that aren't in any
    //    bento's `dishes` list, so bento plan doesn't see them.
    checks.push(check_orphan_dishes(&workspace));

    // 9. Cloud-specific (opt-in) — validate bento:// token + reach
    //    cache.bento.build + api.bento.build endpoints.
    if options.cloud {
        checks.extend(check_cloud(&workspace));
    }

    Ok(finalize(checks))
}

fn check_orphan_dishes(workspace: &Workspace) -> DoctorCheck {
    let orphans = crate::discovery::scan_orphan_dishes(workspace);
    if orphans.is_empty() {
        return check_ok(
            "workspace.orphan_dishes",
            format!(
                "{} wired dish(es); no orphan dish.toml files on disk",
                workspace.dishes_by_name.len()
            ),
        );
    }
    let list = orphans
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    check_warn(
        "workspace.orphan_dishes",
        format!(
            "{} dish.toml file(s) on disk not wired into any bento: {list} — \
             run `bento dish add <path>` to wire them",
            orphans.len()
        ),
    )
}

fn finalize(checks: Vec<DoctorCheck>) -> DoctorReport {
    let mut summary = DoctorSummary {
        total: checks.len(),
        ..Default::default()
    };
    for c in &checks {
        match c.status {
            CheckStatus::Ok => summary.ok += 1,
            CheckStatus::Warn => summary.warn += 1,
            CheckStatus::Fail => summary.fail += 1,
            CheckStatus::Skipped => summary.skipped += 1,
        }
    }
    DoctorReport { checks, summary }
}

// ── Toolchain check ────────────────────────────────────────────────

fn check_toolchains(workspace: &Workspace) -> Vec<DoctorCheck> {
    let registry = AdapterRegistry::builtin();
    let store_root = match Store::default_root() {
        Ok(r) => r,
        Err(e) => {
            return vec![check_warn(
                "toolchain.store",
                format!("could not locate toolchain store: {e}"),
            )];
        }
    };
    let store = Store::new(store_root);

    let mut checks = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();

    for dish in workspace.dishes_by_path.values() {
        let Some(adapter) = resolve_adapter(&registry, dish) else {
            continue;
        };
        let Ok(Some(resolution)) =
            Resolver::resolve(&dish.dir, &dish.config, &workspace.repo, adapter)
        else {
            continue;
        };
        if !matches!(
            resolution.source,
            ResolutionSource::Dish | ResolutionSource::Repo
        ) {
            continue;
        }
        let Some(version) = resolution.version.as_ref() else {
            continue;
        };
        let key = (resolution.tool.clone(), version.clone());
        if !seen.insert(key.clone()) {
            continue;
        }

        let name = format!("toolchain.{}@{}", resolution.tool, version);
        if store.is_installed(&resolution.tool, version) {
            checks.push(check_ok(
                &name,
                format!(
                    "installed at {}",
                    store.bin_dir(&resolution.tool, version).display()
                ),
            ));
        } else {
            checks.push(check_fail(
                &name,
                format!(
                    "pinned by '{}' but not installed — run `bento toolchain install`",
                    resolution.source.label()
                ),
            ));
        }
    }

    if checks.is_empty() {
        checks.push(check_skipped(
            "toolchain",
            "no explicit toolchain pins — nothing to verify",
        ));
    }
    checks
}

fn resolve_adapter<'a>(
    registry: &'a AdapterRegistry,
    dish: &LoadedDish,
) -> Option<&'a dyn LanguageAdapter> {
    if let Some(id) = &dish.config.language {
        return registry.by_id(id);
    }
    registry.detect(&dish.dir)
}

// ── Integration checks ─────────────────────────────────────────────

/// For every built-in integration detected on at least one dish, emit
/// two checks:
///   `integration.<id>.env` — OK when every `required_env` var is set.
///   `integration.<id>.cli` — OK when every `required_cli` binary
///                            resolves via `PATH`.
///
/// Skipped (the whole integration) when it doesn't detect on any dish.
/// This lets `bento doctor` catch "you're about to run `bento deploy`
/// and it's going to fail" before the failure happens.
fn check_integrations(
    workspace: &Workspace,
    secret_aliases: &std::collections::BTreeMap<String, String>,
) -> Vec<DoctorCheck> {
    let registry = IntegrationRegistry::builtin();
    let mut checks = Vec::new();

    // Bucket: integration id → list of dish names that detected it.
    let mut detections: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for dish in workspace.dishes_by_path.values() {
        for integration in registry.detect_all(&dish.dir) {
            detections
                .entry(integration.id().to_string())
                .or_default()
                .push(dish.config.name.clone());
        }
    }

    // Always emit one marker check so agents can distinguish "no
    // integrations detected" from "this integration block didn't run
    // at all" (e.g. an earlier check aborted).
    if detections.is_empty() {
        checks.push(check_skipped(
            "integrations",
            "no deploy / release integrations detected across any dish",
        ));
        return checks;
    }

    for id in registry.ids() {
        let Some(dish_names) = detections.get(&id) else {
            continue;
        };
        let Some(integration) = registry.by_id(&id) else {
            continue;
        };
        let dishes_suffix = format!("(dishes: {})", dish_names.join(", "));

        // Env check.
        let required_env = integration.required_env();
        let env_name = format!("integration.{id}.env");
        if required_env.is_empty() {
            checks.push(check_skipped(
                &env_name,
                format!("no env vars required {dishes_suffix}"),
            ));
        } else {
            // Honour aliases so `doctor --env staging` checks the
            // aliased source vars, not the declared ones.
            let missing: Vec<String> = required_env
                .iter()
                .filter_map(|declared| {
                    let source = secret_aliases
                        .get(declared)
                        .map(|s| s.as_str())
                        .unwrap_or(declared);
                    let ok = std::env::var(source)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                    if ok {
                        None
                    } else if source == declared {
                        Some(declared.clone())
                    } else {
                        Some(format!("{declared} (via ${source})"))
                    }
                })
                .collect();
            if missing.is_empty() {
                checks.push(check_ok(
                    &env_name,
                    format!(
                        "all {} env var(s) present {dishes_suffix}",
                        required_env.len()
                    ),
                ));
            } else {
                checks.push(check_fail(
                    &env_name,
                    format!("missing env var(s): {} {dishes_suffix}", missing.join(", ")),
                ));
            }
        }

        // CLI check.
        let required_cli = integration.required_cli();
        let cli_name = format!("integration.{id}.cli");
        if required_cli.is_empty() {
            checks.push(check_skipped(
                &cli_name,
                format!("no CLI required {dishes_suffix}"),
            ));
        } else {
            let missing: Vec<&CliRequirement> = required_cli
                .iter()
                .filter(|r| !binary_on_path(&r.binary))
                .collect();
            if missing.is_empty() {
                let names: Vec<&str> = required_cli.iter().map(|r| r.binary.as_str()).collect();
                checks.push(check_ok(
                    &cli_name,
                    format!(
                        "all CLI binaries on PATH: {} {dishes_suffix}",
                        names.join(", ")
                    ),
                ));
            } else {
                let hint_lines: Vec<String> = missing
                    .iter()
                    .map(|r| format!("{} (install: {})", r.binary, r.install_hint))
                    .collect();
                checks.push(check_fail(
                    &cli_name,
                    format!("not on PATH: {} {dishes_suffix}", hint_lines.join("; ")),
                ));
            }
        }
    }

    checks
}

fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        if dir.join(name).is_file() {
            return true;
        }
    }
    false
}

// ── Local cache check ──────────────────────────────────────────────

fn check_local_cache() -> DoctorCheck {
    let root = match default_cache_root() {
        Ok(r) => r,
        Err(e) => return check_fail("cache.local", e.to_string()),
    };

    let cache = LocalCache::new(&root);
    if !root.exists() {
        return check_warn(
            "cache.local",
            format!(
                "{} does not exist yet (will be created on first cache write)",
                root.display()
            ),
        );
    }

    if !writable(&root) {
        return check_fail("cache.local", format!("{} is not writable", root.display()));
    }

    match cache.stats() {
        Ok(stats) => check_ok(
            "cache.local",
            format!(
                "{}: {} entr{}, {}",
                root.display(),
                stats.entries,
                if stats.entries == 1 { "y" } else { "ies" },
                format_bytes(stats.total_bytes),
            ),
        ),
        Err(e) => check_warn(
            "cache.local",
            format!("{} exists but stats failed: {e}", root.display()),
        ),
    }
}

fn writable(dir: &Path) -> bool {
    // Probe writability with a transient sidecar file. Safer than
    // trusting metadata permissions on shared filesystems.
    let probe = dir.join(".bento-doctor-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.2} GiB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MiB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.2} KiB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

// ── Remote cache check ─────────────────────────────────────────────

fn check_remote_cache(workspace: &Workspace) -> DoctorCheck {
    let Some(remote) = workspace.repo.cache.remote.as_deref() else {
        return check_skipped("cache.remote", "not configured");
    };

    // `bento://` is a Bearer-auth JWT remote cache (the bento.build
    // hosted service, or any compatible self-hosted endpoint). Deeper
    // checks — JWT shape, token-env presence, edge + control-plane
    // reachability — live behind `bento doctor --cloud` so the default
    // run stays non-network. Here we only confirm the token-env knob is
    // wired, which is a config-time concern.
    if remote.starts_with("bento://") {
        let token_env = workspace.repo.cache.remote_token_env.as_deref();
        return match token_env {
            Some(name) if !name.is_empty() => check_ok(
                "cache.remote",
                format!(
                    "configured: {remote} (bento hosted cache; token via ${name}; \
                     run `bento doctor --cloud` for reachability + JWT validation)"
                ),
            ),
            _ => check_fail(
                "cache.remote",
                format!(
                    "configured: {remote} but [cache] remote_token_env is not set — \
                     name the env var holding the JWT (e.g. remote_token_env = \"BENTO_CACHE_TOKEN\")"
                ),
            ),
        };
    }

    if !remote.starts_with("s3://") {
        return check_fail(
            "cache.remote",
            format!("remote URL must start with s3:// or bento:// (got: {remote})"),
        );
    }

    let has_creds = std::env::var_os("AWS_ACCESS_KEY_ID").is_some();
    if !has_creds {
        return check_fail(
            "cache.remote",
            format!(
                "remote configured ({remote}) but AWS_ACCESS_KEY_ID is not set — \
                 S3-compatible credentials required"
            ),
        );
    }

    let region = workspace
        .repo
        .cache
        .remote_region
        .as_deref()
        .unwrap_or("us-east-1");

    check_ok(
        "cache.remote",
        format!("configured: {remote} (region={region}, reachability not probed)"),
    )
}

// ── GHA cache check ────────────────────────────────────────────────

fn check_gha_cache() -> DoctorCheck {
    // Standard GitHub-provided envs when a workflow is running.
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return check_skipped(
            "cache.gha",
            "not running inside GitHub Actions (GITHUB_ACTIONS unset)",
        );
    }
    let have_url = std::env::var_os("ACTIONS_CACHE_URL").is_some();
    let have_token = std::env::var_os("ACTIONS_RUNTIME_TOKEN").is_some();
    if have_url && have_token {
        check_ok(
            "cache.gha",
            "ACTIONS_CACHE_URL and ACTIONS_RUNTIME_TOKEN present",
        )
    } else {
        // Warn, not Fail: these env vars aren't universally exposed
        // to shell steps inside composite actions — `actions/cache`
        // accesses them via the JS toolkit which gets them through a
        // different channel. Their absence here means bento can't
        // read them directly to hit the GHA cache service, which
        // degrades cache layering (performance regression) but
        // doesn't break correctness — tasks still run, local cache
        // still works. Failing doctor on this was over-strict,
        // especially for `bento deploy` preflight where GHA caching
        // is irrelevant to whether the deploy succeeds.
        let missing = [
            (!have_url).then_some("ACTIONS_CACHE_URL"),
            (!have_token).then_some("ACTIONS_RUNTIME_TOKEN"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        check_warn(
            "cache.gha",
            format!(
                "inside GHA but cache env vars not readable from shell: {}. \
                 Degrades GHA-tier caching; other cache tiers unaffected.",
                missing.join(", ")
            ),
        )
    }
}

// ── Git checks ─────────────────────────────────────────────────────

fn check_git_repo(root: &Path) -> DoctorCheck {
    match git(root, &["rev-parse", "--is-inside-work-tree"]) {
        Some(out) if out.trim() == "true" => check_ok("git.repo", "repository reachable"),
        Some(_) => check_warn("git.repo", "git reports not-inside-worktree"),
        None => check_warn(
            "git.repo",
            "no git repo (diff pre-filter and `--since` will be skipped)",
        ),
    }
}

fn check_git_base_ref(root: &Path) -> DoctorCheck {
    // The default baseline for `bento plan --since` is `origin/main`.
    // If that ref doesn't exist locally we downgrade to Warn rather
    // than Fail — diff-pre-filter just falls back to "everything changed".
    match git(root, &["rev-parse", "--verify", "origin/main"]) {
        Some(sha) if !sha.trim().is_empty() => check_ok(
            "git.base_ref",
            format!("origin/main → {}", &sha.trim()[..sha.trim().len().min(12)]),
        ),
        _ => check_warn(
            "git.base_ref",
            "origin/main not found locally; `bento plan --since=origin/main` will warn",
        ),
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── helpers ────────────────────────────────────────────────────────

fn check_ok(name: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        status: CheckStatus::Ok,
        detail: detail.into(),
    }
}

fn check_warn(name: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        status: CheckStatus::Warn,
        detail: detail.into(),
    }
}

fn check_fail(name: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        status: CheckStatus::Fail,
        detail: detail.into(),
    }
}

fn check_skipped(name: &str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        status: CheckStatus::Skipped,
        detail: detail.into(),
    }
}

// Keep PathBuf usage for clarity in docs/types above.
#[allow(dead_code)]
fn _typecheck_pathbuf(_p: PathBuf) {}

// ── Cloud checks (opt-in via DoctorOptions::cloud) ────────────────
//
// These probe the bento.build hosted cache (or a compatible self-
// hosted endpoint): validate the JWT shape + claims, then ping the
// two public endpoints (CAS edge + control plane) to confirm the user
// can actually reach them. They're behind a flag because the default
// doctor is intentionally non-network — adding cloud probes there
// would mean every CI run pays a few RTTs even when the user has no
// bento:// remote configured.

const CLOUD_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const API_HEALTH_URL: &str = "https://api.bento.build/v1/healthz";

fn check_cloud(workspace: &Workspace) -> Vec<DoctorCheck> {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    let remote = workspace.repo.cache.remote.as_deref();
    let bento_url = match remote {
        Some(u) if u.starts_with("bento://") => u,
        Some(u) => {
            checks.push(check_skipped(
                "cloud.remote",
                format!("[cache] remote is {u}, not bento:// — cloud checks skipped"),
            ));
            return checks;
        }
        None => {
            checks.push(check_skipped(
                "cloud.remote",
                "[cache] remote not configured — cloud checks skipped",
            ));
            return checks;
        }
    };

    // 1. Token env presence + non-empty.
    let token_env = workspace.repo.cache.remote_token_env.as_deref();
    let Some(env_name) = token_env.filter(|s| !s.is_empty()) else {
        checks.push(check_fail(
            "cloud.token.env",
            "[cache] remote_token_env not set — name the env var holding your JWT",
        ));
        return checks;
    };
    let raw_token = match std::env::var(env_name) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            checks.push(check_fail(
                "cloud.token.env",
                format!("${env_name} is unset or empty"),
            ));
            return checks;
        }
    };
    checks.push(check_ok(
        "cloud.token.env",
        format!("${env_name} is set ({} chars)", raw_token.len()),
    ));

    // 2. JWT shape + payload decode.
    match decode_jwt_claims(&raw_token) {
        Ok(claims) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            checks.push(check_ok(
                "cloud.token.claims",
                format!(
                    "iss={iss} team_id={team_id} scope={scope} label={label}",
                    iss = claims.iss,
                    team_id = claims.team_id.as_str(),
                    scope = format!("{:?}", claims.scope).to_lowercase(),
                    label = claims.label.as_str(),
                ),
            ));
            if claims.exp <= now {
                checks.push(check_fail(
                    "cloud.token.expiry",
                    format!(
                        "token expired at unix {exp} (now: {now}) — re-mint at app.bento.build/tokens",
                        exp = claims.exp,
                    ),
                ));
            } else {
                let days = (claims.exp - now) / 86_400;
                checks.push(check_ok(
                    "cloud.token.expiry",
                    format!("expires unix {} (~{} days from now)", claims.exp, days),
                ));
            }
        }
        Err(e) => {
            checks.push(check_fail("cloud.token.claims", e));
            // No point probing endpoints with a broken token, but the
            // health probes don't need auth, so keep going.
        }
    }

    // 3. CAS edge reachability — derive the host from the bento:// URL.
    match cache_health_url(bento_url) {
        Ok(url) => checks.push(probe(&url, "cloud.cache.health")),
        Err(e) => checks.push(check_fail(
            "cloud.cache.health",
            format!("could not derive health URL from {bento_url}: {e}"),
        )),
    }

    // 4. Control-plane reachability. Hardcoded to api.bento.build —
    //    the CLI doesn't currently let users override the CP host
    //    (that lives only on the worker side via the bento:// URL).
    checks.push(probe(API_HEALTH_URL, "cloud.api.health"));

    checks
}

/// Compute `<scheme>://<host>/health` from a `bento://host[/prefix]`
/// URL. Replaces `bento://` with `https://`.
fn cache_health_url(bento_url: &str) -> Result<String, String> {
    let rest = bento_url
        .strip_prefix("bento://")
        .ok_or_else(|| "missing bento:// prefix".to_string())?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err("URL has no host".to_string());
    }
    Ok(format!("https://{host}/health"))
}

/// Decode a JWT's payload segment into [`bento_cas_protocol::Claims`].
/// Does NOT verify the signature — the CLI doesn't hold the worker's
/// public key. The point is to catch shape / claim errors locally so
/// users don't waste a round-trip diagnosing "why does every cache
/// call return 401".
fn decode_jwt_claims(jwt: &str) -> Result<bento_cas_protocol::Claims, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut parts = jwt.split('.');
    let _header = parts.next().ok_or("token has no segments".to_string())?;
    let payload_b64 = parts
        .next()
        .ok_or("token missing payload segment".to_string())?;
    let _sig = parts
        .next()
        .ok_or("token missing signature segment".to_string())?;
    if parts.next().is_some() {
        return Err("token has more than 3 segments".to_string());
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("payload is not valid base64url: {e}"))?;
    serde_json::from_slice::<bento_cas_protocol::Claims>(&payload)
        .map_err(|e| format!("payload JSON does not match Claims: {e}"))
}

/// One-shot HTTP GET with a tight timeout. 200/204 → Ok; non-2xx → Fail
/// (status); transport error → Fail (error text).
fn probe(url: &str, name: &'static str) -> DoctorCheck {
    let agent = ureq::AgentBuilder::new()
        .timeout(CLOUD_PROBE_TIMEOUT)
        .user_agent(concat!("bento-cli/", env!("CARGO_PKG_VERSION"), " doctor"))
        .build();
    match agent.get(url).call() {
        Ok(resp) => {
            let status = resp.status();
            if (200..300).contains(&status) {
                check_ok(name, format!("GET {url} → {status}"))
            } else {
                check_fail(name, format!("GET {url} → {status}"))
            }
        }
        Err(ureq::Error::Status(status, _)) => check_fail(name, format!("GET {url} → {status}")),
        Err(ureq::Error::Transport(t)) => check_fail(name, format!("GET {url} failed: {t}")),
    }
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("bento.toml"), "").unwrap();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["dish"]"#,
        )
        .unwrap();
        std::fs::create_dir(root.join("dish")).unwrap();
        std::fs::write(root.join("dish/dish.toml"), r#"name = "d""#).unwrap();
        tmp
    }

    #[test]
    fn doctor_reports_workspace_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        // No bento.toml / bentos/ anywhere up the tree from /tmp/…
        let report = run(tmp.path()).unwrap();
        let ws_check = report
            .checks
            .iter()
            .find(|c| c.name == "workspace")
            .expect("should have a workspace check");
        assert_eq!(ws_check.status, CheckStatus::Fail);
        assert!(report.exit_code() > 0);
    }

    #[test]
    fn doctor_happy_path_parses_config_and_reports_ok() {
        let tmp = workspace_fixture();
        let report = run(tmp.path()).unwrap();
        let config = report
            .checks
            .iter()
            .find(|c| c.name == "config")
            .expect("config check must be present");
        assert_eq!(config.status, CheckStatus::Ok);
        assert!(config.detail.contains("dish(es)"));
    }

    #[test]
    fn doctor_reports_no_toolchain_pins_as_skipped() {
        let tmp = workspace_fixture();
        let report = run(tmp.path()).unwrap();
        let tc = report
            .checks
            .iter()
            .find(|c| c.name == "toolchain")
            .expect("toolchain skipped check should be present");
        assert_eq!(tc.status, CheckStatus::Skipped);
    }

    #[test]
    fn doctor_reports_missing_toolchain_as_fail() {
        let tmp = workspace_fixture();
        // Add a Go toolchain pin that we (almost certainly) don't have
        // installed, and set the dish's language so the resolver kicks in.
        std::fs::write(
            tmp.path().join("bento.toml"),
            r#"[toolchain]
go = "0.0.0-fake"
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("dish/dish.toml"),
            r#"name = "d"
language = "go"
"#,
        )
        .unwrap();
        let report = run(tmp.path()).unwrap();
        let tc = report
            .checks
            .iter()
            .find(|c| c.name == "toolchain.go@0.0.0-fake")
            .expect("fake-version check should exist");
        assert_eq!(tc.status, CheckStatus::Fail);
        assert!(tc.detail.contains("not installed"));
    }

    #[test]
    fn doctor_skips_integrations_when_none_detected() {
        let tmp = workspace_fixture();
        let report = run(tmp.path()).unwrap();
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "integrations")
            .expect("integrations marker check must be present");
        assert_eq!(check.status, CheckStatus::Skipped);
    }

    #[test]
    fn doctor_flags_missing_vercel_token_when_detected() {
        let tmp = workspace_fixture();
        // Drop a vercel.json into the dish so the integration detects.
        std::fs::write(tmp.path().join("dish/vercel.json"), "{}").unwrap();

        // Make sure VERCEL_TOKEN is not set in this test's env — note
        // that Rust test processes share env so this could collide; use
        // a sentinel name in production checks. For this test, the env
        // var almost certainly isn't exported in the dev shell.
        std::env::remove_var("VERCEL_TOKEN");

        let report = run(tmp.path()).unwrap();
        let env = report
            .checks
            .iter()
            .find(|c| c.name == "integration.vercel.env")
            .expect("vercel env check should fire when detected");
        assert_eq!(env.status, CheckStatus::Fail);
        assert!(env.detail.contains("VERCEL_TOKEN"));
        // CLI check runs too, regardless of env outcome.
        let cli = report
            .checks
            .iter()
            .find(|c| c.name == "integration.vercel.cli")
            .expect("vercel cli check should fire when detected");
        // The check is Ok iff `vercel` happens to be on PATH; otherwise
        // Fail. Both are valid per this test's purpose — just verify
        // it's either Ok or Fail (not Skipped).
        assert!(matches!(cli.status, CheckStatus::Ok | CheckStatus::Fail));
    }

    #[test]
    fn doctor_skips_remote_cache_when_unset() {
        let tmp = workspace_fixture();
        let report = run(tmp.path()).unwrap();
        let rc = report
            .checks
            .iter()
            .find(|c| c.name == "cache.remote")
            .expect("cache.remote must always appear");
        assert_eq!(rc.status, CheckStatus::Skipped);
    }

    #[test]
    fn doctor_gha_cache_states() {
        // Both GHA branches — "not in GHA" (skipped) and "in GHA
        // without cache env vars" (warn) — exercised in one test so
        // we don't race with sibling tests over the process-global
        // `GITHUB_ACTIONS` env var. Tests in Rust run in parallel
        // threads; `std::env` is process-scoped, so splitting
        // across tests would flake on whichever scheduler order
        // happens that day.
        let tmp = workspace_fixture();

        // Case 1: not inside GHA → Skipped.
        std::env::remove_var("GITHUB_ACTIONS");
        std::env::remove_var("ACTIONS_CACHE_URL");
        std::env::remove_var("ACTIONS_RUNTIME_TOKEN");
        let report = run(tmp.path()).unwrap();
        let ghacache = report
            .checks
            .iter()
            .find(|c| c.name == "cache.gha")
            .expect("cache.gha always appears");
        assert_eq!(ghacache.status, CheckStatus::Skipped);

        // Case 2: inside GHA, cache env vars absent → Warn (not
        // Fail). Must keep exit_code == 0 because degraded caching
        // isn't a correctness blocker — tasks still run, local +
        // remote cache tiers still work.
        std::env::set_var("GITHUB_ACTIONS", "true");
        let report = run(tmp.path()).unwrap();
        std::env::remove_var("GITHUB_ACTIONS"); // don't leak
        let ghacache = report
            .checks
            .iter()
            .find(|c| c.name == "cache.gha")
            .expect("cache.gha always appears");
        assert_eq!(ghacache.status, CheckStatus::Warn);
        assert_eq!(report.exit_code(), 0, "warns must not fail doctor");
    }

    #[test]
    fn summary_counts_match_check_vector() {
        let tmp = workspace_fixture();
        let report = run(tmp.path()).unwrap();
        let s = &report.summary;
        assert_eq!(
            s.total,
            s.ok + s.warn + s.fail + s.skipped,
            "summary must total to checks.len()"
        );
        assert_eq!(s.total, report.checks.len());
    }

    #[test]
    fn format_bytes_scales_by_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MiB");
        assert!(format_bytes(3 * 1024 * 1024 * 1024).starts_with("3.00 GiB"));
    }

    #[test]
    fn cache_health_url_derives_https_from_bento_scheme() {
        assert_eq!(
            cache_health_url("bento://cache.bento.build").unwrap(),
            "https://cache.bento.build/health"
        );
        assert_eq!(
            cache_health_url("bento://cache.bento.build/some/prefix").unwrap(),
            "https://cache.bento.build/health"
        );
        assert!(cache_health_url("https://example.com").is_err());
        assert!(cache_health_url("bento://").is_err());
    }

    #[test]
    fn jwt_decode_rejects_malformed_tokens() {
        assert!(decode_jwt_claims("").is_err());
        assert!(decode_jwt_claims("only-one-segment").is_err());
        assert!(decode_jwt_claims("a.b").is_err());
        assert!(decode_jwt_claims("a.b.c.d").is_err());
        // Valid header.payload.sig structure but payload isn't valid
        // base64url, then payload isn't valid Claims JSON.
        assert!(decode_jwt_claims("header.!!notb64!!.sig").is_err());
        assert!(decode_jwt_claims("header.eyJmb28iOiJiYXIifQ.sig").is_err());
    }

    #[test]
    fn jwt_decode_extracts_claims() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let claims_json = serde_json::json!({
            "iss": "bento.build",
            "team_id": "00000000-0000-0000-0000-000000000001",
            "scope": "read_write",
            "label": "ci-prod",
            "iat": 1_700_000_000_u64,
            "exp": 1_700_000_000_u64 + 30 * 86_400,
        });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims_json).unwrap());
        let jwt = format!("HEADER.{payload}.SIG");
        let claims = decode_jwt_claims(&jwt).expect("valid claims should decode");
        assert_eq!(claims.iss, "bento.build");
        assert_eq!(claims.label.as_str(), "ci-prod");
    }
}
