//! Planner — turn a workspace into a [`Plan`] of tasks, annotated with
//! cache keys and hit/miss status.
//!
//! This is *read-only*: no commands are run, no state is mutated. Execution
//! is handled by `bento ci`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use bento_adapters::{
    AdapterRegistry, CliRequirement, DefaultTask, Integration, IntegrationRegistry,
    IntegrationTask, IntegrationTaskKind, LanguageAdapter,
};
use bento_cache::{CacheKey, Hasher, InputManifest, LocalCache, ManifestFile};
use bento_config::{DishConfig, LoadedDish, Workspace};

use crate::diff::GitDiff;

// ── Output types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Plan {
    pub bentos: Vec<PlannedBento>,
    pub summary: PlanSummary,
    /// Workspace-relative paths of `dish.toml` files on disk that aren't
    /// wired into any bento's `dishes = [...]` list. These dishes are
    /// invisible to the planner — flagging them here lets agents nudge
    /// the user toward `bento dish add <path>` instead of silently
    /// missing work.
    ///
    /// Additive field: omitted from JSON when empty to avoid churning
    /// every existing consumer's parse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq, JsonSchema)]
pub struct PlanSummary {
    pub dishes: usize,
    pub tasks: usize,
    pub hits: usize,
    pub misses: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlannedBento {
    pub name: String,
    pub dishes: Vec<PlannedDish>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlannedDish {
    pub name: String,
    pub path: PathBuf,
    pub language: Option<String>,
    pub tasks: Vec<PlannedTask>,
    /// `true` when git-diff pre-filter marked this dish as unchanged and
    /// the planner short-circuited without computing per-task hashes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped_by_diff: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlannedTask {
    pub name: String,
    pub run: String,
    pub key: String,
    pub status: TaskStatus,
    /// When `status == CacheMiss`, attribution for *why* this miss exists.
    /// Lets agents distinguish real misses from structurally-uncacheable
    /// tasks (Deploy/Notify) and CLI-forced reruns without a second
    /// round-trip through `bento why`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miss_reason: Option<MissReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    CacheHit,
    CacheMiss,
    /// No language adapter detected and no explicit task list.
    NoAdapter,
    /// Pre-filter (git diff) said this dish didn't change; cache hit assumed.
    SkippedDiffClean,
}

/// Why a [`TaskStatus::CacheMiss`] entry is a miss. Populated on misses
/// only — [`None`] when status is anything else.
///
/// - `Uncacheable`: the task is structurally opted out of the cache
///   (`no_cache = true`), typically Integration-emitted Deploy / Notify
///   side-effect tasks. Not a real "miss" — the cache never considers it.
/// - `ForceRerun`: the CLI passed `--no-cache`, so every task reads as
///   miss regardless of the store's contents.
/// - `NeverCached`: cache lookup returned `false`. Today this covers both
///   "first time we've seen this key" and "inputs changed so the key
///   differs from what's stored". Distinguishing those two requires a
///   (dish, task) → recent-keys index which the local cache doesn't keep;
///   tracked for future work. Run `bento why <task>` for the per-input
///   breakdown today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissReason {
    Uncacheable,
    ForceRerun,
    NeverCached,
}

// ── Planner ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Restrict to a single bento by name.
    pub bento_filter: Option<String>,
    /// Restrict to a single dish by name. When both filters are set, the
    /// plan only includes the dish when it appears in the requested
    /// bento's `dishes` list (intersection semantics).
    pub dish_filter: Option<String>,
    /// Treat every task as a cache miss (skip cache lookup).
    pub no_cache: bool,
    /// Base ref for git-diff pre-filter. When set, dishes with no changed
    /// file since `since` are short-circuited to [`TaskStatus::SkippedDiffClean`].
    pub since: Option<String>,
}

pub struct Planner {
    workspace: Workspace,
    registry: AdapterRegistry,
    integrations: IntegrationRegistry,
    cache: LocalCache,
    diff: Option<GitDiff>,
}

impl Planner {
    pub fn new(workspace: Workspace, registry: AdapterRegistry, cache: LocalCache) -> Self {
        Self {
            workspace,
            registry,
            integrations: IntegrationRegistry::empty(),
            cache,
            diff: None,
        }
    }

    /// Attach (or replace) the integration registry. Planners default
    /// to an empty registry so existing callers continue to work; the
    /// CLI populates it via [`IntegrationRegistry::builtin`].
    pub fn with_integrations(mut self, integrations: IntegrationRegistry) -> Self {
        self.integrations = integrations;
        self
    }

    pub fn with_diff(mut self, diff: GitDiff) -> Self {
        self.diff = Some(diff);
        self
    }

    pub fn compute(&self, opts: &PlanOptions) -> Result<Plan> {
        let mut clean_dirs: Option<BTreeSet<PathBuf>> = None;
        if let (Some(base_ref), Some(diff)) = (&opts.since, &self.diff) {
            let dish_rels: Vec<PathBuf> = self.workspace.dishes_by_path.keys().cloned().collect();
            let dirty = diff
                .changed_dirs(base_ref, dish_rels.clone())
                .with_context(|| format!("computing diff against {base_ref}"))?;
            clean_dirs = Some(
                dish_rels
                    .into_iter()
                    .filter(|d| !dirty.contains(d))
                    .collect(),
            );
        }

        let mut planned_bentos = Vec::new();
        let mut summary = PlanSummary::default();

        for (name, bento) in &self.workspace.bentos {
            if let Some(filter) = &opts.bento_filter {
                if filter != name {
                    continue;
                }
            }

            // Per-bento dep-signature table; threaded into each task key
            // so a change in any non-force_independent dep invalidates
            // the dependent's cache pessimistically.
            let graph = crate::graph::build(&self.workspace, name)
                .with_context(|| format!("building dep graph for bento '{name}'"))?;
            let dep_sigs = crate::cascade::compute(&self.workspace, &graph, &self.registry)
                .with_context(|| format!("computing dep signatures for bento '{name}'"))?;

            let mut dishes = Vec::new();
            for dish_ref in &bento.config.dishes {
                let rel = PathBuf::from(dish_ref);
                let loaded = self
                    .workspace
                    .dishes_by_path
                    .get(&rel)
                    .expect("workspace load guaranteed this");
                if let Some(dish) = &opts.dish_filter {
                    if &loaded.config.name != dish {
                        continue;
                    }
                }
                let planned = self.plan_dish(loaded, opts, clean_dirs.as_ref(), &dep_sigs)?;
                summary.dishes += 1;
                for task in &planned.tasks {
                    summary.tasks += 1;
                    match task.status {
                        TaskStatus::CacheHit => summary.hits += 1,
                        TaskStatus::CacheMiss => summary.misses += 1,
                        TaskStatus::NoAdapter => {}
                        TaskStatus::SkippedDiffClean => summary.skipped += 1,
                    }
                }
                dishes.push(planned);
            }

            planned_bentos.push(PlannedBento {
                name: name.clone(),
                dishes,
            });
        }

        let orphans = crate::discovery::scan_orphan_dishes(&self.workspace);

        Ok(Plan {
            bentos: planned_bentos,
            summary,
            orphans,
        })
    }

