//! Executor — given a workspace, run every dish's tasks in order, honouring
//! the cache (restore on hit, `cache.put` on successful miss).
//!
//! Within a dish we run tasks sequentially. Cross-dish parallelism lands
//! with the real dep graph in a future release.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use bento_adapters::{
    AdapterRegistry, InstallProbe, IntegrationRegistry, IntegrationTaskKind, LanguageAdapter,
    TaskContext,
};
use bento_cache::{CacheKey, InputManifest, LocalCache, RemoteCache, TaskResult};
use bento_config::{LoadedDish, Workspace};
use bento_toolchain::{Installer, ResolutionSource, Resolver, Target};

use crate::plan::{
    compute_key, resolve_adapter, resolve_integrations, resolve_tasks, ResolvedTask,
};

// ── Output types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecutionReport {
    pub bentos: Vec<ExecutedBento>,
    pub summary: ExecutionSummary,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq, JsonSchema)]
pub struct ExecutionSummary {
    pub dishes: usize,
    pub tasks: usize,
    pub hits: usize,
    pub built: usize,
    pub failed: usize,
    /// Tasks that succeeded only after one or more retries.
    pub flaky: usize,
    /// Dishes that ran dependency install this session (probe reported
    /// missing deps, or `--force-install` was set).
    #[serde(default)]
    pub installs: usize,
    /// Dishes whose install itself failed. Tasks for those dishes are
    /// skipped — they'd fail anyway without deps.
    #[serde(default)]
    pub install_failures: usize,
    /// Notify-kind tasks (garnishes — Slack / Linear / PagerDuty /
    /// custom-script hooks fired after Deploy tasks) that failed.
    /// Deliberately *not* folded into `failed`: a Slack webhook
    /// being down shouldn't red-X a successful deploy. Exit code of
    /// the process treats this counter as informational.
    #[serde(default)]
    pub notify_failures: usize,
    /// Deploy-kind tasks short-circuited because their inputs match
    /// the last successful deploy on record. Surfaced so `bento
    /// deploy` can report "nothing to do" as a distinct outcome from
    /// "everything ran".
    #[serde(default)]
    pub deploy_unchanged: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecutedBento {
    pub name: String,
    pub dishes: Vec<ExecutedDish>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecutedDish {
    pub name: String,
    pub path: PathBuf,
    pub language: Option<String>,
    /// Install step record — populated only when the executor ran
    /// `adapter.install()` this session. Absent when the probe was
    /// `Ready`, when `--skip-install` was set, or when the dish has
    /// no adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallRecord>,
    pub tasks: Vec<ExecutedTask>,
}

/// Record of an install step the executor invoked before running tasks.
/// Populated when [`InstallProbe::Missing`] triggered
/// [`LanguageAdapter::install`], or when `--force-install` was used.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InstallRecord {
    /// Probe's Missing reason (e.g. `"node_modules/.package-lock.json
    /// absent"`) or `"--force-install"` when forced.
    pub reason: String,
    pub duration_ms: u64,
    /// `None` when install succeeded; `Some(error)` when it failed.
    /// On failure, tasks for this dish are skipped — their dependencies
    /// aren't installed, so they'd fail anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecutedTask {
    pub name: String,
    pub run: String,
    pub key: String,
    pub duration_ms: u64,
    pub outcome: TaskOutcome,
    /// Total attempts made, including the first. Always >= 1 for
    /// executed tasks; 0 for cache hits and no-adapter skips.
    #[serde(default)]
    pub attempts: u32,
    /// True when the task succeeded only after one or more retries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flaky: bool,
    /// Child-process stdout/stderr captured from tasks whose output
    /// *is* the result — deploy URLs, release identifiers, webhook
    /// response bodies. Populated for integration-sourced tasks
    /// (`integration_kind.is_some()`) that ran to `Built` or
    /// `Failed`, truncated to 4 KB from the tail (most CLIs put the
    /// useful bit at the end). `None` for adapter / user tasks
    /// where the output is already captured by the user's workflow
    /// (npm run build leaves files on disk; the stdout is noise).
    /// Agents read the URL from this field without re-running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_excerpt: Option<String>,
    /// Structured diagnostics extracted from the task's output. Populated
    /// only on `failed` tasks where the adapter declared a diagnostic
    /// hook AND parsing succeeded. Always omitted for `cache_hit`,
    /// `built`, and `skipped` outcomes. JSON Schema published via
    /// `bento schema diagnostics`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<crate::Diagnostic>,
}

/// Result of executing (or caching) a single task.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskOutcome {
    /// Cache hit — outputs restored into the dish dir.
    CacheHit,
    /// Ran the task and it succeeded. Outputs are cached.
    Built { exit_code: i32 },
    /// Ran the task and it failed. Nothing is cached.
    Failed {
        exit_code: i32,
        stderr_excerpt: String,
    },
    /// Dish had no adapter and no explicit tasks — nothing to run.
    Skipped { reason: String },
    /// Deploy-kind task whose inputs match the last successful deploy
    /// for `(env, dish, task)`. Nothing ran — the currently-deployed
    /// artefact is what we'd have shipped. Override with
    /// `bento deploy --force`.
    DeployUnchanged {
        /// RFC 3339 timestamp of the prior successful deploy.
        last_deployed_at: String,
        /// Deploy URL captured at prior-deploy time, when the
        /// integration surfaced one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deploy_url: Option<String>,
    },
}

// ── Options ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct CiOptions {
    /// Restrict to a bento by name.
    pub bento_filter: Option<String>,
    /// Restrict to a dish by name (matched against `DishConfig.name`, not path).
    pub dish_filter: Option<String>,
    /// Restrict adapter/user tasks to those named in this list
    /// (e.g. `["build"]`). Does not filter integration tasks —
    /// those are selected via [`Self::task_kind_filter`].
    /// `None` / empty `Some(vec![])` = include every adapter/user task.
    pub task_filter: Option<Vec<String>>,
    pub no_cache: bool,
    /// `None` → fall back to the workspace's `defaults.fail_fast`.
    pub fail_fast: Option<bool>,
    /// Skip the adapter install probe entirely (deps are assumed
    /// pre-populated — e.g. containerised CI).
    pub skip_install: bool,
    /// Run `adapter.install()` unconditionally, ignoring the probe.
    /// Useful when the probe can't see a subtle `node_modules`
    /// corruption that's still tripping builds.
    pub force_install: bool,
    /// Filter integration-sourced tasks to a specific kind
    /// (e.g. `Deploy` for `bento deploy`). When `Some(k)`, dishes
    /// with no integration task matching `k` are skipped entirely —
    /// `bento deploy` shouldn't build a dish that has nothing to
    /// deploy. When `None`, every integration task runs.
    pub task_kind_filter: Option<IntegrationTaskKind>,
    /// Short-circuit after the install step — used by `bento install`.
    /// Resolves adapters/integrations and runs `adapter.install()` on
    /// each dish (subject to probe / `force_install`), but skips the
    /// task loop entirely. The returned report has install records
    /// but no task rows.
    pub install_only: bool,
    /// Declared env-var name → source env-var name aliases. When an
    /// integration task's `required_env` declares `RAILWAY_TOKEN` and
    /// `secret_aliases["RAILWAY_TOKEN"]` = `"RAILWAY_TOKEN_STAGING"`,
    /// bento reads `$RAILWAY_TOKEN_STAGING` from the host env and
    /// exposes it to the task under the name `RAILWAY_TOKEN`.
    /// Declared names with no matching entry fall back to direct
    /// lookup, so existing flows keep working unchanged. Never holds
    /// secret *values* — only name-to-name indirection. Populated by
    /// `--env <name>` (config profile) and `--secret-from NAME=SRC`
    /// (ad-hoc) on `bento deploy` / `bento doctor`.
    pub secret_aliases: std::collections::BTreeMap<String, String>,
    /// Run Notify-kind integration tasks (garnishes) after Deploy
    /// tasks complete. Set by `bento deploy` (`true`) and `bento
    /// notify` (`true`); left `false` by `bento ci` so no-side-effect
    /// CI never fires webhooks.
    pub run_notify_kinds: bool,
    /// Value of `bento deploy --env <name>` — flows into each
    /// garnish payload's `environment` field so notify scripts can
    /// format staging vs prod differently without re-reading CLI args.
    /// `None` when `--env` wasn't passed.
    pub environment: Option<String>,
    /// `bento deploy --force`: disable the "skip when unchanged"
    /// short-circuit on Deploy / DeployPreview tasks. The state file
    /// is still updated after a successful forced deploy so
    /// subsequent non-force invocations go back to skipping.
    pub force_deploy: bool,
}

impl CiOptions {
    /// Resolve the declared env-var name to its host source name:
    /// the alias target if present, otherwise the declared name
    /// itself. Pure lookup — does not touch the process env.
    pub fn source_env_name<'a>(&'a self, declared: &'a str) -> &'a str {
        self.secret_aliases
            .get(declared)
            .map(|s| s.as_str())
            .unwrap_or(declared)
    }
}

impl CiOptions {
    fn task_allowed(&self, task: &ResolvedTask) -> bool {
        // Notify-kind tasks never run in the main sequential loop —
        // they're fan-out garnishes fired in Phase 2 off completed
        // Deploy tasks. Gated globally here so every code path
        // (default CI, `bento deploy`, `bento notify`) agrees.
        if task.integration_kind == Some(IntegrationTaskKind::Notify) {
            return false;
        }
        // Kind filter on integration tasks.
        if let Some(wanted) = self.task_kind_filter {
            if let Some(kind) = task.integration_kind {
                if kind != wanted {
                    return false;
                }
            }
            // Adapter/user tasks are preconditions (e.g. build before
            // deploy) — allow them through the kind filter.
        } else if let Some(kind) = task.integration_kind {
            // Default (no explicit kind filter): `bento ci` excludes
            // side-effectful integration kinds — prod deploys,
            // rollbacks, external notifications shouldn't run on
            // every `ci` invocation. `bento deploy` is the explicit
            // verb for those. Non-side-effectful integration tasks
            // (Release, Other) still run in CI.
            if kind.defaults_no_cache() {
                return false;
            }
        }
        // Name filter on adapter/user tasks. Integration tasks bypass
        // (they're gated by kind).
        if task.integration_kind.is_none() {
            if let Some(list) = &self.task_filter {
                if !list.is_empty() && !list.iter().any(|t| t == &task.name) {
                    return false;
                }
            }
        }
        true
    }

    /// True when a dish should be skipped because no task matches the
    /// `task_kind_filter`. Used by `bento deploy` so it only runs on
    /// dishes that actually have a deploy integration wired up.
    fn dish_skipped_by_kind(&self, resolved: &[ResolvedTask]) -> bool {
        let Some(wanted) = self.task_kind_filter else {
            return false;
        };
        !resolved.iter().any(|t| t.integration_kind == Some(wanted))
    }
}

// ── Executor ───────────────────────────────────────────────────────

pub struct Executor {
    workspace: Workspace,
    registry: AdapterRegistry,
    integrations: IntegrationRegistry,
    cache: LocalCache,
    /// Toolchain installer. Best-effort: if the host can't construct one
    /// (e.g. no `$HOME`), the layer silently falls back to system PATH.
    installer: Option<Installer>,
    /// Remote content cache (tier 3). On a local miss we HEAD/GET here
    /// before running the task; after a successful build we PUT the
    /// resulting bundle. Every operation is best-effort — a remote
    /// failure never fails the build.
    remote: Option<std::sync::Arc<dyn RemoteCache>>,
    /// Join handles for in-flight write-through uploads. Drained + joined
    /// at the end of [`Self::execute`] so the process doesn't exit
    /// mid-upload — a >few-MB bundle takes seconds to PUT and an
    /// all-cache-hits run exits in <1s otherwise. Interior-mutable because
    /// `write_through` is called through `&self` from cache-hit paths.
    pending_writes: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Per-workspace record of "what's already deployed" — the
    /// content-addressable keys of the last successful Deploy /
    /// DeployPreview task for each `(env, dish, task)` triple. Loaded
    /// once at executor construction. Interior-mutable because dishes
    /// run in parallel and each may write back on a successful deploy.
    deploy_state: std::sync::Mutex<crate::deploy_state::DeployState>,
}

impl Executor {
    pub fn new(workspace: Workspace, registry: AdapterRegistry, cache: LocalCache) -> Self {
        let remote = build_remote_from(&workspace);
        let deploy_state = load_deploy_state(&workspace);
        Self {
            workspace,
            registry,
            integrations: IntegrationRegistry::empty(),
            cache,
            installer: Installer::builtin().ok(),
            remote,
            pending_writes: std::sync::Mutex::new(Vec::new()),
            deploy_state: std::sync::Mutex::new(deploy_state),
        }
    }

    /// Construct an executor that does NOT attempt any toolchain installs.
    /// Used by tests so they don't try to download Go from the network.
    pub fn without_toolchain(
        workspace: Workspace,
        registry: AdapterRegistry,
        cache: LocalCache,
    ) -> Self {
        let remote = build_remote_from(&workspace);
        let deploy_state = load_deploy_state(&workspace);
        Self {
            workspace,
            registry,
            integrations: IntegrationRegistry::empty(),
            cache,
            installer: None,
            remote,
            pending_writes: std::sync::Mutex::new(Vec::new()),
            deploy_state: std::sync::Mutex::new(deploy_state),
        }
    }

    /// Attach (or replace) the integration registry. Executors default
    /// to an empty registry so existing callers continue to work; the
    /// CLI populates it via [`IntegrationRegistry::builtin`].
    pub fn with_integrations(mut self, integrations: IntegrationRegistry) -> Self {
        self.integrations = integrations;
        self
    }

    /// Attach (or replace) the remote cache tier. Useful for tests that
    /// spin up a local HTTP fake without threading a fake config in.
    ///
    /// Accepts any [`RemoteCache`] implementor (S3Remote, BearerRemote,
    /// or a custom test double).
    pub fn with_remote(mut self, remote: impl RemoteCache + 'static) -> Self {
        self.remote = Some(std::sync::Arc::new(remote));
        self
    }

    /// Opt-in remote-cache reconciliation on a local cache hit. HEADs
    /// the remote for the task's key; if missing, PUTs the local
    /// bundle. Runs on a detached thread so the hit path stays sub-
    /// millisecond; failures are logged and swallowed.
    ///
    /// Off by default (Turborepo / Nx convention: local hit short-
    /// circuits, no network touch). The MISS→BUILT path already PUTs
    /// to remote synchronously, so the steady-state distributed-cache
    /// story works without this — every first-build populates remote,
    /// every teammate pulls on their miss.
    ///
    /// Enable via `[cache] remote_write_through = true` for the one-
    /// time catch-up case: a populated local cache pre-dating the
    /// remote config, where we want the next run to lazily backfill
    /// remote. Also useful on CI runners with a stale shared local
    /// volume where remote should be authoritative.
    fn write_through(&self, key: &CacheKey) {
        if !self.workspace.repo.cache.remote_write_through {
            return;
        }
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        let bundle = self.cache.bundle_path(key);
        if !bundle.exists() {
            // Manifest sidecar without a bundle — nothing to push.
            return;
        }
        let remote = remote.clone();
        let key = key.clone();
        let handle = std::thread::spawn(move || {
            if remote.has(&key) {
                return;
            }
            if let Err(e) = remote.put(&key, &bundle) {
                tracing::debug!("write-through PUT skipped for {}: {e:#}", key.as_hex());
            } else {
                tracing::debug!("write-through pushed {}", key.as_hex());
            }
        });
        // Collect the handle so `drain_write_throughs` can join on the
        // way out. A poisoned mutex here means a previous caller panicked
        // while holding the lock — we still want to record the handle so
        // the upload gets joined before process exit. Best-effort; if the
        // lock truly can't be taken the handle is dropped, matching the
        // pre-fix fire-and-forget behaviour.
        if let Ok(mut pending) = self.pending_writes.lock() {
            pending.push(handle);
        }
    }

    /// Look up a prior successful deploy for `(env, dish, task)`.
    /// Returns `Some(record)` iff the record exists AND its stored
    /// `input_hash` matches `current_key_hex` — only a byte-for-byte
    /// hash match counts as "unchanged."
    ///
    /// Taking the lock under contention can't fail in practice (we
    /// never panic while holding it); on a poisoned lock we recover
    /// the inner value and carry on rather than propagating the
    /// poison into an actionable error the caller can't do anything
    /// useful with.
    fn deploy_state_hit(
        &self,
        env: Option<&str>,
        dish: &str,
        task: &str,
        current_key_hex: &str,
    ) -> Option<crate::deploy_state::DeployRecord> {
        let guard = match self.deploy_state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .get(env, dish, task)
            .filter(|r| r.input_hash == current_key_hex)
            .cloned()
    }

    /// Upsert the deploy-state record for `(env, dish, task)` with the
    /// current input hash + RFC 3339 timestamp + whatever output
    /// excerpt the integration surfaced. Persists to disk after the
    /// in-memory write. Failures to persist are logged — we already
    /// shipped, so the cost of missing the record is one redundant
    /// redeploy on the next run, not a correctness bug.
    fn record_deploy(
        &self,
        env: Option<&str>,
        dish: &str,
        task: &str,
        input_hash: String,
        deploy_url: Option<String>,
    ) {
        let mut guard = match self.deploy_state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.set(
            env,
            dish,
            task,
            crate::deploy_state::DeployRecord {
                input_hash,
                deployed_at: crate::deploy_state::now_rfc3339(),
                deploy_url,
            },
        );
        if let Err(e) = guard.save_to(self.workspace.root.as_path()) {
            tracing::warn!(
                dish = %dish,
                task = %task,
                error = %e,
                "failed to persist deploy state; next run may redeploy unchanged artefact"
            );
        }
    }

    /// Drain + join every in-flight write-through thread. Called at the
    /// end of [`Self::execute`] so the process doesn't exit mid-upload.
    /// Joins are sequential (they all block on I/O so parallel join is
    /// moot) and swallow panics (the worker thread already logged any
    /// failure via `tracing`).
    fn drain_write_throughs(&self) {
        let handles: Vec<_> = match self.pending_writes.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for h in handles {
            if let Err(e) = h.join() {
                tracing::debug!("write-through worker panicked: {e:?}");
            }
        }
    }

