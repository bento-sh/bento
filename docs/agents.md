# Using bento with coding agents

Bento is built for agents first. This page covers how to wire a coding agent — Claude Code, Claude Desktop, Cursor, Windsurf, Codex CLI, OpenCode, Zed, or any other MCP-speaking client — into a bento-managed repo so the agent uses `bento` verbs instead of rediscovering native tooling every turn.

---

## The problem bento solves for agents

A polyglot monorepo without bento punishes agents the way it punishes humans — only harder. The agent has to:

- Discover which package manager each subdir uses (`package-lock.json`? `pnpm-lock.yaml`? `go.mod`? `composer.json`?)
- Pick the right invocation per subdir (`npm ci` vs `pnpm install --frozen-lockfile` vs `yarn install --immutable` vs `go mod download` vs `composer install`)
- Handle deploy per-platform (`vercel deploy --prod --yes`? `railway up --ci --service X`? `wrangler deploy`?)
- Parse each tool's stdout format — different for `npm run test` vs `go test` vs `pytest`

Every one of these is a token-burn opportunity, and every one can go wrong. Bento collapses them into a small set of uniform verbs:

| Agent wants to… | Without bento | With bento |
|-----------------|---------------|------------|
| Install deps | `npm ci` / `pnpm install --frozen-lockfile` / `go mod download` / `composer install` / … | `bento install` |
| Run CI-like checks | `npm test && npm run lint && go test ./... && …` | `bento ci` |
| Deploy to Railway | `railway up --ci --service <name>` from the right dir | `bento deploy --env <env>` |
| See *why* a task ran or cached | Check file mtimes, parse lockfiles, squint | `bento why <hash>` (JSON) |
| Check if everything's wired up | Read the README, hope you didn't miss a step | `bento doctor --env <env>` |

Every output is JSON-available via `--json`, schemaed via `bento schema <type>`, and stable enough to switch on.

---

## Drop-in `CLAUDE.md` / `AGENTS.md` snippet

`bento init` writes this block into `AGENTS.md` (and a one-line `@AGENTS.md` import into `CLAUDE.md`) between HTML-comment markers, so re-running init upgrades it in place without touching your prose. To adopt it by hand, paste it into whichever file your agent reads on session start:

````markdown
> **This repo is managed by [bento](https://bento.build)** — a polyglot monorepo orchestrator. Always prefer `bento` verbs over native package managers (`npm`, `pnpm`, `cargo`, `go`, `pip`, `composer`, …): bento scopes each dish, content-hashes results into a shared cache, and pins toolchains. Start every fresh session with `bento prime`.

## Verbs

| What you want | Run |
|---|---|
| Orient in a fresh session | `bento prime` |
| Install deps | `bento install [target]` |
| Full CI pass (build + check + test + lint) | `bento ci [target]` |
| Build, fast type-check, test, lint | `bento build [target]` · `bento check [target]` · `bento test [target]` · `bento lint [target]` |
| Add a dependency to a dish | `bento add <pkg>… --dish <d> [--dev]` |
| Invoke a `[tasks.<name>]` block ad-hoc | `bento run <dish> <task> -- <args…>` |
| Run a service with hot reload | `bento dev <dish>` · `bento serve <bento>` |
| Deploy; re-fire Slack/Linear hooks | `bento deploy --env <env> [target]` · `bento notify --env <env>` |
| What would run, and why it cached | `bento plan` · `bento why <key-or-dish:task>` |
| Inventory | `bento dish list` · `bento box list` · `bento artifacts --json` |
| Health check (add `--env <env>` before a deploy) | `bento doctor` |

`target` is a bento or dish name; omit it to act on everything.

## Rules

1. **Prefer bento verbs.** A native `npm ci` / `cargo build` / `pytest` fills that tool's own cache but not bento's, and runs whatever is first on `$PATH` instead of the pinned toolchain — the next `bento ci` then rebuilds from scratch.
2. **Read `--json`, don't parse stderr.** Every reporting verb has a published shape (`bento schema <target>`); a failed task carries `outcome.kind = "failed"` with `exit_code`, `stderr_excerpt`, and structured `diagnostics[]`.
3. **Cache surprise → `bento why <key>`**, never guesswork. It returns the full input manifest behind any cache key.
4. **Never pass secret values on the CLI.** Use `[environments.<env>]` profiles in `bento.toml`, or `--secret-from DECLARED=SOURCE` for ad-hoc name-to-name aliasing. `bento secret put <dish> NAME` reads the value from stdin.
5. **Start services yourself.** `bento dev <dish> > /tmp/bento-dev.log 2>&1 &`, poll the health endpoint, read the log you own, kill it when done — don't probe the user's processes with `ss` / `lsof` / `pgrep` / `curl localhost:<port>`.

Not bento's job: file exploration, git, one-off `psql` / `curl` / `dig`. Rule of thumb: could this step live in CI? → bento.

**MCP server**: `bento mcp install` registers `bento-mcp` with every detected agent client, exposing the read-only and execution verbs as `mcp__bento__*` tools.
````

Most coding agents — Claude Code, Claude Desktop, Cursor, Windsurf, Codex CLI, OpenCode, Zed, … — scan top-level markdown files on session start and treat them as persistent instructions. The same content ships as a [Claude Code skill](#claude-code-skill-auto-installed-by-installsh), which loads only when a bento workspace is detected.

---

## What the snippet does

- **Vocabulary anchoring.** Naming bento up front stops the agent from rediscovering "oh, this is a monorepo, I should run npm on one subdir and go on another."
- **Verb table.** The agent already has context for what `npm test` does. Giving them the bento-equivalent in the same shape is enough for them to map the intent across without reinvention.
- **`--json` pointer.** Agents that pipe stdout through string parsing waste tokens on brittle regex. Pointing them at `--json` + `bento schema <target>` gives them stable, declarative access to every decision.
- **`bento why` as the "ask" rather than the "guess".** Agents tend to guess why a build was rebuilt ("probably the dependency changed"). `bento why <hash>` returns the authoritative answer.
- **Secret-handling rule.** The literal-value rejection at the flag parser catches accidental leaks but the agent should learn the pattern. Spelling out "never pass secret values on the CLI" saves a follow-up correction.

---

## When your agent *shouldn't* use bento

Not every command needs to flow through bento. The snippet nudges toward bento but shouldn't block the agent from:

- **Exploring the repo** — `ls`, `cat`, `grep` (or the agent's equivalents) to understand structure.
- **One-off debugging commands** — e.g. `psql` to inspect a dev database, `curl` to probe an API.
- **Git operations** — bento doesn't wrap git and shouldn't.

A good mental rule: "if the agent is about to do something that could've been part of CI, prefer bento; otherwise use whatever fits."

---

## MCP server — `bento-mcp` (preferred for agents)

bento ships a second binary, `bento-mcp`, that exposes every bento verb as a typed [Model Context Protocol](https://modelcontextprotocol.io) tool. Clients that speak MCP — Claude Code, Claude Desktop, Cursor, Windsurf, Codex CLI, OpenCode, and Zed — auto-discover the tools and invoke them directly: no shell-out, no stdout parsing, no per-repo `CLAUDE.md` snippet. The tool outputs match `bento <verb> --json` byte-for-byte.

Install bento as usual (`curl | sh`); `bento-mcp` lands on `PATH` next to `bento`. Then register it in whichever clients you use:

```sh
bento mcp install                # auto-detect every installed client and register
bento mcp install claude-code    # one client at a time (positional arg)
bento mcp install codex --local  # project-scoped (Codex trusted-projects flow)
```

`bento mcp install --help` lists every supported client and the file it writes. Config files with `//` or `/* */` comments (Zed ships a commented `settings.json`) are read as JSONC; when bento only has to add a top-level key it splices the entry in as text so your comments survive. Pass `--pin-workspace <PATH>` to bake `--workspace <PATH>` into the registered command — it is deliberately not spelled `--workspace`, which is bento's own global flag.

With no arguments it auto-detects every installed client (Claude Code, Claude Desktop, Cursor, Windsurf, Codex CLI, OpenCode, Zed) and writes the right config for each; a positional client name registers just that one.

### The tool surface

Sixteen tools, every one carrying MCP annotations so clients know how hard to confirm:

- **Read-only** (`readOnlyHint`, no confirmation needed): `prime`, `plan`, `dish_list`, `box_list`, `doctor`, `why`, `artifacts`, `schema`.
- **Execution** (mutates `node_modules` / `target/` only): `install`, `build`, `check`, `test`, `lint`, `ci`.
- **Destructive + open-world** (`destructiveHint`, client shows stronger confirmation): `deploy`, `notify`. Both require an `env` declared in `[environments.<env>]`; `secret_from` carries name-to-name aliases only, never values.

The MCP surface is deliberately a subset of the CLI: write-path verbs (`init`, `dish add`, `migrate`, `add`, `run`, `dev`/`serve`, `cache`, `toolchain`, `secret`, `login`, `release`) are CLI-only. Keeping the tool list short keeps the agent's tool-definition budget small — the CLI stays the primary surface.

### Results, failures, progress

- Success returns the same JSON as `bento <verb> --json` in `structuredContent`.
- Failures are **tool results**, not JSON-RPC protocol errors: `isError: true` plus the `{kind, message, hint, next_steps}` envelope the CLI emits (`bento schema error`). Switch on `kind`; follow `next_steps`.
- Execution tools set `isError` when `summary.failed > 0` or `summary.install_failures > 0` — the rule that makes the CLI exit non-zero. The report is still in `structuredContent`, so the failing task's `stderr_excerpt` and `diagnostics[]` are right there.
- `ci` / `build` / `check` / `test` / `lint` / `install` / `deploy` send a `notifications/progress` update as each dish finishes (when the client supplied a `progressToken`), and stop at the next dish boundary if the client cancels the request.

### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or the equivalent on Windows / Linux:

```json
{
  "mcpServers": {
    "bento": {
      "command": "bento-mcp",
      "args": ["--workspace", "/abs/path/to/your/repo"]
    }
  }
}
```

Restart Claude Desktop. The `mcp__bento__*` tools appear in the tool picker.

### Claude Code

Run `bento mcp install claude-code` (or `bento mcp install claude-code --local` for project scope). When the `claude` CLI is on `PATH`, the registration is delegated to `claude mcp add --scope user|project` — `~/.claude.json` holds all of Claude Code's user state and a live session rewrites it constantly, so its own CLI does the write. Without `claude` on `PATH`, bento edits the file itself (atomically, keeping the file's mode). Either way the entry lands in:

- User-global → `~/.claude.json` (single dotfile holding all Claude Code state, including `mcpServers`).
- Project-local → `.mcp.json` at the repo root (Claude Code reads this when it lives next to `.claude/settings.json`).

If you'd rather write the file by hand, the entry shape is:

```json
{
  "mcpServers": {
    "bento": {
      "command": "bento-mcp",
      "env": { "BENTO_WORKSPACE_ROOT": "${workspaceFolder}" }
    }
  }
}
```

Claude Code renders the tools as `mcp__bento__<verb>` — check via `/mcp` after connecting.

### Worked example

End-to-end flow an agent follows in a fresh session:

```
1. agent → mcp__bento__prime
   ← {workspace_root, bentos: [...], dishes: [...], plan: {hits, misses},
      recommended_next: ["6 task(s) would miss cache — run `bento ci` ..."]}

2. agent sees misses, calls mcp__bento__plan
   ← {bentos: [{name, dishes: [{name, tasks: [{name, status: "cache_miss",
      miss_reason: "never_cached", key: "73f616..."}, ...]}]}]}

3. agent picks a specific miss it wants to understand:
   mcp__bento__why {target: "marketing:lint"}
   ← {key, manifest: {files: [{path, blake3, size_bytes}, ...],
      env_vars: [...], toolchain: "bun@1.1.30"}}
```

No shell, no stdout-parsing, every step returns structured JSON the agent's tool-call handling already understands.

### Server lifetime + `--workspace` resolution

`bento-mcp` is a single-workspace stdio server — launch one per repo. Workspace resolves in order: `--workspace <PATH>` flag > `$BENTO_WORKSPACE_ROOT` env > current working directory (walking upward for `bento.toml` / `bentos/`). Agents that manage multiple repos should add multiple entries to their MCP client config — one per repo.

---

## Claude Code skill (auto-installed by `install.sh`)

Bento ships a [Claude Code skill](../skills/bento/SKILL.md) that activates automatically when the agent is working in a bento-managed repo — no `CLAUDE.md` snippet required per repo. **The official installer drops the skill under `~/.claude/skills/bento/` for you** — if you ran `curl -fsSL https://bento.build/install | sh`, you already have it.

To verify or update by hand:

```sh
ls ~/.claude/skills/bento/SKILL.md          # should exist
BENTO_FORCE_SKILL=1 curl -fsSL https://bento.build/install | sh   # re-fetch from the latest release tarball
```

If you'd rather grab the file directly without re-running the installer:

```sh
mkdir -p ~/.claude/skills/bento
curl -fsSL https://raw.githubusercontent.com/bento-sh/bento/main/skills/bento/SKILL.md \
  -o ~/.claude/skills/bento/SKILL.md
```

After that, Claude Code auto-loads the skill when it sees a `bento.toml` / `bentos/` / `dish.toml` in the workspace. The skill is deliberately small (under 6 KB — it lands in the agent's context every time it triggers): verb table, five rules, MCP tool names, and links back to this page for the depth.

If you prefer not to install the skill globally, the per-repo `AGENTS.md` snippet above is equivalent — drop it into any repo that bento manages.

The bundle also ships `hooks/bento-guard.sh`, a `PreToolUse` hook that blocks the anti-patterns below — see [the bento-guard hook](#the-bento-guard-hook).

## Anti-patterns: native tooling inside a bento workspace

Each row is a real footgun and the verb that does it correctly. Diagnostic invocations are not exempt — bento's structured output already carries what you'd go hunting for.

| Don't run | Use instead |
|---|---|
| `bun install`, `npm ci`, `pnpm install`, `yarn install`, `pip install`, `uv sync` | `bento install [--bento <name>]` |
| `bun add <pkg>`, `npm i <pkg>`, `uv add <pkg>` | `bento add <pkg> --dish <d> [--dev]` |
| `bun test`, `npm test`, `pytest`, `cargo test`, `go test` | `bento test [<dish>]` |
| `tsc --noEmit`, `eslint`, `prettier --check`, `ruff check`, `mypy`, `golangci-lint run` | `bento lint [<dish>]` (or `bento check` for the fast path) |
| `npm run build`, `vite build`, `cargo build`, `go build` | `bento build [<dish>]` |
| `npm run dev`, `vite`, `wrangler dev` | `bento dev <dish>` (or `bento serve <bento>`) |
| `wrangler deploy`, `railway up`, `vercel --prod` | `bento deploy --env <env> [<dish>]` |
| `tsc --version`, or any tool-version probe | Don't probe — `bento doctor`, or `bento toolchain list` |
| `ss -ltnp`, `lsof -iTCP`, `pgrep -f vite`, `curl localhost:<port>` to find the user's services | Start your own — see below |

The cost of slipping: a native invocation populates that tool's own cache (`.next`, `target/`, `__pycache__`) but registers nothing in bento's content-addressed cache, so the next `bento ci` rebuilds from scratch; and it runs whatever is first on `$PATH` rather than the version pinned in `[toolchain]`.

---

## Smoke-testing services: start your own, don't probe the user's

When an agent needs to hit a running service — reproduce a bug, run a curl-shaped smoke test, watch a log — it should start one itself. Probing whatever is already listening lies in three ways: the agent can't read a pipe it doesn't own (so a 500 has no traceback), a LISTEN socket doesn't mean the process is healthy, and "works on the user's machine right now" quietly replaces "works when freshly started".

```bash
# 1. Start it in the background with logs in a file you own.
mkdir -p /tmp/bento-debug
bento dev <dish> > /tmp/bento-debug/dish.log 2>&1 &
echo $! > /tmp/bento-debug/dish.pid

# 2. Poll for readiness — don't sleep blindly.
for i in {1..30}; do
  curl -fsS -m 1 http://127.0.0.1:8080/healthz >/dev/null 2>&1 && break
  sleep 0.5
done

# 3. Drive the failure, capturing evidence.
curl -sS -w '\n--- HTTP %{http_code} in %{time_total}s ---\n' \
  -X POST http://127.0.0.1:8080/route \
  -H 'Content-Type: application/json' -d '{}'

# 4. Read YOUR log for the server-side traceback, then tear it down.
tail -n 200 /tmp/bento-debug/dish.log
kill "$(cat /tmp/bento-debug/dish.pid)"
```

The exception: if the user is actively driving a service in another terminal and asks the agent to check on it, the agent should say so out loud ("you have `<dish>` on `:8080` already, I'll hit that directly") so they can correct it.

---

## The bento-guard hook

The skill bundle ships `hooks/bento-guard.sh`, a Claude Code `PreToolUse` hook that intercepts Bash calls, checks whether the cwd is inside a bento workspace, and blocks the anti-patterns above with a message naming the right verb. Outside a bento workspace it exits 0 immediately, so a user-wide install is safe.

Per-project:

```sh
mkdir -p .claude/hooks
cp ~/.claude/skills/bento/hooks/bento-guard.sh .claude/hooks/
chmod +x .claude/hooks/bento-guard.sh
```

```jsonc
// .claude/settings.json — or ~/.claude/settings.json with the command
// pointed at $HOME/.claude/skills/bento/hooks/bento-guard.sh for a
// user-wide install that tracks skill updates.
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

Prefix a command with `BENTO_GUARD_BYPASS=1` for the rare genuine one-off. Verify the install:

```sh
echo '{"tool_input":{"command":"bun install"},"cwd":"/path/to/workspace"}' \
  | ~/.claude/skills/bento/hooks/bento-guard.sh
echo "exit=$?"   # expect 2
```

---

## Related

- [configuration.md](./configuration.md) — every TOML field.
- [deploying.md](./deploying.md) — bento's deploy verbs + secret handling in depth.
- [adopt-existing-repo.md](./adopt-existing-repo.md) — dropping bento into an existing monorepo.
- [new-project.md](./new-project.md) — bento from zero.
- [plugins.md](./plugins.md) — subprocess adapter protocol (for languages bento doesn't know about yet).