    fn plan_dish(
        &self,
        loaded: &LoadedDish,
        opts: &PlanOptions,
        clean_dirs: Option<&BTreeSet<PathBuf>>,
        dep_sigs: &std::collections::BTreeMap<String, crate::cascade::DishSig>,
    ) -> Result<PlannedDish> {
        let dish = &loaded.config;
        let adapter = self.resolve_adapter(loaded);
        let language = adapter.map(|a| a.id().to_string());
        let integrations = resolve_integrations(&self.integrations, loaded);

        let tasks_resolved = resolve_tasks(&loaded.dir, dish, adapter, &integrations)
            .with_context(|| format!("dish '{}'", dish.name))?;

        // Pre-filter via git diff.
        let is_clean = clean_dirs
            .map(|clean| clean.contains(&loaded.rel))
            .unwrap_or(false);

        let mut planned_tasks = Vec::new();

        if tasks_resolved.is_empty() && adapter.is_none() {
            // Nothing to plan, nothing to report — leave tasks empty.
        } else {
            let dep_mixins = crate::cascade::deps_for_key(dish, dep_sigs);
            let image = container_image_for_plan(&self.workspace);
            // Track per-task keys as we walk so intra-dish task deps
            // (e.g. `railway:deploy` → `build`) can mix the dep's key
            // into the current task's hash. BTreeMap ordering means
            // alphabetical task names land a `build` key before
            // `railway:deploy` hashes — sufficient today since the
            // only tasks declaring `depends_on` come from integration
            // adapters that declare "build".
            let mut computed_keys: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for task in &tasks_resolved {
                let task_dep_keys: Vec<(&str, &str)> = task
                    .depends_on
                    .iter()
                    .filter_map(|dep| computed_keys.get(dep).map(|k| (dep.as_str(), k.as_str())))
                    .collect();
                let (key, _manifest) = compute_key(
                    &loaded.dir,
                    &dish.name,
                    adapter,
                    task,
                    &dep_mixins,
                    image.as_deref(),
                    &task_dep_keys,
                )?;
                computed_keys.insert(task.name.clone(), key.as_hex().to_string());
                let (status, miss_reason) = if is_clean {
                    (TaskStatus::SkippedDiffClean, None)
                } else if task.no_cache {
                    (TaskStatus::CacheMiss, Some(MissReason::Uncacheable))
                } else if opts.no_cache {
                    (TaskStatus::CacheMiss, Some(MissReason::ForceRerun))
                } else if self.cache.contains(&key) {
                    (TaskStatus::CacheHit, None)
                } else {
                    (TaskStatus::CacheMiss, Some(MissReason::NeverCached))
                };
                planned_tasks.push(PlannedTask {
                    name: task.name.clone(),
                    run: task.run.clone(),
                    key: key.as_hex().to_string(),
                    status,
                    miss_reason,
                });
            }
        }

        if planned_tasks.is_empty() && adapter.is_none() && dish.tasks.is_empty() {
            planned_tasks.push(PlannedTask {
                name: "<none>".to_string(),
                run: "".to_string(),
                key: String::new(),
                status: TaskStatus::NoAdapter,
                miss_reason: None,
            });
        }

        Ok(PlannedDish {
            name: dish.name.clone(),
            path: loaded.rel.clone(),
            language,
            tasks: planned_tasks,
            skipped_by_diff: is_clean,
        })
    }

    fn resolve_adapter(&self, loaded: &LoadedDish) -> Option<&dyn LanguageAdapter> {
        resolve_adapter(&self.registry, loaded)
    }
}

// ── Task resolution ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTask {
    pub name: String,
    pub run: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Output globs anchored at the bento workspace root. Empty unless
    /// the dish declares `[tasks.<name>] workspace_outputs = [...]`.
    /// Lets cargo workspace members cache their compiled binary (which
    /// cargo writes to `<workspace-root>/target/`, outside the dish).
    pub workspace_outputs: Vec<String>,
    pub env: Vec<String>,
    /// Additional attempts after the first. 0 = run once, fail on first error.
    pub retry: u32,
    /// Skip cache lookup AND cache put. Set by integration tasks for
    /// side-effectful operations like deploys. User tasks can also
    /// opt in via `[tasks.<name>] no_cache = true` (future).
    pub no_cache: bool,
    /// Env vars that must be present in the host environment for this
    /// task to run. Integration tasks declare these (e.g. `VERCEL_TOKEN`);
    /// missing vars fail the task fast with a clear error instead of
    /// a cryptic 401 from the underlying CLI. Empty for adapter and
    /// user tasks.
    pub required_env: Vec<String>,
    /// CLI binaries that must be on `PATH` for this task to run.
    /// Integration tasks declare these (e.g. `vercel`, `railway`);
    /// missing binaries fail fast with a clear install hint instead
    /// of a shell exit 127. Empty for adapter and user tasks.
    pub required_cli: Vec<CliRequirement>,
    /// When `Some`, this task originated from an [`Integration`] with
    /// the given role. Drives `bento deploy` filtering and surfaces in
    /// the execution report. `None` for adapter defaults and user tasks.
    pub integration_kind: Option<IntegrationTaskKind>,
    /// Names of other tasks **in the same dish** this one logically
    /// depends on. Used for key-cascade: when computing this task's
    /// content-addressed key, the keys of any listed tasks already
    /// computed in this dish get mixed in — so a Railway deploy
    /// (`depends_on = ["build"]`) invalidates when build's inputs
    /// change even if the deploy's own declared inputs didn't.
    /// Populated today only by integration tasks; user `[tasks.<name>]`
    /// blocks don't expose this in schema yet. Empty for adapter
    /// defaults, garnishes, and plain user tasks.
    pub depends_on: Vec<String>,
}

