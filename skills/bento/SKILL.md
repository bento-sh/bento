---
name: bento
description: Use this skill whenever you're working in a repository managed by bento — look for a `bento.toml` at the repo root, a `bentos/` directory, or per-subdir `dish.toml` files. Bento is a polyglot monorepo orchestrator that wraps native package managers (npm / pnpm / yarn / bun / cargo / go / composer / pip / bundle / mvn / gradle / deno) behind one uniform CLI with content-addressed caching, toolchain pinning, and deploy-target routing. On a fresh session, run `bento prime` first to get a workspace snapshot (inventory, cache state, plan preview, recommended next verb). Reach for `bento` verbs (prime / init / migrate / install / ci / build / test / lint / deploy / notify / doctor / why / plan / artifacts / cache / secret / toolchain / release / graph / login / dish list / box list) instead of invoking native package managers — bento handles per-dish scoping, cache layering, and structured output that agents can reason about. **When you need a running service** (smoke test, reproduce a bug, hit an endpoint), start it yourself via `bento dev <dish>` / `bento serve <bento>` in a background shell with logs captured to a file — do NOT probe the user's running processes via `ss` / `lsof` / `pgrep` / `curl localhost:<port>`. Their terminal state isn't your terminal state. Also use when the user mentions bento explicitly, or asks about polyglot monorepo orchestration, content-addressed build caching, garnishes (post-deploy hooks), or deploy integrations (Railway, Vercel, Cloudflare Workers, Cloudflare Pages).
---

# Bento — polyglot monorepo orchestrator

## When to reach for bento

If the repo has any of these, the repo is bento-managed and you should prefer `bento` verbs over native package managers:

- `bento.toml` at the repo root
- a `bentos/` directory with one or more `<name>.toml` files
- per-subdir `dish.toml` files declaring a `language`

If bento isn't installed on the host, see the **Installing bento** section below.

## Start with `bento prime`

On a fresh session — or after `/clear` / compaction — run **`bento prime`** before anything else. It prints a concise orientation: bentos, dishes, cache state, a plan preview, and a recommended next verb. `bento prime --json` emits a schema-stable object registered via `bento schema prime`; agents can switch on `recommended_next[0]` to decide what to do first.

It does not execute tasks, does not hit the network, and runs in under 2 seconds on a cold workspace.

## If an MCP connection is available, prefer `mcp__bento__*` tools

bento ships a `bento-mcp` binary that exposes bento verbs as typed [Model Context Protocol](https://modelcontextprotocol.io) tools. When your client lists tools starting with `mcp__bento__*`, prefer them over shelling out to `bento`:

- `mcp__bento__prime` → same output as `bento prime --json`
- `mcp__bento__plan` → `bento plan --json` (accepts `target`, `bento`, `no_cache`, `since`)
- `mcp__bento__dish_list` / `mcp__bento__box_list` → inventory surface
- `mcp__bento__doctor` → health checks (add `cloud: true` for endpoint probes)
- `mcp__bento__why` → cache-key explanation (`target` accepts `<dish>:<task>` or hex prefix)
- `mcp__bento__artifacts` → resolved output paths per dish
- `mcp__bento__schema` → JSON Schema for any bento output type
- `mcp__bento__install` / `mcp__bento__build` / `mcp__bento__check` / `mcp__bento__test` / `mcp__bento__lint` / `mcp__bento__ci` → execution; mutate `node_modules` / `target/` only. `check` is the fast type-check verb (`cargo check`, `go vet`) — prefer it over `build` while iterating
- `mcp__bento__deploy` / `mcp__bento__notify` → destructive + open-world; MCP client prompts for stronger confirmation. Both require `env` (matching `[environments.<env>]` in bento.toml)

Write-path verbs (`bento dish add` / `bento init` / `bento migrate`) don't have MCP tools yet — shell out via the verb reference below until they land.

Easiest path: `bento mcp install`. With no arguments it auto-detects every installed agent client (Claude Code, Claude Desktop, Cursor, Windsurf, Codex CLI, OpenCode, Zed) and writes the right config for each. Pass a positional client name (`bento mcp install codex`) to register only one. Run `bento mcp install --help` for the full list and exact paths. If the tools aren't listed in `/mcp` (or your client's equivalent), fall back to shell-out via the verb reference below — both produce identical JSON.

## Verb reference

