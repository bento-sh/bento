//! The [`Integration`] trait and its supporting value types.
//!
//! Integrations are the second extension point, sibling to
//! [`crate::LanguageAdapter`]. Where an adapter classifies a dish's
//! language family (one per dish), an integration *augments* a dish
//! with additional tasks — e.g. `vercel:deploy`, `railway:deploy`,
//! `sentry:release`. A dish can have zero or one adapter and
//! zero-or-more integrations active simultaneously.
//!
//! Design rationale: deploy targets and tools like Sentry/Docker Hub
//! aren't languages — shoehorning them into `LanguageAdapter` would
//! force composition-at-runtime or an `adapter × deploy-target`
//! combinatorial explosion. Keeping them as a separate, strictly
//! additive trait keeps both concerns clean.

use std::path::Path;

/// Role of an integration-emitted task. Drives CLI filtering
/// (`bento deploy` selects `Deploy`) and defaults `no_cache = true`
/// for side-effectful kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegrationTaskKind {
    /// Production deploy. `bento deploy` selects these.
    Deploy,
    /// Preview / staging deploy (PR previews, branch environments).
    DeployPreview,
    /// Rollback to a prior version.
    Rollback,
    /// Notification to an external system (Slack, PagerDuty).
    Notify,
    /// Release-related but not itself a deploy (Sentry release, sourcemap upload).
    Release,
    /// Anything else.
    Other,
}

impl IntegrationTaskKind {
    /// Stable wire id — used in JSON output and filter arguments.
    pub fn as_str(&self) -> &'static str {
        match self {
            IntegrationTaskKind::Deploy => "deploy",
            IntegrationTaskKind::DeployPreview => "deploy_preview",
            IntegrationTaskKind::Rollback => "rollback",
            IntegrationTaskKind::Notify => "notify",
            IntegrationTaskKind::Release => "release",
            IntegrationTaskKind::Other => "other",
        }
    }

    /// Side-effectful kinds default to `no_cache = true` — we never
    /// want to "cache hit" a prod deploy.
    pub fn defaults_no_cache(&self) -> bool {
        matches!(
            self,
            IntegrationTaskKind::Deploy
                | IntegrationTaskKind::DeployPreview
                | IntegrationTaskKind::Rollback
                | IntegrationTaskKind::Notify
        )
    }
}

/// A CLI binary an integration requires at task-run time, plus a
/// human-readable hint for getting it installed. Surfaced by the
/// executor's preflight check before any integration task runs so
/// the failure is "vercel CLI not found on PATH — install via
/// `npm i -g vercel`" rather than a cryptic shell exit 127.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliRequirement {
    /// Program name to look up via `PATH` (e.g. `"vercel"`, `"railway"`).
    pub binary: String,
    /// Install hint shown on failure (e.g. `"npm install -g vercel"` or
    /// a docs URL). Displayed verbatim.
    pub install_hint: String,
}

impl CliRequirement {
    pub fn new(binary: impl Into<String>, install_hint: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            install_hint: install_hint.into(),
        }
    }
}

/// A task contributed by an [`Integration`]. Flows through bento's
/// normal executor — gets a cache key, obeys retry, surfaces
/// diagnostics on failure — unless `no_cache` is set, in which case
/// cache lookup and write are both bypassed.
#[derive(Debug, Clone)]
pub struct IntegrationTask {
    /// Task name. Convention: `<integration>:<verb>` — e.g.
    /// `vercel:deploy`, `sentry:release`. Prefix disambiguates when
    /// multiple integrations contribute verbs of the same shape.
    pub name: String,
    /// Role of this task. Drives `bento deploy` filtering.
    pub kind: IntegrationTaskKind,
    /// Shell command to execute.
    pub run: String,
    /// Other task names in this dish this one logically depends on
    /// (e.g. `["build"]`). Advisory in v1 — task execution order is
    /// list position; integration tasks append after adapter defaults.
    pub depends_on: Vec<String>,
    /// Env-var names to pass through to the task's child process.
    /// Same semantics as `[tasks.<name>] env = [...]` in dish.toml.
    pub env_vars: Vec<String>,
    /// Skip cache entirely — lookup and put. Forced `true` for
    /// side-effectful kinds ([`IntegrationTaskKind::defaults_no_cache`]).
    pub no_cache: bool,
    /// Optional output globs (rare for deploy tasks; deploy log
    /// capture is a potential future use).
    pub outputs: Vec<String>,
}