pub(crate) fn resolve_tasks(
    dish_dir: &Path,
    dish: &DishConfig,
    adapter: Option<&dyn LanguageAdapter>,
    integrations: &[&dyn Integration],
) -> Result<Vec<ResolvedTask>> {
    let mut out: BTreeMap<String, ResolvedTask> = BTreeMap::new();

    // Surface the dish-level `inputs` shadowing footgun before any
    // tasks are resolved — adapters that ship their own `default.inputs`
    // silently override anything declared at the dish root, so the user
    // never sees their globs land in the cache key. See plan.rs tests
    // `dish_inputs_shadowed_by_adapter_defaults_*` for the resolution
    // behaviour this warns about.
    let shadowed = shadowed_dish_inputs(dish, adapter);
    if !shadowed.is_empty() {
        tracing::warn!(
            dish = %dish.name,
            tasks = ?shadowed,
            "dish-level `inputs` are silently overridden by adapter defaults for these tasks; \
             declare `inputs = [...]` under each `[tasks.<name>]` block (or remove the dish-level \
             `inputs` field) — see docs/configuration.md § [tasks.<name>]"
        );
    }

    if let Some(a) = adapter {
        for default in a.default_tasks() {
            out.insert(default.name.clone(), resolved_from_default(default, dish));
        }
    }

    // Integration tasks land before user overrides so a dish can
    // override `[tasks."vercel:deploy"]` with a tweaked command.
    //
    // Per-integration config comes from `dish.toml`'s
    // `[integrations.<id>]` block; absent blocks resolve to an empty
    // map so integrations can treat "nothing set" uniformly.
    let empty_config: toml::Table = toml::Table::new();
    for integration in integrations {
        let required_env = integration.required_env();
        let required_cli = integration.required_cli();
        let config = dish
            .integrations
            .get(integration.id())
            .unwrap_or(&empty_config);
        for t in integration.detected_tasks(dish_dir, config) {
            let name = t.name.clone();
            out.insert(
                name.clone(),
                resolved_from_integration(t, &required_env, &required_cli, dish),
            );
        }
    }

    // `[[garnishes]]` — custom-script Notify tasks declared inline.
    // Treated as synthetic Notify-kind integration tasks so the
    // executor's fan-out and payload wiring pick them up uniformly.
    // Parsed before user `[tasks]` so a user can still override a
    // garnish's `run` via `[tasks.<name>]`.
    for g in &dish.garnishes {
        let required_cli = g
            .required_cli
            .iter()
            .map(|spec| {
                // Format: "binary" or "binary: install hint". Anything
                // after the first colon is the hint.
                let (bin, hint) = match spec.split_once(':') {
                    Some((b, h)) => (b.trim(), h.trim()),
                    None => (spec.trim(), ""),
                };
                CliRequirement::new(bin, hint)
            })
            .collect();
        out.insert(
            g.name.clone(),
            ResolvedTask {
                name: g.name.clone(),
                run: g.run.clone(),
                inputs: dish.inputs.clone(),
                outputs: Vec::new(),
                workspace_outputs: Vec::new(),
                env: g.env.clone(),
                retry: 0,
                no_cache: true,
                required_env: g.required_env.clone(),
                required_cli,
                integration_kind: Some(IntegrationTaskKind::Notify),
                depends_on: Vec::new(),
            },
        );
    }

    for (name, task) in &dish.tasks {
        // A user override preserves the integration_kind / no_cache /
        // required_env / required_cli semantics of the original so
        // `bento deploy` still filters, deploy tasks still skip the
        // cache, and the missing-env / missing-cli gate still fires
        // even after a custom `run` override.
        let existing = out.get(name);
        let (integration_kind, no_cache, required_env, required_cli, depends_on) = existing
            .map(|e| {
                (
                    e.integration_kind,
                    e.no_cache,
                    e.required_env.clone(),
                    e.required_cli.clone(),
                    e.depends_on.clone(),
                )
            })
            .unwrap_or_default();
        // `inputs` / `outputs` inherit from the existing entry (adapter
        // default, integration task) when the user omits them. Same
        // partial-override principle as `run`: a dish that writes
        // `[tasks.build] workspace_outputs = [...]` shouldn't lose the
        // adapter's `src/**` input glob just because it didn't restate
        // it. When there's no existing entry, fall back to the dish's
        // own `inputs`/`outputs` fields (existing behaviour).
        let inputs = task
            .inputs
            .clone()
            .or_else(|| existing.map(|e| e.inputs.clone()))
            .unwrap_or_else(|| dish.inputs.clone());
        let outputs = task
            .outputs
            .clone()
            .or_else(|| existing.map(|e| e.outputs.clone()))
            .unwrap_or_else(|| dish.outputs.clone());
        // `run` inherits from the existing entry (adapter default,
        // integration task, or garnish) when the user omits it — lets
        // a dish add `outputs`/`inputs`/`env`/`retry` to a built-in
        // task without re-declaring the command. A user block with no
        // `run` and no entry to inherit from is a resolve-time error.
        let run = match (&task.run, existing) {
            (Some(r), _) => r.clone(),
            (None, Some(e)) => e.run.clone(),
            (None, None) => {
                anyhow::bail!(
                    "task '{name}' has no 'run' and no adapter default, integration, or garnish \
                     to inherit from"
                );
            }
        };
        // `workspace_outputs` is opt-in per user Task. Inherit from the
        // existing entry when the user doesn't set it (no adapter ships
        // defaults today, but garnish / integration sources could in
        // future without a schema change).
        let workspace_outputs = task
            .workspace_outputs
            .clone()
            .or_else(|| existing.map(|e| e.workspace_outputs.clone()))
            .unwrap_or_default();
        out.insert(
            name.clone(),
            ResolvedTask {
                name: name.clone(),
                run,
                inputs,
                outputs,
                workspace_outputs,
                env: task.env.clone(),
                retry: task.retry,
                no_cache,
                required_env,
                required_cli,
                integration_kind,
                depends_on,
            },
        );
    }

    Ok(out.into_values().collect())
}

pub(crate) fn resolve_integrations<'a>(
    registry: &'a IntegrationRegistry,
    loaded: &LoadedDish,
) -> Vec<&'a dyn Integration> {
    // Union of two signals: filesystem detection (Vercel / Railway —
    // `vercel.json` present, `railway.toml` present) and explicit
    // opt-in via a `[integrations.<id>]` block in dish.toml (Slack /
    // Linear / any future webhook-style integration with no platform
    // file to sniff). Dedupe by id so an integration matching both
    // signals still only emits its task set once.
    let mut out: Vec<&dyn Integration> = registry.detect_all(&loaded.dir);
    for id in loaded.config.integrations.keys() {
        if out.iter().any(|i| i.id() == id) {
            continue;
        }
        if let Some(found) = registry.by_id(id) {
            out.push(found);
        }
    }
    out
}

/// Names of tasks where dish-level `inputs` are silently shadowed by
/// adapter-default `inputs`. Empty when `dish.inputs` is empty, when
/// no adapter is detected, or when no adapter-default task ships its
/// own `inputs`.
pub(crate) fn shadowed_dish_inputs(
    dish: &DishConfig,
    adapter: Option<&dyn LanguageAdapter>,
) -> Vec<String> {
    if dish.inputs.is_empty() {
        return Vec::new();
    }
    let Some(a) = adapter else {
        return Vec::new();
    };
    a.default_tasks()
        .into_iter()
        .filter(|d| d.inputs.is_some())
        .map(|d| d.name)
        .collect()
}

fn resolved_from_default(default: DefaultTask, dish: &DishConfig) -> ResolvedTask {
    let inputs = default.inputs.unwrap_or_else(|| dish.inputs.clone());
    let outputs = default.outputs.unwrap_or_else(|| dish.outputs.clone());
    ResolvedTask {
        name: default.name,
        run: default.run,
        inputs,
        outputs,
        workspace_outputs: Vec::new(),
        env: Vec::new(),
        retry: 0,
        no_cache: false,
        required_env: Vec::new(),
        required_cli: Vec::new(),
        integration_kind: None,
        depends_on: Vec::new(),
    }
}

fn resolved_from_integration(
    task: IntegrationTask,
    integration_required_env: &[String],
    integration_required_cli: &[CliRequirement],
    dish: &DishConfig,
) -> ResolvedTask {
    // Integration tasks take the dish's inputs as-is (the tool's
    // invocation is typically a thin shell over built artefacts —
    // the build task's outputs — so per-task inputs don't buy us
    // much). Users wanting tighter scoping override via dish.toml.
    // The `depends_on` field carries forward so key-cascade can mix
    // in the build task's signature — without it, a deploy's
    // declared inputs (empty for most dishes) wouldn't see source
    // changes and `skip-if-unchanged` would no-op real deploys.
    let inputs = dish.inputs.clone();
    ResolvedTask {
        name: task.name,
        run: task.run,
        inputs,
        outputs: task.outputs,
        workspace_outputs: Vec::new(),
        env: task.env_vars,
        retry: 0,
        // Forced true for side-effectful kinds regardless of the
        // integration's declared preference — we never cache-hit a
        // prod deploy.
        no_cache: task.no_cache || task.kind.defaults_no_cache(),
        required_env: integration_required_env.to_vec(),
        required_cli: integration_required_cli.to_vec(),
        integration_kind: Some(task.kind),
        depends_on: task.depends_on,
    }
}