    pub fn execute(&self, opts: &CiOptions) -> Result<ExecutionReport> {
        let fail_fast = opts
            .fail_fast
            .unwrap_or(self.workspace.repo.defaults.fail_fast);
        let parallelism = self.workspace.repo.defaults.parallelism.max(1);

        let start = Instant::now();
        let mut report = ExecutionReport {
            bentos: Vec::new(),
            summary: ExecutionSummary::default(),
        };
        let mut stop = false;

        for bento_name in self.workspace.bentos.keys() {
            if opts.bento_filter.as_ref().is_some_and(|f| f != bento_name) {
                continue;
            }

            let graph = crate::graph::build(&self.workspace, bento_name)
                .with_context(|| format!("building dep graph for bento '{bento_name}'"))?;
            let dep_sigs = crate::cascade::compute(&self.workspace, &graph, &self.registry)
                .with_context(|| format!("computing dep signatures for bento '{bento_name}'"))?;

            let mut exec_bento = ExecutedBento {
                name: bento_name.clone(),
                dishes: Vec::new(),
            };

            'levels: for level in &graph.levels {
                // Filter: a --dish filter restricts us to one dish name.
                // Graph levels may be empty after filtering — that's fine,
                // nothing to run in this level but we still proceed.
                let targets: Vec<&LoadedDish> = level
                    .iter()
                    .filter_map(|name| self.workspace.dishes_by_name.get(name))
                    .filter(|loaded| match &opts.dish_filter {
                        None => true,
                        Some(f) => f == &loaded.config.name,
                    })
                    .collect();

                if targets.is_empty() {
                    continue;
                }

                // Execute every dish in this level concurrently, up to
                // `parallelism` at a time. Chunk-based batching is simple,
                // deterministic for logs, and gets ≈90% of the benefit of
                // a work-stealing scheduler for typical monorepos.
                for chunk in targets.chunks(parallelism) {
                    let dep_sigs_ref = &dep_sigs;
                    let bento_name_ref = bento_name.as_str();
                    let results = std::thread::scope(|scope| -> Vec<_> {
                        let handles: Vec<_> = chunk
                            .iter()
                            .map(|loaded| {
                                scope.spawn(move || {
                                    self.execute_dish(loaded, bento_name_ref, opts, dep_sigs_ref)
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| h.join().expect("dish worker panicked"))
                            .collect()
                    });

                    for result in results {
                        let (exec_dish, stats) = result?;
                        report.summary.dishes += 1;
                        report.summary.tasks += stats.tasks;
                        report.summary.hits += stats.hits;
                        report.summary.built += stats.built;
                        report.summary.failed += stats.failed;
                        report.summary.flaky += stats.flaky;
                        report.summary.installs += stats.installs;
                        report.summary.install_failures += stats.install_failures;
                        report.summary.notify_failures += stats.notify_failures;
                        report.summary.deploy_unchanged += stats.deploy_unchanged;

                        let had_failure = exec_dish
                            .tasks
                            .iter()
                            .any(|t| matches!(t.outcome, TaskOutcome::Failed { .. }));
                        exec_bento.dishes.push(exec_dish);

                        if had_failure && fail_fast {
                            stop = true;
                        }
                    }
                }

                if stop {
                    break 'levels;
                }
            }

            report.bentos.push(exec_bento);

            if stop {
                break;
            }
        }

        // Drain in-flight write-through uploads before returning so the
        // process doesn't exit mid-PUT. Bounded by the number of cache
        // hits in this run; each HEAD+PUT is network-latency-bound.
        self.drain_write_throughs();

        report.summary.duration_ms = start.elapsed().as_millis() as u64;
        Ok(report)
    }

    fn execute_dish(
        &self,
        loaded: &LoadedDish,
        bento_name: &str,
        opts: &CiOptions,
        dep_sigs: &std::collections::BTreeMap<String, crate::cascade::DishSig>,
    ) -> Result<(ExecutedDish, DishStats)> {
        let adapter = resolve_adapter(&self.registry, loaded);
        let integrations = resolve_integrations(&self.integrations, loaded);
        let language = adapter.map(|a| a.id().to_string());
        let resolved = resolve_tasks(&loaded.dir, &loaded.config, adapter, &integrations)
            .with_context(|| format!("dish '{}'", loaded.config.name))?;

        let mut exec_tasks = Vec::new();
        let mut stats = DishStats::default();

        if resolved.is_empty() {
            exec_tasks.push(ExecutedTask {
                name: "<none>".to_string(),
                run: String::new(),
                key: String::new(),
                duration_ms: 0,
                outcome: TaskOutcome::Skipped {
                    reason: "no adapter detected and no tasks declared".to_string(),
                },
                attempts: 0,
                flaky: false,
                output_excerpt: None,
                diagnostics: Vec::new(),
            });
            return Ok((
                ExecutedDish {
                    name: loaded.config.name.clone(),
                    path: loaded.rel.clone(),
                    language,
                    install: None,
                    tasks: exec_tasks,
                },
                stats,
            ));
        }

        // `bento deploy` / `--task-kind` filter: skip dishes with no
        // matching integration task. We emit a Skipped marker so the
        // dish still appears in the report — silently omitting would
        // hide which dishes we considered vs which were out of scope.
        if opts.dish_skipped_by_kind(&resolved) {
            let kind = opts
                .task_kind_filter
                .expect("dish_skipped_by_kind returned true so kind must be set");
            exec_tasks.push(ExecutedTask {
                name: format!("<no-{}>", kind.as_str()),
                run: String::new(),
                key: String::new(),
                duration_ms: 0,
                outcome: TaskOutcome::Skipped {
                    reason: format!("dish has no integration task of kind '{}'", kind.as_str()),
                },
                attempts: 0,
                flaky: false,
                output_excerpt: None,
                diagnostics: Vec::new(),
            });
            return Ok((
                ExecutedDish {
                    name: loaded.config.name.clone(),
                    path: loaded.rel.clone(),
                    language,
                    install: None,
                    tasks: exec_tasks,
                },
                stats,
            ));
        }

        // Resolve + install toolchain once per dish (it's the same for
        // every task in this dish). Errors in install bubble up as a
        // dish-level failure since we can't safely run any task.
        let toolchain_paths = self.ensure_toolchain(loaded, adapter)?;

        // Check whether the dish's deps are installed; if not, run
        // `adapter.install()` before any task executes. Tasks that cache-hit
        // wouldn't need deps to run, but the dish's dev workflow does —
        // and a cache miss on the first task will fail without them.
        let install_record = self.ensure_installed(loaded, adapter, &toolchain_paths, opts);
        if let Some(rec) = &install_record {
            stats.installs += 1;
            if rec.error.is_some() {
                stats.install_failures += 1;
                // Skip the task loop — deps aren't in place, so each task
                // run would fail with a useless "module not found" style
                // error. The install error itself is the actionable signal.
                return Ok((
                    ExecutedDish {
                        name: loaded.config.name.clone(),
                        path: loaded.rel.clone(),
                        language,
                        install: install_record,
                        tasks: exec_tasks,
                    },
                    stats,
                ));
            }
        }

        // `bento install` short-circuit: we're only here to install
        // deps, not run tasks. Return after the install step so the
        // report surfaces install records without any task rows.
        if opts.install_only {
            return Ok((
                ExecutedDish {
                    name: loaded.config.name.clone(),
                    path: loaded.rel.clone(),
                    language,
                    install: install_record,
                    tasks: exec_tasks,
                },
                stats,
            ));
        }

        // Fold the dish's dep signatures into every task key here;
        // compute_key's slice is consumed by hashing, so one lookup up
        // front beats one per task.
        let dep_mixins = crate::cascade::deps_for_key(&loaded.config, dep_sigs);

        let container_image = crate::plan::container_image_for_plan(&self.workspace);

        // Tracks computed keys within this dish so an integration task
        // with `depends_on = ["build"]` (railway:deploy, cloudflare
        // variants, …) can cascade-hash the build's key into its own.
        // Without this the skip-if-unchanged check for deploy tasks
        // keyed only on dish.inputs (empty by default) and collapsed
        // back-to-back runs into "DeployUnchanged" after the first
        // success, even with real source edits in-between.
        let mut computed_task_keys: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        for task in &resolved {
            if !opts.task_allowed(task) {
                continue;
            }

            // Integration tasks declare env vars + CLI binaries they
            // must see at run time. Fail fast with a clear error
            // instead of letting the underlying CLI emit a cryptic
            // 401 or letting the shell exit 127 for a missing binary.
            // Both checks sit before `compute_key` / `run_or_restore`
            // so we don't waste a cache lookup on a task we can't
            // actually run.
            let started = Instant::now();
            let preflight_error = missing_required_env(&task.required_env, &opts.secret_aliases)
                .map(|missing| format!("missing required env var(s): {}", missing.join(", ")))
                .or_else(|| {
                    missing_required_cli(&task.required_cli).map(|m| format_missing_cli(&m))
                });
            if let Some(stderr_excerpt) = preflight_error {
                exec_tasks.push(ExecutedTask {
                    name: task.name.clone(),
                    run: task.run.clone(),
                    key: String::new(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    outcome: TaskOutcome::Failed {
                        exit_code: -1,
                        stderr_excerpt,
                    },
                    attempts: 0,
                    flaky: false,
                    output_excerpt: None,
                    diagnostics: Vec::new(),
                });
                stats.tasks += 1;
                stats.failed += 1;
                if opts
                    .fail_fast
                    .unwrap_or(self.workspace.repo.defaults.fail_fast)
                {
                    break;
                }
                continue;
            }

            let task_dep_keys: Vec<(&str, &str)> = task
                .depends_on
                .iter()
                .filter_map(|dep| {
                    computed_task_keys
                        .get(dep)
                        .map(|k| (dep.as_str(), k.as_str()))
                })
                .collect();
            let (key, manifest) = compute_key(
                &loaded.dir,
                &loaded.config.name,
                adapter,
                task,
                &dep_mixins,
                container_image.as_deref(),
                &task_dep_keys,
            )?;
            computed_task_keys.insert(task.name.clone(), key.as_hex().to_string());

            // Skip-if-unchanged for Deploy / DeployPreview tasks.
            // Rollback tasks ignore this check — their whole purpose is
            // to move state *backwards*; the recorded input_hash would
            // match the previous forward deploy and mask them. Also
            // respect `--force`.
            let deploy_kind = matches!(
                task.integration_kind,
                Some(IntegrationTaskKind::Deploy) | Some(IntegrationTaskKind::DeployPreview)
            );
            let skip_record = if deploy_kind && !opts.force_deploy {
                self.deploy_state_hit(
                    opts.environment.as_deref(),
                    &loaded.config.name,
                    &task.name,
                    key.as_hex(),
                )
            } else {
                None
            };
            let attempt = match skip_record {
                Some(record) => TaskAttempt {
                    outcome: TaskOutcome::DeployUnchanged {
                        last_deployed_at: record.deployed_at,
                        deploy_url: record.deploy_url,
                    },
                    attempts: 0,
                    flaky: false,
                    output_excerpt: None,
                },
                None => {
                    self.run_or_restore(&key, &loaded.dir, task, &manifest, &toolchain_paths, opts)
                }
            };
            let duration_ms = started.elapsed().as_millis() as u64;

            stats.tasks += 1;
            match &attempt.outcome {
                TaskOutcome::CacheHit => stats.hits += 1,
                TaskOutcome::Built { .. } => {
                    stats.built += 1;
                    if attempt.flaky {
                        stats.flaky += 1;
                    }
                }
                TaskOutcome::Failed { .. } => stats.failed += 1,
                TaskOutcome::Skipped { .. } => {}
                TaskOutcome::DeployUnchanged { .. } => stats.deploy_unchanged += 1,
            }

            // Diagnostic re-run: only on failure, only when the adapter
            // declared a hook for this task. Strictly additive; failure
            // of the re-run leaves diagnostics empty and never blocks.
            let diagnostics = if matches!(attempt.outcome, TaskOutcome::Failed { .. }) {
                capture_diagnostics(
                    self.workspace.root.as_path(),
                    adapter,
                    task,
                    &loaded.dir,
                    &toolchain_paths,
                    container_plan(&self.workspace).as_ref(),
                    &opts.secret_aliases,
                )
            } else {
                Vec::new()
            };

            // Only surface the captured child output for tasks where
            // it's the actual *result* — integration-sourced deploys,
            // releases, notifications. For adapter/user tasks (build,
            // test, lint) the stdout is noise on success — the real
            // output is in dist/ / target/ / the test runner's own
            // reporter. But on FAILURE the combined stdout+stderr is
            // exactly the diagnostic the caller needs (tsc errors,
            // cargo compiler output, vitest failures). Surface it in
            // that case so `bento build` is a real replacement for
            // the native CLI instead of forcing a dogfood-break.
            let task_failed = matches!(attempt.outcome, TaskOutcome::Failed { .. });
            let output_excerpt = if task.integration_kind.is_some() || task_failed {
                attempt.output_excerpt
            } else {
                None
            };

            exec_tasks.push(ExecutedTask {
                name: task.name.clone(),
                run: task.run.clone(),
                key: key.as_hex().to_string(),
                duration_ms,
                outcome: attempt.outcome,
                attempts: attempt.attempts,
                flaky: attempt.flaky,
                output_excerpt,
                diagnostics,
            });

            // Record a successful deploy so the next invocation short-
            // circuits when inputs haven't changed. Runs on --force too
            // so forced deploys update the baseline the same way.
            if deploy_kind
                && matches!(
                    exec_tasks.last().unwrap().outcome,
                    TaskOutcome::Built { .. }
                )
            {
                self.record_deploy(
                    opts.environment.as_deref(),
                    &loaded.config.name,
                    &task.name,
                    key.as_hex().to_string(),
                    exec_tasks.last().unwrap().output_excerpt.clone(),
                );
            }

            if matches!(
                exec_tasks.last().unwrap().outcome,
                TaskOutcome::Failed { .. }
            ) && opts
                .fail_fast
                .unwrap_or(self.workspace.repo.defaults.fail_fast)
            {
                break; // inner fail_fast — caller handles cross-dish stop
            }
        }

        // After every completed Deploy, persist a sidecar with the
        // GarnishPayload so `bento notify` can replay the trigger
        // later. Always written — independent of `run_notify_kinds`,
        // so `bento deploy --no-notify` still leaves something for a
        // follow-up `bento notify` to fire from.
        let deploy_indices = exec_tasks_index_deploys(&resolved, &exec_tasks);
        for &idx in &deploy_indices {
            let payload = build_garnish_payload(
                &exec_tasks[idx],
                &loaded.config.name,
                bento_name,
                opts.environment.as_deref(),
            );
            if let Err(e) = write_garnish_sidecar(
                self.workspace.root.as_path(),
                bento_name,
                &loaded.config.name,
                &exec_tasks[idx].name,
                &payload,
            ) {
                // Don't fail the build over sidecar persistence; a
                // future `bento notify` just won't have this payload.
                tracing::warn!(
                    error = %e,
                    "failed to persist garnish sidecar for {}",
                    exec_tasks[idx].name
                );
            }
        }

        // Phase 2: Notify-kind fan-out (garnishes).
        // For each completed Deploy task, fan out every Notify-kind
        // task in this dish in parallel, piping a JSON payload on
        // stdin. Gated by `run_notify_kinds` so `bento ci` never
        // fires webhooks.
        if opts.run_notify_kinds {
            self.fan_out_garnishes(
                loaded,
                bento_name,
                &resolved,
                &deploy_indices,
                &mut exec_tasks,
                &mut stats,
                opts,
            );
        }

        Ok((
            ExecutedDish {
                name: loaded.config.name.clone(),
                path: loaded.rel.clone(),
                language,
                install: install_record,
                tasks: exec_tasks,
            },
            stats,
        ))
    }

    /// Phase 2 — fire every Notify-kind task once per completed Deploy.
    /// Serial across Deploys (reads stable per-deploy payload), parallel
    /// across Notify tasks within a Deploy (they're independent sinks).
    #[allow(clippy::too_many_arguments)]
    fn fan_out_garnishes(
        &self,
        loaded: &LoadedDish,
        bento_name: &str,
        resolved: &[ResolvedTask],
        deploy_indices: &[usize],
        exec_tasks: &mut Vec<ExecutedTask>,
        stats: &mut DishStats,
        opts: &CiOptions,
    ) {
        // Collect Notify ResolvedTasks up front so we don't reconstruct
        // per deploy. Clone so the borrow on `resolved` ends here — we
        // need a long-lived borrow below for `&ExecutedTask` reads.
        let notify_tasks: Vec<ResolvedTask> = resolved
            .iter()
            .filter(|t| t.integration_kind == Some(IntegrationTaskKind::Notify))
            .cloned()
            .collect();
        if notify_tasks.is_empty() || deploy_indices.is_empty() {
            return;
        }

        // Shared runtime context that all notify tasks read. Installer-
        // populated toolchain paths are only relevant to adapter-backed
        // tasks; notify scripts run via plain shell with host PATH.
        let toolchain_paths: Vec<PathBuf> = Vec::new();
        let container = container_plan(&self.workspace);

        for &deploy_idx in deploy_indices {
            // Snapshot the deploy's state; we'll build a payload from
            // it that every notify task in the inner fan-out sees.
            let payload = build_garnish_payload(
                &exec_tasks[deploy_idx],
                &loaded.config.name,
                bento_name,
                opts.environment.as_deref(),
            );
            let payload_line = payload.to_ndjson_line();

            // Parallel fan-out across Notify tasks. Each returns its
            // own ExecutedTask row; we collect serially afterward so
            // report ordering stays deterministic.
            let notify_results: Vec<ExecutedTask> = std::thread::scope(|scope| {
                let handles: Vec<_> = notify_tasks
                    .iter()
                    .map(|nt| {
                        let payload_line = payload_line.clone();
                        let toolchain_paths = &toolchain_paths;
                        let container = container.as_ref();
                        scope.spawn(move || {
                            run_notify_task(
                                nt,
                                &loaded.dir,
                                toolchain_paths,
                                container,
                                &opts.secret_aliases,
                                &payload_line,
                            )
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("notify worker panicked"))
                    .collect()
            });

            for et in notify_results {
                stats.tasks += 1;
                match &et.outcome {
                    TaskOutcome::Built { .. } => stats.built += 1,
                    TaskOutcome::Failed { .. } => {
                        // Notify failures log loudly so agents see them
                        // even when the process exits 0 (they don't
                        // contribute to `failed` / the exit code).
                        tracing::warn!(
                            task = %et.name,
                            dish = %loaded.config.name,
                            "notify task failed — Slack / Linear / webhook likely unreachable; not failing the build"
                        );
                        stats.notify_failures += 1;
                    }
                    TaskOutcome::CacheHit
                    | TaskOutcome::Skipped { .. }
                    | TaskOutcome::DeployUnchanged { .. } => {}
                }
                exec_tasks.push(et);
            }
        }
    }

    fn run_or_restore(
        &self,
        key: &CacheKey,
        dish_dir: &Path,
        task: &ResolvedTask,
        manifest: &InputManifest,
        toolchain_paths: &[PathBuf],
        opts: &CiOptions,
    ) -> TaskAttempt {
        // Tasks with `no_cache = true` (integration deploys, side-
        // effectful ops) always run. `opts.no_cache` only skips the
        // lookup tier — the task still runs and its outputs still
        // get stashed in the cache. `task.no_cache` suppresses both.
        let skip_cache_lookup = opts.no_cache || task.no_cache;

        let workspace_root = Some(self.workspace.root.as_path());

        if !skip_cache_lookup {
            // Tier 1: local cache.
            if self.cache.contains(key) {
                match self.cache.get(key, dish_dir, workspace_root) {
                    Ok(Some(_result)) => {
                        // Write-through: if a remote is configured, kick
                        // a fire-and-forget HEAD + PUT so teammates
                        // building the same key from a fresh clone get
                        // the bundle from remote instead of having to
                        // rebuild. Without this, only the machine that
                        // first experienced a cache-miss ever populates
                        // remote — distributed-team caching becomes a
                        // single-machine cache.
                        self.write_through(key);
                        return TaskAttempt::cache_hit();
                    }
                    Ok(None) => { /* fall through */ }
                    Err(e) => {
                        return TaskAttempt::failed_once(-1, format!("cache restore failed: {e}"));
                    }
                }
            }

            // Tier 3: remote cache (read-through). HEAD first so we
            // don't waste a GET on a miss — every additional request
            // is network latency inside a parallel executor.
            if let Some(remote) = &self.remote {
                if remote.has(key) {
                    let bundle = self.cache.bundle_path(key);
                    match remote.get(key, &bundle) {
                        Ok(true) => match self.cache.get(key, dish_dir, workspace_root) {
                            Ok(Some(_)) => return TaskAttempt::cache_hit(),
                            Ok(None) => {
                                tracing::warn!(
                                    "remote returned bundle but local extract found nothing — rebuilding"
                                );
                            }
                            Err(e) => {
                                tracing::warn!("remote-fetched bundle failed to extract: {e}");
                            }
                        },
                        Ok(false) => { /* remote said miss */ }
                        Err(e) => {
                            tracing::warn!("remote GET failed: {e:#} — falling through to rebuild");
                        }
                    }
                }
            }
        }

        // Try up to 1 + retry times. We only retry on genuine failure
        // (nonzero exit or exec error), not on cache restore issues.
        let max_attempts = task.retry.saturating_add(1);
        let mut last_exit_code = -1;
        let mut last_stderr = String::new();

        let container = container_plan(&self.workspace);

        let mut last_output: Option<String> = None;
        for attempt in 1..=max_attempts {
            match run_task(
                task,
                dish_dir,
                toolchain_paths,
                container.as_ref(),
                &opts.secret_aliases,
            ) {
                Ok(result) if result.exit_code == 0 => {
                    // Capture child output before the TaskResult is moved
                    // into the cache bundle — surfaced on `ExecutedTask`
                    // so deploy URLs / release IDs appear without needing
                    // `bento why`.
                    let output_excerpt = build_output_excerpt(&result.stdout, &result.stderr);
                    // `task.no_cache` — e.g. integration deploys —
                    // bypass cache.put and manifest persistence too.
                    // Caching a deploy would either never hit (unique
                    // per run) or cause correctness issues on hit.
                    if !task.no_cache {
                        let local_ok = match self.cache.put(
                            key,
                            dish_dir,
                            &task.outputs,
                            workspace_root,
                            &task.workspace_outputs,
                            &result,
                        ) {
                            Ok(()) => true,
                            Err(e) => {
                                tracing::warn!("cache.put failed: {e}");
                                false
                            }
                        };
                        if let Err(e) = self.cache.put_manifest(key, manifest) {
                            tracing::warn!("cache.put_manifest failed: {e}");
                        }
                        // Only push upstream when the local bundle is valid
                        // (otherwise we'd upload a 0-byte file).
                        if local_ok {
                            if let Some(remote) = &self.remote {
                                let bundle = self.cache.bundle_path(key);
                                if let Err(e) = remote.put(key, &bundle) {
                                    tracing::warn!("remote cache PUT failed (local kept): {e:#}");
                                }
                            }
                        }
                    }
                    return TaskAttempt {
                        outcome: TaskOutcome::Built { exit_code: 0 },
                        attempts: attempt,
                        flaky: attempt > 1,
                        output_excerpt,
                    };
                }
                Ok(result) => {
                    last_exit_code = result.exit_code;
                    last_stderr = truncated_stderr(&result.stderr);
                    last_output = build_output_excerpt(&result.stdout, &result.stderr);
                }
                Err(e) => {
                    last_exit_code = -1;
                    last_stderr = format!("exec error: {e}");
                    last_output = None;
                }
            }
            if attempt < max_attempts {
                tracing::warn!(
                    task = %task.name,
                    attempt,
                    of = max_attempts,
                    "task failed — retrying"
                );
            }
        }

        TaskAttempt {
            outcome: TaskOutcome::Failed {
                exit_code: last_exit_code,
                stderr_excerpt: last_stderr,
            },
            attempts: max_attempts,
            flaky: false,
            output_excerpt: last_output,
        }
    }

    /// Run `adapter.install()` when the dish's deps look incomplete.
    /// Returns `Some(InstallRecord)` if install was invoked (success or
    /// failure), `None` when the probe reported Ready / install was
    /// skipped / no adapter.
    fn ensure_installed(
        &self,
        loaded: &LoadedDish,
        adapter: Option<&dyn LanguageAdapter>,
        toolchain_paths: &[PathBuf],
        opts: &CiOptions,
    ) -> Option<InstallRecord> {
        if opts.skip_install {
            return None;
        }
        let adapter = adapter?;

        let reason = if opts.force_install {
            "--force-install".to_string()
        } else {
            match adapter.install_probe(&loaded.dir) {
                InstallProbe::Ready => {
                    // `bento install` is an explicit user action — run
                    // the adapter's install command even when the probe
                    // says Ready, so the same verb works uniformly
                    // across every language family. `bento ci`'s
                    // auto-trigger still respects the probe.
                    if opts.install_only {
                        "explicit bento install".to_string()
                    } else {
                        return None;
                    }
                }
                InstallProbe::Missing { reason } => reason,
            }
        };

        tracing::info!(
            dish = %loaded.config.name,
            adapter = %adapter.id(),
            %reason,
            "dependencies missing — running install"
        );

        let started = Instant::now();
        let ctx = TaskContext::new(&loaded.dir, &loaded.config.name)
            .with_toolchain_paths(toolchain_paths.to_vec());
        let result = adapter.install(&ctx);
        let duration_ms = started.elapsed().as_millis() as u64;

        let error = result.err().map(|e| format!("{e:#}"));
        if let Some(e) = &error {
            tracing::warn!(
                dish = %loaded.config.name,
                adapter = %adapter.id(),
                error = %e,
                "install failed — skipping tasks for this dish"
            );
        }

        Some(InstallRecord {
            reason,
            duration_ms,
            error,
        })
    }

    /// Resolve + install (best-effort) toolchains for `loaded`. Returns
    /// the directories that should be prepended to child-process `PATH`.
    ///
    /// Auto-install only triggers when an explicit pin is set (dish or
    /// repo `[toolchain]`). Adapter-detected versions (e.g. parsed from
    /// `go.mod`) are *not* auto-installed — they only feed the cache key.
    fn ensure_toolchain(
        &self,
        loaded: &LoadedDish,
        adapter: Option<&dyn LanguageAdapter>,
    ) -> Result<Vec<PathBuf>> {
        let installer = match &self.installer {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        let adapter = match adapter {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };

        let resolution =
            match Resolver::resolve(&loaded.dir, &loaded.config, &self.workspace.repo, adapter)? {
                Some(r) => r,
                None => return Ok(Vec::new()),
            };

        // Only auto-install when explicitly pinned. Adapter-detected
        // versions stay system-resolved so opting in is intentional.
        if !matches!(
            resolution.source,
            ResolutionSource::Dish | ResolutionSource::Repo
        ) {
            return Ok(Vec::new());
        }
        let version = match resolution.version.as_ref() {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        let target = match Target::current() {
            Some(t) => t,
            None => {
                tracing::warn!("unsupported host target — skipping toolchain install");
                return Ok(Vec::new());
            }
        };

        // Tools without a built-in installer (today: bun, deno) reach
        // here when they're pinned in `[toolchain]`. The action wrapper
        // installs them out-of-band (curl bun.sh/install) and puts
        // them on the runner's PATH; we just trust that and don't
        // shell out to `installer.ensure` (which would error with
        // "no built-in tool registered"). Same posture as install_all,
        // which reports them as `skipped` instead of failing.
        let Some(primary) = installer.tool(&resolution.tool) else {
            tracing::info!(
                tool = %resolution.tool,
                version = %version,
                "no built-in installer; trusting system PATH",
            );
            return Ok(Vec::new());
        };

        // Co-required tools install first so the primary's
        // `delegated_ensure` (e.g. python → uv) finds its sibling on
        // PATH. Their bin dirs are also returned to the caller so the
        // task subprocess sees them — a python task running `uv sync`
        // needs `uv` discoverable just like it needs `python`. Each
        // co-tool's bin dir is prepended to *this* process's PATH the
        // moment it's installed so the very next `installer.ensure`
        // for the primary picks it up via `Command::new(...)`.
        let mut paths: Vec<PathBuf> = Vec::new();
        for co in primary.co_required() {
            let co_version = self
                .workspace
                .repo
                .toolchain
                .pins
                .get(co.tool)
                .cloned()
                .unwrap_or_else(|| co.default_version.to_string());
            let co_bin = installer
                .ensure(co.tool, &co_version, target)
                .with_context(|| {
                    format!(
                        "installing co-required toolchain {}@{} (for {})",
                        co.tool, co_version, resolution.tool
                    )
                })?;
            prepend_path_env(&co_bin);
            paths.push(co_bin);
        }

        let bin_dir = installer
            .ensure(&resolution.tool, version, target)
            .with_context(|| {
                format!(
                    "installing toolchain {}@{} (pinned in {})",
                    resolution.tool,
                    version,
                    resolution.source.label()
                )
            })?;
        paths.push(bin_dir);
        Ok(paths)
    }

    /// `bento notify` entry point — replays persisted garnish payloads
    /// through each dish's Notify-kind tasks. No Deploy runs. Payloads
    /// come from `.bento/garnish/<bento>/<dish>/*.json` sidecars that
    /// prior `bento deploy` invocations wrote. Dishes with no sidecars
    /// emit a `Skipped` marker with a clear "run `bento deploy` first"
    /// message so agents / humans know why nothing fired.
    pub fn notify_only(&self, opts: &CiOptions) -> Result<ExecutionReport> {
        let start = Instant::now();
        let mut report = ExecutionReport {
            bentos: Vec::new(),
            summary: ExecutionSummary::default(),
        };

        for bento_name in self.workspace.bentos.keys() {
            if opts.bento_filter.as_ref().is_some_and(|f| f != bento_name) {
                continue;
            }
            let bento = &self.workspace.bentos[bento_name];
            let mut exec_bento = ExecutedBento {
                name: bento_name.clone(),
                dishes: Vec::new(),
            };

            for dish_ref in &bento.config.dishes {
                let loaded = match self.workspace.dishes_by_path.get(Path::new(dish_ref)) {
                    Some(l) => l,
                    None => continue,
                };
                if opts
                    .dish_filter
                    .as_ref()
                    .is_some_and(|f| f != &loaded.config.name)
                {
                    continue;
                }

                let (exec_dish, stats) =
                    self.notify_only_dish(loaded, bento_name.as_str(), opts)?;
                report.summary.dishes += 1;
                report.summary.tasks += stats.tasks;
                report.summary.built += stats.built;
                report.summary.failed += stats.failed;
                report.summary.notify_failures += stats.notify_failures;
                exec_bento.dishes.push(exec_dish);
            }

            report.bentos.push(exec_bento);
        }

        report.summary.duration_ms = start.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// Replay persisted garnish payloads for a single dish through
    /// its Notify-kind tasks. Sibling to `execute_dish` but skips
    /// toolchain resolution, install probe, and Phase 1 entirely.
    fn notify_only_dish(
        &self,
        loaded: &LoadedDish,
        bento_name: &str,
        opts: &CiOptions,
    ) -> Result<(ExecutedDish, DishStats)> {
        let adapter = resolve_adapter(&self.registry, loaded);
        let integrations = resolve_integrations(&self.integrations, loaded);
        let language = adapter.map(|a| a.id().to_string());
        let resolved = resolve_tasks(&loaded.dir, &loaded.config, adapter, &integrations)
            .with_context(|| format!("dish '{}'", loaded.config.name))?;

        let notify_tasks: Vec<ResolvedTask> = resolved
            .iter()
            .filter(|t| t.integration_kind == Some(IntegrationTaskKind::Notify))
            .cloned()
            .collect();

        let mut stats = DishStats::default();
        let mut exec_tasks = Vec::new();

        if notify_tasks.is_empty() {
            exec_tasks.push(ExecutedTask {
                name: "<no-notify>".into(),
                run: String::new(),
                key: String::new(),
                duration_ms: 0,
                outcome: TaskOutcome::Skipped {
                    reason: "dish has no Notify-kind integration task".into(),
                },
                attempts: 0,
                flaky: false,
                output_excerpt: None,
                diagnostics: Vec::new(),
            });
            return Ok((
                ExecutedDish {
                    name: loaded.config.name.clone(),
                    path: loaded.rel.clone(),
                    language,
                    install: None,
                    tasks: exec_tasks,
                },
                stats,
            ));
        }

        let sidecars = read_garnish_sidecars(
            self.workspace.root.as_path(),
            bento_name,
            &loaded.config.name,
        );
        if sidecars.is_empty() {
            exec_tasks.push(ExecutedTask {
                name: "<no-prior-deploy>".into(),
                run: String::new(),
                key: String::new(),
                duration_ms: 0,
                outcome: TaskOutcome::Skipped {
                    reason: format!(
                        "no prior deploy cached for dish '{}' — run `bento deploy` first",
                        loaded.config.name
                    ),
                },
                attempts: 0,
                flaky: false,
                output_excerpt: None,
                diagnostics: Vec::new(),
            });
            return Ok((
                ExecutedDish {
                    name: loaded.config.name.clone(),
                    path: loaded.rel.clone(),
                    language,
                    install: None,
                    tasks: exec_tasks,
                },
                stats,
            ));
        }

        let toolchain_paths: Vec<PathBuf> = Vec::new();
        let container = container_plan(&self.workspace);

        for (_trigger_name, payload) in sidecars {
            let payload_line = payload.to_ndjson_line();
            let notify_results: Vec<ExecutedTask> = std::thread::scope(|scope| {
                let handles: Vec<_> = notify_tasks
                    .iter()
                    .map(|nt| {
                        let payload_line = payload_line.clone();
                        let toolchain_paths = &toolchain_paths;
                        let container = container.as_ref();
                        scope.spawn(move || {
                            run_notify_task(
                                nt,
                                &loaded.dir,
                                toolchain_paths,
                                container,
                                &opts.secret_aliases,
                                &payload_line,
                            )
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("notify worker panicked"))
                    .collect()
            });

            for et in notify_results {
                stats.tasks += 1;
                match &et.outcome {
                    TaskOutcome::Built { .. } => stats.built += 1,
                    TaskOutcome::Failed { .. } => {
                        tracing::warn!(
                            task = %et.name,
                            dish = %loaded.config.name,
                            "notify task failed — Slack / Linear / webhook likely unreachable; not failing the build"
                        );
                        stats.notify_failures += 1;
                    }
                    _ => {}
                }
                exec_tasks.push(et);
            }
        }

        Ok((
            ExecutedDish {
                name: loaded.config.name.clone(),
                path: loaded.rel.clone(),
                language,
                install: None,
                tasks: exec_tasks,
            },
            stats,
        ))
    }
}

/// Per-dish counters produced by `execute_dish` and folded back into
/// [`ExecutionSummary`] by the outer `execute` loop. Splitting this out
/// lets us execute dishes in parallel without sharing the summary across
/// threads.
#[derive(Debug, Clone, Copy, Default)]
struct DishStats {
    tasks: usize,
    hits: usize,
    built: usize,
    failed: usize,
    flaky: usize,
    installs: usize,
    install_failures: usize,
    notify_failures: usize,
    deploy_unchanged: usize,
}

/// Return the indices into `exec_tasks` whose entry corresponds to a
/// Deploy-kind `ResolvedTask` (matched by `name`). Used by the Notify
/// fan-out to pick the trigger rows. Matching by name is safe because
/// task names are unique within a dish (enforced by `resolve_tasks`).
fn exec_tasks_index_deploys(resolved: &[ResolvedTask], exec_tasks: &[ExecutedTask]) -> Vec<usize> {
    let deploy_names: std::collections::BTreeSet<&str> = resolved
        .iter()
        .filter(|t| t.integration_kind == Some(IntegrationTaskKind::Deploy))
        .map(|t| t.name.as_str())
        .collect();
    exec_tasks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            if deploy_names.contains(t.name.as_str()) {
                Some(i)
            } else {
                None
            }
        })
        .collect()
}

/// Build a [`GarnishPayload`] describing a completed Deploy. Called
/// once per Deploy trigger; the resulting JSON is piped on stdin to
/// every Notify task fired off that trigger.
fn build_garnish_payload(
    deploy: &ExecutedTask,
    dish_name: &str,
    bento_name: &str,
    environment: Option<&str>,
) -> crate::GarnishPayload {
    let (outcome, exit_code, stderr_excerpt) = match &deploy.outcome {
        TaskOutcome::Built { exit_code } => ("built", *exit_code, None),
        TaskOutcome::CacheHit => ("cache_hit", 0, None),
        TaskOutcome::Failed {
            exit_code,
            stderr_excerpt,
        } => (
            "failed",
            *exit_code,
            Some(stderr_excerpt.clone()).filter(|s| !s.is_empty()),
        ),
        TaskOutcome::Skipped { .. } => ("skipped", -1, None),
        // Deploy was a no-op (inputs match the last deploy on record).
        // Garnishes only fire for actual state changes — there's
        // nothing new to announce when nothing shipped.
        TaskOutcome::DeployUnchanged { .. } => ("deploy_unchanged", 0, None),
    };
    crate::GarnishPayload {
        schema_version: crate::GARNISH_PAYLOAD_SCHEMA_VERSION,
        bento_version: env!("CARGO_PKG_VERSION").to_string(),
        environment: environment.map(str::to_string),
        trigger: crate::GarnishPayloadTrigger {
            task_name: deploy.name.clone(),
            dish_name: dish_name.to_string(),
            bento_name: bento_name.to_string(),
            outcome: outcome.to_string(),
            exit_code,
            duration_ms: deploy.duration_ms,
            cache_key: deploy.key.clone(),
            integration_kind: IntegrationTaskKind::Deploy.as_str().to_string(),
            output_excerpt: deploy.output_excerpt.clone().unwrap_or_default(),
            stderr_excerpt,
        },
    }
}

/// Sidecar directory holding one garnish payload per recent Deploy.
/// Lives under the workspace root's `.bento/` (already gitignored);
/// survives across `bento deploy` / `bento notify` invocations on the
/// same host so the notify verb has something to replay.
fn garnish_sidecar_dir(workspace_root: &Path, bento_name: &str, dish_name: &str) -> PathBuf {
    workspace_root
        .join(".bento")
        .join("garnish")
        .join(sanitize_component(bento_name))
        .join(sanitize_component(dish_name))
}

/// Full path of the sidecar for `task_name` under the given bento/dish.
fn garnish_sidecar_path(
    workspace_root: &Path,
    bento_name: &str,
    dish_name: &str,
    task_name: &str,
) -> PathBuf {
    let mut file = sanitize_component(task_name);
    file.push_str(".json");
    garnish_sidecar_dir(workspace_root, bento_name, dish_name).join(file)
}

/// Replace any non-alphanumeric character (other than `-` or `_`) with
/// `_`. Keeps filenames safe on every supported host (colons in task
/// names like `railway:deploy` would break Windows fs).
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Serialise `payload` to its sidecar path. Creates parent dirs.
fn write_garnish_sidecar(
    workspace_root: &Path,
    bento_name: &str,
    dish_name: &str,
    task_name: &str,
    payload: &crate::GarnishPayload,
) -> Result<()> {
    let path = garnish_sidecar_path(workspace_root, bento_name, dish_name, task_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating garnish sidecar dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(payload)?;
    std::fs::write(&path, json)
        .with_context(|| format!("writing garnish sidecar {}", path.display()))?;
    Ok(())
}

/// Load every garnish sidecar under the given bento/dish. Returns
/// `(task_name, payload)` pairs. Unparseable sidecars are logged and
/// skipped — never fatal. Empty Vec when no sidecars exist.
fn read_garnish_sidecars(
    workspace_root: &Path,
    bento_name: &str,
    dish_name: &str,
) -> Vec<(String, crate::GarnishPayload)> {
    let dir = garnish_sidecar_dir(workspace_root, bento_name, dish_name);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "read garnish sidecar failed");
                continue;
            }
        };
        let payload: crate::GarnishPayload = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "parse garnish sidecar failed");
                continue;
            }
        };
        out.push((payload.trigger.task_name.clone(), payload));
    }
    // Deterministic order (sidecars read back alphabetically by task
    // name) keeps reports / logs stable across runs.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Run a single Notify task with the garnish payload piped on stdin.
/// Mirrors `run_task`'s shape (preflight, env allowlist, container /
/// bare-shell split) but writes the payload to the child's stdin and
/// captures stdout/stderr into an [`ExecutedTask`] row. Notify tasks
/// do not consult the cache — each garnish is side-effectful by
/// definition.
fn run_notify_task(
    task: &ResolvedTask,
    dish_dir: &Path,
    toolchain_paths: &[PathBuf],
    container: Option<&ContainerPlan>,
    aliases: &std::collections::BTreeMap<String, String>,
    payload_line: &str,
) -> ExecutedTask {
    use std::io::Write;
    use std::process::Stdio;

    let started = Instant::now();

    // Preflight: required env + required CLI, same shape as Phase 1.
    // A webhook URL env var being absent is a common Slack-integration
    // misconfiguration — catch it with a clear error instead of letting
    // `curl` fail 3 lines of diagnostics deep.
    let preflight_error = missing_required_env(&task.required_env, aliases)
        .map(|missing| format!("missing required env var(s): {}", missing.join(", ")))
        .or_else(|| missing_required_cli(&task.required_cli).map(|m| format_missing_cli(&m)));
    if let Some(stderr_excerpt) = preflight_error {
        return ExecutedTask {
            name: task.name.clone(),
            run: task.run.clone(),
            key: String::new(),
            duration_ms: started.elapsed().as_millis() as u64,
            outcome: TaskOutcome::Failed {
                exit_code: -1,
                stderr_excerpt,
            },
            attempts: 0,
            flaky: false,
            output_excerpt: None,
            diagnostics: Vec::new(),
        };
    }

    let mut cmd = match container {
        Some(plan) => build_container_command(plan, task, dish_dir, aliases),
        None => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&task.run).current_dir(dish_dir);
            if !toolchain_paths.is_empty() {
                c.env("PATH", build_path(toolchain_paths));
            }
            for declared in &task.env {
                let source = aliases
                    .get(declared)
                    .map(|s| s.as_str())
                    .unwrap_or(declared);
                if let Ok(value) = std::env::var(source) {
                    c.env(declared, value);
                }
            }
            c
        }
    };

    // Pipe the payload on stdin; capture stdout/stderr so agents can
    // see Slack's response body or curl's exit message via the report.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecutedTask {
                name: task.name.clone(),
                run: task.run.clone(),
                key: String::new(),
                duration_ms: started.elapsed().as_millis() as u64,
                outcome: TaskOutcome::Failed {
                    exit_code: -1,
                    stderr_excerpt: format!("failed to spawn notify task: {e}"),
                },
                attempts: 1,
                flaky: false,
                output_excerpt: None,
                diagnostics: Vec::new(),
            };
        }
    };

