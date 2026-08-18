//! `bento-mcp` — Model Context Protocol server for bento.
//!
//! Exposes bento's CLI verb surface as typed tool calls over stdio
//! JSON-RPC. MCP clients (Claude Desktop, Claude Code, Cursor, Codex)
//! auto-discover the tool list and invoke them without shelling out
//! to `bento` or parsing `--json` stdout.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bento_config::Workspace;
use clap::Parser;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CallToolResult, Content, ProgressNotificationParam, ProtocolVersion, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

mod workspace_ctx;

use workspace_ctx::WorkspaceCtx;

/// CLI flags for `bento-mcp`.
///
/// Deliberately tiny — MCP servers are launched by clients via a
/// `command` + `args` config block, so every flag we accept has to be
/// safe as a server-lifetime default.
#[derive(Parser, Debug)]
#[command(
    name = "bento-mcp",
    version,
    about = "MCP server for bento — agent-facing tool surface over stdio"
)]
struct Cli {
    /// Pin the server to a specific workspace root. When unset, tools
    /// fall back to `$BENTO_WORKSPACE_ROOT` or the process cwd.
    /// Individual tool calls MAY override this via a per-call
    /// `workspace` input (once Phase 1 tools ship).
    #[arg(long, value_name = "PATH", env = "BENTO_WORKSPACE_ROOT")]
    workspace: Option<PathBuf>,
}

/// The MCP server's single shared handler.
///
/// Holds a [`WorkspaceCtx`] (currently just the resolved root; Phase 1
/// will add a cached `Workspace` + `LocalCache`) plus the macro-built
/// tool router.
#[derive(Clone)]
struct BentoServer {
    ctx: Arc<Mutex<WorkspaceCtx>>,
    // The macro `#[tool_handler]` expands to code that routes
    // tools/call via this field, but the macro expansion isn't
    // visible to the dead-code pass. Silence the warning without
    // opting the whole struct out of lints.
    #[allow(dead_code)]
    tool_router: ToolRouter<BentoServer>,
}