pub(crate) fn resolve_adapter<'a>(
    registry: &'a AdapterRegistry,
    loaded: &LoadedDish,
) -> Option<&'a dyn LanguageAdapter> {
    if let Some(id) = &loaded.config.language {
        return registry.by_id(id);
    }
    registry.detect(&loaded.dir)
}

// ── Hashing ────────────────────────────────────────────────────────

pub(crate) fn bento_version_major_minor() -> String {
    let v = env!("CARGO_PKG_VERSION");
    let parts: Vec<&str> = v.split('.').take(2).collect();
    parts.join(".")
}

/// Host triple — `"<arch>-<os>"` — used to disambiguate cache entries
/// across architectures. Without this in the cache key, an x86_64
/// runner could write a `target/release/<binary>` to a remote cache
/// that an aarch64 puller would then try to execute (silent
/// corruption: the cache hits but the binary is wrong). Both adapter-
/// level toolchain probes (e.g. `go version`) and this universal mix-in
/// catch the case; this one is belt-and-braces and protects every
/// adapter equally, including those whose `--version` probe is
/// arch-blind (rustc, python, node, ruby, etc.).
///
/// `std::env::consts::ARCH` and `OS` are compile-time constants —
/// since bento itself is compiled for a specific host and only runs
/// natively on that host, they reflect the actual runtime architecture
/// exactly.
///
/// Limitation: on Linux, `OS = "linux"` for both glibc and musl
/// builds. Cross-libc cache sharing is rare and out of scope for v0.1;
/// upgrade to a full rustc target triple if it ever bites.
pub(crate) fn host_triple() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

pub(crate) fn compute_key(
    dish_dir: &Path,
    dish_name: &str,
    adapter: Option<&dyn LanguageAdapter>,
    task: &ResolvedTask,
    dep_signatures: &[(&str, &crate::cascade::DishSig)],
    container_image: Option<&str>,
    task_dep_keys: &[(&str, &str)],
) -> Result<(CacheKey, InputManifest)> {
    let mut hasher = Hasher::new();
    let bento_version = bento_version_major_minor();
    let host = host_triple();
    hasher.add_extra("bento_version", &bento_version);
    hasher.add_extra("host", &host);
    hasher.add_extra("task_name", &task.name);
    hasher.add_extra("task_command", &task.run);
    if let Some(img) = container_image {
        // Image ref covers digest + tag; when the user flips from a
        // tag like `debian:12` to a digest-pinned reference the key
        // naturally changes.
        hasher.add_extra("container_image", img);
    }

    let mut adapter_id: Option<String> = None;
    let mut toolchain_desc: Option<String> = None;
    if let Some(a) = adapter {
        adapter_id = Some(a.id().to_string());
        hasher.add_extra("adapter", a.id());
        if let Some(v) = a
            .required_toolchain(dish_dir)
            .with_context(|| format!("resolving toolchain for {}", dish_dir.display()))?
        {
            let desc = format!("{}:{}", v.tool, v.version);
            hasher.add_extra("toolchain", &desc);
            toolchain_desc = Some(desc);
        }
        // Patch-level drift underneath a declared pin: hash the actual
        // installed toolchain version so a system `go 1.22.3 → 1.22.5`
        // bump invalidates cache entries that would otherwise give a
        // stale hit. Probe is memoised per-adapter-id for the process.
        if let Some(resolved) = a.resolved_toolchain_fingerprint() {
            hasher.add_extra("toolchain_resolved", &resolved);
        }
    }

    for name in &task.env {
        let value = std::env::var(name).unwrap_or_default();
        hasher.add_extra(&format!("env:{name}"), &value);
    }

    // Pessimistic-correct cascade: mix in each declared dep's transitive
    // input signature so a change in any non-force_independent dep
    // invalidates this task's cache. Deps passed in here are already
    // filtered by dish.force_independent; an empty slice means "skip".
    for (dep_name, sig) in dep_signatures {
        hasher.add_extra(&format!("dep:{dep_name}"), &crate::cascade::sig_to_hex(sig));
    }

    // Intra-dish task-level cascade: mix in the content-addressed key of
    // any task in the **same dish** this one declares it depends on. The
    // caller is responsible for passing only keys it has already
    // computed (tasks are walked in BTreeMap order; an earlier-named
    // dep's key is available by the time a later-named task hashes).
    // Closes a skip-if-unchanged hole for Deploy-kind tasks: without
    // this, a `railway:deploy` with empty declared inputs would hash
    // identically across source edits, `deploy_state_hit` would find a
    // match, and `bento deploy` would silently no-op. With this mix-in
    // the build task's key (which already hashes `src/**`) propagates
    // into the deploy task's key, so real source changes invalidate.
    for (dep_task, key_hex) in task_dep_keys {
        hasher.add_extra(&format!("task_dep:{dep_task}"), key_hex);
    }

    // Union of task inputs and adapter fingerprints.
    let mut globs: Vec<String> = task.inputs.clone();
    if let Some(a) = adapter {
        for f in a.fingerprint_files() {
            if !globs.contains(&f) {
                globs.push(f);
            }
        }
    }

    // Adapter-declared derived paths — files the adapter writes as a
    // side effect of running (lockfiles, egg-info, dist/, __pycache__,
    // bundler vendor dirs, …). Filter these out of the matched walk
    // so same-source-same-key holds even after the first run has
    // littered the dish with generated artefacts.
    let derived_matcher = match adapter {
        Some(a) => {
            let derived = a.derived_paths();
            if derived.is_empty() {
                None
            } else {
                Some(build_matcher(&derived)?)
            }
        }
        None => None,
    };

    let mut manifest_files = Vec::new();

    if !globs.is_empty() && dish_dir.is_dir() {
        let matcher = build_matcher(&globs)?;

        let mut matched: Vec<PathBuf> = Vec::new();
        for entry in walkdir::WalkDir::new(dish_dir).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = match entry.path().strip_prefix(dish_dir) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            if let Some(ref d) = derived_matcher {
                if d.is_match(&rel) {
                    continue;
                }
            }
            if matcher.is_match(&rel) {
                matched.push(rel);
            }
        }
        matched.sort();

        for rel in matched {
            let full = dish_dir.join(&rel);
            let content =
                std::fs::read(&full).with_context(|| format!("reading {}", full.display()))?;
            let file_hash = blake3::hash(&content);
            hasher.add_file(&rel, &content);
            manifest_files.push(ManifestFile {
                path: rel,
                blake3: file_hash.to_hex().to_string(),
                size_bytes: content.len() as u64,
            });
        }
    }

    let manifest = InputManifest {
        version: InputManifest::CURRENT_VERSION,
        task_name: task.name.clone(),
        run: task.run.clone(),
        dish: dish_name.to_string(),
        adapter: adapter_id,
        toolchain: toolchain_desc,
        bento_version,
        host: Some(host),
        env_vars: task.env.clone(),
        files: manifest_files,
    };

    Ok((hasher.finalize(), manifest))
}

fn build_matcher(globs: &[String]) -> Result<globset::GlobSet> {
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        b.add(globset::Glob::new(g).with_context(|| format!("compiling input glob `{g}`"))?);
    }
    Ok(b.build()?)
}

// ── Top-level entry points ─────────────────────────────────────────