    // Write the payload + close stdin before waiting so short-lived
    // notify scripts (e.g. `jq` pipelines) don't deadlock on a held
    // pipe. Write errors surface as stderr_excerpt rather than
    // aborting: the child may still emit a useful message.
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(payload_line.as_bytes()) {
            tracing::warn!(task = %task.name, error = %e, "notify stdin write failed");
        }
        // Explicit drop closes the pipe; keeps intent obvious.
        drop(stdin);
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return ExecutedTask {
                name: task.name.clone(),
                run: task.run.clone(),
                key: String::new(),
                duration_ms: started.elapsed().as_millis() as u64,
                outcome: TaskOutcome::Failed {
                    exit_code: -1,
                    stderr_excerpt: format!("notify task wait failed: {e}"),
                },
                attempts: 1,
                flaky: false,
                output_excerpt: None,
                diagnostics: Vec::new(),
            };
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let output_excerpt = build_output_excerpt(&output.stdout, &output.stderr);
    let outcome = if output.status.success() {
        TaskOutcome::Built { exit_code }
    } else {
        TaskOutcome::Failed {
            exit_code,
            stderr_excerpt: truncated_stderr(&output.stderr),
        }
    };

    ExecutedTask {
        name: task.name.clone(),
        run: task.run.clone(),
        key: String::new(),
        duration_ms: started.elapsed().as_millis() as u64,
        outcome,
        attempts: 1,
        flaky: false,
        output_excerpt,
        diagnostics: Vec::new(),
    }
}

