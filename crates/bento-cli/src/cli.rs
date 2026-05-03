use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// bento — polyglot monorepo orchestrator.
///
/// Wraps native package managers (Go, Bun, Deno, npm/pnpm, Cargo) and only
/// rebuilds what changed via content hashing. Multi-tier cache: local, CI,
/// remote. One-line GitHub Action.
#[derive(Parser, Debug)]
#[command(
    name = "bento",
    version,
    about = "Polyglot monorepo orchestrator. Cargo-grade speed, Vercel-grade DX.",
    propagate_version = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalFlags,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone, Default)]
pub struct GlobalFlags {
    /// Emit machine-readable JSON output
    #[arg(long, global = true)]
    pub json: bool,

    /// Bypass cache lookups (still writes results to cache)
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Restrict to a specific bento (by name)
    #[arg(long, value_name = "NAME", global = true)]
    pub bento: Option<String>,

    /// Base ref for change detection (default: origin/main)
    #[arg(long, value_name = "REF", global = true)]
    pub since: Option<String>,

    /// Enable verbose (debug) tracing
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Write the structured ExecutionReport JSON to this path (in addition
    /// to whatever the command prints to stdout). Used by the GitHub Action
    /// to expose the report as a step output without parsing stdout.
    #[arg(long, global = true, value_name = "PATH")]
    pub report_file: Option<PathBuf>,

    /// Skip the adapter install probe entirely. Use when deps are
    /// already populated (e.g. containerised CI) and the probe cost
    /// is wasted. Does not affect individual task runs.
    #[arg(long, global = true)]
    pub skip_install: bool,

    /// Force `adapter.install()` to run regardless of the probe.
    /// Useful when the probe can't detect a subtle corruption that's
    /// tripping builds.
    #[arg(long, global = true)]
    pub force_install: bool,

    /// Point bento at a workspace other than the current directory.
    /// Resolution order: --workspace > $BENTO_WORKSPACE_ROOT > cwd.
    /// Lets MCP servers, CI harnesses, and orchestration wrappers
    /// run bento against a workspace they haven't `cd`'d into.
    #[arg(long, value_name = "PATH", global = true, env = "BENTO_WORKSPACE_ROOT")]
    pub workspace: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    // ── Scaffolding ────────────────────────────────────────────────
    /// Scaffold bento.toml and bentos/ in the current repo. By default,
    /// walks subdirectories looking for languages bento knows about and
    /// adopts each as a dish (writes only dish.toml, sources untouched).
    Init {
        /// Skip dish detection — write only the placeholder bento.toml +
        /// empty bentos/prod.toml, equivalent to old init behaviour.
        #[arg(long)]
        no_detect: bool,
    },

    /// Convert a competing monorepo tool's config into bento config.
    /// Reads the source tool's config (turbo.json, nx.json, etc.) plus
    /// per-package manifests; emits a workspace bento.toml, per-package
    /// dish.toml, and a starter bentos/prod.toml. Refuses to overwrite
    /// existing files unless `--force` is set.
    #[command(subcommand)]
    Migrate(MigrateSource),

    /// Manage dishes (apps/services inside a bento)
    #[command(subcommand)]
    Dish(DishAction),

    /// Manage bentos (deployment units; the outer boxes)
    #[command(subcommand)]
    Box(BoxAction),

    // ── Planning + execution ───────────────────────────────────────
    /// Orient an agent to this workspace — inventory, cache state,
    /// plan preview, and a recommended next verb. Meant as the first
    /// command an agent (or human) runs in a fresh session.
    ///
    /// Unlike `bento doctor`, prime is advisory — nothing fails, every
    /// field is informational. Runs without executing tasks and without
    /// network (use `bento doctor --cloud` for reachability checks).
    Prime,

    /// Show what would build, and why
    Plan {
        /// Bento or dish name (omit for every bento in the workspace).
        /// Same target shape as `build` / `test` / `lint` / `deploy`.
        target: Option<String>,
    },

    /// Plan and execute — the GitHub Action entry point
    Ci,

    /// Install dish dependencies (node_modules, vendor, .venv, …) via
    /// each adapter's native command. The one-liner equivalent of
    /// running `npm ci` / `go mod download` / `composer install` /
    /// `pnpm install` / … per dish, so agents never need to remember
    /// which tool goes with which dish.
    Install {
        /// Bento or dish name. Omit to install every dish.
        target: Option<String>,

        /// Run install unconditionally, ignoring the adapter's probe.
        /// Useful when the probe can't see a subtle `node_modules`
        /// corruption that's tripping builds. Equivalent to the
        /// global `--force-install` flag — use whichever reads better
        /// (`bento install --force` for the install verb itself,
        /// `bento ci --force-install` to force re-install before any
        /// other verb).
        #[arg(long)]
        force: bool,
    },

    /// Build a bento or single dish
    Build {
        /// Bento or dish name (omit for all bentos)
        target: Option<String>,
    },

    /// Type-check a bento or single dish — the language-native
    /// fast-feedback verb (`cargo check`, `go vet`, …). Order of
    /// magnitude faster than `bento build` for catching compile / type
    /// errors during agent iteration loops. Adapter defaults exist
    /// for cargo and go; other ecosystems run the verb only when the
    /// dish defines `[tasks.check]`.
    Check {
        /// Bento or dish name (omit for all bentos)
        target: Option<String>,
    },

    /// Test a bento or single dish
    Test {
        /// Bento or dish name (omit for all bentos)
        target: Option<String>,
    },

    /// Lint a bento or single dish
    Lint {
        /// Bento or dish name (omit for all bentos)
        target: Option<String>,
    },

    /// Deploy dishes with active deploy integrations (Vercel, Railway, …)
    Deploy {
        /// Bento or dish name. Omit to deploy every dish with a matching
        /// integration task.
        target: Option<String>,

        /// Run preview / staging deploys instead of production
        /// (e.g. `vercel deploy` without `--prod`).
        #[arg(long, conflicts_with = "rollback")]
        preview: bool,

        /// Roll back to the previous deploy. Integrations that don't
        /// support rollback will skip their dish with a clear message.
        #[arg(long)]
        rollback: bool,

        /// Named deploy environment from `bento.toml`
        /// (`[environments.<name>]`). Applies that environment's
        /// `secrets.*` aliases before running.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Alias a declared env-var name to a source env-var name,
        /// reading the value from the source and exposing it to the
        /// task under the declared name. Repeatable. Overrides
        /// anything from `--env`. Format: `DECLARED=SOURCE` — e.g.
        /// `--secret-from RAILWAY_TOKEN=RAILWAY_TOKEN_STAGING`.
        /// Never pass literal secret values here; always point at a
        /// host env var.
        #[arg(long, value_name = "DECLARED=SOURCE", value_parser = parse_secret_alias)]
        secret_from: Vec<(String, String)>,

        /// Skip Notify-kind integration tasks (garnishes — Slack
        /// posts, Linear status flips, etc). Use when re-deploying
        /// after a fix and you don't want to spam the same channel
        /// twice.
        #[arg(long)]
        no_notify: bool,

        /// Always run Deploy / DeployPreview tasks, even when their
        /// inputs match the last successful deploy on record. Without
        /// this, bento short-circuits unchanged deploys against
        /// `.bento/state/deploys.json`.
        #[arg(long)]
        force: bool,
    },

    /// Re-fire Notify-kind integration tasks (garnishes) using the
    /// last deploy's payload — useful after fixing a broken webhook
    /// without re-deploying the code.
    Notify {
        /// Bento or dish name. Omit to notify every dish with a prior
        /// deploy on record.
        target: Option<String>,

        /// Named deploy environment from `bento.toml`
        /// (`[environments.<name>]`). Applies that environment's
        /// `secrets.*` aliases before running — typically the same
        /// env you passed to the original `bento deploy`.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Alias a declared env-var name to a source env-var name.
        /// Same semantics as `bento deploy --secret-from`.
        #[arg(long, value_name = "DECLARED=SOURCE", value_parser = parse_secret_alias)]
        secret_from: Vec<(String, String)>,
    },

    // ── Dev experience ─────────────────────────────────────────────
    /// Run all dishes in a bento with hot reload. `<bento>` is required —
    /// pass the bento name from `bento box list`.
    Serve {
        /// Bento to serve
        bento: String,
    },

    /// Run a single dish in dev mode. `<dish>` is required — pass the
    /// dish name from `bento dish list`.
    Dev {
        /// Dish to run
        dish: String,
    },

    /// Add one or more packages as dependencies of a dish. Wraps the
    /// dish's native package manager (`cargo add`, `bun add`, `npm
    /// install --save`, `pnpm add`, `yarn add`, `go get`) so agents
    /// don't need to know which one applies. Lockfiles + manifests
    /// are updated by the underlying tool.
    ///
    /// Examples:
    ///   bento add tailwindcss --dish dashboard --dev
    ///   bento add tokio anyhow --dish control-plane
    ///
    /// In a single-dish workspace `--dish` can be omitted; multi-dish
    /// workspaces require it (or a positional cwd resolution).
    /// Adapters without a dev / runtime split (Go) ignore `--dev` and
    /// surface the silent demotion as a per-package `note` in the
    /// JSON output.
    Add {
        /// Package specs. Format is adapter-specific — bare names
        /// (`tailwindcss`), version pins (`tailwindcss@3.4.0`), or
        /// the package manager's own grammar (`serde`, `tokio` for
        /// cargo). At least one package required.
        #[arg(required = true)]
        packages: Vec<String>,

        /// Dish to add to. Required when the workspace has more than
        /// one dish; inferred when there's exactly one.
        #[arg(long, value_name = "DISH")]
        dish: Option<String>,

        /// Add as a dev / build-time dependency. Maps to the package
        /// manager's native dev flag (`cargo add --dev`, `bun add -d`,
        /// `npm install --save-dev`, `pnpm add -D`, `yarn add --dev`).
        #[arg(long)]
        dev: bool,
    },

    /// Invoke a named `[tasks.<name>]` block in a dish, forwarding any
    /// trailing args. Bypasses the cache — use `bento build|test|lint`
    /// for cacheable lifecycle verbs. Stdout/stderr stream straight
    /// through; bento's own status output goes to stderr only.
    ///
    /// Example — given a dish with `[tasks.admin] run = "cargo run --bin my-admin -- \"$@\""`,
    /// invoke it as: `bento run server admin -- migrate --dry-run`
    ///
    /// Use `--` to separate task args from bento's own flags. Args
    /// after `--` are passed to `sh -c` as positional `$1..$N`, so
    /// shell quoting in `run` works the way you'd expect.
    Run {
        /// Dish name (looks up `[tasks.<task>]` in `<dish>/dish.toml`).
        dish: String,
        /// Task name. Must match a `[tasks.<task>]` block with an
        /// explicit `run = "..."` field.
        task: String,
        /// Trailing args forwarded to the task command. The leading
        /// `--` is consumed by clap; everything after it lands here
        /// as positional arguments to the spawned shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    // ── Cache ──────────────────────────────────────────────────────
    /// Cache management
    #[command(subcommand)]
    Cache(CacheAction),

    // ── Secrets ────────────────────────────────────────────────────
    /// Manage deploy-target secrets (Cloudflare Workers/Pages, Railway).
    /// Thin wrapper over each platform's native CLI — values never
    /// enter bento's surface beyond the single put call.
    #[command(subcommand)]
    Secret(SecretAction),

    // ── Debugging + introspection ──────────────────────────────────
    /// Explain why a task's cache entry is what it is — prints the
    /// full input manifest (every hashed file, toolchain, env var).
    /// Accepts either `<dish>:<task>` (e.g. `bento why marketing:lint`)
    /// or a hex prefix of a cache key (any prefix; get one from
    /// `bento plan --json` or `bento ci --json`).
    Why {
        /// Target: either `<dish>:<task>` or a cache-key hex prefix.
        target: String,
    },

    /// Print the dependency graph
    Graph {
        /// Bento name (omit for all bentos)
        bento: Option<String>,

        /// Output format
        #[arg(long, value_enum, default_value_t = GraphFormat::Ascii)]
        format: GraphFormat,
    },

    /// Health check: config, toolchains, cache, integrations
    Doctor {
        /// Named deploy environment from `bento.toml`
        /// (`[environments.<name>]`). Integration env-var checks
        /// look up aliased source names instead of the declared ones
        /// — e.g. `bento doctor --env staging` checks whether
        /// `$RAILWAY_TOKEN_STAGING` is set instead of
        /// `$RAILWAY_TOKEN`, and the same alias-lookup applies to
        /// every integration that uses `[environments.*]` (Cloudflare
        /// Workers / Pages, Vercel, Slack, Linear).
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Ad-hoc alias (see `bento deploy --secret-from`). Repeatable.
        /// Overrides anything from `--env`.
        #[arg(long, value_name = "DECLARED=SOURCE", value_parser = parse_secret_alias)]
        secret_from: Vec<(String, String)>,

        /// Add cloud checks: validate the bento:// remote-cache JWT,
        /// ping cache.bento.build/health, ping api.bento.build/v1/healthz.
        /// Off by default since the rest of doctor is non-network.
        #[arg(long)]
        cloud: bool,
    },

    /// List resolved output paths per dish (post-build artefact paths)
    Artifacts,

    /// Print JSON Schema for agent-consumable output types
    Schema {
        /// Schema to print; omit to list available schemas
        #[arg(value_enum)]
        target: Option<SchemaTarget>,
    },

    // ── MCP ────────────────────────────────────────────────────────
    /// Manage the MCP (Model Context Protocol) server entry across
    /// agent clients (Claude Code, Claude Desktop, Cursor, Windsurf).
    #[command(subcommand)]
    Mcp(McpAction),

    // ── Toolchains ─────────────────────────────────────────────────
    /// Toolchain management — install, list, and pin language versions
    #[command(subcommand)]
    Toolchain(ToolchainAction),

    /// Cut a release: bump workspace version, refresh Cargo.lock,
    /// commit, tag locally. Does not push — prints the push commands
    /// for you to run after reviewing.
    Release {
        /// Version to cut. `X.Y.Z` for an explicit version, or one of
        /// `patch` / `minor` / `major` to bump relative to the current
        /// workspace version.
        #[arg(value_name = "VERSION")]
        spec: String,
    },

    /// Sign in to bento.build and stash the returned JWT in the OS
    /// keychain (or `~/.bento/credentials` as a 0600 fallback).
    /// After this, `bento build|ci|…` pick up the token automatically
    /// and you can stop setting `BENTO_CACHE_TOKEN` by hand.
    Login,

    // ── Internal (agent / integration use only) ────────────────────
    /// Internal: Slack webhook poster. Invoked by `SlackIntegration`'s
    /// emitted task; reads a GarnishPayload on stdin and POSTs. Not
    /// a user-facing verb.
    #[command(name = "_slack-post", hide = true)]
    SlackPost {
        /// Env-var name holding the webhook URL.
        #[arg(long, value_name = "NAME", default_value = "SLACK_WEBHOOK_URL")]
        webhook_env: String,

        /// Optional channel override (webhooks pin one at creation time).
        #[arg(long)]
        channel: Option<String>,

        /// Optional username override.
        #[arg(long)]
        username: Option<String>,
    },

    /// Internal: Linear issue transitioner. Invoked by
    /// `LinearIntegration`; reads a GarnishPayload on stdin,
    /// extracts issue IDs, transitions them to a target state via
    /// Linear GraphQL.
    #[command(name = "_linear-notify", hide = true)]
    LinearNotify {
        /// Env-var name holding the Linear Personal API key.
        #[arg(long, value_name = "NAME", default_value = "LINEAR_API_KEY")]
        api_key_env: String,

        /// Workflow-state name to transition matched issues to.
        #[arg(long, default_value = "Deployed")]
        target_state: String,

        /// Fallback issue ID for comments when no issue refs match.
        #[arg(long)]
        fallback_issue_id: Option<String>,

        /// Optional team key to scope state lookups.
        #[arg(long)]
        team: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MigrateSource {
    /// Migrate a Turborepo workspace. Reads root turbo.json (v2 `tasks`
    /// or v1 `pipeline`), discovers packages via root package.json's
    /// `workspaces` glob, emits per-package dish.toml + workspace
    /// bento.toml + bentos/prod.toml. Per-package turbo.json overrides
    /// are detected and noted but not currently applied — surface
    /// in the migration report so users can hand-port them.
    Turbo {
        /// Workspace root containing turbo.json. Defaults to cwd.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,

        /// Show what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing bento.toml / dish.toml / bentos/prod.toml.
        /// Without this, the migrator refuses to clobber any of those.
        #[arg(long)]
        force: bool,
    },

    /// Migrate an Nx workspace. Reads root nx.json (targetDefaults,
    /// namedInputs, workspaceLayout) plus per-project project.json
    /// files; emits per-project dish.toml + workspace bento.toml +
    /// bentos/prod.toml. Common Nx executors map to canonical CLI
    /// invocations (`@nx/vite:build` → `vite build`, `@nx/jest:jest`
    /// → `jest`, …); unknown executors emit `nx run …` shims with
    /// an Inferred note. Configurations and per-target dependsOn are
    /// surfaced as notes — bento derives task ordering from the dish
    /// graph, not per-target deps.
    Nx {
        /// Workspace root containing nx.json. Defaults to cwd.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,

        /// Show what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing bento.toml / dish.toml / bentos/prod.toml.
        /// Without this, the migrator refuses to clobber any of those.
        #[arg(long)]
        force: bool,
    },

    /// Migrate a Lerna workspace. Reads `lerna.json` (packages glob,
    /// useWorkspaces, npmClient) plus each package's `package.json`
    /// scripts. Emits per-package dish.toml mirroring scripts as
    /// `[tasks.<name>]` blocks plus workspace bento.toml + bentos/prod.toml.
    /// Lerna's task graph is shallow (no cross-package dependsOn) so
    /// dish-level ordering is left to the user with a TODO note.
    Lerna {
        /// Workspace root containing lerna.json. Defaults to cwd.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,

        /// Show what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing bento.toml / dish.toml / bentos/prod.toml.
        #[arg(long)]
        force: bool,
    },

    /// Best-effort `Makefile` migrator. Parses top-level targets,
    /// prerequisites (treated as `dependsOn`), and recipe lines (treated
    /// as shell commands). Cannot translate variable expansion, pattern
    /// rules, or automatic variables — those surface as notes the user
    /// must hand-port. `.PHONY` targets handled best-effort. Single-dish
    /// shape (the Makefile root becomes one bento with one dish).
    Make {
        /// Directory containing the Makefile. Defaults to cwd.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,

        /// Show what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing bento.toml / dish.toml / bentos/prod.toml.
        #[arg(long)]
        force: bool,
    },

    /// Migrate a moonrepo workspace. Reads `.moon/workspace.yml`
    /// (project glob patterns, vcs, runner) plus each project's
    /// `moon.yml`. Maps moon's task definitions (`command`, `deps`,
    /// `inputs`, `outputs`, `options.cache`, `platform`) onto bento
    /// dish tasks. Moon's first-class language toolchain blocks
    /// (`rust`, `node`, `deno`) surface as notes — bento doesn't
    /// have a direct equivalent yet.
    Moon {
        /// Workspace root containing `.moon/workspace.yml`. Defaults
        /// to cwd.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,

        /// Show what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing bento.toml / dish.toml / bentos/prod.toml.
        #[arg(long)]
        force: bool,
    },

    /// Migrate a Rush.js workspace. Reads `rush.json` (projects array
    /// with packageName + projectFolder) plus each project's
    /// `package.json` scripts and Rush-specific
    /// `config/rush/command-line.json` (custom bulk commands). Emits
    /// per-package dish.toml mirroring scripts; bulk commands surface
    /// as notes for the user to wire up manually.
    Rush {
        /// Workspace root containing rush.json. Defaults to cwd.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,

        /// Show what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing bento.toml / dish.toml / bentos/prod.toml.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DishAction {
    /// Scaffold a new dish (source files + dish.toml) and wire it into a bento
    Add {
        /// Path to the dish directory (relative to workspace root)
        path: PathBuf,

        /// Language ecosystem to scaffold. Accepted values: `go`,
        /// `cargo`, `python`, `python-uv`, `ruby`, `php`, `maven`,
        /// `gradle`, `node-npm`, `node-pnpm`, `node-yarn`, `bun`,
        /// `deno`. Required when `<path>` is an empty directory;
        /// auto-detected when adopting an existing dish.
        #[arg(long, value_name = "LANG")]
        lang: Option<String>,
    },

    /// List every dish in the workspace with its path, language, and
    /// which bentos include it. Flags orphan `dish.toml` files on disk
    /// that aren't wired into any bento.
    List,
}

#[derive(Subcommand, Debug)]
pub enum BoxAction {
    /// Create a new bento definition
    Add {
        /// Bento name (becomes bentos/<name>.toml)
        name: String,
    },

    /// List every bento in the workspace with its source file and
    /// the dishes it includes.
    List,
}

#[derive(Subcommand, Debug)]
pub enum CacheAction {
    /// Hit rate, size, and location per tier
    Stats,

    /// Clear the local cache
    Clear,

    /// Push the local cache to remote (force)
    Push,

    /// Pull the remote cache to local (force)
    Pull,
}

#[derive(Subcommand, Debug)]
pub enum SecretAction {
    /// Set or update a secret on the dish's deploy target. Value is
    /// read from stdin so agents can pipe it in; use
    /// `echo -n "$VALUE" | bento secret put <target> <name>`.
    Put {
        /// Dish name, optionally `<dish>:<integration>` when the dish
        /// has more than one secret-capable integration.
        target: String,

        /// Secret name. Platform-specific naming rules apply.
        name: String,
    },

    /// List secret names (not values) on the dish's deploy target.
    List {
        /// Dish name, optionally `<dish>:<integration>`.
        target: String,
    },

    /// Delete a secret on the dish's deploy target.
    Delete {
        /// Dish name, optionally `<dish>:<integration>`.
        target: String,

        /// Secret name to remove.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// Register `bento-mcp` in one or more agent clients' MCP config.
    /// Supported clients: Claude Code (`~/.claude.json`), Claude
    /// Desktop, Cursor (`~/.cursor/mcp.json`), Windsurf
    /// (`~/.codeium/windsurf/mcp_config.json`), Codex CLI
    /// (`~/.codex/config.toml`), OpenCode
    /// (`~/.config/opencode/opencode.json`), and Zed
    /// (`~/.config/zed/settings.json`). Idempotent — re-running
    /// updates the existing entry.
    Install {
        /// Which client(s) to install for. `auto` (default) detects every
        /// installed client and registers in each.
        #[arg(value_enum, default_value_t = McpClient::Auto)]
        client: McpClient,

        /// Write the project-local config (`.cursor/mcp.json`,
        /// `.mcp.json` at the repo root for Claude Code) instead of
        /// the user-global one. Only meaningful for clients that
        /// support project-local config.
        #[arg(long)]
        local: bool,

        /// Bake `--workspace <PATH>` into the registered command so
        /// `bento-mcp` always pins to that workspace. Defaults to none
        /// (`bento-mcp` resolves cwd at startup). NOTE: this flag is
        /// independent of bento's global `--workspace` /
        /// `$BENTO_WORKSPACE_ROOT` — it controls what gets *written
        /// into the MCP config file*, not where `bento mcp install`
        /// itself runs. Pass an absolute path you want the MCP server
        /// to anchor on.
        #[arg(long, value_name = "PATH")]
        workspace: Option<std::path::PathBuf>,

        /// Override the server-key written into the config (default
        /// `bento` — surfaces as `mcp__bento__<verb>` in client tool
        /// pickers). Useful when a workspace already has an entry
        /// named `bento` pointing somewhere else.
        #[arg(long, default_value = "bento")]
        name: String,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum McpClient {
    /// Auto-detect every installed client and register in each.
    Auto,
    /// Claude Code (anthropic CLI). Config: `~/.claude.json` (user) or
    /// `.mcp.json` at the repo root (with `--local`).
    ClaudeCode,
    /// Claude Desktop (anthropic desktop app). Config:
    /// `~/Library/Application Support/Claude/claude_desktop_config.json`
    /// (macOS) / equivalent paths on Windows + Linux.
    ClaudeDesktop,
    /// Cursor IDE. Config: `~/.cursor/mcp.json` or `.cursor/mcp.json`
    /// (with `--local`).
    Cursor,
    /// Windsurf IDE (Codeium). Config:
    /// `~/.codeium/windsurf/mcp_config.json`.
    Windsurf,
    /// Codex CLI (OpenAI). Config: `~/.codex/config.toml` (user) or
    /// `.codex/config.toml` at the repo root (with `--local`). TOML
    /// shape — entries land under `[mcp_servers.<name>]`.
    Codex,
    /// OpenCode (sst/opencode). Config:
    /// `~/.config/opencode/opencode.json` (user) or `opencode.json`
    /// at the repo root (with `--local`).
    Opencode,
    /// Zed editor. Config: `~/.config/zed/settings.json` (user) or
    /// `.zed/settings.json` at the repo root (with `--local`). MCP
    /// servers register under the top-level `context_servers` key.
    Zed,
}

#[derive(Subcommand, Debug)]
pub enum ToolchainAction {
    /// List installed toolchains
    List,

    /// Install missing toolchains for the current project
    Install,

    /// Pin a toolchain version (e.g. `go=1.22.3`)
    Pin {
        /// `<tool>=<version>` (e.g. `go=1.22.3`)
        pin: String,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum GraphFormat {
    Ascii,
    Dot,
}

/// Output types for which `bento schema` can emit JSON Schema.
///
/// These are the stable agent-integration contract. Bumping the shape of
/// any of these is a breaking change.
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum SchemaTarget {
    /// Output of `bento plan` (and `--json`).
    Plan,
    /// Output of `bento ci`, `bento build|test|lint`.
    Report,
    /// Output of `bento why <hash>`.
    Why,
    /// Output of `bento dish add --json`.
    Scaffold,
    /// The InputManifest sidecar written alongside each cache entry.
    Manifest,
    /// Output of `bento doctor`.
    Doctor,
    /// The structured error envelope emitted on any failure with `--json`.
    Error,
    /// Structured tool diagnostics — the `diagnostics` array on each
    /// failed task in an ExecutionReport. Compiler/linter records.
    Diagnostics,
    /// Garnish payload piped on stdin to Notify-kind integration
    /// tasks after a Deploy task completes.
    GarnishPayload,
    /// Output of `bento prime` (and `--json`).
    Prime,
}

/// Parse `DECLARED=SOURCE` into a name pair. Rejects empty halves so
/// `--secret-from =SOMETHING` and `--secret-from NAME=` both fail at
/// the flag layer rather than silently disabling the alias. Passed to
/// clap via `value_parser`.
fn parse_secret_alias(s: &str) -> Result<(String, String), String> {
    let Some((declared, source)) = s.split_once('=') else {
        return Err(format!("expected DECLARED=SOURCE, got `{s}`"));
    };
    let declared = declared.trim();
    let source = source.trim();
    if declared.is_empty() {
        return Err("declared env-var name is empty".to_string());
    }
    if source.is_empty() {
        return Err("source env-var name is empty".to_string());
    }
    // Catch the most common footgun: an agent passing the resolved
    // *value* (from `${{ secrets.FOO }}`) instead of an env-var name.
    // Real env-var names are shell-identifier-shaped; values typically
    // contain other characters.
    if !source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        || source.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "source `{source}` doesn't look like an env-var name \
             (alphanumerics + underscore, not starting with a digit). \
             Did you pass the secret value instead of a var name?"
        ));
    }
    Ok((declared.to_string(), source.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_bento_verb_in_in_tree_docs_resolves_to_a_real_subcommand() {
        // Ground truth: build the verb set by walking the clap CommandFactory
        // tree. Includes top-level subcommands AND second-level sub-actions
        // (e.g. `dish add`, `box list`, `cache push`).
        let mut verbs: std::collections::HashSet<String> = std::collections::HashSet::new();
        let cmd = Cli::command();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name().to_string();
            verbs.insert(name.clone());
            for sub2 in sub.get_subcommands() {
                verbs.insert(format!("{} {}", name, sub2.get_name()));
            }
        }

        // In-tree agent-facing docs. Paths relative to the crate root
        // (cargo unit-test cwd). External marketing copy is out of scope.
        let docs = [
            "../../README.md",
            "../../CHANGELOG.md",
            "../../CLAUDE.md",
            "../../docs/agents.md",
            "../../docs/configuration.md",
            "../../docs/deploying.md",
            "../../docs/plugins.md",
            "../../docs/adopt-existing-repo.md",
            "../../docs/new-project.md",
            "../../skills/bento/SKILL.md",
        ];

        let mut failures: Vec<String> = Vec::new();
        for path in docs {
            let body = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            // Two contexts qualify as "invocation":
            //   - inside a fenced code block, lines that begin with
            //     `bento ` (after optional `$ ` / `> ` prompt + lead
            //     whitespace) are the user being asked to type the
            //     command.
            //   - outside code blocks, code spans whose content begins
            //     with `bento ` are the same — `` `bento install` ``.
            // Anything else (prose, sample-output rows, comments) is
            // not an invocation and is left alone.
            let mut in_code_block = false;
            for (line_no, line) in body.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("```") {
                    in_code_block = !in_code_block;
                    continue;
                }
                let candidates: Vec<&str> = if in_code_block {
                    invocation_candidate(line).into_iter().collect()
                } else {
                    code_span_segments(line)
                        .into_iter()
                        .filter_map(invocation_candidate)
                        .collect()
                };
                for after_bento in candidates {
                    let Some(token) = parse_verb_token(after_bento) else {
                        continue;
                    };
                    let words: Vec<&str> = token.split_whitespace().collect();
                    let one = words.first().copied().unwrap_or("").to_string();
                    let two = if words.len() >= 2 {
                        format!("{} {}", words[0], words[1])
                    } else {
                        one.clone()
                    };
                    if !verbs.contains(&one) && !verbs.contains(&two) {
                        failures.push(format!(
                            "{}:{} → `bento {}` — not in CLI subcommand set",
                            path,
                            line_no + 1,
                            token,
                        ));
                    }
                }
            }
        }

        if !failures.is_empty() {
            let mut msg = String::from(
                "doc → CLI verb drift (every `bento <verb>` mention in in-tree docs \
                              must resolve to a real clap subcommand):\n",
            );
            for f in &failures {
                msg.push_str("  ");
                msg.push_str(f);
                msg.push('\n');
            }
            panic!("{msg}");
        }
    }

    /// Strip leading whitespace + shell-prompt + the `bento ` literal
    /// from `text`. Returns `Some(rest)` when `text` looks like an
    /// invocation (the user is being asked to type `bento ...`),
    /// `None` for prose, comments, or non-bento commands.
    fn invocation_candidate(text: &str) -> Option<&str> {
        let s = text.trim_start();
        let s = s.strip_prefix("$ ").unwrap_or(s);
        let s = s.strip_prefix("> ").unwrap_or(s);
        s.strip_prefix("bento ")
    }

    /// Given the substring after `bento `, return the verb token
    /// (one- or two-word form) or `None` if this looks like a flag,
    /// a placeholder, or a field reference (`bento version: 0.1`).
    fn parse_verb_token(after_bento: &str) -> Option<String> {
        let bytes = after_bento.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        // Reject `bento -<flag>` — that's a global flag, not a verb.
        // Reject `bento <placeholder>` — explicit angle-bracket form.
        if bytes[0] == b'-' || bytes[0] == b'<' {
            return None;
        }
        let first = take_word(bytes);
        if first.is_empty() {
            return None;
        }
        let after_first = first.len();
        // Reject sample-output forms `bento foo: bar` — the colon
        // means `foo` is a field, not a verb.
        if after_first < bytes.len() && bytes[after_first] == b':' {
            return None;
        }
        let first_str = std::str::from_utf8(first).expect("ascii guaranteed");
        let mut whole = first_str.to_string();
        if after_first < bytes.len() && bytes[after_first] == b' ' {
            let second = take_word(&bytes[after_first + 1..]);
            if !second.is_empty() {
                let second_str = std::str::from_utf8(second).expect("ascii guaranteed");
                whole.push(' ');
                whole.push_str(second_str);
            }
        }
        Some(whole)
    }

    fn take_word(bytes: &[u8]) -> &[u8] {
        let mut end = 0;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_lowercase() || c == b'-' || c == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        &bytes[..end]
    }

    /// Return the substrings of `line` that sit between matching
    /// backtick pairs — the code-span content. Unbalanced backticks
    /// (a single ` ` ` on a line) are treated as opening a span that
    /// runs to end-of-line, matching how most markdown renderers cope.
    fn code_span_segments(line: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let span_start = i + 1;
            let mut j = span_start;
            while j < bytes.len() && bytes[j] != b'`' {
                j += 1;
            }
            if j > span_start {
                out.push(&line[span_start..j]);
            }
            i = j + 1;
        }
        out
    }

    #[test]
    fn parses_plan() {
        let cli = Cli::try_parse_from(["bento", "plan"]).unwrap();
        assert!(matches!(cli.command, Command::Plan { target: None }));
    }

    #[test]
    fn parse_secret_alias_accepts_valid_pair() {
        let (declared, source) = parse_secret_alias("RAILWAY_TOKEN=RAILWAY_TOKEN_STAGING").unwrap();
        assert_eq!(declared, "RAILWAY_TOKEN");
        assert_eq!(source, "RAILWAY_TOKEN_STAGING");
    }

    #[test]
    fn parse_secret_alias_rejects_missing_equals() {
        assert!(parse_secret_alias("RAILWAY_TOKEN").is_err());
    }

    #[test]
    fn parse_secret_alias_rejects_empty_halves() {
        assert!(parse_secret_alias("=SOURCE").is_err());
        assert!(parse_secret_alias("DECLARED=").is_err());
    }

    #[test]
    fn parse_secret_alias_rejects_literal_values() {
        // Catch the CI footgun — an agent passing the resolved secret
        // value instead of an env-var name should get a clear error.
        let err = parse_secret_alias("RAILWAY_TOKEN=rlw_sk_abc123+/=").unwrap_err();
        assert!(
            err.contains("doesn't look like an env-var name"),
            "expected clear hint, got: {err}"
        );
    }

    #[test]
    fn parse_secret_alias_rejects_leading_digit() {
        assert!(parse_secret_alias("NAME=1SOURCE").is_err());
    }

    #[test]
    fn parses_deploy_with_env_and_secret_from() {
        let cli = Cli::try_parse_from([
            "bento",
            "deploy",
            "--env",
            "staging",
            "--secret-from",
            "RAILWAY_TOKEN=RAILWAY_TOKEN_STAGING",
            "--secret-from",
            "VERCEL_TOKEN=VERCEL_TOKEN_STAGING",
        ])
        .unwrap();
        match cli.command {
            Command::Deploy {
                env, secret_from, ..
            } => {
                assert_eq!(env.as_deref(), Some("staging"));
                assert_eq!(secret_from.len(), 2);
                assert_eq!(secret_from[0].0, "RAILWAY_TOKEN");
                assert_eq!(secret_from[0].1, "RAILWAY_TOKEN_STAGING");
            }
            _ => panic!("expected Deploy"),
        }
    }

    #[test]
    fn parses_ci_with_json() {
        let cli = Cli::try_parse_from(["bento", "ci", "--json"]).unwrap();
        assert!(matches!(cli.command, Command::Ci));
        assert!(cli.global.json);
    }

    #[test]
    fn parses_build_with_target_and_no_cache() {
        let cli = Cli::try_parse_from(["bento", "build", "api", "--no-cache"]).unwrap();
        match cli.command {
            Command::Build { target } => assert_eq!(target.as_deref(), Some("api")),
            _ => panic!("expected Build"),
        }
        assert!(cli.global.no_cache);
    }

    #[test]
    fn parses_check_with_target() {
        let cli = Cli::try_parse_from(["bento", "check", "api"]).unwrap();
        match cli.command {
            Command::Check { target } => assert_eq!(target.as_deref(), Some("api")),
            _ => panic!("expected Check"),
        }
    }

    #[test]
    fn parses_check_without_target() {
        let cli = Cli::try_parse_from(["bento", "check"]).unwrap();
        assert!(matches!(cli.command, Command::Check { target: None }));
    }

    #[test]
    fn parses_dish_add() {
        let cli = Cli::try_parse_from(["bento", "dish", "add", "apps/api"]).unwrap();
        match cli.command {
            Command::Dish(DishAction::Add { path, lang }) => {
                assert_eq!(path.to_str(), Some("apps/api"));
                assert!(lang.is_none());
            }
            _ => panic!("expected Dish(Add)"),
        }
    }

    #[test]
    fn parses_dish_add_with_lang_and_bento() {
        let cli = Cli::try_parse_from([
            "bento", "--bento", "prod", "dish", "add", "apps/api", "--lang", "go",
        ])
        .unwrap();
        match cli.command {
            Command::Dish(DishAction::Add { path, lang }) => {
                assert_eq!(path.to_str(), Some("apps/api"));
                assert_eq!(lang.as_deref(), Some("go"));
            }
            _ => panic!("expected Dish(Add)"),
        }
        assert_eq!(cli.global.bento.as_deref(), Some("prod"));
    }

    #[test]
    fn parses_dish_list() {
        let cli = Cli::try_parse_from(["bento", "dish", "list"]).unwrap();
        match cli.command {
            Command::Dish(DishAction::List) => {}
            _ => panic!("expected Dish(List)"),
        }
    }

    #[test]
    fn parses_box_list() {
        let cli = Cli::try_parse_from(["bento", "box", "list"]).unwrap();
        match cli.command {
            Command::Box(BoxAction::List) => {}
            _ => panic!("expected Box(List)"),
        }
    }

    #[test]
    fn parses_box_add() {
        let cli = Cli::try_parse_from(["bento", "box", "add", "prod"]).unwrap();
        match cli.command {
            Command::Box(BoxAction::Add { name }) => assert_eq!(name, "prod"),
            _ => panic!("expected Box(Add)"),
        }
    }

    #[test]
    fn parses_cache_stats() {
        let cli = Cli::try_parse_from(["bento", "cache", "stats"]).unwrap();
        assert!(matches!(cli.command, Command::Cache(CacheAction::Stats)));
    }

    #[test]
    fn parses_why_requires_hash() {
        assert!(Cli::try_parse_from(["bento", "why"]).is_err());
        let cli = Cli::try_parse_from(["bento", "why", "abc123"]).unwrap();
        match cli.command {
            Command::Why { target } => assert_eq!(target, "abc123"),
            _ => panic!("expected Why"),
        }
    }

    #[test]
    fn parses_graph_with_dot_format() {
        let cli = Cli::try_parse_from(["bento", "graph", "--format", "dot"]).unwrap();
        match cli.command {
            Command::Graph { format, .. } => assert_eq!(format, GraphFormat::Dot),
            _ => panic!("expected Graph"),
        }
    }

    #[test]
    fn parses_add_single_package_no_dish() {
        let cli = Cli::try_parse_from(["bento", "add", "tailwindcss"]).unwrap();
        match cli.command {
            Command::Add {
                packages,
                dish,
                dev,
            } => {
                assert_eq!(packages, vec!["tailwindcss"]);
                assert_eq!(dish, None);
                assert!(!dev);
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parses_add_multiple_packages_with_dish_and_dev() {
        let cli = Cli::try_parse_from([
            "bento",
            "add",
            "serde",
            "tokio",
            "anyhow",
            "--dish",
            "control-plane",
            "--dev",
        ])
        .unwrap();
        match cli.command {
            Command::Add {
                packages,
                dish,
                dev,
            } => {
                assert_eq!(packages, vec!["serde", "tokio", "anyhow"]);
                assert_eq!(dish.as_deref(), Some("control-plane"));
                assert!(dev);
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parses_add_with_version_pin() {
        let cli = Cli::try_parse_from(["bento", "add", "tailwindcss@3.4.0"]).unwrap();
        match cli.command {
            Command::Add { packages, .. } => {
                assert_eq!(packages, vec!["tailwindcss@3.4.0"]);
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parses_add_requires_at_least_one_package() {
        assert!(Cli::try_parse_from(["bento", "add"]).is_err());
        assert!(Cli::try_parse_from(["bento", "add", "--dish", "api"]).is_err());
    }

    #[test]
    fn parses_run_with_dish_and_task() {
        let cli = Cli::try_parse_from(["bento", "run", "control-plane", "admin"]).unwrap();
        match cli.command {
            Command::Run { dish, task, args } => {
                assert_eq!(dish, "control-plane");
                assert_eq!(task, "admin");
                assert!(args.is_empty());
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn parses_run_with_trailing_args_after_dash_dash() {
        let cli = Cli::try_parse_from([
            "bento",
            "run",
            "control-plane",
            "admin",
            "--",
            "waitlist",
            "broadcast",
            "--dry-run",
            "--message",
            "hi there",
        ])
        .unwrap();
        match cli.command {
            Command::Run { dish, task, args } => {
                assert_eq!(dish, "control-plane");
                assert_eq!(task, "admin");
                assert_eq!(
                    args,
                    vec![
                        "waitlist",
                        "broadcast",
                        "--dry-run",
                        "--message",
                        "hi there",
                    ]
                );
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn parses_run_requires_dish_and_task() {
        assert!(Cli::try_parse_from(["bento", "run"]).is_err());
        assert!(Cli::try_parse_from(["bento", "run", "control-plane"]).is_err());
    }

    #[test]
    fn parses_serve_requires_bento_name() {
        assert!(Cli::try_parse_from(["bento", "serve"]).is_err());
        let cli = Cli::try_parse_from(["bento", "serve", "prod"]).unwrap();
        match cli.command {
            Command::Serve { bento } => assert_eq!(bento, "prod"),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn parses_global_since_override() {
        let cli = Cli::try_parse_from(["bento", "plan", "--since", "HEAD~5"]).unwrap();
        assert_eq!(cli.global.since.as_deref(), Some("HEAD~5"));
    }

    #[test]
    fn parses_verbose_short_flag() {
        let cli = Cli::try_parse_from(["bento", "-v", "plan"]).unwrap();
        assert!(cli.global.verbose);
    }

    #[test]
    fn parses_toolchain_pin() {
        let cli = Cli::try_parse_from(["bento", "toolchain", "pin", "go=1.22.3"]).unwrap();
        match cli.command {
            Command::Toolchain(ToolchainAction::Pin { pin }) => assert_eq!(pin, "go=1.22.3"),
            _ => panic!("expected Toolchain(Pin)"),
        }
    }
}