| What you want | Run |
|---------------|-----|
| Orient yourself in a fresh session (inventory + next verb) | `bento prime` |
| List every dish (name, path, language, bentos) + flag orphans | `bento dish list` |
| List every bento (name, source, dishes) | `bento box list` |
| Scaffold bento.toml + bentos/ in this repo (auto-detects dishes) | `bento init` |
| Convert a Turborepo workspace into bento config | `bento migrate turbo [--dry-run] [--force]` |
| Convert an Nx workspace into bento config | `bento migrate nx [--dry-run] [--force]` |
| Convert a Lerna workspace into bento config | `bento migrate lerna [--dry-run] [--force]` |
| Convert a Makefile into bento config (best-effort) | `bento migrate make [--dry-run] [--force]` |
| Convert a moonrepo workspace into bento config | `bento migrate moon [--dry-run] [--force]` |
| Convert a Rush.js workspace into bento config | `bento migrate rush [--dry-run] [--force]` |
| Install every dish's deps (replaces `npm ci` / `go mod download` / `composer install` / …) | `bento install` |
| Install one dish | `bento install <dish>` |
| Full CI pass (build + check + test + lint, no deploy/notify) | `bento ci` |
| Build / check / test / lint | `bento build [target]` · `bento check [target]` · `bento test [target]` · `bento lint [target]` |
| Fast type-check across a target (`cargo check --locked --all-targets`, `go vet ./...`) — order of magnitude faster than `bento build` for tight iteration loops | `bento check [target]` |
| Deploy to a named environment | `bento deploy --env <env> [target]` |
| Preview / staging deploy | `bento deploy --preview --env <env>` |
| Rollback | `bento deploy --rollback --env <env>` |
| Force a deploy even when inputs unchanged | `bento deploy --force --env <env>` |
| Re-fire Slack/Linear notifications without re-deploying | `bento notify --env <env> [target]` |
| Run a bento with hot reload | `bento serve <bento>` |
| Run a single dish in dev mode | `bento dev <dish>` |
| Invoke a `[tasks.<name>]` block ad-hoc (CLIs, migrations, one-offs); bypasses the cache | `bento run <dish> <task> -- <args…>` |
| Add a dependency to a dish (cargo / bun / npm / pnpm / yarn / go) | `bento add <pkg>… --dish <d> [--dev]` |
| Show what would run, without running | `bento plan` |
| Explain a cache decision | `bento why <cache-key-prefix>` |
| Print the dependency graph | `bento graph [--format dot]` |
| Resolved artifact paths | `bento artifacts --json` |
| Health check before a deploy | `bento doctor --env <env>` |
| Health check + cloud probes (JWT, cache.bento.build, api.bento.build) | `bento doctor --cloud` |
| Scaffold a new dish | `bento dish add <path> --lang <ecosystem>` |
| Create a new bento (deployment unit) | `bento box add <name>` |
| Cache management | `bento cache stats|clear|push|pull` |
| Manage deploy-target secrets (Cloudflare / Railway) | `bento secret put|list|delete <target> <name>` |
| Toolchain management | `bento toolchain list|install|pin <tool=ver>` |
| Cut a release (bump workspace version, refresh lockfile, commit, tag) | `bento release <patch|minor|major|X.Y.Z>` |
| Sign in to bento.build + stash the cache JWT in OS keychain (or `~/.bento/credentials` 0600 fallback) | `bento login` |

Global flags worth knowing: `--json`, `--no-cache`, `--bento <name>`, `--since <ref>`, `--report-file <path>`, `--skip-install`, `--force-install`, `-v` / `--verbose`.

## Agent-friendly output

Every reporting command supports `--json` with a stable, documented schema. (Streaming verbs — `bento dev`, `bento serve`, `bento run` — pass output straight through, so `--json` is a no-op there.) When reasoning about output:

- Use `--json` (or `--report-file <path>`) instead of parsing stdout.
- Every output type has a JSON Schema: `bento schema [plan|report|why|scaffold|doctor|manifest|error|diagnostics|garnish-payload|prime]`. Run `bento schema` with no arg to list available schemas.
- Task failures include structured `diagnostics[]` for compiler / linter errors (cargo, eslint, golangci-lint, ruff). Don't parse tool-specific stderr — read `diagnostics`.
- Cache decisions aren't mysterious: `bento why <hash>` returns the full input manifest behind any cache key (adapter, toolchain version, env-var names, every hashed file + its blake3 digest).
- Integration tasks (Deploy / Notify / Release) capture an `output_excerpt` (4 KB tail of stdout+stderr) on the `ExecutedTask`, so the deploy URL / build-log URL surfaces in both human + JSON output without needing a second call.