/// Build the right remote-cache backend from the workspace's `[cache]`
/// block, if `remote` is set. Returning `None` is the "no remote
/// configured" case — the executor silently skips all remote calls.
///
/// Dispatches on URL scheme: `s3://…` → S3Remote (AWS-signed object store),
/// `bento://…` → BearerRemote (JWT-auth'd HTTP cache). Token for the
/// bento:// path comes from [`bento_cache::token::resolve_cache_token`]:
/// env var first (CI / explicit override), then keychain, then
/// `~/.bento/credentials`. `remote_token_env` names the env var
/// (default: `BENTO_CACHE_TOKEN`).
fn build_remote_from(workspace: &Workspace) -> Option<std::sync::Arc<dyn RemoteCache>> {
    let cache_cfg = &workspace.repo.cache;
    let url = cache_cfg.remote.as_deref()?;
    let region = cache_cfg.remote_region.as_deref();
    let endpoint = cache_cfg.remote_endpoint.as_deref();
    let token_env = cache_cfg
        .remote_token_env
        .as_deref()
        .unwrap_or("BENTO_CACHE_TOKEN");
    let token = bento_cache::token::resolve_cache_token(token_env);
    match bento_cache::build_remote(url, region, endpoint, token.as_deref()) {
        Ok(r) => Some(std::sync::Arc::from(r)),
        Err(e) => {
            tracing::warn!(
                "failed to build remote cache client ({url}): {e:#} — disabling remote tier"
            );
            None
        }
    }
}

/// Load deploy-state at executor construction, tolerating absence or
/// parse failure. A corrupt state file is treated as "no prior state"
/// with a warning — we'd rather ship one redundant deploy than panic
/// the executor over a file we can rewrite anyway.
fn load_deploy_state(workspace: &Workspace) -> crate::deploy_state::DeployState {
    match crate::deploy_state::DeployState::load_from(workspace.root.as_path()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load deploy state; treating as empty"
            );
            crate::deploy_state::DeployState::default()
        }
    }
}

/// Internal result of `run_or_restore` before we fold it back into
/// `ExecutedTask`. Keeps attempts/flaky bookkeeping together with the
/// outcome + captured child I/O so callers don't juggle tuples.
struct TaskAttempt {
    outcome: TaskOutcome,
    attempts: u32,
    flaky: bool,
    /// Combined stdout+stderr excerpt from the child process when
    /// the task actually ran (Built / Failed). `None` for cache hits
    /// — agents can `bento why <hash>` for the cached version.
    /// Truncated from the tail at [`OUTPUT_EXCERPT_LIMIT`].
    output_excerpt: Option<String>,
}

impl TaskAttempt {
    fn cache_hit() -> Self {
        Self {
            outcome: TaskOutcome::CacheHit,
            attempts: 0,
            flaky: false,
            output_excerpt: None,
        }
    }

    fn failed_once(exit_code: i32, msg: String) -> Self {
        Self {
            outcome: TaskOutcome::Failed {
                exit_code,
                stderr_excerpt: msg,
            },
            attempts: 1,
            flaky: false,
            output_excerpt: None,
        }
    }
}

/// Cap for `ExecutedTask.output_excerpt`. `railway up` / `vercel
/// deploy` / similar CLIs typically print the build-log URL near the
/// tail of their output (after progress chatter), so we tail-truncate
/// rather than head-truncate — the useful signal is always the last
/// thing printed.
const OUTPUT_EXCERPT_LIMIT: usize = 4 * 1024;

/// Combine stdout + stderr into a single excerpt suitable for the
/// report. Stderr is prefixed so the boundary is clear on inspection;
/// tails are kept when the combined length exceeds the cap.
fn build_output_excerpt(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);
    let combined = match (out.is_empty(), err.is_empty()) {
        (true, true) => return None,
        (false, true) => out.into_owned(),
        (true, false) => format!("[stderr]\n{err}"),
        (false, false) => format!("{out}[stderr]\n{err}"),
    };
    Some(tail_truncate(&combined, OUTPUT_EXCERPT_LIMIT))
}

fn tail_truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    // Slice at a char boundary to avoid panicking on multi-byte UTF-8.
    let start = s.len() - limit;
    let mut boundary = start;
    while !s.is_char_boundary(boundary) && boundary < s.len() {
        boundary += 1;
    }
    format!("… [{} bytes truncated]\n{}", start, &s[boundary..])
}

/// Re-run a failed task with the adapter's diagnostic hook applied,
/// then dispatch the captured output to the parser registry. Returns
/// an empty `Vec` whenever anything goes wrong (no adapter, no hook
/// for this task, re-run errored, parser found nothing) — diagnostic
/// capture is strictly additive and must never affect the build's
/// exit status or the original task's reported outcome.
fn capture_diagnostics(
    workspace_root: &Path,
    adapter: Option<&dyn LanguageAdapter>,
    task: &ResolvedTask,
    dish_dir: &Path,
    toolchain_paths: &[PathBuf],
    container: Option<&ContainerPlan>,
    aliases: &std::collections::BTreeMap<String, String>,
) -> Vec<crate::Diagnostic> {
    use bento_adapters::DiagnosticRerun;

    let Some(adapter) = adapter else {
        return Vec::new();
    };
    let Some(hook) = adapter.diagnostic_hook(&task.name) else {
        return Vec::new();
    };

    let modified_run = match &hook.rerun {
        DiagnosticRerun::AppendArgs(args) => {
            if args.is_empty() {
                task.run.clone()
            } else {
                format!("{} {}", task.run, args.join(" "))
            }
        }
        DiagnosticRerun::Replace(cmd) => cmd.clone(),
    };

    let modified = ResolvedTask {
        name: task.name.clone(),
        run: modified_run,
        inputs: task.inputs.clone(),
        outputs: task.outputs.clone(),
        workspace_outputs: task.workspace_outputs.clone(),
        env: task.env.clone(),
        retry: 0,
        no_cache: task.no_cache,
        required_env: task.required_env.clone(),
        required_cli: task.required_cli.clone(),
        integration_kind: task.integration_kind,
        depends_on: task.depends_on.clone(),
    };

    let result = match run_task(&modified, dish_dir, toolchain_paths, container, aliases) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                task = %task.name,
                error = %e,
                "diagnostic re-run failed; no diagnostics captured"
            );
            return Vec::new();
        }
    };

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    match hook.parser {
        bento_adapters::DiagnosticParser::Builtin(parser_id) => {
            crate::diagnostic_parsers::parse(parser_id, &stdout, &stderr, dish_dir, workspace_root)
        }
        bento_adapters::DiagnosticParser::Plugin => {
            // Subprocess plugin owns the parsing; bento-core just
            // hands the captured output back via the trait method.
            // Built-in adapters that nominally declare Plugin parser
            // inherit the trait default (empty Vec), which is fine.
            adapter.parse_diagnostics(&task.name, &stdout, &stderr, dish_dir, workspace_root)
        }
    }
}

fn run_task(
    task: &ResolvedTask,
    dish_dir: &Path,
    toolchain_paths: &[PathBuf],
    container: Option<&ContainerPlan>,
    aliases: &std::collections::BTreeMap<String, String>,
) -> Result<TaskResult> {
    let mut cmd = match container {
        Some(plan) => build_container_command(plan, task, dish_dir, aliases),
        None => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&task.run).current_dir(dish_dir);
            if !toolchain_paths.is_empty() {
                c.env("PATH", build_path(toolchain_paths));
            }
            // Pass-through env allowlist. Declared names may alias to
            // a different host var (e.g. `RAILWAY_TOKEN` ← reads
            // `$RAILWAY_TOKEN_STAGING`); in that case we read from
            // the source and export under the declared name so the
            // child sees the name the task's `run` expects.
            for declared in &task.env {
                let source = aliases
                    .get(declared)
                    .map(|s| s.as_str())
                    .unwrap_or(declared);
                if let Ok(value) = std::env::var(source) {
                    c.env(declared, value);
                }
            }
            c
        }
    };

    let output = cmd.output().with_context(|| {
        format!(
            "running `{}` for task '{}' in {}",
            task.run,
            task.name,
            dish_dir.display()
        )
    })?;

    Ok(TaskResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Settled plan for running a task inside a container. Captures the
/// chosen runtime (`docker` / `podman` / `nerdctl`) and the image
/// reference so cache keys and error messages can cite specifics.
#[derive(Debug, Clone)]
struct ContainerPlan {
    runtime: &'static str,
    image: String,
}

fn container_plan(workspace: &Workspace) -> Option<ContainerPlan> {
    use bento_config::ContainerMode;
    let exec = &workspace.repo.execution;
    let image = exec.image.clone();
    let want_container = match exec.container {
        ContainerMode::Never => false,
        ContainerMode::Always => true,
        // `auto` = container when an image is declared AND a runtime is
        // reachable. Both conditions so a stray image in a shared config
        // doesn't accidentally containerise on dev laptops without docker.
        ContainerMode::Auto => image.is_some() && detect_runtime().is_some(),
    };
    if !want_container {
        return None;
    }
    let runtime = match detect_runtime() {
        Some(r) => r,
        None => {
            tracing::error!(
                "container execution requested but no runtime on PATH (tried: docker, podman, nerdctl) — falling back to native"
            );
            return None;
        }
    };
    let image = match image {
        Some(i) => i,
        None => {
            tracing::error!(
                "container execution requested but no `[execution] image` set in bento.toml — falling back to native"
            );
            return None;
        }
    };
    Some(ContainerPlan { runtime, image })
}

fn detect_runtime() -> Option<&'static str> {
    ["docker", "podman", "nerdctl"]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
}