/// An integration teaches bento about a dev-tool or deploy platform
/// (Vercel, Railway, Sentry, Docker Hub, …) that *augments* a dish
/// with additional tasks — distinct from [`crate::LanguageAdapter`],
/// which classifies the dish's language family.
///
/// A dish can be claimed by zero or one adapter and zero-or-more
/// integrations simultaneously.
///
/// Implementations must be cheap to construct and detect — the
/// registry holds them behind `Box<dyn Integration>` and calls them
/// during workspace discovery.
pub trait Integration: Send + Sync {
    /// Stable identifier (`"vercel"`, `"railway"`, `"sentry"`).
    fn id(&self) -> &str;

    /// Human-readable name. Defaults to `id`.
    fn display_name(&self) -> &str {
        self.id()
    }

    /// Does this integration apply to `dir`? Cheap file-existence
    /// checks — called once per dish during workspace discovery.
    fn detect(&self, dir: &Path) -> bool;

    /// Environment variables this integration needs at task-run time.
    /// Surfaced by `bento doctor` and checked by the executor before
    /// running any integration-owned task — missing env vars fail
    /// fast with a clear error instead of a cryptic 401 from the
    /// underlying CLI.
    fn required_env(&self) -> Vec<String> {
        Vec::new()
    }

    /// CLI binaries this integration needs on `PATH` at task-run
    /// time, paired with install hints. Checked before the task runs
    /// so the failure mode is a clear "vercel CLI not found on PATH
    /// — `npm install -g vercel`" instead of a shell exit-127 the
    /// caller has to decode.
    fn required_cli(&self) -> Vec<CliRequirement> {
        Vec::new()
    }

    /// Tasks this integration contributes to the dish. `config` is
    /// the `[integrations.<id>]` block from the dish's `dish.toml`
    /// (empty table if not declared) — integrations pull fields
    /// they recognise and ignore the rest. Value types follow the
    /// block's TOML shape: strings, arrays, nested tables.
    ///
    /// For example: the Railway integration reads `service`
    /// (string) to inject `--service <name>` into `railway up`,
    /// or `services` (array of strings) to fan out to one deploy
    /// task per service. The Vercel integration might read
    /// `team` / `project` for `--scope` tuning.
    ///
    /// Empty `Vec` is a valid return (e.g. an integration that only
    /// emits tasks conditional on project contents).
    fn detected_tasks(&self, dir: &Path, config: &toml::Table) -> Vec<IntegrationTask>;

    // ── Secret management ──────────────────────────────────────────
    //
    // Deploy integrations (Cloudflare, Railway, Vercel) wrap platform
    // CLIs that manage per-target secrets. These methods let
    // `bento secret put|list|delete` dispatch uniformly without the
    // agent needing to know which CLI fronts which platform. Default
    // impls return a "not supported" error for integrations that have
    // no secret model (Slack webhooks, Linear API — auth comes from
    // env vars the user sets directly).

    /// Advertise whether this integration implements the `*_secret`
    /// trio. CLI uses this to pick the single secret-capable
    /// integration on a dish, or to surface a helpful
    /// "which-integration" error when multiple coexist.
    fn supports_secrets(&self) -> bool {
        false
    }

    /// Set or update a secret named `name` to `value` on the platform.
    /// `cwd` is the dish directory — many platform CLIs (wrangler,
    /// railway) infer their target from the current working dir's
    /// config file, so callers MUST change the process's working
    /// directory to `cwd` before invoking the underlying CLI.
    ///
    /// Value lifetime: bento reads it from stdin, hands it here once,
    /// and drops it. Never logged, never persisted, never returned
    /// through any other API.
    fn put_secret(
        &self,
        _cwd: &Path,
        _config: &toml::Table,
        _name: &str,
        _value: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!(
            "integration '{}' does not support secret management",
            self.id()
        )
    }

    /// List secret NAMES (never values — platforms refuse to return
    /// values once set, by design). Returned vector is ordered as the
    /// platform returns it; CLI re-sorts for stable output.
    fn list_secrets(&self, _cwd: &Path, _config: &toml::Table) -> anyhow::Result<Vec<String>> {
        anyhow::bail!(
            "integration '{}' does not support secret management",
            self.id()
        )
    }

    /// Delete a secret by name. Idempotent at the bento level — a
    /// delete for a non-existent name should succeed (or fail with a
    /// clear "not found" that the CLI can surface as a warning). The
    /// underlying platform CLI decides the exact semantics.
    fn delete_secret(&self, _cwd: &Path, _config: &toml::Table, _name: &str) -> anyhow::Result<()> {
        anyhow::bail!(
            "integration '{}' does not support secret management",
            self.id()
        )
    }
}

/// Lookup for built-in and (later) plugin-supplied integrations.
/// Mirrors [`crate::AdapterRegistry`] but is *additive* — multiple
/// integrations per dish are expected, not first-match-wins.
pub struct IntegrationRegistry {
    integrations: Vec<Box<dyn Integration>>,
}