## Deploys

The `bento deploy` verb wraps each platform's native CLI (Railway, Vercel, Cloudflare Workers, Cloudflare Pages). Key rules:

- **Run `bento doctor --env <env>` first.** It's a preflight — fails with structured check names (`integration.railway.env`, `integration.railway.cli`, …) before any real upload. Exit non-zero on any `fail`. Add `--cloud` to additionally validate the remote-cache JWT + ping `cache.bento.build/health` + `api.bento.build/v1/healthz`.
- **Never pass secret values on the CLI.** Use `[environments.<name>]` in `bento.toml` for named secret profiles, or `--secret-from DECLARED=SOURCE` for ad-hoc aliasing. The flag rejects literal-looking values at parse time. To set platform secrets, use `bento secret put <dish> NAME` (reads value from stdin).
- **`bento ci` deliberately excludes side-effectful integration tasks** (Deploy / DeployPreview / Rollback / Notify). Deploys only happen via explicit `bento deploy`.
- **Deploys short-circuit when inputs haven't changed.** Bento records the last successful deploy's input manifest in `.bento/state/deploys.json`; subsequent `bento deploy` runs skip Deploy tasks whose inputs match. Override with `bento deploy --force` when you need to re-deploy regardless (e.g. external env changed).
- **Railway adapter uses `railway up --ci`** so non-TTY callers (which is everything bento launches) actually block on the server-side build outcome. Plain `railway up` silently detaches in non-TTY contexts and exits 0 before Railway has built anything — `--ci` is the only correct form.

## Garnishes — post-deploy hooks (Slack, Linear, custom)

After every Deploy task, bento fans out **Notify-kind** tasks (garnishes) in parallel. They receive a structured `GarnishPayload` JSON on stdin (`bento schema garnish-payload`) and **never affect the deploy's exit code** — a broken Slack webhook can't red-X a successful deploy.

Built-ins (config-driven opt-in via `[integrations.<id>]` in `dish.toml`):

- **`[integrations.slack]`** — POSTs a templated message to a Slack Incoming Webhook. Outcome-driven emoji (rocket / siren / package), URL auto-extraction from the deploy's captured output, stderr code-block on failure (2 KB-capped). Config keys: `webhook_url_env`, `channel`, `username`.
- **`[integrations.linear]`** — Scans the payload for `[A-Z]{2,}-\d+` issue refs across task name / dish name / captured output, then transitions each matched issue to a configurable `target_state` via Linear's GraphQL API. Config keys: `api_key_env`, `target_state`, `fallback_issue_id`, `team`. Failed deploys skip transitions (so a broken release is never marked shipped) but still comment on the fallback issue.
- **`[[garnishes]]` block** — inline custom Notify-kind tasks (GitHub PR comments, PagerDuty triggers, custom log forwards). Each entry becomes a synthetic Notify task with `env` / `required_env` / `required_cli` preflight.