fn build_container_command(
    plan: &ContainerPlan,
    task: &ResolvedTask,
    dish_dir: &Path,
    aliases: &std::collections::BTreeMap<String, String>,
) -> Command {
    let mut cmd = Command::new(plan.runtime);
    cmd.arg("run")
        .arg("--rm")
        .arg("-i")
        // Run as the invoking UID so files written inside the container
        // keep host-side permissions; essential for caching outputs.
        .arg("--user")
        .arg(current_uid_gid())
        .arg("--volume")
        .arg(format!("{}:/work", dish_dir.display()))
        .arg("--workdir")
        .arg("/work")
        // Default HOME inside the container to the mount. Without this,
        // tools that default their cache dir to `$HOME/.cache/<tool>` —
        // Go (GOCACHE), Cargo (CARGO_HOME/.cargo), pnpm, npm — try to
        // write under `/root/.cache` (or `/.cache` when HOME is unset),
        // which `--user <host-uid>` can't touch. Pointing HOME at the
        // volume mount means the cache lives under the dish's own
        // writable scratch space and survives between invocations on
        // the same host. If a task's env allowlist explicitly includes
        // HOME, the forwarded host value wins (docker `--env` applies
        // last-write per variable), so users who genuinely need a
        // different HOME can still get one via `[tasks.<name>] env`.
        .arg("--env")
        .arg("HOME=/work");

    // Forward the task's declared env allowlist. Aliased names read
    // from their source host var (e.g. RAILWAY_TOKEN_STAGING) and are
    // re-exposed to the container under the declared name
    // (RAILWAY_TOKEN) via the two-step pattern:
    //   1. `cmd.env(declared, value)` → sets it on the container
    //       runtime's own process env (not its cmdline — no ps leak).
    //   2. `--env declared` → tells docker/podman to forward that
    //       env var into the container.
    // This matches the bare-shell path's semantics exactly.
    for declared in &task.env {
        let source = aliases
            .get(declared)
            .map(|s| s.as_str())
            .unwrap_or(declared);
        if let Ok(value) = std::env::var(source) {
            cmd.env(declared, value);
            cmd.arg("--env").arg(declared);
        }
    }

    cmd.arg(&plan.image).arg("sh").arg("-c").arg(&task.run);
    cmd
}

#[cfg(unix)]
fn current_uid_gid() -> String {
    // SAFETY: getuid/getgid are always-safe POSIX syscalls that return
    // the caller's real UID/GID — no allocation, no errors.
    unsafe { format!("{}:{}", libc_getuid(), libc_getgid()) }
}

#[cfg(not(unix))]
fn current_uid_gid() -> String {
    // Windows containers don't use numeric UIDs the same way; let the
    // image's configured USER apply.
    "1000:1000".to_string()
}

// Bento doesn't depend on `libc` — use the two tiny getuid/getgid
// FFI wrappers directly so we don't pull a whole crate in for two
// syscalls.
#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
    #[link_name = "getgid"]
    fn libc_getgid() -> u32;
}

/// Build a `PATH` env value with `prepend` first, then the host's own.
/// Cross-platform via `std::env::join_paths` (`:` on unix, `;` elsewhere).
fn build_path(prepend: &[PathBuf]) -> OsString {
    let mut entries: Vec<PathBuf> = prepend.to_vec();
    if let Some(existing) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&existing) {
            entries.push(p);
        }
    }
    std::env::join_paths(entries).unwrap_or_default()
}

/// Prepend `bin_dir` to the *current* process's `PATH` env var. Used
/// inside `ensure_toolchain` so a freshly-installed co-required tool
/// (e.g. uv) is visible to the next `installer.ensure(primary)` call,
/// which shells out via `Command::new(...)` and inherits PATH.
///
/// Edition 2021 — `set_var` is safe. bento's CLI is single-threaded
/// at this point (toolchain installs run sequentially before any
/// task work spawns); nothing else is mutating PATH concurrently.
fn prepend_path_env(bin_dir: &Path) {
    let cur = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&cur).collect();
    paths.insert(0, bin_dir.to_path_buf());
    if let Ok(joined) = std::env::join_paths(paths) {
        std::env::set_var("PATH", joined);
    }
}

/// Return the list of required env var names that aren't resolvable
/// — either via direct host-env lookup or through an alias in
/// `secret_aliases`. The report uses the *declared* name (what the
/// integration / task asked for) in the error, with the source name
/// in parentheses when aliased, so the failure tells the operator
/// both "what was needed" and "where we looked."
///
/// `None` when every name resolves to a non-empty value.
fn missing_required_env(
    required: &[String],
    aliases: &std::collections::BTreeMap<String, String>,
) -> Option<Vec<String>> {
    let missing: Vec<String> = required
        .iter()
        .filter_map(|declared| {
            let source = aliases
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
                // Aliased: surface both names so the operator knows which
                // host var was actually checked.
                Some(format!("{declared} (via ${source})"))
            }
        })
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

/// Return the list of CLI requirements whose binary isn't resolvable
/// via `PATH`. `None` when every binary is present. Walks `PATH`
/// entries and checks each for an executable file matching the name
/// — no subprocess spawn, just filesystem probes.
fn missing_required_cli(
    required: &[bento_adapters::CliRequirement],
) -> Option<Vec<bento_adapters::CliRequirement>> {
    if required.is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let missing: Vec<_> = required
        .iter()
        .filter(|req| !binary_on_path(&path, &req.binary))
        .cloned()
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

fn binary_on_path(path: &std::ffi::OsStr, name: &str) -> bool {
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            // On Unix, any regular file in PATH is assumed executable
            // (the loader will refuse if it isn't, which is the same
            // error shape as "not found"). Windows would need a
            // `.exe` / `.cmd` / etc. suffix walk — defer until bento
            // actually supports Windows hosts.
            return true;
        }
    }
    false
}

fn format_missing_cli(missing: &[bento_adapters::CliRequirement]) -> String {
    let mut out = String::from("CLI binary not found on PATH:");
    for req in missing {
        out.push_str(&format!(
            "\n  {} — install: {}",
            req.binary, req.install_hint
        ));
    }
    out
}

fn truncated_stderr(stderr: &[u8]) -> String {
    const LIMIT: usize = 2_000;
    let lossy = String::from_utf8_lossy(stderr);
    if lossy.len() <= LIMIT {
        lossy.into_owned()
    } else {
        let mut s = lossy.chars().take(LIMIT).collect::<String>();
        s.push_str("\n… (truncated)");
        s
    }
}

// ── Target resolution ──────────────────────────────────────────────

/// Result of resolving a user-provided target string against a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRef {
    /// Target names a bento. Callers typically set `CiOptions.bento_filter`.
    Bento(String),
    /// Target names a dish (matched against `DishConfig.name`).
    Dish(String),
}

/// Typed failure modes for [`resolve_target`]. Exposed so the CLI's error
/// classifier can emit stable `kind` codes (`target_not_found` /
/// `target_ambiguous`) and populate `next_steps` with the actual available
/// bento / dish names rather than a raw string.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TargetRefError {
    #[error("'{target}' is not a known bento or dish in this workspace")]
    NotFound {
        target: String,
        available_bentos: Vec<String>,
        available_dishes: Vec<String>,
    },

    #[error("'{target}' is ambiguous — it names both a bento and a dish; rename one")]
    Ambiguous { target: String },
}

/// Resolve a user-provided target (as in `bento build <target>`) to either a
/// bento or a dish. Errors if it matches neither, or if the name is used by
/// both a bento and a dish (ambiguous — rename one).
pub fn resolve_target(workspace: &Workspace, target: &str) -> Result<TargetRef> {
    let is_bento = workspace.bentos.contains_key(target);
    let is_dish = workspace.dishes_by_name.contains_key(target);
    match (is_bento, is_dish) {
        (true, false) => Ok(TargetRef::Bento(target.to_string())),
        (false, true) => Ok(TargetRef::Dish(target.to_string())),
        (true, true) => Err(TargetRefError::Ambiguous {
            target: target.to_string(),
        }
        .into()),
        (false, false) => Err(TargetRefError::NotFound {
            target: target.to_string(),
            available_bentos: workspace.bentos.keys().cloned().collect(),
            available_dishes: workspace.dishes_by_name.keys().cloned().collect(),
        }
        .into()),
    }
}

// ── Top-level entry point ──────────────────────────────────────────

pub fn ci_at(root: impl AsRef<Path>, opts: &CiOptions) -> Result<ExecutionReport> {
    let root = root.as_ref();
    let workspace = Workspace::load(root)
        .with_context(|| format!("loading workspace at {}", root.display()))?;
    let registry = AdapterRegistry::builtin();
    let integrations = IntegrationRegistry::builtin();
    let cache = LocalCache::new(crate::plan::default_cache_root()?);
    Executor::new(workspace, registry, cache)
        .with_integrations(integrations)
        .execute(opts)
}

/// `bento notify` entry point. Replays persisted garnish payloads
/// through each dish's Notify-kind integration tasks. Sibling to
/// [`ci_at`] but with no Deploy / build step — used for re-sending
/// a Slack post or Linear flip after a manual fix to the notify
/// config, without re-running the deploy itself.
pub fn notify_at(root: impl AsRef<Path>, opts: &CiOptions) -> Result<ExecutionReport> {
    let root = root.as_ref();
    let workspace = Workspace::load(root)
        .with_context(|| format!("loading workspace at {}", root.display()))?;
    let registry = AdapterRegistry::builtin();
    let integrations = IntegrationRegistry::builtin();
    let cache = LocalCache::new(crate::plan::default_cache_root()?);
    Executor::new(workspace, registry, cache)
        .with_integrations(integrations)
        .notify_only(opts)
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    /// A workspace whose only dish has a shell task that echoes into a
    /// file. Lets tests verify both exec and cache-restore paths without
    /// depending on a real Go or Node toolchain.
    fn shell_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        write(root, "bento.toml", b"[defaults]\nfail_fast = true\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );

        write(root, "dish/input.txt", b"hello");
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"
inputs = ["input.txt"]
outputs = ["out.txt"]

[tasks.build]
run = "cp input.txt out.txt"
"#,
        );

        tmp
    }

    fn executor(root: &Path) -> (Executor, tempfile::TempDir) {
        let workspace = Workspace::load(root).unwrap();
        let registry = AdapterRegistry::builtin();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        (Executor::new(workspace, registry, cache), cache_dir)
    }

    #[test]
    fn first_run_executes_and_caches() {
        let tmp = shell_workspace();
        let (exec, _cache) = executor(tmp.path());

        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.bentos.len(), 1);
        assert_eq!(report.summary.built, 1);
        assert_eq!(report.summary.hits, 0);
        assert_eq!(report.summary.failed, 0);

        // Real output exists on disk.
        let out = tmp.path().join("dish/out.txt");
        assert_eq!(std::fs::read(&out).unwrap(), b"hello");
    }

    #[test]
    fn second_run_hits_cache() {
        let tmp = shell_workspace();
        let (exec, _cache) = executor(tmp.path());

        // First: build.
        exec.execute(&CiOptions::default()).unwrap();

        // Blow away the output to prove cache-restore produced it.
        std::fs::remove_file(tmp.path().join("dish/out.txt")).unwrap();

        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.hits, 1);
        assert_eq!(report.summary.built, 0);

        // Output restored from cache.
        assert_eq!(
            std::fs::read(tmp.path().join("dish/out.txt")).unwrap(),
            b"hello"
        );
    }

    /// Slow fake remote: records has/get/put counts, sleeps on `put` so
    /// tests can distinguish fire-and-forget-loss from drained-join.
    struct SlowRemote {
        has_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        put_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        delay: std::time::Duration,
    }

    impl SlowRemote {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                has_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                put_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                delay,
            }
        }
    }

    impl bento_cache::RemoteCache for SlowRemote {
        fn has(&self, _: &bento_cache::CacheKey) -> bool {
            self.has_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        }
        fn get(&self, _: &bento_cache::CacheKey, _: &std::path::Path) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn put(&self, _: &bento_cache::CacheKey, _: &std::path::Path) -> anyhow::Result<()> {
            std::thread::sleep(self.delay);
            self.put_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn display_url(&self) -> &str {
            "slow-fake://test"
        }
    }

    #[test]
    fn execute_drains_write_through_uploads_before_returning() {
        // Write-through uploads must finish before `execute()` returns,
        // otherwise short-running runs (all cache hits) kill the upload
        // thread at process exit.
        //
        // Opt into `remote_write_through` explicitly — the default is
        // `false` so a local hit short-circuits without touching the
        // remote. The drain invariant still matters for users who
        // enable write-through, which is the whole point of this test.
        let tmp = shell_workspace();
        std::fs::write(
            tmp.path().join("bento.toml"),
            b"[defaults]\nfail_fast = true\n\n[cache]\nremote_write_through = true\n",
        )
        .unwrap();
        let workspace = Workspace::load(tmp.path()).unwrap();
        let registry = AdapterRegistry::builtin();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        let slow = SlowRemote::new(std::time::Duration::from_millis(400));
        let put_count = slow.put_count.clone();
        let exec = Executor::new(workspace, registry, cache).with_remote(slow);

        // First run: cache miss → build → put to remote synchronously in
        // the cache-miss branch (not via write_through). Second run: hit
        // → write_through fires.
        exec.execute(&CiOptions::default()).unwrap();
        let baseline = put_count.load(std::sync::atomic::Ordering::SeqCst);

        let start = std::time::Instant::now();
        exec.execute(&CiOptions::default()).unwrap();
        let elapsed = start.elapsed();

        let after = put_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            after > baseline,
            "write-through PUT was never observed (baseline={baseline}, after={after})"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(350),
            "execute() returned in {elapsed:?} — looks like it didn't wait for the \
             400 ms write-through upload"
        );
    }

    #[test]
    fn local_hit_short_circuits_without_remote_touch_by_default() {
        // Warm runs used to HEAD the remote on every local-hit via
        // write_through. With `remote_write_through` default `false`,
        // a local hit must not touch the remote at all — no HEAD, no
        // PUT.
        let tmp = shell_workspace();
        let workspace = Workspace::load(tmp.path()).unwrap();
        let registry = AdapterRegistry::builtin();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        let slow = SlowRemote::new(std::time::Duration::from_millis(5));
        let has_count = slow.has_count.clone();
        let put_count = slow.put_count.clone();
        let exec = Executor::new(workspace, registry, cache).with_remote(slow);

        // First run: miss → build → synchronous remote.put on the
        // miss→built path (not via write_through). This establishes
        // a post-miss baseline where PUT was exercised exactly once.
        exec.execute(&CiOptions::default()).unwrap();
        let has_baseline = has_count.load(std::sync::atomic::Ordering::SeqCst);
        let put_baseline = put_count.load(std::sync::atomic::Ordering::SeqCst);

        // Second run: local hit. Must not touch the remote.
        exec.execute(&CiOptions::default()).unwrap();
        let has_after = has_count.load(std::sync::atomic::Ordering::SeqCst);
        let put_after = put_count.load(std::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            has_after, has_baseline,
            "local-hit path did a remote HEAD (baseline={has_baseline}, after={has_after})"
        );
        assert_eq!(
            put_after, put_baseline,
            "local-hit path did a remote PUT (baseline={put_baseline}, after={put_after})"
        );
    }

    #[test]
    fn no_cache_flag_bypasses_hit() {
        let tmp = shell_workspace();
        let (exec, _cache) = executor(tmp.path());

        exec.execute(&CiOptions::default()).unwrap();

        let report = exec
            .execute(&CiOptions {
                no_cache: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.summary.hits, 0);
        assert_eq!(report.summary.built, 1);
    }

    #[test]
    fn input_change_invalidates_cache() {
        let tmp = shell_workspace();
        let (exec, _cache) = executor(tmp.path());

        exec.execute(&CiOptions::default()).unwrap();

        // Change an input.
        std::fs::write(tmp.path().join("dish/input.txt"), b"goodbye").unwrap();

        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.hits, 0);
        assert_eq!(report.summary.built, 1);
        assert_eq!(
            std::fs::read(tmp.path().join("dish/out.txt")).unwrap(),
            b"goodbye"
        );
    }

    #[test]
    fn failed_task_is_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"[defaults]\nfail_fast = false\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"