/// Peek the workspace's container-image config purely for cache-key
/// purposes — we don't actually need a runtime here; we just want the
/// string in the hash when the user has declared an image and opted
/// into container execution.
pub(crate) fn container_image_for_plan(workspace: &Workspace) -> Option<String> {
    use bento_config::ContainerMode;
    let exec = &workspace.repo.execution;
    match exec.container {
        ContainerMode::Never => None,
        ContainerMode::Always => exec.image.clone(),
        // In "auto" we assume the image fingerprint is relevant whenever
        // one is declared — that matches how "auto" resolves at exec
        // time (image + runtime ⇒ containerise).
        ContainerMode::Auto => exec.image.clone(),
    }
}

/// Returned when `find_workspace_root` walks to `/` without finding
/// a `bento.toml` or `bentos/`. Downcast-friendly so the CLI can classify
/// it as `kind = "workspace_not_found"` on JSON output.
#[derive(Debug, thiserror::Error)]
#[error("no bento workspace (bento.toml or bentos/) found at or above {start}")]
pub struct WorkspaceNotFound {
    pub start: PathBuf,
}

/// Walk upward from `start` looking for a bento workspace root
/// (anywhere with a `bento.toml` or a `bentos/` dir).
pub fn find_workspace_root(start: &Path) -> Result<PathBuf> {
    let canonical = start
        .canonicalize()
        .with_context(|| format!("canonicalising start path {}", start.display()))?;
    let mut cursor = canonical.as_path();
    loop {
        if cursor.join("bento.toml").is_file() || cursor.join("bentos").is_dir() {
            return Ok(cursor.to_path_buf());
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => {
                return Err(WorkspaceNotFound {
                    start: start.to_path_buf(),
                }
                .into());
            }
        }
    }
}

/// Local cache root. Resolution order:
///
/// 1. `$BENTO_CACHE_DIR` (absolute path) — explicit override, used
///    by container images that want the cache on a mounted volume
///    and by the e2e test harness to isolate parallel test runs.
/// 2. Default: `$HOME/.bento/cache`.
///
/// Empty / missing env var falls through to the default; a set-but-
/// empty value is treated the same as unset so `BENTO_CACHE_DIR=` in
/// a `.env` doesn't silently wreck caching.
pub fn default_cache_root() -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os("BENTO_CACHE_DIR") {
        if !custom.is_empty() {
            return Ok(PathBuf::from(custom));
        }
    }
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home.join(".bento").join("cache"))
}