Use `bento deploy --no-notify` to suppress garnishes for a single deploy. Re-fire after a webhook fix without re-deploying with `bento notify --env <env> [target]` (replays the last deploy's sidecar at `.bento/garnish/<bento>/<dish>/<task>.json`).

## Build reports — automatic for `bento ci` / `bento build`

When a `bento://` remote is configured in `[cache]` and the token env var is set, `bento ci` and `bento build` POST a `BuildReport` to `<base>/report/build` after the run completes. Same Bearer auth as cache writes; best-effort (failures log a warning, never fail the build). One report per invocation (summary of all dishes), not per dish. Test/lint runs deliberately don't emit. Self-hosted backends can reject with 404 and bento silently no-ops — the protocol is opt-in for backends that don't care about dashboards. Schema: `bento_cas_protocol::BuildReport` (`package`, `branch`, `sha`, `cache_hit_ratio`, `status`, `duration_ms`).

## Config surface you'll encounter

Three TOML files define a bento workspace:

- **`bento.toml`** at repo root — cache tiers, toolchain pins, `[environments.<name>]` secret profiles.
- **`bentos/<name>.toml`** — one or more — each lists which dishes ship together.
- **`<dish>/dish.toml`** — per-dish config: `language`, `depends_on`, `[toolchain]` overrides, `[tasks.<name>]` custom recipes, `[integrations.<id>]` deploy config (e.g. Railway service name, Cloudflare project name, Slack webhook env), and `[[garnishes]]` blocks for custom post-deploy hooks.

Full reference: `docs/configuration.md`. Deploys + garnishes: `docs/deploying.md`. Agent integration patterns: `docs/agents.md`.

## Installing bento

If `bento` isn't on the host:

```sh
curl -fsSL https://bento.build/install | sh
```

Installs the latest release binary to `~/.local/bin/bento`. Set `BENTO_INSTALL_DIR` to override. On first use, run `bento doctor` to verify the workspace.

## When NOT to use bento

Bento isn't for everything. Keep using the right tool for:

- **Exploratory file operations** — `ls`, `cat`, `grep`, your agent's standard file tools.
- **One-off debugging** — `psql`, `curl`, `dig`, etc.
- **Git operations** — bento doesn't wrap git. `gh` and other repo-management tools are fine.
- **Unfamiliar commands already running** — if the user has a script they're attached to, ask before refactoring it through bento.

If in doubt: "is this something that could live in CI?" → prefer bento. Otherwise → native tool.

## Smoke-testing services: start your own, don't probe the user's

When you need to hit a running service — to reproduce a bug, run a curl-shaped smoke test, or watch a log — **start it yourself** in this session. Do not assume the user has `bento dev <dish>` open in another terminal, and do not go hunting for it with `ss` / `lsof` / `pgrep` / `ps` / `curl localhost:<port>`. Their terminal state isn't your terminal state: the process you find may be wedged, stale, missing key env vars, or about to be killed; the logs you'd want are in a pipe you can't read; and you'll mistake "the user already had this running" for "this works."

**The recipe** (works for any HTTP service in a bento workspace):

```bash
# 1. Start the service in the background with logs captured to a file you own.
#    Use `bento dev <dish>` for one dish, `bento serve <bento>` for a bundle.
mkdir -p /tmp/bento-debug
bento dev <dish> > /tmp/bento-debug/<dish>.log 2>&1 &
echo $! > /tmp/bento-debug/<dish>.pid

# 2. Wait for it to be ready. Don't sleep blindly — poll the readiness probe
#    until it answers, with a hard cap.
for i in {1..30}; do
  curl -fsS -m 1 http://127.0.0.1:<port>/healthz >/dev/null 2>&1 && break
  sleep 0.5
done

# 3. Drive the failure. Capture the full response so you have evidence.
curl -sS -w '\n--- HTTP %{http_code} in %{time_total}s ---\n' \
  -X POST http://127.0.0.1:<port>/<route> \
  -H 'Content-Type: application/json' -d '<payload>'

# 4. Read the log file (the one YOU wrote) for the server-side traceback.
tail -n 200 /tmp/bento-debug/<dish>.log

# 5. Tear it down when you're done. Always.
kill "$(cat /tmp/bento-debug/<dish>.pid)" 2>/dev/null
```

**Why this matters.** Probing the user's terminal-running processes is a subtle category of "I'm using their state as my test fixture." It looks innocent (`curl /healthz` is read-only!) but it lies to you in three ways: (1) you can't read the server's stdout because you don't own the pipe, so a 500 has no traceback to attach to; (2) ports being LISTEN doesn't mean the process is healthy — it can be wedged with a backed-up accept queue; (3) you start treating "it works on the user's machine right now" as the source of truth instead of "it works when freshly started under my recipe", which is the only thing CI / the next session / the user-after-a-reboot will see.

**Specific port-probe shapes to avoid** inside a bento workspace:

- `ss -ltnp` / `lsof -iTCP -sTCP:LISTEN` / `netstat -ltnp` to discover what's running
- `pgrep -f vite|wrangler|uvicorn|firebase|next|nuxt|node` (or the same with `ps -ef | grep`)
- `curl http://localhost:<port>/...` against ports you didn't start in this session
- Reading `/proc/<pid>/fd/{1,2}` to recover stdout/stderr from a process you didn't launch

If the user is actively driving a service in another terminal and asks you to "check what it's doing," that's the exception — but say so out loud ("you have <dish> running on :<port> already, I'll hit that directly") so the user can correct you if their terminal has since moved on.

## Anti-patterns: do NOT reach for native tooling inside a bento workspace

These come up over and over. Each row is a real footgun and the bento verb that does it correctly. **Reach for the right column, not the left, even when you're "just checking" or "just debugging."** Diagnostic invocations are not exempt — bento's structured output already carries the diagnostic info you'd otherwise hunt for.

| ❌ Don't run | ✅ Use instead |
|---|---|
| `bun install`, `npm ci`, `pnpm install`, `yarn install`, `pip install`, `uv sync`, `uv pip install` | `bento install [--bento <name>]` |
| `bun add <pkg>`, `npm i <pkg>`, `uv add <pkg>` | `bento add <pkg> --dish <d> [--dev]` |
| `bun test`, `npm test`, `pytest`, `cargo test`, `go test`, `python -m pytest`, `uv run pytest` | `bento test [<dish>]` |
| `bunx tsc --noEmit`, `tsc --noEmit`, `eslint`, `prettier --check`, `ruff check`, `mypy`, `golangci-lint run`, `python -m compileall` | `bento lint [<dish>]` (or `bento check` for the fast path) |
| `bun run build`, `npm run build`, `bunx vite build`, `cargo build`, `go build` | `bento build [<dish>]` |
| `bun run dev`, `vite`, `bunx wrangler dev` | `bento dev <dish>` (or `bento serve <bento>` for the whole bento) |
| `bunx wrangler deploy`, `railway up`, `vercel --prod` | `bento deploy --env <env> [<dish>]` |
| `bunx tsc --version` (or any tool-version probe) | Don't probe. The version is in `bento doctor` and the dish's `[toolchain]`. If you genuinely need it, use `bento toolchain list`. |
| `ss -ltnp`, `lsof -iTCP`, `pgrep -f vite\|wrangler\|uvicorn`, `curl localhost:<port>` to find/hit the user's running services | Start your own via `bento dev <dish>` in the background with logs to a file. See **Smoke-testing services** above. |

The cost of slipping is real:
- **Lost cache** — a native invocation populates the tool's own cache (`.next`, `target/`, `__pycache__`, `node_modules`) but does not register the result in bento's content-addressed cache, so the next `bento ci` re-builds from scratch.
- **Wrong toolchain** — native invocations use whatever's first on `$PATH`, not the version pinned in `bento.toml [toolchain]` or `dish.toml [toolchain]`. Trains future-you to assume the wrong contract.
- **Missing scoping** — `bun install` at the wrong dir installs at the wrong scope; `bento install --bento <name>` installs exactly the dishes that bento ship together.

If a verb you need genuinely isn't in the list above, file an upstream ask rather than working around it locally — the workaround tends to outlive the missing feature.

## Recommended: install the bento-guard hook

The skill ships a `PreToolUse` hook (`hooks/bento-guard.sh`) that intercepts the Bash tool, detects whether the cwd is inside a bento workspace, and blocks the patterns in the anti-patterns table above with a stderr message naming the right `bento <verb>`. Outside a bento workspace, the hook is a no-op, so it's safe to install user-wide.

**Per-project install** (most conservative — only fires in workspaces that opted in):

1. Drop the hook into the project:

   ```sh
   mkdir -p .claude/hooks
   cp ~/.claude/skills/bento/hooks/bento-guard.sh .claude/hooks/
   chmod +x .claude/hooks/bento-guard.sh
   ```

2. Register it in `.claude/settings.json`:

   ```jsonc
   {
     "hooks": {
       "PreToolUse": [
         {
           "matcher": "Bash",
           "hooks": [
             { "type": "command", "command": "$CLAUDE_PROJECT_DIR/.claude/hooks/bento-guard.sh" }
           ]
         }
       ]
     }
   }
   ```

**User-wide install** (one config, applies in any bento workspace you ever open):

Add the same `PreToolUse` block to `~/.claude/settings.json`, but point at the canonical skill copy so updates flow automatically:

```jsonc
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "$HOME/.claude/skills/bento/hooks/bento-guard.sh" }
        ]
      }
    ]
  }
}
```

The hook walks up from cwd looking for `bento.toml`; outside a bento workspace it exits 0 immediately, so it's safe everywhere.

**Bypass.** If you genuinely need to run a native command (e.g. a one-off debugging probe that bento doesn't cover), prefix the command with `BENTO_GUARD_BYPASS=1 ` and it'll pass through. Reach for this rarely — most "I just need to check X" cases are already covered by `bento doctor`, `bento why`, or `bento <verb> --json`.

**Verifying the install.** Run a known-blocked command in any bento workspace; the hook should refuse with the stderr message. The skill ships a test harness:

```sh
echo '{"tool_input":{"command":"bun install"},"cwd":"<your bento workspace>"}' \
  | ~/.claude/skills/bento/hooks/bento-guard.sh
echo "exit=$?"   # expect 2
```