[tasks.build]
run = "exit 7"
"#,
        );

        let (exec, cache_dir) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.failed, 1);
        match &report.bentos[0].dishes[0].tasks[0].outcome {
            TaskOutcome::Failed { exit_code, .. } => assert_eq!(*exit_code, 7),
            other => panic!("expected Failed, got {other:?}"),
        }

        // Cache directory should have no entries (failed tasks are not cached).
        let cache = LocalCache::new(cache_dir.path());
        assert_eq!(cache.stats().unwrap().entries, 0);
    }

    #[test]
    fn fail_fast_stops_at_next_level_after_failure() {
        // 'a' is in level 0, 'b' depends on 'a' so it's in level 1.
        // With fail_fast = true, 'b' must be skipped after 'a' fails.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"[defaults]\nfail_fast = true\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );
        write(
            root,
            "a/dish.toml",
            br#"name = "a"

[tasks.build]
run = "exit 1"
"#,
        );
        write(
            root,
            "b/dish.toml",
            br#"name = "b"
depends_on = ["a"]

[tasks.build]
run = "true"
"#,
        );

        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.built, 0);
        assert_eq!(report.bentos[0].dishes.len(), 1);
        assert_eq!(report.bentos[0].dishes[0].name, "a");
    }

    #[test]
    fn independent_dishes_run_in_same_level_despite_fail_fast() {
        // 'a' and 'b' have no deps → both in level 0 → both run even
        // with fail_fast. Parallel semantics: we only gate the *next*
        // level, not in-flight dishes in the current level.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"[defaults]\nfail_fast = true\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );
        write(
            root,
            "a/dish.toml",
            b"name = \"a\"\n\n[tasks.build]\nrun = \"exit 1\"\n",
        );
        write(
            root,
            "b/dish.toml",
            b"name = \"b\"\n\n[tasks.build]\nrun = \"true\"\n",
        );

        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.built, 1);
        assert_eq!(report.bentos[0].dishes.len(), 2);
    }

    #[test]
    fn fail_fast_false_continues_past_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"[defaults]\nfail_fast = false\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );
        write(
            root,
            "a/dish.toml",
            br#"name = "a"

[tasks.build]
run = "exit 1"
"#,
        );
        write(
            root,
            "b/dish.toml",
            br#"name = "b"

[tasks.build]
run = "true"
"#,
        );

        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.built, 1);
        assert_eq!(report.bentos[0].dishes.len(), 2);
    }

    #[test]
    fn task_filter_restricts_to_named_tasks() {
        let tmp = shell_workspace();
        // Add a second task so we have something to filter out.
        let dish_toml = tmp.path().join("dish/dish.toml");
        let mut contents = std::fs::read_to_string(&dish_toml).unwrap();
        contents.push_str("\n\n[tasks.other]\nrun = \"true\"\n");
        std::fs::write(&dish_toml, contents).unwrap();

        let (exec, _cache) = executor(tmp.path());
        let report = exec
            .execute(&CiOptions {
                task_filter: Some(vec!["build".to_string()]),
                ..Default::default()
            })
            .unwrap();

        let tasks = &report.bentos[0].dishes[0].tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "build");
    }

    #[test]
    fn dish_filter_skips_non_matching_dishes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );
        write(
            root,
            "a/dish.toml",
            br#"name = "first"

[tasks.build]
run = "true"
"#,
        );
        write(
            root,
            "b/dish.toml",
            br#"name = "second"

[tasks.build]
run = "true"
"#,
        );

        let (exec, _cache) = executor(root);
        let report = exec
            .execute(&CiOptions {
                dish_filter: Some("first".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.bentos[0].dishes.len(), 1);
        assert_eq!(report.bentos[0].dishes[0].name, "first");
    }

    #[test]
    fn resolve_target_disambiguates_bento_vs_dish() {
        let tmp = shell_workspace();
        let workspace = Workspace::load(tmp.path()).unwrap();

        assert_eq!(
            resolve_target(&workspace, "prod").unwrap(),
            TargetRef::Bento("prod".into())
        );
        assert_eq!(
            resolve_target(&workspace, "d").unwrap(),
            TargetRef::Dish("d".into())
        );

        let err = resolve_target(&workspace, "nothing").unwrap_err();
        assert!(err.to_string().contains("not a known bento or dish"));
        let typed = err
            .downcast_ref::<TargetRefError>()
            .expect("should downcast to TargetRefError");
        match typed {
            TargetRefError::NotFound {
                target,
                available_bentos,
                available_dishes,
            } => {
                assert_eq!(target, "nothing");
                assert!(
                    available_bentos.contains(&"prod".to_string()),
                    "expected 'prod' in available_bentos, got {available_bentos:?}"
                );
                assert!(
                    available_dishes.contains(&"d".to_string()),
                    "expected 'd' in available_dishes, got {available_dishes:?}"
                );
            }
            other => panic!("expected TargetRefError::NotFound, got {other:?}"),
        }
    }

    #[test]
    fn env_allowlist_is_passed_through() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"
outputs = ["out.txt"]

[tasks.build]
run = "printf %s \"$MY_VAR\" > out.txt"
env = ["MY_VAR"]
"#,
        );

        std::env::set_var("MY_VAR", "hello-env");
        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();
        std::env::remove_var("MY_VAR");

        assert_eq!(report.summary.built, 1);
        assert_eq!(
            std::fs::read(tmp.path().join("dish/out.txt")).unwrap(),
            b"hello-env"
        );
    }

    // ── retry + flakiness ──────────────────────────────────────────

    /// Writes a workspace whose task counts invocations in a file and
    /// succeeds only on the Nth attempt. Returns the workspace root.
    fn flaky_workspace(succeed_on_attempt: u32, retry: u32) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"[defaults]\nfail_fast = false\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        let counter = root.join("attempts.txt").display().to_string();
        // Script: increment counter, succeed iff counter reached target.
        // Using a plain shell loop keeps this test self-contained and
        // portable to any POSIX sh.
        let dish_toml = format!(
            r#"name = "d"

[tasks.build]
run = "n=$(cat {counter} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {counter}; test $n -ge {target}"
retry = {retry}
"#,
            counter = counter,
            target = succeed_on_attempt,
            retry = retry,
        );
        write(root, "dish/dish.toml", dish_toml.as_bytes());
        tmp
    }

    #[test]
    fn retry_lets_flaky_task_succeed_and_marks_flaky() {
        // Succeeds on the 3rd attempt, retry=2 → 3 total attempts allowed.
        let tmp = flaky_workspace(3, 2);
        let (exec, _cache) = executor(tmp.path());
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.built, 1, "should have built once");
        assert_eq!(report.summary.failed, 0);
        assert_eq!(report.summary.flaky, 1, "success on retry → flaky");

        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.attempts, 3);
        assert!(task.flaky);
        assert!(matches!(task.outcome, TaskOutcome::Built { .. }));
    }

    #[test]
    fn retry_zero_means_one_attempt_only() {
        // retry=0 + succeed-on-attempt=2 → we only get 1 attempt → fail.
        let tmp = flaky_workspace(2, 0);
        let (exec, _cache) = executor(tmp.path());
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.flaky, 0);
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.attempts, 1);
        assert!(!task.flaky);
    }

    #[test]
    fn retry_exhausted_reports_failure_with_correct_attempts() {
        // Succeeds on attempt 5, retry=2 → only 3 attempts → failure.
        let tmp = flaky_workspace(5, 2);
        let (exec, _cache) = executor(tmp.path());
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.failed, 1);
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.attempts, 3, "should have attempted retry+1 times");
        assert!(!task.flaky, "exhausted retries is not flaky");
        assert!(matches!(task.outcome, TaskOutcome::Failed { .. }));
    }

    #[test]
    fn first_attempt_success_is_not_flaky() {
        let tmp = flaky_workspace(1, 3);
        let (exec, _cache) = executor(tmp.path());
        let report = exec.execute(&CiOptions::default()).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.attempts, 1);
        assert!(!task.flaky);
        assert_eq!(report.summary.flaky, 0);
    }

    // ── graph-driven execution ─────────────────────────────────────

    #[test]
    fn depends_on_enforces_sequential_order() {
        // 'b' depends on 'a'. b's task reads a file 'a' writes. If order
        // is wrong, b sees an empty file and test fails.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );

        let shared = root.join("shared.txt").display().to_string();
        write(
            root,
            "a/dish.toml",
            format!(
                "name = \"a\"\noutputs = [\"out.txt\"]\n\n[tasks.build]\nrun = \"echo done > {shared}; echo a > out.txt\"\n"
            )
            .as_bytes(),
        );
        write(
            root,
            "b/dish.toml",
            format!(
                "name = \"b\"\noutputs = [\"out.txt\"]\ndepends_on = [\"a\"]\n\n[tasks.build]\nrun = \"grep -q done {shared} && echo b > out.txt\"\n"
            )
            .as_bytes(),
        );

        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.failed, 0, "{report:?}");
        assert_eq!(report.summary.built, 2);
    }

    #[test]
    fn independent_dishes_execute_in_parallel() {
        // Two dishes that each sleep 250ms. Sequential would take ≥500ms;
        // parallel should finish in ≲350ms. Generous margin to avoid
        // flakes on loaded CI.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"[defaults]\nparallelism = 2\n");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );
        write(
            root,
            "a/dish.toml",
            br#"name = "a"

[tasks.build]
run = "sleep 0.25"
"#,
        );
        write(
            root,
            "b/dish.toml",
            br#"name = "b"

[tasks.build]
run = "sleep 0.25"
"#,
        );

        let (exec, _cache) = executor(root);
        let start = std::time::Instant::now();
        let report = exec.execute(&CiOptions::default()).unwrap();
        let elapsed = start.elapsed().as_millis() as u64;

        assert_eq!(report.summary.built, 2);
        assert!(
            elapsed < 450,
            "parallel execution expected <450ms, got {elapsed}ms"
        );
    }

    // ── cascade / force_independent ────────────────────────────────

    /// Build a "lib ← app" workspace where the app's test task echoes
    /// success. Leaves the concrete content values to the caller.
    fn lib_app_workspace(app_toml_body: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["lib", "app"]"#,
        );
        write(
            root,
            "lib/dish.toml",
            br#"name = "lib"
inputs = ["src.txt"]

[tasks.build]
run = "true"
"#,
        );
        write(root, "lib/src.txt", b"v1");
        write(root, "app/dish.toml", app_toml_body.as_bytes());
        write(root, "app/src.txt", b"app-v1");
        tmp
    }

    #[test]
    fn dep_source_change_invalidates_dependent_cache() {
        let tmp = lib_app_workspace(
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]

[tasks.build]
run = "true"
"#,
        );
        let (exec, _cache) = executor(tmp.path());

        // First run: app builds fresh.
        exec.execute(&CiOptions::default()).unwrap();

        // Second run with no changes: app hits the cache.
        let warm = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(warm.summary.hits, 2, "{warm:?}");
        assert_eq!(warm.summary.built, 0);

        // Change the lib's source. App must now rebuild (pessimistic cascade).
        std::fs::write(tmp.path().join("lib/src.txt"), b"v2").unwrap();
        let cascade = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(cascade.summary.built, 2, "{cascade:?}");
        assert_eq!(cascade.summary.hits, 0);
    }

    #[test]
    fn force_independent_blocks_cache_invalidation_through_that_dish() {
        let tmp = lib_app_workspace(
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]
force_independent = true

[tasks.build]
run = "true"
"#,
        );
        let (exec, _cache) = executor(tmp.path());

        exec.execute(&CiOptions::default()).unwrap();
        // Warm.
        let warm = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(warm.summary.hits, 2);

        // Change lib. force_independent on app means app still hits cache.
        std::fs::write(tmp.path().join("lib/src.txt"), b"v2").unwrap();
        let after = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(after.summary.built, 1, "only lib rebuilds: {after:?}");
        assert_eq!(after.summary.hits, 1, "app still hits: {after:?}");
    }

    // ── container execution ────────────────────────────────────────

    #[test]
    fn container_plan_returns_none_by_default() {
        // Default ContainerMode is Never — no container.
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "bentos/prod.toml",
            b"name = \"prod\"\ndishes = []\n",
        );
        std::fs::create_dir(tmp.path().join("bentos")).ok(); // idempotent
        let ws = Workspace::load(tmp.path()).unwrap();
        assert!(super::container_plan(&ws).is_none());
    }

    #[test]
    fn container_plan_never_even_with_image() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "bento.toml",
            b"[execution]\ncontainer = \"never\"\nimage = \"debian:12\"\n",
        );
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        write(
            tmp.path(),
            "bentos/prod.toml",
            b"name = \"prod\"\ndishes = []\n",
        );
        let ws = Workspace::load(tmp.path()).unwrap();
        assert!(super::container_plan(&ws).is_none());
    }

    #[test]
    fn cache_image_ref_changes_key() {
        // Two workspaces identical except the container image config —
        // we should get different cache keys for the same task.
        use crate::plan::compute_key;

        let base = |image: &str| -> tempfile::TempDir {
            let tmp = tempfile::tempdir().unwrap();
            write(
                tmp.path(),
                "bento.toml",
                format!("[execution]\ncontainer = \"always\"\nimage = \"{image}\"\n").as_bytes(),
            );
            std::fs::create_dir(tmp.path().join("bentos")).unwrap();
            write(
                tmp.path(),
                "bentos/prod.toml",
                b"name = \"prod\"\ndishes = [\"d\"]\n",
            );
            write(
                tmp.path(),
                "d/dish.toml",
                b"name = \"d\"\n\n[tasks.build]\nrun = \"true\"\n",
            );
            tmp
        };

        let tmp1 = base("debian:12");
        let tmp2 = base("debian:13");
        let ws1 = Workspace::load(tmp1.path()).unwrap();
        let ws2 = Workspace::load(tmp2.path()).unwrap();

        let task = crate::plan::ResolvedTask {
            name: "build".into(),
            run: "true".into(),
            inputs: vec![],
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

        let img1 = crate::plan::container_image_for_plan(&ws1);
        let img2 = crate::plan::container_image_for_plan(&ws2);

        let (k1, _) = compute_key(
            &tmp1.path().join("d"),
            "d",
            None,
            &task,
            &[],
            img1.as_deref(),
            &[],
        )
        .unwrap();
        let (k2, _) = compute_key(
            &tmp2.path().join("d"),
            "d",
            None,
            &task,
            &[],
            img2.as_deref(),
            &[],
        )
        .unwrap();

        assert_ne!(
            k1.as_hex(),
            k2.as_hex(),
            "changing the container image must change the cache key"
        );
    }

    #[test]
    fn task_dep_key_cascades_into_dependent_task_hash() {
        // Regression: `railway:deploy` with an empty declared inputs
        // set used to hash identically across source edits because the
        // `depends_on = ["build"]` signal was ignored by `compute_key`.
        // Verify a task-dep mix-in changes the hash so
        // `deploy_state_hit` can actually invalidate.
        use crate::plan::compute_key;

        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "bento.toml", b"");
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        write(
            tmp.path(),
            "bentos/prod.toml",
            b"name = \"prod\"\ndishes = [\"d\"]\n",
        );
        write(
            tmp.path(),
            "d/dish.toml",
            b"name = \"d\"\n\n[tasks.build]\nrun = \"true\"\n",
        );

        let task = crate::plan::ResolvedTask {
            name: "railway:deploy".into(),
            run: "railway up --ci".into(),
            inputs: vec![],
            outputs: vec![],
            workspace_outputs: vec![],
            env: vec![],
            retry: 0,
            no_cache: true,
            required_env: vec![],
            required_cli: vec![],
            integration_kind: None,
            depends_on: vec!["build".into()],
        };

        let (without_dep, _) =
            compute_key(&tmp.path().join("d"), "d", None, &task, &[], None, &[]).unwrap();
        let (with_dep_a, _) = compute_key(
            &tmp.path().join("d"),
            "d",
            None,
            &task,
            &[],
            None,
            &[("build", "aaaaaaaa")],
        )
        .unwrap();
        let (with_dep_b, _) = compute_key(
            &tmp.path().join("d"),
            "d",
            None,
            &task,
            &[],
            None,
            &[("build", "bbbbbbbb")],
        )
        .unwrap();

        assert_ne!(
            without_dep.as_hex(),
            with_dep_a.as_hex(),
            "task-dep mix-in must change the deploy task key"
        );
        assert_ne!(
            with_dep_a.as_hex(),
            with_dep_b.as_hex(),
            "different dep keys must produce different deploy task keys"
        );
    }

    #[test]
    fn shared_dish_across_bentos_shares_cache() {
        // Same dish listed by two bentos. First ci run: shared dish builds
        // once (in whichever bento comes alphabetically first), hits cache
        // in the other bento via identical cache key.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["shared"]"#,
        );
        write(
            root,
            "bentos/staging.toml",
            br#"name = "staging"
dishes = ["shared"]"#,
        );
        write(
            root,
            "shared/dish.toml",
            br#"name = "shared"
inputs = ["src.txt"]

[tasks.build]
run = "true"
"#,
        );
        write(root, "shared/src.txt", b"v1");

        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();

        // Two bento entries in the report, but only one rebuild + one hit.
        assert_eq!(report.bentos.len(), 2);
        assert_eq!(report.summary.built, 1);
        assert_eq!(report.summary.hits, 1);

        // Key is identical across bentos.
        let key_prod = &report.bentos[0].dishes[0].tasks[0].key;
        let key_staging = &report.bentos[1].dishes[0].tasks[0].key;
        assert_eq!(key_prod, key_staging);
    }

    #[test]
    fn cycle_in_depends_on_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["a", "b"]"#,
        );
        write(
            root,
            "a/dish.toml",
            br#"name = "a"
depends_on = ["b"]
"#,
        );
        write(
            root,
            "b/dish.toml",
            br#"name = "b"
depends_on = ["a"]
"#,
        );

        let (exec, _cache) = executor(root);
        let err = exec.execute(&CiOptions::default()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("cycle"), "got: {chain}");
    }

    #[test]
    fn cache_hit_reports_zero_attempts() {
        // Run once to populate the cache, then run again.
        let tmp = shell_workspace();
        let (exec, _cache) = executor(tmp.path());
        exec.execute(&CiOptions::default()).unwrap();
        let report = exec.execute(&CiOptions::default()).unwrap();

        assert_eq!(report.summary.hits, 1);
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.attempts, 0);
        assert!(!task.flaky);
    }

    // ── diagnostic capture integration ─────────────────────────────

    /// Stub adapter that declares a diagnostic hook returning a
    /// pre-baked cargo-message JSON line. Lets us exercise the
    /// executor's two-pass-on-failure path without depending on a
    /// real cargo / golangci-lint / etc. binary in the test sandbox.
    struct StubAdapterWithHook {
        rerun_command: String,
    }

    impl LanguageAdapter for StubAdapterWithHook {
        fn id(&self) -> &str {
            "stub"
        }
        fn detect(&self, _: &Path) -> bool {
            false
        }
        fn fingerprint_files(&self) -> Vec<String> {
            Vec::new()
        }
        fn required_toolchain(
            &self,
            _: &Path,
        ) -> anyhow::Result<Option<bento_adapters::ToolVersion>> {
            Ok(None)
        }
        fn install(&self, _: &bento_adapters::TaskContext) -> anyhow::Result<()> {
            Ok(())
        }
        fn default_tasks(&self) -> Vec<bento_adapters::DefaultTask> {
            Vec::new()
        }
        fn diagnostic_hook(&self, task: &str) -> Option<bento_adapters::DiagnosticHook> {
            if task != "lint" {
                return None;
            }
            Some(bento_adapters::DiagnosticHook {
                rerun: bento_adapters::DiagnosticRerun::Replace(self.rerun_command.clone()),
                parser: bento_adapters::DiagnosticParser::Builtin(
                    bento_adapters::ParserId::CargoMessage,
                ),
            })
        }
    }

    fn stub_workspace_with_failing_lint() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"