/// Load a workspace at `root`, wire up the built-in adapter registry and
/// default cache, run the planner, and return the [`Plan`].
pub fn plan_at(root: impl AsRef<Path>, opts: &PlanOptions) -> Result<Plan> {
    let root = root.as_ref();
    let workspace = Workspace::load(root)
        .with_context(|| format!("loading workspace at {}", root.display()))?;
    let registry = AdapterRegistry::builtin();
    let integrations = IntegrationRegistry::builtin();
    let cache = LocalCache::new(default_cache_root()?);

    let mut planner = Planner::new(workspace, registry, cache).with_integrations(integrations);
    if opts.since.is_some() {
        planner = planner.with_diff(GitDiff::new(root));
    }
    planner.compute(opts)
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 2-dish sample workspace (Go service + node-npm web) in a tempdir.
    fn two_dish_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        std::fs::write(root.join("bento.toml"), "[defaults]\nparallelism = 2\n").unwrap();

        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["apps/api", "apps/web"]"#,
        )
        .unwrap();

        let api = root.join("apps/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(api.join("go.mod"), "module example.com/api\n\ngo 1.22\n").unwrap();
        std::fs::write(api.join("go.sum"), "").unwrap();
        std::fs::write(api.join("main.go"), "package main\n\nfunc main() {}\n").unwrap();
        std::fs::write(
            api.join("dish.toml"),
            r#"name = "sample-api"
language = "go""#,
        )
        .unwrap();

        let web = root.join("apps/web");
        std::fs::create_dir_all(web.join("src")).unwrap();
        std::fs::write(
            web.join("package.json"),
            r#"{"name":"web","scripts":{"build":"echo build","test":"echo test","lint":"echo lint"}}"#,
        )
        .unwrap();
        std::fs::write(web.join("package-lock.json"), "{}").unwrap();
        std::fs::write(web.join("src/App.tsx"), "export default () => null;\n").unwrap();
        std::fs::write(
            web.join("dish.toml"),
            r#"name = "sample-web"
language = "node-npm""#,
        )
        .unwrap();

        tmp
    }

    fn planner_with_fresh_cache(root: &Path) -> (Planner, tempfile::TempDir) {
        let workspace = Workspace::load(root).unwrap();
        let registry = AdapterRegistry::builtin();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        (Planner::new(workspace, registry, cache), cache_dir)
    }

    #[test]
    fn plan_produces_one_bento_two_dishes() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());

        let plan = planner.compute(&PlanOptions::default()).unwrap();
        assert_eq!(plan.bentos.len(), 1);
        assert_eq!(plan.bentos[0].name, "prod");
        assert_eq!(plan.bentos[0].dishes.len(), 2);
        assert_eq!(plan.summary.dishes, 2);
    }

    #[test]
    fn plan_detects_languages_per_dish() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());

        let plan = planner.compute(&PlanOptions::default()).unwrap();
        let by_name: BTreeMap<_, _> = plan.bentos[0]
            .dishes
            .iter()
            .map(|d| (d.name.as_str(), d.language.as_deref()))
            .collect();
        assert_eq!(by_name["sample-api"], Some("go"));
        assert_eq!(by_name["sample-web"], Some("node-npm"));
    }

    #[test]
    fn plan_tasks_use_adapter_defaults() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());

        let plan = planner.compute(&PlanOptions::default()).unwrap();
        let api = plan.bentos[0]
            .dishes
            .iter()
            .find(|d| d.name == "sample-api")
            .unwrap();
        let names: BTreeSet<_> = api.tasks.iter().map(|t| t.name.clone()).collect();
        assert!(names.contains("build"));
        assert!(names.contains("test"));
        assert!(names.contains("lint"));
    }

    #[test]
    fn plan_starts_with_all_misses() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());

        let plan = planner.compute(&PlanOptions::default()).unwrap();
        for bento in &plan.bentos {
            for dish in &bento.dishes {
                for task in &dish.tasks {
                    assert_eq!(
                        task.status,
                        TaskStatus::CacheMiss,
                        "{}/{} should miss",
                        dish.name,
                        task.name
                    );
                }
            }
        }
        assert_eq!(plan.summary.hits, 0);
        assert_eq!(plan.summary.misses, plan.summary.tasks);
    }

    #[test]
    fn plan_reports_hits_when_cache_populated() {
        let tmp = two_dish_fixture();
        let (planner, cache_dir) = planner_with_fresh_cache(tmp.path());

        // First plan — all miss — but capture the keys.
        let first = planner.compute(&PlanOptions::default()).unwrap();
        let first_api = first.bentos[0]
            .dishes
            .iter()
            .find(|d| d.name == "sample-api")
            .unwrap();
        let build_key = first_api
            .tasks
            .iter()
            .find(|t| t.name == "build")
            .unwrap()
            .key
            .clone();

        // Prime the cache with a dummy bundle for that key.
        let cache = LocalCache::new(cache_dir.path());
        let key = CacheKey::from_hex(build_key);
        cache
            .put(
                &key,
                tmp.path(),
                &[],
                None,
                &[],
                &bento_cache::TaskResult {
                    exit_code: 0,
                    stdout: b"ok\n".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .unwrap();

        // Re-plan: build should hit, others still miss.
        let second = planner.compute(&PlanOptions::default()).unwrap();
        let api = second.bentos[0]
            .dishes
            .iter()
            .find(|d| d.name == "sample-api")
            .unwrap();
        let build = api.tasks.iter().find(|t| t.name == "build").unwrap();
        assert_eq!(build.status, TaskStatus::CacheHit);
        assert_eq!(build.miss_reason, None);
        assert_eq!(second.summary.hits, 1);
    }

    #[test]
    fn never_cached_miss_is_tagged_never_cached() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());
        let plan = planner.compute(&PlanOptions::default()).unwrap();
        for bento in &plan.bentos {
            for dish in &bento.dishes {
                for task in &dish.tasks {
                    if task.status == TaskStatus::CacheMiss {
                        assert_eq!(
                            task.miss_reason,
                            Some(MissReason::NeverCached),
                            "{}/{}",
                            dish.name,
                            task.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn no_cache_flag_tags_misses_as_force_rerun() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());
        let plan = planner
            .compute(&PlanOptions {
                no_cache: true,
                ..Default::default()
            })
            .unwrap();
        let mut saw_force = false;
        for bento in &plan.bentos {
            for dish in &bento.dishes {
                for task in &dish.tasks {
                    if task.status == TaskStatus::CacheMiss {
                        assert_eq!(
                            task.miss_reason,
                            Some(MissReason::ForceRerun),
                            "{}/{}",
                            dish.name,
                            task.name
                        );
                        saw_force = true;
                    }
                }
            }
        }
        assert!(saw_force, "expected at least one ForceRerun-tagged miss");
    }

    #[test]
    fn hits_have_no_miss_reason() {
        // Regression on the invariant: miss_reason is None unless the
        // task is actually a CacheMiss.
        let tmp = two_dish_fixture();
        let (planner, cache_dir) = planner_with_fresh_cache(tmp.path());
        let first = planner.compute(&PlanOptions::default()).unwrap();
        let first_task = first.bentos[0].dishes[0].tasks[0].clone();
        let cache = LocalCache::new(cache_dir.path());
        cache
            .put(
                &CacheKey::from_hex(first_task.key),
                tmp.path(),
                &[],
                None,
                &[],
                &bento_cache::TaskResult::default(),
            )
            .unwrap();
        let second = planner.compute(&PlanOptions::default()).unwrap();
        for bento in &second.bentos {
            for dish in &bento.dishes {
                for task in &dish.tasks {
                    if task.status == TaskStatus::CacheHit {
                        assert_eq!(task.miss_reason, None);
                    }
                }
            }
        }
    }

    #[test]
    fn plan_no_cache_flag_forces_miss() {
        let tmp = two_dish_fixture();
        let (planner, cache_dir) = planner_with_fresh_cache(tmp.path());

        // Populate cache with something that would otherwise hit.
        let first = planner.compute(&PlanOptions::default()).unwrap();
        let first_task = first.bentos[0].dishes[0].tasks[0].clone();
        let cache = LocalCache::new(cache_dir.path());
        cache
            .put(
                &CacheKey::from_hex(first_task.key),
                tmp.path(),
                &[],
                None,
                &[],
                &bento_cache::TaskResult::default(),
            )
            .unwrap();

        let plan = planner
            .compute(&PlanOptions {
                no_cache: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(plan.summary.hits, 0);
    }

    #[test]
    fn plan_dish_filter_restricts_to_one_dish() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());
        let plan = planner
            .compute(&PlanOptions {
                dish_filter: Some("sample-api".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(plan.bentos.len(), 1, "the single bento is preserved");
        assert_eq!(
            plan.bentos[0].dishes.len(),
            1,
            "only the filtered dish plans: {:?}",
            plan.bentos[0]
                .dishes
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(plan.bentos[0].dishes[0].name, "sample-api");
    }

    #[test]
    fn plan_bento_filter_restricts_to_one() {
        let tmp = two_dish_fixture();

        // Add a second bento definition so filtering is meaningful.
        std::fs::write(
            tmp.path().join("bentos/staging.toml"),
            r#"name = "staging"
dishes = ["apps/api"]"#,
        )
        .unwrap();

        let (planner, _cache) = planner_with_fresh_cache(tmp.path());
        let plan = planner
            .compute(&PlanOptions {
                bento_filter: Some("staging".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(plan.bentos.len(), 1);
        assert_eq!(plan.bentos[0].name, "staging");
        assert_eq!(plan.summary.dishes, 1);
    }

    #[test]
    fn host_triple_returns_arch_dash_os() {
        let h = host_triple();
        assert!(
            h.contains('-'),
            "host_triple should be '<arch>-<os>', got {h:?}"
        );
        let (arch, os) = h.split_once('-').unwrap();
        // Sanity: matches the well-known std::env::consts strings. We
        // don't enumerate every arch/os here — the contract is just
        // "non-empty halves separated by `-`".
        assert!(!arch.is_empty(), "arch half empty in {h:?}");
        assert!(!os.is_empty(), "os half empty in {h:?}");
    }

    #[test]
    fn cache_key_includes_host_in_manifest() {
        // Belt-and-braces against silent cache corruption across archs:
        // every cache key carries `host`, surfaced in the manifest so
        // `bento why <hash>` tells the user (or a remote-cache operator
        // chasing a "bad CPU type" report) which arch the entry was
        // built for. If this test breaks, audit any compute_key call
        // sites that bypass the host mix-in.
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());
        let plan = planner.compute(&PlanOptions::default()).unwrap();
        // Pick any task — the manifest carrying `host` is universal.
        let task = plan.bentos[0]
            .dishes
            .iter()
            .find(|d| d.name == "sample-api")
            .unwrap()
            .tasks
            .iter()
            .find(|t| t.name == "build")
            .unwrap();
        // The plan output exposes manifest fields via the executor's
        // input-manifest persistence path; for this unit-level check
        // we re-derive from compute_key so the assertion is direct.
        let dish_dir = tmp.path().join("apps/api");
        let resolved = crate::plan::ResolvedTask {
            name: task.name.clone(),
            run: task.run.clone(),
            inputs: vec!["**".into()],
            outputs: vec![],
            workspace_outputs: vec![],
            env: vec![],
            retry: 0,
            no_cache: false,
            required_env: vec![],
            required_cli: vec![],
            integration_kind: None,
            depends_on: vec![],
        };
        let (_, manifest) = compute_key(&dish_dir, "sample-api", None, &resolved, &[], None, &[])
            .expect("compute_key");
        assert_eq!(
            manifest.host.as_deref(),
            Some(host_triple().as_str()),
            "manifest must carry the host triple so a stale cross-arch cache entry is diagnosable",
        );
    }

    #[test]
    fn cache_key_changes_when_source_changes() {
        let tmp = two_dish_fixture();
        let (planner, _cache) = planner_with_fresh_cache(tmp.path());

        let before = planner.compute(&PlanOptions::default()).unwrap();
        let before_key = before.bentos[0]
            .dishes
            .iter()
            .find(|d| d.name == "sample-api")
            .unwrap()
            .tasks
            .iter()
            .find(|t| t.name == "build")
            .unwrap()
            .key
            .clone();

        std::fs::write(
            tmp.path().join("apps/api/main.go"),
            "package main\n\nfunc main() { println(42) }\n",
        )
        .unwrap();

        let after = planner.compute(&PlanOptions::default()).unwrap();
        let after_key = after.bentos[0]
            .dishes
            .iter()
            .find(|d| d.name == "sample-api")
            .unwrap()
            .tasks
            .iter()
            .find(|t| t.name == "build")
            .unwrap()
            .key
            .clone();

        assert_ne!(before_key, after_key);
    }

    #[test]
    fn find_workspace_root_walks_upward() {
        let tmp = two_dish_fixture();
        let nested = tmp.path().join("apps/api");
        let root = find_workspace_root(&nested).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn find_workspace_root_errors_outside_any_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_workspace_root(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("no bento workspace"), "got: {err}");
    }

    #[test]
    fn resolve_tasks_overrides_adapter_defaults() {
        let dish = bento_config::DishConfig {
            name: "api".into(),
            language: Some("go".into()),
            inputs: vec!["**/*.go".into()],
            tasks: [
                (
                    "build".to_string(),
                    bento_config::Task {
                        run: Some("go build -tags custom ./...".into()),
                        inputs: None,
                        outputs: None,
                        workspace_outputs: None,
                        env: vec![],
                        retry: 0,
                    },
                ),
                (
                    "deploy".to_string(),
                    bento_config::Task {
                        run: Some("./deploy.sh".into()),
                        inputs: None,
                        outputs: None,
                        workspace_outputs: None,
                        env: vec![],
                        retry: 0,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let adapter = bento_adapters::GoAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, Some(&adapter), &[]).unwrap();
        let by_name: BTreeMap<_, _> = resolved
            .iter()
            .map(|t| (t.name.as_str(), t.run.as_str()))
            .collect();

        assert_eq!(by_name["build"], "go build -tags custom ./...");
        assert_eq!(by_name["test"], "go test ./...");
        assert_eq!(by_name["lint"], "golangci-lint run");
        assert_eq!(by_name["deploy"], "./deploy.sh");
    }

    #[test]
    fn partial_override_inherits_run_from_adapter_default() {
        // `[tasks.build] outputs = [...]` with no `run` should pick up
        // the adapter's build command — lets users add outputs (or
        // inputs/env/retry) to a built-in task without re-declaring the
        // command.
        let dish = bento_config::DishConfig {
            name: "api".into(),
            language: Some("go".into()),
            inputs: vec!["**/*.go".into()],
            tasks: [(
                "build".to_string(),
                bento_config::Task {
                    run: None,
                    inputs: None,
                    outputs: Some(vec!["bin/".into()]),
                    workspace_outputs: None,
                    env: vec![],
                    retry: 0,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let adapter = bento_adapters::GoAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, Some(&adapter), &[]).unwrap();
        let build = resolved.iter().find(|t| t.name == "build").unwrap();
        // Inherited run from GoAdapter's default.
        assert_eq!(build.run, "go build ./...");
        // User-declared outputs stick.
        assert_eq!(build.outputs, vec!["bin/".to_string()]);
    }

    #[test]
    fn partial_override_inherits_inputs_and_outputs_from_adapter_default() {
        // Regression: a `[tasks.build]` block that sets only
        // `workspace_outputs` used to wipe the adapter default's
        // `inputs` (`src/**`, ...) because the fallback went straight
        // to `dish.inputs`, skipping the existing `out` entry produced
        // by `resolved_from_default`. Result was that every cargo dish
        // with a partial override cached forever — the input
        // fingerprint never saw the source tree. Now the user-task
        // path inherits `inputs`/`outputs` from the existing entry the
        // same way `run` does.
        let dish = bento_config::DishConfig {
            name: "ctrl-plane".into(),
            language: Some("cargo".into()),
            tasks: [(
                "build".to_string(),
                bento_config::Task {
                    run: None,
                    inputs: None,
                    outputs: None,
                    workspace_outputs: Some(vec!["target/debug/ctrl-plane".into()]),
                    env: vec![],
                    retry: 0,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let adapter = bento_adapters::CargoAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, Some(&adapter), &[]).unwrap();
        let build = resolved.iter().find(|t| t.name == "build").unwrap();
        // Adapter default inputs must survive the partial override —
        // the key point of this test.
        assert!(
            build.inputs.iter().any(|g| g == "src/**"),
            "src/** missing from resolved inputs: {:?}",
            build.inputs
        );
        assert!(
            build.inputs.iter().any(|g| g == "Cargo.toml"),
            "Cargo.toml missing from resolved inputs: {:?}",
            build.inputs
        );
        // User-declared workspace_outputs flows through.
        assert_eq!(
            build.workspace_outputs,
            vec!["target/debug/ctrl-plane".to_string()]
        );
    }

    #[test]
    fn user_task_workspace_outputs_flow_through_to_resolved() {
        // A dish that opts in to workspace-scoped outputs (e.g. cargo
        // workspace member caching its binary) has the field preserved
        // on ResolvedTask so the run-phase plumbing can pass it to the
        // cache layer.
        let dish = bento_config::DishConfig {
            name: "ctrl-plane".into(),
            language: Some("cargo".into()),
            tasks: [(
                "build".to_string(),
                bento_config::Task {
                    run: None,
                    inputs: None,
                    outputs: None,
                    workspace_outputs: Some(vec!["target/release/ctrl-plane".into()]),
                    env: vec![],
                    retry: 0,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let adapter = bento_adapters::CargoAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, Some(&adapter), &[]).unwrap();
        let build = resolved.iter().find(|t| t.name == "build").unwrap();
        assert_eq!(
            build.workspace_outputs,
            vec!["target/release/ctrl-plane".to_string()]
        );
        // And still inherits run from the cargo adapter default.
        assert_eq!(build.run, "cargo build --locked");
    }

    #[test]
    fn partial_override_without_existing_entry_errors() {
        // `[tasks.custom]` with no `run` and no adapter default / integration
        // / garnish to inherit from is a resolve-time error.
        let dish = bento_config::DishConfig {
            name: "api".into(),
            tasks: [(
                "custom".to_string(),
                bento_config::Task {
                    run: None,
                    inputs: None,
                    outputs: Some(vec!["dist/".into()]),
                    workspace_outputs: None,
                    env: vec![],
                    retry: 0,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_tasks(tmp.path(), &dish, None, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("custom") && msg.contains("no 'run'"),
            "got: {msg}"
        );
    }

    #[test]
    fn config_driven_integration_opt_in_loads_even_without_file_detection() {
        // Mock integration whose `detect(dir)` always returns false —
        // like Slack / Linear, where there's no platform-side file to
        // sniff. A dish opts in purely by setting `[integrations.<id>]`
        // in its dish.toml. resolve_integrations must honour that.
        use bento_adapters::{
            Integration, IntegrationRegistry, IntegrationTask, IntegrationTaskKind,
        };
        use bento_config::LoadedDish;

        struct ConfigOnly;
        impl Integration for ConfigOnly {
            fn id(&self) -> &str {
                "config-only"
            }
            fn detect(&self, _dir: &std::path::Path) -> bool {
                false
            }
            fn detected_tasks(&self, _: &std::path::Path, _: &toml::Table) -> Vec<IntegrationTask> {
                vec![IntegrationTask {
                    name: "config-only:notify".into(),
                    kind: IntegrationTaskKind::Notify,
                    run: "true".into(),
                    depends_on: vec![],
                    env_vars: vec![],
                    no_cache: true,
                    outputs: vec![],
                }]
            }
        }

        let registry = IntegrationRegistry::empty()
            .with_plugins([Box::new(ConfigOnly) as Box<dyn Integration>]);

        // Dish that explicitly opts in via config — no file exists
        // on disk, yet the integration should still be resolved.
        let tmp = tempfile::tempdir().unwrap();
        let mut integrations = std::collections::BTreeMap::new();
        integrations.insert("config-only".to_string(), toml::Table::new());
        let loaded = LoadedDish {
            dir: tmp.path().to_path_buf(),
            rel: std::path::PathBuf::from("d"),
            config: DishConfig {
                name: "d".into(),
                integrations,
                ..Default::default()
            },
        };
        let resolved = resolve_integrations(&registry, &loaded);
        assert_eq!(
            resolved.len(),
            1,
            "config-only integration should load via [integrations.<id>] opt-in"
        );
        assert_eq!(resolved[0].id(), "config-only");
    }

    #[test]
    fn resolve_integrations_dedupes_when_both_detect_and_config_match() {
        // An integration that self-detects AND has an `[integrations.<id>]`
        // block should appear exactly once, not twice.
        use bento_adapters::{Integration, IntegrationRegistry, IntegrationTask};
        use bento_config::LoadedDish;

        struct BothDetectAndConfig;
        impl Integration for BothDetectAndConfig {
            fn id(&self) -> &str {
                "dual"
            }
            fn detect(&self, _dir: &std::path::Path) -> bool {
                true
            }
            fn detected_tasks(&self, _: &std::path::Path, _: &toml::Table) -> Vec<IntegrationTask> {
                Vec::new()
            }
        }

        let registry = IntegrationRegistry::empty()
            .with_plugins([Box::new(BothDetectAndConfig) as Box<dyn Integration>]);
        let tmp = tempfile::tempdir().unwrap();
        let mut integrations = std::collections::BTreeMap::new();
        integrations.insert("dual".to_string(), toml::Table::new());
        let loaded = LoadedDish {
            dir: tmp.path().to_path_buf(),
            rel: std::path::PathBuf::from("d"),
            config: DishConfig {
                name: "d".into(),
                integrations,
                ..Default::default()
            },
        };
        let resolved = resolve_integrations(&registry, &loaded);
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn garnish_spec_resolves_to_notify_kind_task() {
        // `[[garnishes]]` in dish.toml: each entry resolves to a
        // Notify-kind task so the executor fans it out like Slack /
        // Linear garnishes do.
        use bento_config::GarnishSpec;

        let dish = DishConfig {
            name: "d".into(),
            garnishes: vec![GarnishSpec {
                name: "github-comment".into(),
                run: "./notify.sh".into(),
                env: vec!["GITHUB_TOKEN".into()],
                required_env: vec!["GITHUB_TOKEN".into()],
                required_cli: vec!["curl: install-hint".into(), "jq".into()],
            }],
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, None, &[]).unwrap();
        assert_eq!(resolved.len(), 1);
        let t = &resolved[0];
        assert_eq!(t.name, "github-comment");
        assert_eq!(t.run, "./notify.sh");
        assert_eq!(t.integration_kind, Some(IntegrationTaskKind::Notify));
        assert!(t.no_cache);
        assert_eq!(t.env, vec!["GITHUB_TOKEN"]);
        assert_eq!(t.required_env, vec!["GITHUB_TOKEN"]);
        // required_cli "curl: install-hint" splits into binary + hint.
        assert_eq!(t.required_cli.len(), 2);
        assert_eq!(t.required_cli[0].binary, "curl");
        assert_eq!(t.required_cli[0].install_hint, "install-hint");
        // Bare "jq" has an empty hint — agents can still see the
        // missing binary clearly.
        assert_eq!(t.required_cli[1].binary, "jq");
        assert_eq!(t.required_cli[1].install_hint, "");
    }

    #[test]
    fn user_task_can_override_garnish_run_but_preserves_notify_kind() {
        // A user-declared `[tasks.<name>]` for a garnish's name
        // takes over the run command while keeping the synthetic
        // Notify kind + no_cache semantics intact.
        use bento_config::{GarnishSpec, Task};

        let mut tasks = BTreeMap::new();
        tasks.insert(
            "my-notify".to_string(),
            Task {
                run: Some("overridden.sh".into()),
                inputs: None,
                outputs: None,
                workspace_outputs: None,
                env: vec![],
                retry: 0,
            },
        );
        let dish = DishConfig {
            name: "d".into(),
            tasks,
            garnishes: vec![GarnishSpec {
                name: "my-notify".into(),
                run: "original.sh".into(),
                env: vec![],
                required_env: vec!["TOKEN".into()],
                required_cli: vec![],
            }],
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, None, &[]).unwrap();
        assert_eq!(resolved.len(), 1);
        let t = &resolved[0];
        assert_eq!(t.run, "overridden.sh");
        assert_eq!(t.integration_kind, Some(IntegrationTaskKind::Notify));
        assert!(t.no_cache);
        // required_env carries over from the synthetic garnish — the
        // override is purely a run/inputs change.
        assert_eq!(t.required_env, vec!["TOKEN"]);
    }

    #[test]
    fn shadowed_dish_inputs_lists_adapter_defaults_with_inputs() {
        // Cargo's adapter ships `inputs` on every default task — so a
        // dish that writes `inputs = ["openapi.yaml"]` at the root has
        // every lifecycle task's cache key silently miss the file.
        let dish = bento_config::DishConfig {
            name: "ctrl-plane".into(),
            language: Some("cargo".into()),
            inputs: vec!["openapi.yaml".into()],
            ..Default::default()
        };
        let adapter = bento_adapters::CargoAdapter;
        let mut shadowed = shadowed_dish_inputs(&dish, Some(&adapter));
        shadowed.sort();
        // Cargo ships build/check/test/lint with inputs.
        assert_eq!(shadowed, vec!["build", "check", "lint", "test"]);
    }

    #[test]
    fn shadowed_dish_inputs_empty_when_dish_inputs_empty() {
        let dish = bento_config::DishConfig {
            name: "api".into(),
            language: Some("cargo".into()),
            inputs: vec![],
            ..Default::default()
        };
        let adapter = bento_adapters::CargoAdapter;
        assert!(shadowed_dish_inputs(&dish, Some(&adapter)).is_empty());
    }

    #[test]
    fn shadowed_dish_inputs_empty_when_no_adapter() {
        let dish = bento_config::DishConfig {
            name: "scripts".into(),
            inputs: vec!["src/**".into()],
            ..Default::default()
        };
        assert!(shadowed_dish_inputs(&dish, None).is_empty());
    }

    #[test]
    fn dish_inputs_silently_dropped_for_adapter_default_tasks() {
        // Regression coverage for the footgun the warn surfaces:
        // dish.inputs = ["openapi.yaml"] does NOT land in cargo build's
        // resolved inputs, even though the docs once promised it would.
        let dish = bento_config::DishConfig {
            name: "ctrl-plane".into(),
            language: Some("cargo".into()),
            inputs: vec!["openapi.yaml".into()],
            ..Default::default()
        };
        let adapter = bento_adapters::CargoAdapter;
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_tasks(tmp.path(), &dish, Some(&adapter), &[]).unwrap();
        let build = resolved.iter().find(|t| t.name == "build").unwrap();
        assert!(
            !build.inputs.iter().any(|g| g == "openapi.yaml"),
            "dish-level `openapi.yaml` unexpectedly landed in cargo build inputs — \
             behaviour changed; update warn + docs to match. inputs={:?}",
            build.inputs
        );
    }
}
