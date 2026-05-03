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
use rmcp::model::{CallToolResult, Content, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
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

    #[tool(description = "Round-trip sanity check. Returns `pong` plus the \
                       workspace root the server resolved at startup. \
                       Kept as a lightweight liveness probe; the real \
                       agent-surface tools start with `prime`.")]
    async fn ping(&self) -> Result<CallToolResult, McpError> {
        let ctx = self.ctx.lock().await;
        let root = ctx
            .workspace_root()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unresolved)".to_string());
        Ok(CallToolResult::success(vec![Content::text(format!(
            "pong · workspace_root = {root}"
        ))]))
    }

    #[tool(description = "Agent orientation — workspace inventory, cache state, \
                       plan preview, and a ranked list of recommended next \
                       verbs. Call this first in a fresh session. Advisory \
                       only; does not execute tasks and does not hit the \
                       network. Same output shape as `bento prime --json`.")]
    async fn prime(&self) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;
        let workspace =
            Workspace::load(&root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;
        let out = bento_core::prime::compute(&workspace).map_err(tool_error_from_anyhow)?;
        let value = serde_json::to_value(&out).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(description = "JSON Schema for a named bento output. `target` must \
                       be one of: plan, report, manifest, doctor, \
                       diagnostics, garnish-payload, prime. Matches \
                       `bento schema <target>` for the bento-core-owned \
                       types.")]
    async fn schema(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            SchemaArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        let schema_value = render_schema(&input.target)?;
        Ok(CallToolResult::structured(schema_value))
    }

    #[tool(description = "Cache-aware task plan — which tasks would hit, \
                       miss, or skip on `bento ci`. Same output shape as \
                       `bento plan --json`, including per-task miss_reason \
                       and workspace-level orphan dish.toml list. \
                       `target` accepts a bento or dish name (like \
                       `bento plan <target>`); omit to plan every bento.")]
    async fn plan(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            PlanArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;

        let mut bento_filter = input.bento.clone();
        let mut dish_filter: Option<String> = None;
        if let Some(target) = &input.target {
            let workspace = Workspace::load(&root)
                .map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;
            match bento_core::resolve_target(&workspace, target).map_err(tool_error_from_anyhow)? {
                bento_core::TargetRef::Bento(name) => bento_filter = Some(name),
                bento_core::TargetRef::Dish(name) => dish_filter = Some(name),
            }
        }

        let opts = bento_core::PlanOptions {
            bento_filter,
            dish_filter,
            no_cache: input.no_cache.unwrap_or(false),
            since: input.since,
        };
        let plan = bento_core::plan_at(&root, &opts).map_err(tool_error_from_anyhow)?;
        let value = serde_json::to_value(&plan).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(description = "List every dish in the workspace with its path, \
                       language, and which bentos include it. Flags orphan \
                       dish.toml files on disk. Same output shape as \
                       `bento dish list --json`.")]
    async fn dish_list(&self) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;
        let workspace =
            Workspace::load(&root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;
        let out = bento_core::inventory::dish_list(&workspace);
        let value = serde_json::to_value(&out).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(description = "List every bento in the workspace with its source \
                       file and the dishes it includes. Same output shape \
                       as `bento box list --json`.")]
    async fn box_list(&self) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;
        let workspace =
            Workspace::load(&root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;
        let out = bento_core::inventory::box_list(&workspace);
        let value = serde_json::to_value(&out).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(description = "Structured health checks over the workspace — \
                       config parse, toolchain pins, integrations, local + \
                       remote cache, git, orphan dishes. Pass `cloud: true` \
                       to also probe cache.bento.build / api.bento.build \
                       reachability. Same output shape as `bento doctor \
                       --json`.")]
    async fn doctor(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            DoctorArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;
        let aliases = std::collections::BTreeMap::new();
        let options = bento_core::doctor::DoctorOptions {
            cloud: input.cloud.unwrap_or(false),
        };
        let report = bento_core::doctor::run_with_options(&root, &aliases, options)
            .map_err(tool_error_from_anyhow)?;
        let value = serde_json::to_value(&report).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(description = "Explain a cache entry — returns the stored input \
                       manifest (every hashed file, toolchain, env var). \
                       `target` is either `<dish>:<task>` (e.g. \
                       `marketing:lint`) or a cache-key hex prefix. Same \
                       output shape as `bento why <target> --json`.")]
    async fn why(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            WhyArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        let target = input.target;
        let cache = bento_core::LocalCache::new(
            bento_core::default_cache_root().map_err(tool_error_from_anyhow)?,
        );

        let prefix: String = if target.contains(':') {
            let root = self.require_workspace_root().await?;
            bento_core::why::resolve_dish_task_key(&root, &target)
                .map_err(tool_error_from_anyhow)?
        } else {
            if target.is_empty() || !target.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(tool_error_from_anyhow(anyhow::Error::new(
                    bento_core::why::WhyTargetError::InvalidDishTask {
                        input: target.clone(),
                    },
                )));
            }
            target.clone()
        };

        let results = bento_core::why::explain(&cache, &prefix).map_err(tool_error_from_anyhow)?;
        if results.is_empty() && target.contains(':') {
            let (dish, task) = target.split_once(':').unwrap();
            return Err(tool_error_from_anyhow(anyhow::Error::new(
                bento_core::why::WhyTargetError::NoCacheEntry {
                    dish: dish.to_string(),
                    task: task.to_string(),
                    key: prefix,
                },
            )));
        }

        let value = serde_json::to_value(&results).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(description = "Resolved absolute output paths per dish — walks \
                       each dish's `[outputs]` (dish-level plus task-level, \
                       deduped) against the filesystem. Dishes with no \
                       resolved artefacts are omitted. Same output shape as \
                       `bento artifacts --json`.")]
    async fn artifacts(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ArtifactsArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;
        let workspace =
            Workspace::load(&root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;
        let by_dish = bento_core::artifacts::collect(&workspace, input.bento.as_deref())
            .map_err(tool_error_from_anyhow)?;
        let payload: std::collections::BTreeMap<String, Vec<String>> = by_dish
            .iter()
            .map(|(name, paths)| {
                (
                    name.clone(),
                    paths.iter().map(|p| p.display().to_string()).collect(),
                )
            })
            .collect();
        let value = serde_json::to_value(&payload).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
    }

    // ── Phase 2: execution tools ───────────────────────────────────

    #[tool(description = "Install dish dependencies (node_modules, vendor, \
                       .venv, …) via each adapter's native command. \
                       Same behaviour as `bento install` — skips the task \
                       loop entirely; the returned report has install \
                       records but no task rows. Pass `force: true` to \
                       run install unconditionally, ignoring the \
                       adapter's probe.")]
    async fn install(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            InstallArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
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
        run_and_emit(&root, &opts).await
    }

    #[tool(description = "Build a bento or a single dish. Resolves adapter \
                       `build` tasks + user-defined tasks named `build`; \
                       cache-hit tasks skip execution. Same behaviour as \
                       `bento build [target]` — returns an \
                       ExecutionReport.")]
    async fn build(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "build").await
    }

    #[tool(description = "Fast type-check via the adapter's `check` task — \
                       `cargo check --locked --all-targets` for cargo, \
                       `go vet ./...` for go. Order of magnitude faster \
                       than `build` for catching compile / type \
                       errors during agent iteration. Cache hits skip \
                       execution. Same behaviour as `bento check [target]`.")]
    async fn check(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "check").await
    }

    #[tool(description = "Run every `test` task for a bento or dish. Cache \
                       hits skip execution. Same behaviour as \
                       `bento test [target]`.")]
    async fn test(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "test").await
    }

    #[tool(description = "Run every `lint` task for a bento or dish. Cache \
                       hits skip execution. Same behaviour as \
                       `bento lint [target]`.")]
    async fn lint(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
        self.run_task_tool(input, "lint").await
    }

    #[tool(description = "Full CI pass — build + test + lint across every \
                       bento/dish (or a `target` if provided). Install is \
                       performed first, then every adapter/user task \
                       except integration Deploy/Notify tasks. Same \
                       behaviour as `bento ci`.")]
    async fn ci(
        &self,
        rmcp::handler::server::wrapper::Parameters(input): rmcp::handler::server::wrapper::Parameters<
            ExecArgs,
        >,
    ) -> Result<CallToolResult, McpError> {
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
        run_and_emit(&root, &opts).await
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
    ) -> Result<CallToolResult, McpError> {
        let root = self.require_workspace_root().await?;
        let workspace =
            Workspace::load(&root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;

        let kind = if input.rollback.unwrap_or(false) {
            bento_core::IntegrationTaskKind::Rollback
        } else if input.preview.unwrap_or(false) {
            bento_core::IntegrationTaskKind::DeployPreview
        } else {
            bento_core::IntegrationTaskKind::Deploy
        };

        let mut bento_filter: Option<String> = None;
        let mut dish_filter: Option<String> = None;
        match bento_core::resolve_target(&workspace, &input.target)
            .map_err(tool_error_from_anyhow)?
        {
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

        let root_for_run = root.clone();
        let opts_for_run = opts.clone();
        let report =
            tokio::task::spawn_blocking(move || bento_core::ci_at(&root_for_run, &opts_for_run))
                .await
                .map_err(|e| tool_error_from_anyhow(anyhow::anyhow!("task join failed: {e}")))?
                .map_err(tool_error_from_anyhow)?;

        // Post-run: explicit single dish + only <no-{kind}> rows →
        // classified integration_not_configured.
        if let Some((dish, configured)) = single_dish_preflight {
            let kind_str = kind.as_str();
            let no_integration_marker = format!("<no-{kind_str}>");
            if let Some(d) = report
                .bentos
                .iter()
                .flat_map(|b| &b.dishes)
                .find(|d| d.name == dish)
            {
                let all_skips =
                    !d.tasks.is_empty() && d.tasks.iter().all(|t| t.name == no_integration_marker);
                if all_skips {
                    return Err(tool_error_from_anyhow(anyhow::anyhow!(
                        "dish '{dish}' has no '{kind_str}' integration task — \
                         configured integrations: {configured:?}",
                    )));
                }
            }
        }

        let value = serde_json::to_value(&report).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
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
        let root = self.require_workspace_root().await?;
        let workspace =
            Workspace::load(&root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;

        let (bento_filter, dish_filter) = self
            .resolve_target_filters(&root, input.target.as_deref())
            .await?;

        let secret_aliases =
            resolve_secret_aliases(&workspace, Some(&input.env), input.secret_from.as_ref())?;

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

        let root_for_run = root.clone();
        let opts_for_run = opts.clone();
        let report = tokio::task::spawn_blocking(move || {
            bento_core::notify_at(&root_for_run, &opts_for_run)
        })
        .await
        .map_err(|e| tool_error_from_anyhow(anyhow::anyhow!("task join failed: {e}")))?
        .map_err(tool_error_from_anyhow)?;

        let value = serde_json::to_value(&report).map_err(tool_error_from_json)?;
        Ok(CallToolResult::structured(value))
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
) -> Result<std::collections::BTreeMap<String, String>, McpError> {
    let mut aliases = std::collections::BTreeMap::new();
    if let Some(name) = env {
        let Some(environment) = workspace.repo.environments.get(name) else {
            let known: Vec<&String> = workspace.repo.environments.keys().collect();
            return Err(McpError::invalid_params(
                format!(
                    "environment `{name}` is not defined in bento.toml \
                     (known: {known:?}). Add an `[environments.{name}]` \
                     block with `secrets.<VAR> = \"<SOURCE_VAR>\"` entries."
                ),
                None,
            ));
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

async fn run_and_emit(
    root: &std::path::Path,
    opts: &bento_core::CiOptions,
) -> Result<CallToolResult, McpError> {
    // `ci_at` is synchronous but internally spawns a tokio runtime
    // for the S3Remote cache + runs child processes that block.
    // Running it directly from this async tool handler would nest
    // tokio runtimes and panic on drop — delegate to the blocking
    // thread pool instead.
    let root = root.to_path_buf();
    let opts = opts.clone();
    let report = tokio::task::spawn_blocking(move || bento_core::ci_at(&root, &opts))
        .await
        .map_err(|e| tool_error_from_anyhow(anyhow::anyhow!("task join failed: {e}")))?
        .map_err(tool_error_from_anyhow)?;
    let value = serde_json::to_value(&report).map_err(tool_error_from_json)?;
    // Structurally-failed runs (task failure, install failure) still
    // return CallToolResult::structured — the ExecutionReport's
    // summary.failed + task outcomes carry the signal, and MCP
    // clients display the structured content. Agents branch on
    // `summary.failed > 0` or per-task `outcome.kind`.
    Ok(CallToolResult::structured(value))
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

fn render_schema(target: &str) -> Result<serde_json::Value, McpError> {
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
            return Err(McpError::invalid_params(
                format!(
                    "unknown schema target '{other}' — expected one of: \
                     plan, report, manifest, doctor, diagnostics, \
                     garnish-payload, prime"
                ),
                None,
            ));
        }
    };
    serde_json::to_value(schema).map_err(tool_error_from_json)
}

impl BentoServer {
    async fn require_workspace_root(&self) -> Result<std::path::PathBuf, McpError> {
        let ctx = self.ctx.lock().await;
        match ctx.workspace_root() {
            Some(p) => Ok(p.to_path_buf()),
            None => Err(McpError::invalid_request(
                "no bento workspace resolved — launch `bento-mcp` with \
                 `--workspace <PATH>` or export `$BENTO_WORKSPACE_ROOT`",
                None,
            )),
        }
    }

    /// Resolve a `target` string (bento or dish name) into `(bento_filter,
    /// dish_filter)` via `bento_core::resolve_target`. When `target` is
    /// `None`, both filters come back `None` (run every bento).
    async fn resolve_target_filters(
        &self,
        root: &std::path::Path,
        target: Option<&str>,
    ) -> Result<(Option<String>, Option<String>), McpError> {
        let Some(target) = target else {
            return Ok((None, None));
        };
        let workspace =
            Workspace::load(root).map_err(|e| tool_error_from_anyhow(anyhow::Error::new(e)))?;
        match bento_core::resolve_target(&workspace, target).map_err(tool_error_from_anyhow)? {
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
    ) -> Result<CallToolResult, McpError> {
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
        run_and_emit(&root, &opts).await
    }
}

/// Flatten any anyhow error into an MCP `invalid_request`. A future
/// iteration should preserve the BentoError envelope (`kind` +
/// `next_steps`) on the MCP wire.
fn tool_error_from_anyhow(err: anyhow::Error) -> McpError {
    McpError::invalid_request(format!("{err:#}"), None)
}

fn tool_error_from_json(err: serde_json::Error) -> McpError {
    McpError::invalid_request(format!("{err}"), None)
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
                "bento-mcp exposes the bento polyglot-monorepo \
                 orchestrator as typed MCP tools. Start with \
                 `prime` for a workspace snapshot once Phase 1 \
                 ships. Today only the `ping` scaffold tool is wired.",
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