language = "stub"

[tasks.lint]
run = "exit 1"
"#,
        );
        tmp
    }

    fn executor_with_adapter(
        root: &Path,
        adapter: Box<dyn LanguageAdapter>,
    ) -> (Executor, tempfile::TempDir) {
        let workspace = Workspace::load(root).unwrap();
        let registry = AdapterRegistry::empty().with_plugins([adapter]);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        (Executor::new(workspace, registry, cache), cache_dir)
    }

    #[test]
    fn failed_task_with_hook_populates_diagnostics() {
        // Pre-baked cargo-message JSON line — what `cargo --message-format=json`
        // would emit on a compile error. Single-line so `printf` doesn't
        // get tangled up in shell quoting.
        let cargo_json = r#"{"reason":"compiler-message","message":{"level":"error","message":"undefined symbol Foo","code":{"code":"E0425","explanation":"x"},"spans":[{"file_name":"main.rs","is_primary":true,"line_start":7,"line_end":7,"column_start":3,"column_end":6}]}}"#;
        let rerun = format!("printf '%s' '{cargo_json}'");

        let tmp = stub_workspace_with_failing_lint();
        let (exec, _cache) = executor_with_adapter(
            tmp.path(),
            Box::new(StubAdapterWithHook {
                rerun_command: rerun,
            }),
        );
        let report = exec.execute(&CiOptions::default()).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];

        assert!(matches!(task.outcome, TaskOutcome::Failed { .. }));
        assert_eq!(
            task.diagnostics.len(),
            1,
            "expected 1 diagnostic, got: {:?}",
            task.diagnostics
        );
        let d = &task.diagnostics[0];
        assert_eq!(d.file, "dish/main.rs");
        assert_eq!(d.line, 7);
        assert_eq!(d.col, Some(3));
        assert_eq!(d.severity, crate::Severity::Error);
        assert_eq!(d.message, "undefined symbol Foo");
        assert_eq!(d.rule.as_deref(), Some("E0425"));
        assert_eq!(d.source, "cargo");
    }

    #[test]
    fn failed_task_with_no_hook_leaves_diagnostics_empty() {
        struct NoHookAdapter;
        impl LanguageAdapter for NoHookAdapter {
            fn id(&self) -> &str {
                "stub"
            }
            fn detect(&self, _: &Path) -> bool {
                false
            }
            fn fingerprint_files(&self) -> Vec<String> {
                Vec::new()
            }
            fn required_toolchain(
                &self,
                _: &Path,
            ) -> anyhow::Result<Option<bento_adapters::ToolVersion>> {
                Ok(None)
            }
            fn install(&self, _: &bento_adapters::TaskContext) -> anyhow::Result<()> {
                Ok(())
            }
            fn default_tasks(&self) -> Vec<bento_adapters::DefaultTask> {
                Vec::new()
            }
            // diagnostic_hook intentionally inherits the trait's None default.
        }

        let tmp = stub_workspace_with_failing_lint();
        let (exec, _cache) = executor_with_adapter(tmp.path(), Box::new(NoHookAdapter));
        let report = exec.execute(&CiOptions::default()).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert!(matches!(task.outcome, TaskOutcome::Failed { .. }));
        assert!(task.diagnostics.is_empty());
    }

    #[test]
    fn successful_task_never_re_runs_for_diagnostics() {
        // Hook would parse output but the task succeeds, so the re-run
        // path should not fire and diagnostics stay empty.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"
language = "stub"

[tasks.lint]
run = "true"
"#,
        );

        let (exec, _cache) = executor_with_adapter(
            root,
            Box::new(StubAdapterWithHook {
                rerun_command: "false".into(),
            }),
        );
        let report = exec.execute(&CiOptions::default()).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert!(matches!(task.outcome, TaskOutcome::Built { .. }));
        assert!(task.diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_rerun_failure_is_strictly_additive() {
        // The re-run command itself crashes (exit nonzero AND prints
        // garbage). The original failure must still propagate; the
        // diagnostics array is just left empty.
        let tmp = stub_workspace_with_failing_lint();
        let (exec, _cache) = executor_with_adapter(
            tmp.path(),
            Box::new(StubAdapterWithHook {
                rerun_command: "echo 'not-json'; exit 99".into(),
            }),
        );
        let report = exec.execute(&CiOptions::default()).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];
        // Original failure outcome preserved.
        assert!(matches!(task.outcome, TaskOutcome::Failed { .. }));
        // Diagnostics stay empty when the parser found nothing.
        assert!(task.diagnostics.is_empty());
        // Build summary still reflects the original failure, not any
        // collateral from the re-run.
        assert_eq!(report.summary.failed, 1);
    }

    // ── Install step tests ─────────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Test-only adapter with configurable `install_probe` and
    /// `install()` behaviour, plus a counter so tests can assert how
    /// many times the executor actually invoked install.
    struct InstallMockAdapter {
        probe: InstallProbe,
        install_fails: bool,
        install_calls: Arc<AtomicUsize>,
    }

    impl LanguageAdapter for InstallMockAdapter {
        fn id(&self) -> &str {
            "install-mock"
        }
        fn detect(&self, _: &Path) -> bool {
            false
        }
        fn fingerprint_files(&self) -> Vec<String> {
            Vec::new()
        }
        fn required_toolchain(
            &self,
            _: &Path,
        ) -> anyhow::Result<Option<bento_adapters::ToolVersion>> {
            Ok(None)
        }
        fn install(&self, _: &bento_adapters::TaskContext) -> anyhow::Result<()> {
            self.install_calls.fetch_add(1, Ordering::SeqCst);
            if self.install_fails {
                anyhow::bail!("boom");
            }
            Ok(())
        }
        fn install_probe(&self, _: &Path) -> InstallProbe {
            self.probe.clone()
        }
        fn default_tasks(&self) -> Vec<bento_adapters::DefaultTask> {
            Vec::new()
        }
    }

    fn install_mock_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"
language = "install-mock"