#[tool_router]
impl BentoServer {
    fn new(ctx: WorkspaceCtx) -> Self {
        Self {
            ctx: Arc::new(Mutex::new(ctx)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Agent orientation — workspace inventory, cache state, \
                       plan preview, and a ranked list of recommended next \
                       verbs. Call this first in a fresh session. Advisory \
                       only; does not execute tasks and does not hit the \
                       network. Same output shape as `bento prime --json`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn prime(&self) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let root = self.require_workspace_root().await?;
                let workspace = Workspace::load(&root)?;
                let out = bento_core::prime::compute(&workspace)?;
                anyhow::Ok(serde_json::to_value(&out)?)
            }
            .await,
        ))
    }

    #[tool(
        description = "JSON Schema for a named bento output. `target` must \
                       be one of: plan, report, manifest, doctor, \
                       diagnostics, garnish-payload, prime. Matches \
                       `bento schema <target>` for the bento-core-owned \
                       types.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn schema(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            SchemaArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        Ok(render_schema(&input.target))
    }

    #[tool(
        description = "Cache-aware task plan — which tasks would hit, \
                       miss, or skip on `bento ci`. Same output shape as \
                       `bento plan --json`, including per-task miss_reason \
                       and workspace-level orphan dish.toml list. \
                       `target` accepts a bento or dish name (like \
                       `bento plan <target>`); omit to plan every bento.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn plan(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            PlanArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let root = self.require_workspace_root().await?;

                let mut bento_filter = input.bento.clone();
                let mut dish_filter: Option<String> = None;
                if let Some(target) = &input.target {
                    let workspace = Workspace::load(&root)?;
                    match bento_core::resolve_target(&workspace, target)? {
                        bento_core::TargetRef::Bento(name) => bento_filter = Some(name),
                        bento_core::TargetRef::Dish(name) => dish_filter = Some(name),
                    }
                }

                let opts = bento_core::PlanOptions {
                    bento_filter,
                    dish_filter,
                    no_cache: input.no_cache.unwrap_or(false),
                    since: input.since,
                    ..Default::default()
                };
                let plan = bento_core::plan_at(&root, &opts)?;
                anyhow::Ok(serde_json::to_value(&plan)?)
            }
            .await,
        ))
    }

    #[tool(
        description = "List every dish in the workspace with its path, \
                       language, and which bentos include it. Flags orphan \
                       dish.toml files on disk. Same output shape as \
                       `bento dish list --json`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn dish_list(&self) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let root = self.require_workspace_root().await?;
                let workspace = Workspace::load(&root)?;
                let out = bento_core::inventory::dish_list(&workspace);
                anyhow::Ok(serde_json::to_value(&out)?)
            }
            .await,
        ))
    }

    #[tool(
        description = "List every bento in the workspace with its source \
                       file and the dishes it includes. Same output shape \
                       as `bento box list --json`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn box_list(&self) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let root = self.require_workspace_root().await?;
                let workspace = Workspace::load(&root)?;
                let out = bento_core::inventory::box_list(&workspace);
                anyhow::Ok(serde_json::to_value(&out)?)
            }
            .await,
        ))
    }

    #[tool(
        description = "Structured health checks over the workspace — \
                       config parse, toolchain pins, integrations, local + \
                       remote cache, git, orphan dishes. Pass `cloud: true` \
                       to also probe cache.bento.build / api.bento.build \
                       reachability. Same output shape as `bento doctor \
                       --json`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true,
            idempotent_hint = true
        )
    )]
    async fn doctor(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            DoctorArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let root = self.require_workspace_root().await?;
                let aliases = std::collections::BTreeMap::new();
                let options = bento_core::doctor::DoctorOptions {
                    cloud: input.cloud.unwrap_or(false),
                };
                let report = bento_core::doctor::run_with_options(&root, &aliases, options)?;
                anyhow::Ok(serde_json::to_value(&report)?)
            }
            .await,
        ))
    }

    #[tool(
        description = "Explain a cache entry — returns the stored input \
                       manifest (every hashed file, toolchain, env var). \
                       `target` is either `<dish>:<task>` (e.g. \
                       `marketing:lint`) or a cache-key hex prefix. Same \
                       output shape as `bento why <target> --json`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn why(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            WhyArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let target = input.target;
                let cache = bento_core::LocalCache::new(bento_core::default_cache_root()?);

                let prefix: String = if target.contains(':') {
                    let root = self.require_workspace_root().await?;
                    bento_core::why::resolve_dish_task_key(&root, &target)?
                } else {
                    if target.is_empty() || !target.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(bento_core::why::WhyTargetError::InvalidDishTask {
                            input: target.clone(),
                        }
                        .into());
                    }
                    target.clone()
                };

                let results = bento_core::why::explain(&cache, &prefix)?;
                if results.is_empty() && target.contains(':') {
                    let (dish, task) = target.split_once(':').unwrap();
                    return Err(bento_core::why::WhyTargetError::NoCacheEntry {
                        dish: dish.to_string(),
                        task: task.to_string(),
                        key: prefix,
                    }
                    .into());
                }

                anyhow::Ok(serde_json::to_value(&results)?)
            }
            .await,
        ))
    }

    #[tool(
        description = "Resolved absolute output paths per dish — walks \
                       each dish's `[outputs]` (dish-level plus task-level, \
                       deduped) against the filesystem. Dishes with no \
                       resolved artefacts are omitted. Same output shape as \
                       `bento artifacts --json`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn artifacts(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ArtifactsArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_result(
            async {
                let root = self.require_workspace_root().await?;
                let workspace = Workspace::load(&root)?;
                let by_dish = bento_core::artifacts::collect(&workspace, input.bento.as_deref())?;
                let payload: std::collections::BTreeMap<String, Vec<String>> = by_dish
                    .iter()
                    .map(|(name, paths)| {
                        (
                            name.clone(),
                            paths.iter().map(|p| p.display().to_string()).collect(),
                        )
                    })
                    .collect();
                anyhow::Ok(serde_json::to_value(&payload)?)
            }
            .await,
        ))
    }

    // ── Phase 2: execution tools ───────────────────────────────────

    #[tool(
        description = "Install dish dependencies (node_modules, vendor, \
                       .venv, …) via each adapter's native command. \
                       Same behaviour as `bento install` — skips the task \
                       loop entirely; the returned report has install \
                       records but no task rows. Pass `force: true` to \
                       run install unconditionally, ignoring the \
                       adapter's probe.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn install(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            InstallArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(report_result(
            async {
                let root = self.require_workspace_root().await?;
                let (bento_filter, dish_filter) = self
                    .resolve_target_filters(&root, input.target.as_deref())
                    .await?;
                let opts = bento_core::CiOptions {
                    bento_filter,
                    dish_filter,
                    task_filter: None,
                    no_cache: false,
                    fail_fast: None,
                    skip_install: false,
                    force_install: input.force.unwrap_or(false),
                    task_kind_filter: None,
                    install_only: true,
                    secret_aliases: std::collections::BTreeMap::new(),
                    run_notify_kinds: false,
                    environment: None,
                    force_deploy: false,
                };
                run_blocking(&root, &opts, ctx).await
            }
            .await,
        ))
    }

    #[tool(
        description = "Build a bento or a single dish. Resolves adapter \
                       `build` tasks + user-defined tasks named `build`; \
                       cache-hit tasks skip execution. Same behaviour as \
                       `bento build [target]` — returns an \
                       ExecutionReport.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn build(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "build", ctx).await
    }

    #[tool(
        description = "Fast type-check via the adapter's `check` task — \
                       `cargo check --locked --all-targets` for cargo, \
                       `go vet ./...` for go. Order of magnitude faster \
                       than `build` for catching compile / type \
                       errors during agent iteration. Cache hits skip \
                       execution. Same behaviour as `bento check [target]`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn check(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "check", ctx).await
    }

    #[tool(
        description = "Run every `test` task for a bento or dish. Cache \
                       hits skip execution. Same behaviour as \
                       `bento test [target]`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn test(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "test", ctx).await
    }

    #[tool(
        description = "Run every `lint` task for a bento or dish. Cache \
                       hits skip execution. Same behaviour as \
                       `bento lint [target]`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn lint(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "lint", ctx).await
    }

    #[tool(
        description = "Full CI pass — build + test + lint across every \
                       bento/dish (or a `target` if provided). Install is \
                       performed first, then every adapter/user task \
                       except integration Deploy/Notify tasks. Same \
                       behaviour as `bento ci`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false,
            idempotent_hint = true
        )
    )]
    async fn ci(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(report_result(
            async {
                let root = self.require_workspace_root().await?;
                let (bento_filter, dish_filter) = self
                    .resolve_target_filters(&root, input.target.as_deref())
                    .await?;
                let opts = bento_core::CiOptions {
                    bento_filter,
                    dish_filter,
                    task_filter: None,
                    no_cache: input.no_cache.unwrap_or(false),
                    fail_fast: None,
                    skip_install: input.skip_install.unwrap_or(false),
                    force_install: input.force_install.unwrap_or(false),
                    task_kind_filter: None,
                    install_only: false,
                    secret_aliases: std::collections::BTreeMap::new(),
                    run_notify_kinds: false,
                    environment: None,
                    force_deploy: false,
                };
                run_blocking(&root, &opts, ctx).await
            }
            .await,
        ))
    }

    // ── Phase 3a: destructive-external tools ───────────────────────

    #[tool(
        description = "Deploy a target (bento or dish) to a named \
                       environment via its configured integration \
                       (Railway / Cloudflare Pages / Cloudflare Workers / \
                       …). Build is run first as a prerequisite so \
                       deploys never ship stale artefacts. \
                       DESTRUCTIVE — touches remote infrastructure. \
                       `env` MUST be declared in `[environments.<env>]`. \
                       Pass `preview: true` for preview / staging shape \
                       deploys, `rollback: true` to revert to the prior \
                       deploy. `secret_from` is a name-to-source alias \
                       map (VALUES never appear on this wire).",
        annotations(
            destructive_hint = true,
            open_world_hint = true,
            read_only_hint = false,
            idempotent_hint = false
        )
    )]
    async fn deploy(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            DeployArgs,
        >,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(report_result(self.deploy_inner(input, ctx).await))
    }

    #[tool(
        description = "Re-fire Notify-kind integration tasks (garnishes — \
                       Slack, Linear, GitHub, …) against the persisted \
                       garnish payload from the last deploy. No deploy, \
                       no build — just the hooks. Useful when fixing a \
                       broken webhook without touching code. \
                       DESTRUCTIVE + open-world because it sends \
                       outbound messages. `env` MUST be declared.",
        annotations(
            destructive_hint = true,
            open_world_hint = true,
            read_only_hint = false,
            idempotent_hint = false
        )
    )]
    async fn notify(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            NotifyArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        Ok(report_result(
            async {
                let root = self.require_workspace_root().await?;
                let workspace = Workspace::load(&root)?;

                let (bento_filter, dish_filter) = self
                    .resolve_target_filters(&root, input.target.as_deref())
                    .await?;

                let secret_aliases = resolve_secret_aliases(
                    &workspace,
                    Some(&input.env),
                    input.secret_from.as_ref(),
                )?;

                let opts = bento_core::CiOptions {
                    bento_filter,
                    dish_filter,
                    task_filter: None,
                    no_cache: false,
                    fail_fast: None,
                    skip_install: true,
                    force_install: false,
                    task_kind_filter: Some(bento_core::IntegrationTaskKind::Notify),
                    install_only: false,
                    secret_aliases,
                    run_notify_kinds: true,
                    environment: Some(input.env),
                    force_deploy: false,
                };

                let opts_for_run = opts.clone();
                tokio::task::spawn_blocking(move || bento_core::notify_at(&root, &opts_for_run))
                    .await
                    .map_err(|e| anyhow::anyhow!("task join failed: {e}"))?
            }
            .await,
        ))
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DeployArgs {
    /// Bento or dish name to deploy. Required for destructive tools —
    /// explicit targets only.
    target: String,
    /// Named deploy environment from `bento.toml`
    /// (`[environments.<env>]`). Applies that environment's
    /// `secrets.*` aliases before running.
    env: String,
    /// Run a preview / staging-shape deploy instead of production.
    #[serde(default)]
    preview: Option<bool>,
    /// Roll back to the previous deploy. Integrations that don't
    /// support rollback will report a Skipped row instead.
    #[serde(default)]
    rollback: Option<bool>,
    /// `bento deploy --force`: skip the deploy-unchanged short-circuit
    /// so a forced re-deploy always executes.
    #[serde(default)]
    force: Option<bool>,
    /// `bento deploy --no-notify`: skip the post-deploy garnish fan-out
    /// (Slack / Linear / custom Notify-kind tasks). Useful for re-deploys
    /// after a fix when you don't want to re-spam the channel.
    #[serde(default)]
    no_notify: Option<bool>,
    /// Alias a declared env-var name to a source env-var name, read
    /// from the host environment and exposed to the task under the
    /// declared name. VALUES are never accepted here — only
    /// name-to-name indirection.
    #[serde(default)]
    secret_from: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NotifyArgs {
    /// Bento or dish name. Omit to notify every dish that has a prior
    /// garnish payload persisted.
    #[serde(default)]
    target: Option<String>,
    /// Named deploy environment — same requirement as `deploy`.
    env: String,
    /// Ad-hoc env-var alias map (same shape as `deploy.secret_from`).
    #[serde(default)]
    secret_from: Option<std::collections::BTreeMap<String, String>>,
}

/// Mirror the CLI's `resolve_secret_aliases` — layer `--secret-from`
/// on top of `[environments.<env>].secrets` so an MCP tool's
/// `secret_from` input can override a named-environment default.
///
/// Never touches the process env — only name-to-name indirection.
fn resolve_secret_aliases(
    workspace: &Workspace,
    env: Option<&str>,
    secret_from: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut aliases = std::collections::BTreeMap::new();
    if let Some(name) = env {
        let Some(environment) = workspace.repo.environments.get(name) else {
            let known: Vec<&String> = workspace.repo.environments.keys().collect();
            anyhow::bail!(
                "environment `{name}` is not defined in bento.toml \
                 (known: {known:?}). Add an `[environments.{name}]` \
                 block with `secrets.<VAR> = \"<SOURCE_VAR>\"` entries."
            );
        };
        for (declared, source) in &environment.secrets {
            aliases.insert(declared.clone(), source.clone());
        }
    }
    if let Some(sf) = secret_from {
        for (declared, source) in sf {
            aliases.insert(declared.clone(), source.clone());
        }
    }
    Ok(aliases)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct InstallArgs {
    /// Bento or dish name. Omit to install every dish.
    #[serde(default)]
    target: Option<String>,
    /// Run install unconditionally, ignoring the adapter's probe.
    /// Useful when the probe can't see a subtle `node_modules`
    /// corruption that's tripping builds.
    #[serde(default)]
    force: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ExecArgs {
    /// Bento or dish name. Omit to run every bento.
    #[serde(default)]
    target: Option<String>,
    /// Bypass cache lookups (still writes results to cache).
    #[serde(default)]
    no_cache: Option<bool>,
    /// Skip the adapter install probe entirely — assumes deps are
    /// already populated (e.g. containerised CI).
    #[serde(default)]
    skip_install: Option<bool>,
    /// Force install to run regardless of the probe result.
    #[serde(default)]
    force_install: Option<bool>,
}

async fn run_blocking(
    root: &std::path::Path,
    opts: &bento_core::CiOptions,
    ctx: RequestContext<RoleServer>,
) -> Result<bento_core::ExecutionReport> {
    let root = root.to_path_buf();
    let opts = opts.clone();
    let ct = ctx.ct.clone();

    // Progress is only legal when the client asked for it with a
    // progressToken. The executor calls the observer from its own
    // thread, so hand dishes to the async side over a channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    match ctx.meta.get_progress_token() {
        Some(token) => {
            let peer = ctx.peer.clone();
            tokio::spawn(async move {
                let mut done = 0.0;
                while let Some(message) = rx.recv().await {
                    done += 1.0;
                    let _ = peer
                        .notify_progress(
                            ProgressNotificationParam::new(token.clone(), done)
                                .with_message(message),
                        )
                        .await;
                }
            });
        }
        None => drop(rx),
    }

    // `ci_at` is synchronous but internally spawns a tokio runtime
    // for the S3Remote cache + runs child processes that block.
    // Running it directly from this async tool handler would nest
    // tokio runtimes and panic on drop — delegate to the blocking
    // thread pool instead.
    tokio::task::spawn_blocking(move || {
        bento_core::ci_at_with(&root, &opts, |executor| {
            executor
                .with_cancel(move || ct.is_cancelled())
                .with_observer(move |dish| {
                    let _ = tx.send(dish_progress(dish));
                })
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("task join failed: {e}"))?
}

fn dish_progress(dish: &bento_core::ExecutedDish) -> String {
    let failed = dish
        .tasks
        .iter()
        .filter(|t| matches!(t.outcome, bento_core::TaskOutcome::Failed { .. }))
        .count();
    match failed {
        0 => format!("{} ok ({} task(s))", dish.name, dish.tasks.len()),
        n => format!("{} FAILED ({n} of {} task(s))", dish.name, dish.tasks.len()),
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DoctorArgs {
    /// Add cloud probes (bento:// token validation, cache.bento.build
    /// and api.bento.build health pings). Off by default — the rest
    /// of doctor is non-network.
    #[serde(default)]
    cloud: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WhyArgs {
    /// `<dish>:<task>` (e.g. `marketing:lint`) or a cache-key hex
    /// prefix.
    target: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ArtifactsArgs {
    /// Restrict to a single bento by name. When omitted, returns
    /// artefacts for every dish in every bento.
    #[serde(default)]
    bento: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PlanArgs {
    /// Bento or dish name to restrict the plan to (same shape as
    /// `bento plan <target>`). When omitted, plans every bento.
    #[serde(default)]
    target: Option<String>,
    /// Restrict to a single bento by name (global `--bento` flag
    /// equivalent). Compounds with `target` as an additional filter.
    #[serde(default)]
    bento: Option<String>,
    /// Treat every task as a cache miss (skip cache lookup). Same as
    /// the CLI's `--no-cache` flag.
    #[serde(default)]
    no_cache: Option<bool>,
    /// Git base ref for change detection; dishes without changed
    /// inputs short-circuit to `skipped_diff_clean`.
    #[serde(default)]
    since: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SchemaArgs {
    /// One of: plan, report, manifest, doctor, diagnostics,
    /// garnish-payload, prime.
    target: String,
}

fn render_schema(target: &str) -> CallToolResult {
    // Hand-dispatch keeps bento-mcp decoupled from bento-cli's
    // SchemaTarget enum. Three CLI-only targets (error / scaffold /
    // why) aren't exposed here yet — add cases when those types move
    // into bento-core.
    let schema = match target {
        "plan" => schemars::schema_for!(bento_core::Plan),
        "report" => schemars::schema_for!(bento_core::ExecutionReport),
        "manifest" => schemars::schema_for!(bento_core::InputManifest),
        "doctor" => schemars::schema_for!(bento_core::DoctorReport),
        "diagnostics" => schemars::schema_for!(bento_core::Diagnostic),
        "garnish-payload" => schemars::schema_for!(bento_core::GarnishPayload),
        "prime" => schemars::schema_for!(bento_core::prime::Output),
        other => {
            return envelope_result(
                bento_core::BentoError::new(
                    "unknown_schema_target",
                    format!("unknown schema target '{other}'"),
                )
                .with_hint(
                    "expected one of: plan, report, manifest, doctor, \
                     diagnostics, garnish-payload, prime",
                )
                .with_next_steps(["re-call `schema` with one of: plan, report, manifest, \
                     doctor, diagnostics, garnish-payload, prime"]),
            );
        }
    };
    match serde_json::to_value(schema) {
        Ok(value) => CallToolResult::structured(value),
        Err(e) => envelope_result(bento_core::BentoError::new("internal", e.to_string())),
    }
}

impl BentoServer {
    async fn require_workspace_root(&self) -> Result<std::path::PathBuf> {
        self.ctx.lock().await.require_root()
    }

    /// Everything `deploy` does apart from wrapping the outcome — kept
    /// out of the `#[tool]` fn so the whole flow can use `?` and land
    /// in one classified envelope.
    async fn deploy_inner(
        &self,
        input: DeployArgs,
        ctx: RequestContext<RoleServer>,
    ) -> Result<bento_core::ExecutionReport> {
        let root = self.require_workspace_root().await?;
        let workspace = Workspace::load(&root)?;

        let kind = if input.rollback.unwrap_or(false) {
            bento_core::IntegrationTaskKind::Rollback
        } else if input.preview.unwrap_or(false) {
            bento_core::IntegrationTaskKind::DeployPreview
        } else {
            bento_core::IntegrationTaskKind::Deploy
        };

        let mut bento_filter: Option<String> = None;
        let mut dish_filter: Option<String> = None;
        match bento_core::resolve_target(&workspace, &input.target)? {
            bento_core::TargetRef::Bento(name) => bento_filter = Some(name),
            bento_core::TargetRef::Dish(name) => dish_filter = Some(name),
        }

        // Single-dish preflight — match the CLI's integration_not_configured
        // classification so destructive tool calls fail fast instead of
        // round-tripping an empty ExecutionReport.
        let single_dish_preflight: Option<(String, Vec<String>)> =
            dish_filter.as_ref().and_then(|name| {
                workspace.dishes_by_name.get(name).map(|d| {
                    (
                        name.clone(),
                        d.config.integrations.keys().cloned().collect(),
                    )
                })
            });

        let secret_aliases =
            resolve_secret_aliases(&workspace, Some(&input.env), input.secret_from.as_ref())?;

        let opts = bento_core::CiOptions {
            bento_filter,
            dish_filter,
            task_filter: Some(vec!["build".to_string()]),
            no_cache: false,
            fail_fast: None,
            skip_install: false,
            force_install: false,
            task_kind_filter: Some(kind),
            install_only: false,
            secret_aliases,
            run_notify_kinds: !input.no_notify.unwrap_or(false),
            environment: Some(input.env.clone()),
            force_deploy: input.force.unwrap_or(false),
        };

        let report = run_blocking(&root, &opts, ctx).await?;

        // Post-run: explicit single dish + only <no-{kind}> rows →
        // classified integration_not_configured.
        if let Some((dish, configured_integrations)) = single_dish_preflight {
            let no_integration_marker = format!("<no-{}>", kind.as_str());
            if let Some(d) = report
                .bentos
                .iter()
                .flat_map(|b| &b.dishes)
                .find(|d| d.name == dish)
            {
                let all_skips =
                    !d.tasks.is_empty() && d.tasks.iter().all(|t| t.name == no_integration_marker);
                if all_skips {
                    return Err(bento_core::DeployError::IntegrationNotConfigured {
                        dish,
                        kind: kind.as_str().to_string(),
                        configured_integrations,
                    }
                    .into());
                }
            }
        }

        Ok(report)
    }

    /// Resolve a `target` string (bento or dish name) into `(bento_filter,
    /// dish_filter)` via `bento_core::resolve_target`. When `target` is
    /// `None`, both filters come back `None` (run every bento).
    async fn resolve_target_filters(
        &self,
        root: &std::path::Path,
        target: Option<&str>,
    ) -> Result<(Option<String>, Option<String>)> {
        let Some(target) = target else {
            return Ok((None, None));
        };
        let workspace = Workspace::load(root)?;
        match bento_core::resolve_target(&workspace, target)? {
            bento_core::TargetRef::Bento(name) => Ok((Some(name), None)),
            bento_core::TargetRef::Dish(name) => Ok((None, Some(name))),
        }
    }

    /// Shared machinery for build / test / lint — they differ only in
    /// `task_filter` value, everything else is the same shape.
    async fn run_task_tool(
        &self,
        input: ExecArgs,
        task_name: &str,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(report_result(
            async {
                let root = self.require_workspace_root().await?;
                let (bento_filter, dish_filter) = self
                    .resolve_target_filters(&root, input.target.as_deref())
                    .await?;
                let opts = bento_core::CiOptions {
                    bento_filter,
                    dish_filter,
                    task_filter: Some(vec![task_name.to_string()]),
                    no_cache: input.no_cache.unwrap_or(false),
                    fail_fast: None,
                    skip_install: input.skip_install.unwrap_or(false),
                    force_install: input.force_install.unwrap_or(false),
                    task_kind_filter: None,
                    install_only: false,
                    secret_aliases: std::collections::BTreeMap::new(),
                    run_notify_kinds: false,
                    environment: None,
                    force_deploy: false,
                };
                run_blocking(&root, &opts, ctx).await
            }
            .await,
        ))
    }
}

/// Tool failures are results, not protocol errors: an MCP `-32xxx`
/// error is "the server couldn't process the request", whereas "this
/// workspace has no dish called `web`" is an answer the agent can act
/// on. Both carry the same `{kind, message, hint, next_steps}`
/// envelope `bento <verb> --json` emits.
fn json_result(res: Result<serde_json::Value>) -> CallToolResult {
    match res {
        Ok(value) => CallToolResult::structured(value),
        Err(err) => envelope_result(bento_core::classify(&err)),
    }
}

/// Execution tools: the report IS the answer either way, so it's
/// always the structured content — but a run with failures is flagged
/// `is_error` so clients don't render a red build as a success. Mirrors
/// the CLI's non-zero exit rule.
fn report_result(res: Result<bento_core::ExecutionReport>) -> CallToolResult {
    let report = match res {
        Ok(report) => report,
        Err(err) => return envelope_result(bento_core::classify(&err)),
    };
    let failed = report.summary.failed > 0 || report.summary.install_failures > 0;
    match serde_json::to_value(&report) {
        Ok(value) if failed => CallToolResult::structured_error(value),
        Ok(value) => CallToolResult::structured(value),
        Err(e) => envelope_result(bento_core::BentoError::new("internal", e.to_string())),
    }
}

fn envelope_result(envelope: bento_core::BentoError) -> CallToolResult {
    match serde_json::to_value(&envelope) {
        Ok(value) => CallToolResult::structured_error(value),
        Err(_) => CallToolResult::error(vec![Content::text(envelope.message)]),
    }
}

#[tool_handler]
impl ServerHandler for BentoServer {
    fn get_info(&self) -> ServerInfo {
        let implementation =
            rmcp::model::Implementation::new("bento-mcp", env!("CARGO_PKG_VERSION"))
                .with_title("bento")
                .with_website_url("https://github.com/bento-sh/bento");

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_server_info(implementation)
            .with_instructions(
                "bento — polyglot monorepo orchestrator. Call `prime` first \
                 for the workspace snapshot and recommended next verb. \
                 Read-only: prime, plan, dish_list, box_list, doctor, why, \
                 artifacts, schema. Execution (mutates node_modules/target \
                 only): install, build, check, test, lint, ci. deploy and \
                 notify touch remote infrastructure. Every result is the \
                 same JSON as `bento <verb> --json`. Failures come back as \
                 tool results with isError and a {kind, message, hint, \
                 next_steps} object — read next_steps, don't retry blindly. \
                 Execution results set isError when summary.failed > 0.",
            )
    }
}

fn init_tracing() {
    // MCP uses stdout for JSON-RPC. Every log line MUST go to stderr
    // or the wire protocol corrupts. Default filter = `info`; clients
    // can override via RUST_LOG.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("bento_mcp=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let ctx = WorkspaceCtx::resolve(cli.workspace.as_deref())?;
    tracing::info!(
        workspace_root = ?ctx.workspace_root(),
        "bento-mcp starting"
    );

    let server = BentoServer::new(ctx);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
