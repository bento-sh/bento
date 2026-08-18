---
name: bento
description: Use in any repo managed by bento — a `bento.toml` at the root, a `bentos/` dir, or per-subdir `dish.toml` files. Bento is a polyglot monorepo orchestrator wrapping every dish's native package manager (npm / pnpm / yarn / bun / cargo / go / composer / pip / bundle / mvn / gradle / deno) behind uniform verbs, with content-addressed caching and toolchain pinning. Run `bento prime` first, then prefer bento verbs over the native tools. Also use when the user mentions bento, polyglot monorepo orchestration, build caching, garnishes, or deploys to Railway / Vercel / Cloudflare.
---

# bento — polyglot monorepo orchestrator

A repo with `bento.toml`, a `bentos/` dir, or `dish.toml` files is bento-managed:
reach for bento verbs, not the native package managers.

## Start with `bento prime`

Run it at session start and after `/clear` or compaction. It returns inventory,
cache state, a plan preview, and a ranked `recommended_next` — in under 2s, with
no network and no task execution. `bento prime --json` is schema-stable
(`bento schema prime`); switch on `recommended_next[0]`.

## Verbs

| What you want | Run |
|---|---|
| Orient in a fresh session | `bento prime` |
| Install deps (replaces `npm ci`, `go mod download`, `composer install`, …) | `bento install [target]` |
| Full CI pass (build + check + test + lint) | `bento ci [target]` |
| Build, fast type-check, test, lint | `bento build [target]` · `bento check [target]` · `bento test [target]` · `bento lint [target]` |
| Add a dependency to a dish | `bento add <pkg>… --dish <d> [--dev]` |
| Invoke a `[tasks.<name>]` block ad-hoc (bypasses cache) | `bento run <dish> <task> -- <args…>` |
| Run a service with hot reload | `bento dev <dish>` · `bento serve <bento>` |
| Deploy; re-fire Slack/Linear hooks without re-deploying | `bento deploy --env <env> [target]` · `bento notify --env <env>` |
| What would run, and why it cached | `bento plan` · `bento why <key-or-dish:task>` |
| Inventory | `bento dish list` · `bento box list` · `bento artifacts --json` |
| Health check (add `--env <env>` before a deploy, `--cloud` for remote probes) | `bento doctor` |
| Scaffold / adopt | `bento init` · `bento migrate turbo\|nx\|lerna\|make\|moon\|rush` · `bento dish add <path> --lang <ecosystem>` · `bento box add <name>` |
| Cache, toolchains, secrets, release | `bento cache stats\|clear\|prune\|push\|pull` · `bento toolchain list\|install\|pin` · `bento secret put\|list\|delete <dish> <NAME>` · `bento release <patch\|minor\|major>` |

`target` is a bento or dish name; omit it for everything. Global flags:
`--json`, `--no-cache`, `--bento <name>`, `--report-file <path>`, `-v`.

`bento migrate` emits a starting point, not a finished translation: read `notes[]`
in its `--json` report for what it couldn't port, and note that it never
overwrites an existing `bento.toml` (exit 1 means a conflict needs `--force`).

## Rules that matter

1. **Prefer bento verbs.** A native `npm ci` / `cargo build` / `pytest` fills that
   tool's own cache but not bento's, and runs whatever is first on `$PATH` rather
   than the pinned toolchain — the next `bento ci` rebuilds from scratch. This
   includes "just checking" invocations.
2. **Read `--json`; don't parse stderr.** Every reporting verb has a published
   shape (`bento schema [plan|report|why|doctor|manifest|error|diagnostics|prime]`).
   A failed task carries `outcome.kind = "failed"` with `exit_code`,
   `stderr_excerpt`, and structured `diagnostics[]` for cargo / eslint /
   golangci-lint / ruff. Output is compact when piped, pretty in a terminal.
   Streaming verbs (`bento dev`, `bento serve`, `bento run`) pass through — `--json`
   is a no-op there.
3. **Cache surprise → `bento why <key>`.** It returns the full input manifest behind
   any cache key: adapter, toolchain version, env-var names, every hashed file with
   its blake3 digest. Every task in a report carries its key.
4. **Never pass secret values on the CLI.** Use `[environments.<env>]` profiles in
   `bento.toml`, or `--secret-from DECLARED=SOURCE` for ad-hoc name-to-name
   aliasing (literal-looking values are rejected at parse time).
   `bento secret put <dish> NAME` reads the value from stdin.
5. **Start services yourself.** To smoke-test or reproduce, run
   `bento dev <dish> > /tmp/bento-dev.log 2>&1 &` (or `bento serve <bento>`), poll
   the health endpoint until it answers, read the log *you* own, and kill it when
   done. Never probe the user's processes with `ss` / `lsof` / `pgrep` /
   `curl localhost:<port>` — their terminal state isn't yours, and you can't read
   a pipe you didn't open.

## MCP tools

If the client lists `mcp__bento__*`, prefer them over shelling out — same JSON,
no shell:

- Read-only: `prime`, `plan`, `dish_list`, `box_list`, `doctor`, `why`,
  `artifacts`, `schema`
- Execution: `install`, `build`, `check`, `test`, `lint`, `ci`
- Destructive (remote infra): `deploy`, `notify` — both require `env`

Failures arrive as tool results with `isError` and a
`{kind, message, hint, next_steps}` object — read `next_steps` instead of
retrying blindly. Write-path verbs (`init`, `dish add`, `migrate`, `add`, `run`,
`dev`, `serve`, `cache`, `toolchain`, `secret`) are CLI-only. Register the server
with `bento mcp install` (auto-detects every installed client).

## Not bento's job

File exploration (`ls`, `grep`, reads), git, one-off `psql` / `curl` / `dig`.
Rule of thumb: could this step live in CI? → bento. Otherwise → native tool.

## Installing bento

```sh
curl -fsSL https://bento.build/install | sh
```

Installs `bento` + `bento-mcp` to `~/.local/bin` (override with
`BENTO_INSTALL_DIR`), then run `bento doctor`.

## Deeper detail

- [docs/agents.md](https://github.com/bento-sh/bento/blob/main/docs/agents.md) —
  MCP wiring, the `PreToolUse` guard hook in `hooks/bento-guard.sh`, anti-pattern
  table, service-smoke-test recipe.
- [docs/adopt-existing-repo.md](https://github.com/bento-sh/bento/blob/main/docs/adopt-existing-repo.md) —
  `bento migrate` per-tool coverage and the non-destructive contract.
- [docs/deploying.md](https://github.com/bento-sh/bento/blob/main/docs/deploying.md) —
  deploy preflight, garnishes (Slack / Linear / custom), secret handling.
- [docs/configuration.md](https://github.com/bento-sh/bento/blob/main/docs/configuration.md) —
  every `bento.toml` / `bentos/*.toml` / `dish.toml` field.
