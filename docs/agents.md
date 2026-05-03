# Using bento with coding agents

Bento is built for agents first. This page covers how to wire a coding agent (Claude Code, Cursor, Codex, whatever) into a bento-managed repo so the agent uses `bento` verbs instead of rediscovering native tooling every turn.

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

Paste this into `CLAUDE.md` at the root of any bento-managed repo. It tells the agent "this repo uses bento, here's the verb surface, prefer it over native tooling."

````markdown
## Build, test, deploy — use `bento`, not native tooling

This repo is managed by **bento** (<https://github.com/bento-sh/bento>). Bento wraps every dish's native package manager (npm / pnpm / yarn / bun / cargo / go / composer / pip / bundle / mvn / gradle / deno) behind one uniform CLI. **Always prefer `bento` verbs over running the native tools directly** — bento handles per-dish scoping, content-addressed caching, toolchain pinning, and deploy-target routing in one step.

### Verb reference

| Task | Command |
|------|---------|
| Orient yourself in a fresh session (inventory + recommended next verb) | `bento prime` |
| Install every dish's deps | `bento install` |
| Single dish only | `bento install <dish>` |
| Full CI pass (build + check + test + lint on everything) | `bento ci` |
| Build one target | `bento build [bento-or-dish]` |
| Fast type-check (`cargo check`, `go vet`, …) | `bento check [bento-or-dish]` |
| Run tests | `bento test [bento-or-dish]` |
| Run linters | `bento lint [bento-or-dish]` |
| Add a dependency to a dish (one or many packages, optionally dev) | `bento add <pkg>… --dish <d> [--dev]` |
| Invoke a custom `[tasks.<name>]` block ad-hoc (CLIs, migrations, one-off scripts) | `bento run <dish> <task> -- <args…>` |
| Deploy to staging | `bento deploy --env staging` |
| Deploy to prod | `bento deploy --env prod` |
| Preview deploy | `bento deploy --preview --env staging` |
| Re-send Slack / Linear notifications without re-deploying | `bento notify --env <env> [target]` |
| Explain a cache decision | `bento why <cache-key-prefix>` |
| Health check (config + toolchains + integrations) | `bento doctor` |
| Show what would run without running it | `bento plan` |
| Show resolved artifact paths per dish | `bento artifacts --json` |

### Hot tips

- **Start with `bento prime`.** It's purpose-built for session-start: workspace inventory, cache state, plan preview, and a recommended next verb in one command. `bento prime --json` has a schema-stable shape (`bento schema prime`); `recommended_next[0]` is the first thing to do.
- **Always pass `--json` when you want to reason about the output.** Every reporting command emits structured JSON via the flag; shapes are stable and documented via `bento schema <target>`. Streaming verbs (`bento dev`, `bento serve`, `bento run`) pass through to the wrapped process — `--json` is a no-op there.
- **Don't read stderr to decide what went wrong on a failed task.** Use `bento ci --json` — the `executedTask.outcome` tagged union has `kind: "failed"` with `exit_code` and `stderr_excerpt`, plus structured `diagnostics[]` for compiler / linter errors when available.
- **If something looks miscached or wrongly-built, use `bento why <hash>`** rather than guessing. It returns the full input manifest (every hashed file with its blake3 digest, toolchain version, env-var names, …). The cache key itself is visible on every task in the report.
- **Before a `bento deploy`, run `bento doctor --env <env>`** first — the preflight fails fast with structured check names (`integration.railway.env`, `integration.railway.cli`, …) so you know which knob to tweak.
- **Never pass secret values on the CLI.** Use `[environments.<name>]` in `bento.toml` for named secret profiles, or `--secret-from DECLARED=SOURCE` for ad-hoc aliasing. The flag rejects literal-looking values so an accidental `--secret-from TOKEN=rlw_abc123` errors at parse time.
- **Check if bento itself knows about the work you're about to do.** Unfamiliar task names? `bento plan` shows the resolved task list for every dish. Unfamiliar dish? `bento dish add <path>` scaffolds one.

### Installing bento (if the binary isn't on the host yet)

```sh
curl -fsSL https://raw.githubusercontent.com/bento-sh/bento/main/install.sh | sh
```

Or pin a version:

```sh
curl -fsSL https://raw.githubusercontent.com/bento-sh/bento/v0.1.0/install.sh | sh -s -- 0.1.0
```

Installs to `~/.local/bin/bento` by default. Set `BENTO_INSTALL_DIR` to override.
````

Drop that block in as-is. Most coding agents (Claude Code, Cursor, Codex, ...) scan top-level markdown files on session start and treat them as persistent instructions.

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

`bento mcp install --help` lists every supported client and the file it writes.

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

Restart Claude Desktop. The `mcp__bento__*` tools appear in the tool picker — grouped by capability:

- **Read-only** (no confirmation needed): `bento_prime`, `bento_plan`, `bento_dish_list`, `bento_box_list`, `bento_doctor`, `bento_why`, `bento_artifacts`, `bento_schema`.
- **Execution** (mutates `node_modules` / `target/`): `bento_install`, `bento_build`, `bento_check`, `bento_test`, `bento_lint`, `bento_ci`.
- **Destructive + open-world** (client shows stronger confirmation): `bento_deploy`, `bento_notify`.

Write-path tools (`bento_dish_add`, `bento_init`, `bento_migrate`) are a follow-up — their CLI modules need to land in `bento-core` first.

### Claude Code

Run `bento mcp install claude-code` (or `bento mcp install claude-code --local` for project scope). The installer writes:

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

## Claude Code skill (opt-in, one-time setup)

Bento ships a [Claude Code skill](../skills/bento/SKILL.md) that activates automatically when the agent is working in a bento-managed repo — no `CLAUDE.md` snippet required per repo. Install once:

```sh
mkdir -p ~/.claude/skills/bento
curl -fsSL https://raw.githubusercontent.com/bento-sh/bento/main/skills/bento/SKILL.md \
  -o ~/.claude/skills/bento/SKILL.md
```

After that, Claude Code auto-loads the skill when it sees a `bento.toml` / `bentos/` / `dish.toml` in the workspace. The skill covers the same verb reference as the `CLAUDE.md` snippet above, plus deploy preflight rules, secret handling, and when-not-to-use guidance.

If you prefer not to install the skill globally, the per-repo `CLAUDE.md` snippet above is equivalent — drop it into any repo that bento manages.

## Related

- [configuration.md](./configuration.md) — every TOML field.
- [deploying.md](./deploying.md) — bento's deploy verbs + secret handling in depth.
- [adopt-existing-repo.md](./adopt-existing-repo.md) — dropping bento into an existing monorepo.
- [new-project.md](./new-project.md) — bento from zero.
- [plugins.md](./plugins.md) — subprocess adapter protocol (for languages bento doesn't know about yet).