[tasks.build]
run = "true"
"#,
        );
        tmp
    }

    fn install_mock_executor(
        root: &Path,
        probe: InstallProbe,
        install_fails: bool,
    ) -> (Executor, Arc<AtomicUsize>, tempfile::TempDir) {
        let workspace = Workspace::load(root).unwrap();
        let install_calls = Arc::new(AtomicUsize::new(0));
        let registry = AdapterRegistry::empty().with_plugins([Box::new(InstallMockAdapter {
            probe,
            install_fails,
            install_calls: install_calls.clone(),
        })
            as Box<dyn LanguageAdapter>]);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        (
            Executor::new(workspace, registry, cache),
            install_calls,
            cache_dir,
        )
    }

    #[test]
    fn probe_ready_does_not_run_install() {
        let tmp = install_mock_workspace();
        let (exec, install_calls, _cache) =
            install_mock_executor(tmp.path(), InstallProbe::Ready, false);
        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.installs, 0);
        assert_eq!(install_calls.load(Ordering::SeqCst), 0);
        // Build task still ran.
        assert_eq!(report.summary.built, 1);
        // install field is absent on the dish when no install ran.
        assert!(report.bentos[0].dishes[0].install.is_none());
    }

    #[test]
    fn probe_missing_runs_install_then_proceeds_to_tasks() {
        let tmp = install_mock_workspace();
        let (exec, install_calls, _cache) =
            install_mock_executor(tmp.path(), InstallProbe::missing("deps absent"), false);
        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.installs, 1);
        assert_eq!(report.summary.install_failures, 0);
        assert_eq!(install_calls.load(Ordering::SeqCst), 1);
        // Build task still ran after install succeeded.
        assert_eq!(report.summary.built, 1);
        let install = report.bentos[0].dishes[0].install.as_ref().unwrap();
        assert_eq!(install.reason, "deps absent");
        assert!(install.error.is_none());
    }

    #[test]
    fn install_failure_skips_tasks() {
        let tmp = install_mock_workspace();
        let (exec, _install_calls, _cache) =
            install_mock_executor(tmp.path(), InstallProbe::missing("deps absent"), true);
        let report = exec.execute(&CiOptions::default()).unwrap();
        assert_eq!(report.summary.installs, 1);
        assert_eq!(report.summary.install_failures, 1);
        // Tasks skipped.
        assert_eq!(report.summary.tasks, 0);
        assert_eq!(report.summary.built, 0);
        let install = report.bentos[0].dishes[0].install.as_ref().unwrap();
        assert!(install.error.is_some());
        // No task rows either — the install record is the signal.
        assert!(report.bentos[0].dishes[0].tasks.is_empty());
    }

    #[test]
    fn skip_install_option_bypasses_probe() {
        let tmp = install_mock_workspace();
        let (exec, install_calls, _cache) =
            install_mock_executor(tmp.path(), InstallProbe::missing("deps absent"), false);
        let opts = CiOptions {
            skip_install: true,
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        assert_eq!(report.summary.installs, 0);
        assert_eq!(install_calls.load(Ordering::SeqCst), 0);
        // Build task still runs.
        assert_eq!(report.summary.built, 1);
    }

    #[test]
    fn install_only_mode_skips_task_loop() {
        let tmp = install_mock_workspace();
        let (exec, install_calls, _cache) =
            install_mock_executor(tmp.path(), InstallProbe::missing("deps absent"), false);
        let opts = CiOptions {
            install_only: true,
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        // Install ran.
        assert_eq!(install_calls.load(Ordering::SeqCst), 1);
        assert_eq!(report.summary.installs, 1);
        // But no tasks.
        assert_eq!(report.summary.tasks, 0);
        assert_eq!(report.summary.built, 0);
        assert_eq!(report.summary.hits, 0);
        // Install record still surfaces.
        assert!(report.bentos[0].dishes[0].install.is_some());
        // No task rows.
        assert!(report.bentos[0].dishes[0].tasks.is_empty());
    }

    #[test]
    fn force_install_runs_even_when_probe_ready() {
        let tmp = install_mock_workspace();
        let (exec, install_calls, _cache) =
            install_mock_executor(tmp.path(), InstallProbe::Ready, false);
        let opts = CiOptions {
            force_install: true,
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        assert_eq!(report.summary.installs, 1);
        assert_eq!(install_calls.load(Ordering::SeqCst), 1);
        let install = report.bentos[0].dishes[0].install.as_ref().unwrap();
        assert_eq!(install.reason, "--force-install");
    }

    // ── Integration flow tests ─────────────────────────────────────

    use bento_adapters::{Integration, IntegrationRegistry, IntegrationTask, IntegrationTaskKind};

    /// Test-only integration with configurable tasks + required env.
    /// Declared as a field struct so each test can customise behaviour.
    struct MockIntegration {
        id: &'static str,
        detect_sentinel: &'static str,
        required_env_vars: Vec<String>,
        tasks: Vec<IntegrationTask>,
    }

    impl Integration for MockIntegration {
        fn id(&self) -> &str {
            self.id
        }
        fn detect(&self, dir: &Path) -> bool {
            dir.join(self.detect_sentinel).exists()
        }
        fn required_env(&self) -> Vec<String> {
            self.required_env_vars.clone()
        }
        fn detected_tasks(&self, _: &Path, _: &toml::Table) -> Vec<IntegrationTask> {
            self.tasks.clone()
        }
    }

    fn integration_mock_workspace(dish_files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"

[tasks.build]
run = "true"
"#,
        );
        for (name, bytes) in dish_files {
            write(root, &format!("dish/{name}"), bytes);
        }
        tmp
    }

    fn integration_mock_executor(
        root: &Path,
        integration: MockIntegration,
    ) -> (Executor, tempfile::TempDir) {
        let workspace = Workspace::load(root).unwrap();
        let registry = AdapterRegistry::empty();
        let integrations = IntegrationRegistry::empty()
            .with_plugins([Box::new(integration) as Box<dyn Integration>]);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        (
            Executor::new(workspace, registry, cache).with_integrations(integrations),
            cache_dir,
        )
    }

    fn deploy_task(name: &str, run: &str) -> IntegrationTask {
        IntegrationTask {
            name: name.into(),
            kind: IntegrationTaskKind::Deploy,
            run: run.into(),
            depends_on: vec![],
            env_vars: vec![],
            no_cache: true,
            outputs: vec![],
        }
    }

    #[test]
    fn tail_truncate_respects_char_boundaries() {
        // Multi-byte UTF-8: "é" is 2 bytes. Truncating mid-byte would
        // panic. We should slide the boundary forward and prefix with
        // the marker. The output can exceed the limit (marker adds
        // bytes) but must not panic and must preserve the tail.
        let s = "aé".repeat(10); // 30 bytes, 20 chars
        let out = tail_truncate(&s, 10);
        assert!(out.starts_with("…"), "got: {out}");
        assert!(out.ends_with("aé"), "tail preserved; got: {out}");
    }

    #[test]
    fn tail_truncate_passes_through_when_under_limit() {
        assert_eq!(tail_truncate("short", 100), "short");
    }

    #[test]
    fn build_output_excerpt_combines_stdout_and_stderr() {
        let ex = build_output_excerpt(b"deploy URL: https://x", b"warn: slow").unwrap();
        assert!(ex.contains("deploy URL"));
        assert!(ex.contains("[stderr]"));
        assert!(ex.contains("warn: slow"));
    }

    #[test]
    fn build_output_excerpt_none_when_both_empty() {
        assert!(build_output_excerpt(b"", b"").is_none());
    }

    #[test]
    fn integration_task_output_excerpt_populated_on_success() {
        // Task that prints a URL-shaped line on stdout — must show
        // up verbatim in ExecutedTask.output_excerpt so agents /
        // operators see it without digging into the cache.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![IntegrationTask {
                    name: "mock:deploy".into(),
                    kind: IntegrationTaskKind::Deploy,
                    run: "printf 'Deploy URL: https://example.com/deploy/abc123'".into(),
                    depends_on: vec![],
                    env_vars: vec![],
                    no_cache: true,
                    outputs: vec![],
                }],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let deploy = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:deploy")
            .expect("mock:deploy present");
        let output = deploy
            .output_excerpt
            .as_deref()
            .expect("integration task should surface stdout");
        assert!(output.contains("Deploy URL"), "got: {output}");
        assert!(output.contains("abc123"), "got: {output}");
    }

    #[test]
    fn non_integration_task_has_no_output_excerpt() {
        // Adapter/user task stdout should NOT be surfaced —
        // `npm run build` and friends produce multi-KB log noise
        // that would bloat every CI report.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "bento.toml", b"");
        std::fs::create_dir(root.join("bentos")).unwrap();
        write(
            root,
            "bentos/prod.toml",
            br#"name = "prod"
dishes = ["dish"]"#,
        );
        write(
            root,
            "dish/dish.toml",
            br#"name = "d"

[tasks.build]
run = "printf 'hello from build'"
"#,
        );
        let (exec, _cache) = executor(root);
        let report = exec.execute(&CiOptions::default()).unwrap();
        let build = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "build")
            .unwrap();
        assert!(build.output_excerpt.is_none());
    }

    #[test]
    fn integration_task_runs_alongside_build() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        // Side-effectful kinds are excluded by default; explicit kind
        // filter (the `bento deploy` path) is required to include them.
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        // User-declared build + integration-emitted mock:deploy both ran.
        assert!(names.contains(&"build"));
        assert!(names.contains(&"mock:deploy"));
    }

    #[test]
    fn default_ci_excludes_side_effectful_integration_tasks() {
        // A dish has build + deploy integration. Running `bento ci`
        // (no task_kind_filter) must NOT run the deploy task, only
        // build. The deploy verb is explicit via `bento deploy`.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "true"),
                    // Non-side-effectful kinds should still run under
                    // `bento ci` — Release is typical (sentry release
                    // upload after a build).
                    IntegrationTask {
                        name: "mock:release".into(),
                        kind: IntegrationTaskKind::Release,
                        run: "true".into(),
                        depends_on: vec![],
                        env_vars: vec![],
                        no_cache: false,
                        outputs: vec![],
                    },
                ],
            },
        );
        let report = exec.execute(&CiOptions::default()).unwrap();
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert!(names.contains(&"build"), "build should run in ci");
        assert!(
            !names.contains(&"mock:deploy"),
            "deploy-kind task must NOT run in ci (side effect)"
        );
        assert!(
            names.contains(&"mock:release"),
            "release-kind task should run in ci (not side effect)"
        );
    }

    #[test]
    fn deploy_kind_filter_skips_dishes_without_matching_task() {
        let tmp = integration_mock_workspace(&[]);
        // No sentinel means the integration won't detect — dish has no deploy task.
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        // The only task reported is the synthetic Skipped marker.
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert!(matches!(task.outcome, TaskOutcome::Skipped { .. }));
        assert_eq!(task.name, "<no-deploy>");
        // Build didn't run — the kind filter short-circuited the dish.
        assert_eq!(report.summary.built, 0);
    }

    #[test]
    fn deploy_kind_filter_runs_build_and_deploy_for_matching_dish() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "true"),
                    // Include a Release task too; the kind filter should
                    // exclude it because we asked for Deploy only.
                    IntegrationTask {
                        name: "mock:release".into(),
                        kind: IntegrationTaskKind::Release,
                        run: "true".into(),
                        depends_on: vec![],
                        env_vars: vec![],
                        no_cache: false,
                        outputs: vec![],
                    },
                ],
            },
        );
        let opts = CiOptions {
            task_filter: Some(vec!["build".into()]),
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert!(names.contains(&"build"), "build should run as prerequisite");
        assert!(
            names.contains(&"mock:deploy"),
            "deploy-kind task should run"
        );
        assert!(
            !names.contains(&"mock:release"),
            "release-kind task should not run under --task-kind=deploy"
        );
    }

    #[test]
    fn no_cache_task_never_cache_hits() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );

        // Deploy-kind tasks are side-effectful; only the explicit
        // `bento deploy` path (kind_filter = Deploy) runs them.
        let opts = || CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let first = exec.execute(&opts()).unwrap();
        let deploy_outcome = |report: &ExecutionReport| -> TaskOutcome {
            report.bentos[0].dishes[0]
                .tasks
                .iter()
                .find(|t| t.name == "mock:deploy")
                .unwrap()
                .outcome
                .clone()
        };
        assert!(matches!(deploy_outcome(&first), TaskOutcome::Built { .. }));

        let second = exec.execute(&opts()).unwrap();
        // Second run: the deploy must NEVER come back as CacheHit
        // (no_cache=true is what forbids that). It may legitimately
        // come back as `DeployUnchanged` under the d98 skip-if-unchanged
        // gate — inputs are identical — but never as a cached replay of
        // an old deploy's bundle.
        assert!(
            !matches!(deploy_outcome(&second), TaskOutcome::CacheHit),
            "deploy with no_cache=true must not CacheHit; got {:?}",
            deploy_outcome(&second)
        );
    }

    #[test]
    fn deploy_unchanged_short_circuits_second_run() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };

        // First run deploys; state file now records the input hash.
        let first = exec.execute(&opts).unwrap();
        let deploy = |r: &ExecutionReport| -> TaskOutcome {
            r.bentos[0].dishes[0]
                .tasks
                .iter()
                .find(|t| t.name == "mock:deploy")
                .unwrap()
                .outcome
                .clone()
        };
        assert!(matches!(deploy(&first), TaskOutcome::Built { .. }));

        // Second run with identical inputs: skip-if-unchanged fires.
        let second = exec.execute(&opts).unwrap();
        match deploy(&second) {
            TaskOutcome::DeployUnchanged {
                last_deployed_at,
                deploy_url: _,
            } => {
                assert!(
                    last_deployed_at.ends_with('Z'),
                    "last_deployed_at should be RFC 3339, got {last_deployed_at}"
                );
            }
            other => panic!("expected DeployUnchanged on second run, got {other:?}"),
        }
        assert_eq!(
            second.summary.deploy_unchanged, 1,
            "deploy_unchanged should increment on skip"
        );

        // --force re-runs the deploy even though nothing changed.
        let opts_force = CiOptions {
            force_deploy: true,
            ..opts.clone()
        };
        let third = exec.execute(&opts_force).unwrap();
        assert!(
            matches!(deploy(&third), TaskOutcome::Built { .. }),
            "--force should override skip-if-unchanged; got {:?}",
            deploy(&third)
        );

        // State file exists at the expected path after all of the above.
        let state_path = crate::deploy_state::DeployState::path_for(tmp.path());
        assert!(
            state_path.is_file(),
            "deploy state file should exist at {}",
            state_path.display()
        );
    }

    #[test]
    fn required_env_missing_fails_task_fast() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        // Env var name unlikely to ever exist on a host.
        let never_set = "BENTO_TEST_ABSOLUTELY_NOT_SET_ZQX9K2";
        assert!(
            std::env::var(never_set).is_err(),
            "test precondition: {never_set} must be unset"
        );
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![never_set.into()],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let deploy = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:deploy")
            .unwrap();
        match &deploy.outcome {
            TaskOutcome::Failed { stderr_excerpt, .. } => {
                assert!(
                    stderr_excerpt.contains("missing required env"),
                    "stderr: {stderr_excerpt}"
                );
                assert!(stderr_excerpt.contains(never_set));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn required_cli_missing_fails_task_with_install_hint() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        // Binary name unlikely to ever be on a test host.
        let missing_bin = "bento_test_no_such_binary_9fk2x";
        struct CliGatedIntegration {
            missing_bin: &'static str,
        }
        impl Integration for CliGatedIntegration {
            fn id(&self) -> &str {
                "mock-deploy"
            }
            fn detect(&self, dir: &Path) -> bool {
                dir.join("DEPLOY_SENTINEL").exists()
            }
            fn required_cli(&self) -> Vec<bento_adapters::CliRequirement> {
                vec![bento_adapters::CliRequirement::new(
                    self.missing_bin,
                    "brew install no-such-thing",
                )]
            }
            fn detected_tasks(&self, _: &Path, _: &toml::Table) -> Vec<IntegrationTask> {
                vec![IntegrationTask {
                    name: "mock:deploy".into(),
                    kind: IntegrationTaskKind::Deploy,
                    run: "true".into(),
                    depends_on: vec![],
                    env_vars: vec![],
                    no_cache: true,
                    outputs: vec![],
                }]
            }
        }

        let workspace = Workspace::load(tmp.path()).unwrap();
        let registry = AdapterRegistry::empty();
        let integrations = IntegrationRegistry::empty()
            .with_plugins([Box::new(CliGatedIntegration { missing_bin }) as Box<dyn Integration>]);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = LocalCache::new(cache_dir.path());
        let exec = Executor::new(workspace, registry, cache).with_integrations(integrations);
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let deploy = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:deploy")
            .unwrap();
        match &deploy.outcome {
            TaskOutcome::Failed { stderr_excerpt, .. } => {
                assert!(
                    stderr_excerpt.contains("CLI binary not found"),
                    "stderr: {stderr_excerpt}"
                );
                assert!(stderr_excerpt.contains(missing_bin));
                assert!(
                    stderr_excerpt.contains("brew install no-such-thing"),
                    "install hint should surface: {stderr_excerpt}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn secret_alias_satisfies_missing_declared_var() {
        // Task declares `RAILWAY_TOKEN`. The declared name itself is
        // not set in the host env, but we alias it to `PATH` (which
        // definitely is set). The preflight should pass via the alias.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        // Use a declared name guaranteed not to exist so the alias
        // is the only way the task can resolve it.
        let declared = "BENTO_TEST_ALIAS_ONLY_XQK2L";
        assert!(std::env::var(declared).is_err());
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![declared.into()],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let mut aliases = std::collections::BTreeMap::new();
        aliases.insert(declared.to_string(), "PATH".to_string());
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            secret_aliases: aliases,
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let deploy = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:deploy")
            .unwrap();
        assert!(
            matches!(deploy.outcome, TaskOutcome::Built { .. }),
            "alias should satisfy the declared requirement; got {:?}",
            deploy.outcome
        );
    }

    #[test]
    fn alias_error_surfaces_both_declared_and_source_names() {
        // Both declared and source are unset — the failure message
        // should mention both so the operator knows what bento looked
        // for (the declared name) and where (the source name).
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let declared = "BENTO_TEST_DECLARED_ZZZ9";
        let source = "BENTO_TEST_SOURCE_ZZZ9";
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![declared.into()],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let mut aliases = std::collections::BTreeMap::new();
        aliases.insert(declared.to_string(), source.to_string());
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            secret_aliases: aliases,
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let deploy = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:deploy")
            .unwrap();
        match &deploy.outcome {
            TaskOutcome::Failed { stderr_excerpt, .. } => {
                assert!(
                    stderr_excerpt.contains(declared),
                    "stderr: {stderr_excerpt}"
                );
                assert!(stderr_excerpt.contains(source), "stderr: {stderr_excerpt}");
                assert!(stderr_excerpt.contains("via $"), "stderr: {stderr_excerpt}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn required_env_present_allows_task_to_run() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        // PATH is always set during rustc tests.
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec!["PATH".into()],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let deploy = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:deploy")
            .unwrap();
        assert!(
            matches!(deploy.outcome, TaskOutcome::Built { .. }),
            "expected Built, got {:?}",
            deploy.outcome
        );
    }

    // ── Garnish (Notify-kind) fan-out tests ────────────────────────

    fn notify_task(name: &str, run: &str) -> IntegrationTask {
        IntegrationTask {
            name: name.into(),
            kind: IntegrationTaskKind::Notify,
            run: run.into(),
            depends_on: vec![],
            env_vars: vec![],
            no_cache: true,
            outputs: vec![],
        }
    }

    #[test]
    fn bento_ci_does_not_run_notify_tasks() {
        // Default `bento ci` path — `run_notify_kinds` is false — must
        // never fire Notify-kind tasks even when an integration emits
        // them. Prevents Slack spam on every ci invocation.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "true"),
                    notify_task("mock:notify", "true"),
                ],
            },
        );
        let report = exec.execute(&CiOptions::default()).unwrap();
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert!(!names.contains(&"mock:notify"), "notify must not run in ci");
        assert!(!names.contains(&"mock:deploy"), "deploy must not run in ci");
    }

    #[test]
    fn bento_deploy_fires_notify_after_each_deploy() {
        // With run_notify_kinds = true (set by `bento deploy`), every
        // Notify-kind task in the dish fires once the Deploy completes.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let payload_sink = tmp.path().join("dish/payload.txt");
        let sink_str = payload_sink.display().to_string();
        let notify_run = format!("cat > {sink_str}");
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "printf 'Build Logs: https://x/abc'"),
                    notify_task("mock:notify", &notify_run),
                ],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            run_notify_kinds: true,
            environment: Some("staging".into()),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert!(names.contains(&"mock:deploy"), "deploy should run");
        assert!(names.contains(&"mock:notify"), "notify should fire");
        // Notify task's stdin got the JSON payload — the mock wrote
        // it to a file. Verify the payload shape round-trips.
        let payload_bytes = std::fs::read(&payload_sink).expect("notify should have written stdin");
        let payload: crate::GarnishPayload = serde_json::from_slice(payload_bytes.trim_ascii_end())
            .unwrap_or_else(|e| panic!("payload not valid JSON: {e} / {payload_bytes:?}"));
        assert_eq!(
            payload.schema_version,
            crate::GARNISH_PAYLOAD_SCHEMA_VERSION
        );
        assert_eq!(payload.environment.as_deref(), Some("staging"));
        assert_eq!(payload.trigger.task_name, "mock:deploy");
        assert_eq!(payload.trigger.dish_name, "d");
        assert_eq!(payload.trigger.bento_name, "prod");
        assert_eq!(payload.trigger.outcome, "built");
        assert_eq!(payload.trigger.integration_kind, "deploy");
        assert!(payload.trigger.output_excerpt.contains("Build Logs"));
        assert!(payload.trigger.stderr_excerpt.is_none());
    }

    #[test]
    fn notify_failure_does_not_fail_build() {
        // A notify script that exits nonzero — common case is a
        // Slack webhook 5xx. The build's exit code must stay clean;
        // the only signal is `summary.notify_failures` and a warn log.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "true"),
                    notify_task("mock:notify-bad", "exit 7"),
                ],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            run_notify_kinds: true,
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        assert_eq!(
            report.summary.notify_failures, 1,
            "notify failure should increment counter"
        );
        assert_eq!(
            report.summary.failed, 0,
            "notify failures must NOT fold into `failed`"
        );
        let notify = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .find(|t| t.name == "mock:notify-bad")
            .expect("notify row present");
        match &notify.outcome {
            TaskOutcome::Failed { exit_code, .. } => assert_eq!(*exit_code, 7),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn notify_tasks_fan_out_in_parallel() {
        // Two notify tasks that each sleep for a non-trivial slice.
        // If they ran serially, the dish's wall-clock would be ≥ 2×
        // the per-task sleep. With fan-out it's ≈ 1×.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let sleep_ms = 300; // big enough to dwarf thread-spawn overhead
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "true"),
                    notify_task(
                        "mock:notify-a",
                        &format!("sleep {}", sleep_ms as f64 / 1000.0),
                    ),
                    notify_task(
                        "mock:notify-b",
                        &format!("sleep {}", sleep_ms as f64 / 1000.0),
                    ),
                ],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            run_notify_kinds: true,
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let report = exec.execute(&opts).unwrap();
        let wall_ms = start.elapsed().as_millis() as u64;
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert!(names.contains(&"mock:notify-a"));
        assert!(names.contains(&"mock:notify-b"));
        // Serial would be ≥ 600ms; parallel should be well under that.
        // Allow generous slack (e.g. CI jitter) — we just want to
        // detect an obviously-serial regression.
        assert!(
            wall_ms < (sleep_ms * 2) - 50,
            "notify tasks ran serially: wall={wall_ms}ms (expected < {}ms)",
            sleep_ms * 2 - 50
        );
    }

    #[test]
    fn deploy_failure_notify_payload_has_failed_outcome() {
        // When the Deploy task itself fails, the garnish still fires
        // with `outcome: "failed"` + stderr_excerpt populated — so
        // PagerDuty-style notify scripts can trigger on the shape.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let payload_sink = tmp.path().join("dish/failed_payload.txt");
        let sink_str = payload_sink.display().to_string();
        let notify_run = format!("cat > {sink_str}");
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "echo 'boom' >&2; exit 9"),
                    notify_task("mock:notify", &notify_run),
                ],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            run_notify_kinds: true,
            fail_fast: Some(false),
            ..Default::default()
        };
        let report = exec.execute(&opts).unwrap();
        // Deploy failed — should fold into `failed`.
        assert_eq!(report.summary.failed, 1);
        let payload_bytes =
            std::fs::read(&payload_sink).expect("notify should still fire on failed deploy");
        let payload: crate::GarnishPayload =
            serde_json::from_slice(payload_bytes.trim_ascii_end()).unwrap();
        assert_eq!(payload.trigger.outcome, "failed");
        assert_eq!(payload.trigger.exit_code, 9);
        assert!(payload
            .trigger
            .stderr_excerpt
            .as_deref()
            .unwrap_or("")
            .contains("boom"));
    }

    #[test]
    fn garnish_sidecar_written_after_deploy() {
        // Every completed Deploy should leave a sidecar under
        // `.bento/garnish/<bento>/<dish>/<task>.json` so a follow-up
        // `bento notify` has something to replay — even when the
        // deploy itself ran with no notify tasks wired up.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![deploy_task(
                    "mock:deploy",
                    "printf 'Build Logs: https://x/abc'",
                )],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            // run_notify_kinds = false: no notify phase, but the
            // sidecar must still land on disk.
            ..Default::default()
        };
        exec.execute(&opts).unwrap();
        let sidecar = tmp.path().join(".bento/garnish/prod/d/mock_deploy.json");
        assert!(
            sidecar.exists(),
            "sidecar should be written at {}",
            sidecar.display()
        );
        let bytes = std::fs::read(&sidecar).unwrap();
        let payload: crate::GarnishPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload.trigger.task_name, "mock:deploy");
        assert_eq!(payload.trigger.outcome, "built");
        assert!(payload.trigger.output_excerpt.contains("Build Logs"));
    }

    #[test]
    fn bento_notify_replays_sidecar_through_notify_tasks() {
        // Deploy once (writes sidecar), then simulate a fresh invocation:
        // `bento notify` should fire the Notify task with the sidecar
        // payload on stdin, without re-running the deploy.
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let notify_sink = tmp.path().join("dish/resend.txt");
        let sink_str = notify_sink.display().to_string();
        let notify_run = format!("cat > {sink_str}");
        let mk_exec = || {
            integration_mock_executor(
                tmp.path(),
                MockIntegration {
                    id: "mock-deploy",
                    detect_sentinel: "DEPLOY_SENTINEL",
                    required_env_vars: vec![],
                    tasks: vec![
                        deploy_task("mock:deploy", "printf 'Deploy URL: https://x/abc'"),
                        notify_task("mock:notify", &notify_run),
                    ],
                },
            )
        };

        // First: a real deploy. This writes the sidecar.
        let (exec1, _cache1) = mk_exec();
        let opts_deploy = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Deploy),
            run_notify_kinds: true,
            environment: Some("staging".into()),
            ..Default::default()
        };
        exec1.execute(&opts_deploy).unwrap();
        // Clear the notify sink so we can tell the resend path from
        // the original Phase-2 fire.
        std::fs::remove_file(&notify_sink).unwrap();

        // Now: notify-only replay. No deploy runs.
        let (exec2, _cache2) = mk_exec();
        let opts_notify = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Notify),
            run_notify_kinds: true,
            ..Default::default()
        };
        let report = exec2.notify_only(&opts_notify).unwrap();
        // Sidecar replay must have triggered the notify task.
        let replayed = std::fs::read(&notify_sink).expect("notify should replay from sidecar");
        let payload: crate::GarnishPayload =
            serde_json::from_slice(replayed.trim_ascii_end()).unwrap();
        assert_eq!(payload.trigger.task_name, "mock:deploy");
        assert_eq!(payload.environment.as_deref(), Some("staging"));
        // Report contains only the notify row — no deploy, no build.
        let names: Vec<&str> = report.bentos[0].dishes[0]
            .tasks
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["mock:notify"]);
    }

    #[test]
    fn bento_notify_with_no_prior_deploy_emits_skipped_marker() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![
                    deploy_task("mock:deploy", "true"),
                    notify_task("mock:notify", "true"),
                ],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Notify),
            run_notify_kinds: true,
            ..Default::default()
        };
        let report = exec.notify_only(&opts).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.name, "<no-prior-deploy>");
        match &task.outcome {
            TaskOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("run `bento deploy` first"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert_eq!(
            report.summary.tasks, 0,
            "skipped markers do not count as tasks"
        );
    }

    #[test]
    fn bento_notify_on_dish_without_notify_tasks_skips_cleanly() {
        let tmp = integration_mock_workspace(&[("DEPLOY_SENTINEL", b"")]);
        let (exec, _cache) = integration_mock_executor(
            tmp.path(),
            MockIntegration {
                id: "mock-deploy",
                detect_sentinel: "DEPLOY_SENTINEL",
                required_env_vars: vec![],
                tasks: vec![deploy_task("mock:deploy", "true")],
            },
        );
        let opts = CiOptions {
            task_kind_filter: Some(IntegrationTaskKind::Notify),
            run_notify_kinds: true,
            ..Default::default()
        };
        let report = exec.notify_only(&opts).unwrap();
        let task = &report.bentos[0].dishes[0].tasks[0];
        assert_eq!(task.name, "<no-notify>");
        assert!(matches!(task.outcome, TaskOutcome::Skipped { .. }));
    }
}