impl IntegrationRegistry {
    /// Empty registry — tests and specialised configurations.
    pub fn empty() -> Self {
        Self {
            integrations: Vec::new(),
        }
    }

    /// Registry populated with every built-in integration.
    pub fn builtin() -> Self {
        use crate::cloudflare_pages::CloudflarePagesIntegration;
        use crate::cloudflare_worker::CloudflareWorkerIntegration;
        use crate::linear::LinearIntegration;
        use crate::railway::RailwayIntegration;
        use crate::slack::SlackIntegration;
        use crate::vercel::VercelIntegration;
        Self {
            integrations: vec![
                Box::new(VercelIntegration),
                Box::new(RailwayIntegration),
                Box::new(CloudflareWorkerIntegration),
                Box::new(CloudflarePagesIntegration),
                Box::new(SlackIntegration),
                Box::new(LinearIntegration),
            ],
        }
    }

    /// Register an additional integration (plugin entry point).
    pub fn register(&mut self, integration: Box<dyn Integration>) {
        self.integrations.push(integration);
    }

    /// Builder-style variant of [`Self::register`].
    pub fn with_plugins(mut self, plugins: impl IntoIterator<Item = Box<dyn Integration>>) -> Self {
        for p in plugins {
            self.integrations.push(p);
        }
        self
    }

    /// All integrations whose `detect()` fires in `dir`. Unlike
    /// adapters (first-match), integrations are additive — Vercel
    /// + Sentry + Docker push can all activate on the same dish.
    pub fn detect_all(&self, dir: &Path) -> Vec<&dyn Integration> {
        self.integrations
            .iter()
            .filter(|i| i.detect(dir))
            .map(AsRef::as_ref)
            .collect()
    }

    /// Look up by stable id.
    pub fn by_id(&self, id: &str) -> Option<&dyn Integration> {
        self.integrations
            .iter()
            .find(|i| i.id() == id)
            .map(AsRef::as_ref)
    }

    /// Ids of all registered integrations, in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.integrations
            .iter()
            .map(|i| i.id().to_string())
            .collect()
    }
}

impl Default for IntegrationRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopIntegration;

    impl Integration for NoopIntegration {
        fn id(&self) -> &str {
            "noop-test"
        }
        fn detect(&self, _: &Path) -> bool {
            true
        }
        fn detected_tasks(&self, _: &Path, _: &toml::Table) -> Vec<IntegrationTask> {
            Vec::new()
        }
    }

    #[test]
    fn kind_as_str_is_stable() {
        assert_eq!(IntegrationTaskKind::Deploy.as_str(), "deploy");
        assert_eq!(
            IntegrationTaskKind::DeployPreview.as_str(),
            "deploy_preview"
        );
        assert_eq!(IntegrationTaskKind::Rollback.as_str(), "rollback");
        assert_eq!(IntegrationTaskKind::Notify.as_str(), "notify");
        assert_eq!(IntegrationTaskKind::Release.as_str(), "release");
        assert_eq!(IntegrationTaskKind::Other.as_str(), "other");
    }

    #[test]
    fn deploy_kinds_default_to_no_cache() {
        assert!(IntegrationTaskKind::Deploy.defaults_no_cache());
        assert!(IntegrationTaskKind::DeployPreview.defaults_no_cache());
        assert!(IntegrationTaskKind::Rollback.defaults_no_cache());
        assert!(IntegrationTaskKind::Notify.defaults_no_cache());
        // Release + Other are cacheable by default.
        assert!(!IntegrationTaskKind::Release.defaults_no_cache());
        assert!(!IntegrationTaskKind::Other.defaults_no_cache());
    }

    #[test]
    fn empty_registry_has_no_detections() {
        let reg = IntegrationRegistry::empty();
        let tmp = tempfile::tempdir().unwrap();
        assert!(reg.detect_all(tmp.path()).is_empty());
        assert!(reg.by_id("anything").is_none());
    }

    #[test]
    fn register_adds_custom_integration() {
        let mut reg = IntegrationRegistry::empty();
        reg.register(Box::new(NoopIntegration));
        assert_eq!(reg.ids(), vec!["noop-test".to_string()]);
        assert!(reg.by_id("noop-test").is_some());
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(reg.detect_all(tmp.path()).len(), 1);
    }

    #[test]
    fn detect_all_is_additive() {
        let reg = IntegrationRegistry::empty()
            .with_plugins([Box::new(NoopIntegration) as Box<dyn Integration>])
            .with_plugins([Box::new(NoopIntegration) as Box<dyn Integration>]);
        let tmp = tempfile::tempdir().unwrap();
        // Both instances detect — additive, not first-match.
        assert_eq!(reg.detect_all(tmp.path()).len(), 2);
    }
}
