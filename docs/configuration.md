# Configuration reference

Bento is configured by three TOML files, by convention:

| File | Purpose | Required? |
|------|---------|-----------|
| `bento.toml` | Repo-wide defaults: cache tiers, toolchain pins, plugin filters, container execution | optional (every field defaulted) |
| `bentos/<name>.toml` | Names a deployment grouping and lists its dishes | at least one |
| `<dish>/dish.toml` | Names a dish, declares its language and tasks | one per dish |

A minimal workspace needs just one `bentos/<name>.toml` and one `dish.toml`. The repo-wide `bento.toml` is optional and every field has a working default.

This page documents every field. For the conceptual model (bentos vs dishes vs tasks) see the [README](../README.md#vocabulary).

---

## `bento.toml`

Optional repo-wide defaults. Place at the repo root next to `bentos/`. Every field shown here matches the built-in default — you only need to write the file at all to override something.

```toml
# bento.toml — repo-wide defaults

[defaults]
# Max dishes to run in parallel within one level of the dep graph.
# Omit to auto-size to std::thread::available_parallelism().
parallelism = 4
# Abort at the next dep-graph level boundary on the first failed dish.
fail_fast = true

[cache]
# Local content-addressed cache at ~/.bento/cache.
local = true
# GitHub Actions cache tier. true | false | "auto" (= on inside a workflow).
gha = "auto"

# Remote cache — pick ONE of the two URL schemes below.
#
# 1. S3-compatible (any bucket: AWS, Cloudflare R2, MinIO, Backblaze B2):
remote = "s3://my-bucket/optional/prefix"
remote_region = "us-east-1"
# remote_endpoint = "https://<account>.r2.cloudflarestorage.com"  # non-AWS only
# Credentials from the AWS env chain:
#   AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN
#
# 2. Hosted bento cache (or any Bearer-auth HTTP server implementing the
#    same wire protocol):
# remote = "bento://cache.bento.build"
# remote_token_env = "BENTO_CACHE_TOKEN"   # env var holding the JWT
# Credential resolution: env var first, then the OS keychain entry
# written by `bento login`, then ~/.bento/credentials (0600) as a
# headless fallback. Run `bento login` once for interactive setup;
# use $BENTO_CACHE_TOKEN in CI.

[telemetry]
# Anonymous usage metrics. Set false to opt out.
enabled = true

[execution]
# Container execution mode. never | auto | always.
#  - never: tasks run on the host (default).
#  - always: every task is wrapped in `<runtime> run --rm ...`.
#  - auto: containerise when an image is declared AND a runtime is on PATH.
container = "never"
# Container image ref to wrap tasks in. Required for container = "always".
image = "ghcr.io/your-org/runner:1"

[toolchain]
# Repo-wide tool version pins. Each `<tool> = "<version>"` writes the
# version into bento's content-cache key, so a toolchain bump invalidates
# every dish that uses it. Per-dish pins (in dish.toml) override these.
go = "1.22.3"
node = "22.1.0"
java = "21"
# When true, bento doesn't try to install pinned versions itself —
# it expects the system PATH to already have the right tool.
use_system = false

[plugins]
# Adapter ids that should never be loaded even if found on $PATH.
disable = ["zig"]
# If set, ONLY these adapter ids are loaded; everything else is skipped silently.
allowlist = ["erlang", "elixir"]
```

### `[defaults]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `parallelism` | int | `available_parallelism()` | Max concurrent dishes per dep-graph level. |
| `fail_fast` | bool | `true` | Stop at the next dep-graph level boundary on first failure. |

### `[cache]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `local` | bool | `true` | Use the local content-addressed cache at `~/.bento/cache`. |
| `gha` | bool \| `"auto"` | `"auto"` | Use the GitHub Actions cache tier (the composite action wraps `~/.bento/cache` with `actions/cache@v4`). `"auto"` activates only when running inside a GHA workflow. |
| `remote` | string | unset | Remote cache URL. Two schemes: `s3://<bucket>/<optional/prefix>` (any AWS-signed object store), or `bento://<host>[/<prefix>]` (JWT-auth'd HTTP cache — `bento://cache.bento.build` for the hosted service). See README's "Caching" section. |
| `remote_region` | string | `"us-east-1"` | AWS region for the bucket. S3 scheme only. |
| `remote_endpoint` | string | unset | Custom S3-compatible endpoint URL. Required for non-AWS services (Cloudflare R2, MinIO, Backblaze B2); omit for native AWS S3. S3 scheme only. |
| `remote_token_env` | string | `"BENTO_CACHE_TOKEN"` | Name of the env var holding the JWT, for the `bento://` scheme. Resolver walks env var → OS keychain entry `("bento", "cache-token")` (populated by `bento login`) → `~/.bento/credentials` (0600 fallback). Bento never stores the token in repo state. |

### `[telemetry]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Anonymous usage metrics. |

### `[execution]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `container` | `"never"` \| `"auto"` \| `"always"` | `"never"` | Container execution mode. |
| `image` | string | unset | Container image ref. Required for `container = "always"`; advisory for `"auto"`. |

When containerised, bento runs each task as `<runtime> run --rm -u <uid>:<gid> -v <dish>:/work -w /work --env HOME=/work --env <name> <image> sh -c <run>`. Runtime auto-detection order: `docker` → `podman` → `nerdctl`. UID is preserved so output files stay host-owned.

**Default `HOME=/work`:** the container's `$HOME` defaults to the mounted workdir. `--user <host-uid>` leaves the image's root `$HOME` (often `/root`) unwritable by the invoking UID, so without this default, tools that default their cache dir to `$HOME/.cache/<tool>` — Go (`GOCACHE`), Cargo (`CARGO_HOME`), pnpm, npm — would fail on first run with a permission error. Pointing HOME at the volume mount puts those caches under the dish's writable scratch space and keeps them across invocations. If you genuinely need a different HOME, declare it in `[tasks.<name>] env = ["HOME"]`: the forwarded host value wins (docker `--env` applies last-write per variable).

### `[toolchain]`

A free-form table of `<tool> = "<version>"` pairs, plus the boolean `use_system`. The keys aren't enumerated — bento accepts any `<tool>` name and includes `<tool>:<version>` in the content-cache key.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `use_system` | bool | `false` | If `true`, bento expects the pinned tools to already be on `$PATH` and won't try to install them itself. |
| `<tool>` | string | unset | Pin a tool to a specific version. Examples: `go = "1.22.3"`, `node = "22.1.0"`, `python = "3.12"`, `ruby = "3.2.2"`, `java = "21"`. |

Per-dish `dish.toml` `[toolchain]` overrides these.

### `[plugins]`

Filters applied to subprocess plugin discovery (binaries on `$PATH` matching `bento-adapter-<id>`). See [plugins.md](./plugins.md) for the full plugin protocol.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `disable` | `string[]` | `[]` | Adapter ids to never load. |
| `allowlist` | `string[]` \| unset | unset | If set, ONLY these adapter ids are loaded. |

Built-in adapters always win on id collision regardless of `[plugins]` settings.

### `[environments.<name>]`

Named deploy environments with saved **secret aliases** for `bento deploy --env <name>` and `bento doctor --env <name>`. Each entry maps a **declared** env-var name (what integrations look for, e.g. `RAILWAY_TOKEN`) to a **source** env-var name (what the host shell / CI secret layer exports, e.g. `RAILWAY_TOKEN_STAGING`). Never holds secret *values* — only name-to-name aliases.

```toml
[environments.staging]
secrets.RAILWAY_TOKEN = "RAILWAY_TOKEN_STAGING"
secrets.VERCEL_TOKEN  = "VERCEL_TOKEN_STAGING"

[environments.prod]
secrets.RAILWAY_TOKEN = "RAILWAY_TOKEN_PROD"
secrets.VERCEL_TOKEN  = "VERCEL_TOKEN_PROD"
```

With that block in place, `bento deploy --env staging` reads `$RAILWAY_TOKEN_STAGING` from the host env and exposes it to the deploy task under the name `RAILWAY_TOKEN` (which is what the Railway integration declares as its required env). The same mapping works identically local and in CI — in a GHA workflow you set `env: RAILWAY_TOKEN_STAGING: ${{ secrets.X }}` at the step level and bento resolves through the alias.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `secrets.<DECLARED>` | string | — | Source env-var name whose value should be exposed to tasks under the declared name. Declared must match what an integration / task's `required_env` declares. |

Ad-hoc alternative: `bento deploy --secret-from DECLARED=SOURCE` on the CLI. See [deploying.md](./deploying.md) for the full workflow.

---

## `bentos/<name>.toml`

One file per bento. The file's basename **is** the bento's name in CLI references (e.g. `bentos/release.toml` → `bento ci --bento release`). The `name` field inside the file must match the basename.

A bento is whatever logical grouping makes sense to you — environment, release stage, logical layer, customer tier. Bento is unopinionated about why; only that the dishes listed here ship together.

```toml
# bentos/release.toml — every dish in this bento ships as a unit
name = "release"
dishes = [
  "apps/api",
  "apps/web",
  "services/billing",
]
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Bento name. Must match the file's basename and be unique across `bentos/`. No `/` or platform path separators allowed. |
| `dishes` | `string[]` | yes | List of dish directory paths, relative to the workspace root, in any order. Forward-slashes regardless of host OS. Empty `[]` is valid (a freshly initialised workspace). |

### Multiple bentos

A repo can (and often does) have several bentos:

```
bentos/
├── backend.toml        # api + billing + scheduler
├── frontend.toml       # web + admin
└── release.toml        # everything that goes out together
```

A dish can appear in **multiple bentos**. Its content-cache key is derived from the dish, not the bento, so the same `api` dish in both `backend` and `release` is built once and reused.

---

## `<dish>/dish.toml`

One file per dish, in the dish's directory (which is also the working directory for its tasks). The `name` field is the dish's CLI handle (`bento build api`).

```toml
# apps/api/dish.toml
name = "api"
language = "go"

# Files outside any task's [inputs] that should still invalidate the
# cache when they change. Adapters add their own fingerprint files
# automatically (lockfiles, toolchain pin files, .tool-versions, ...).
inputs = ["openapi.yaml"]

# Build artefacts. Globs allowed. Used by `bento artifacts` and by the
# GHA action's `artifacts` output.
outputs = ["bin/api"]

# Other dishes this one depends on. Bento builds dependencies first,
# and any change to their content invalidates this dish's cache (the
# pessimistic cascade — opt out with force_independent below).
depends_on = ["lib-shared"]

# Skip the dep-cascade for this dish — its cache key is computed from
# its own inputs only. Useful for utility dishes that genuinely don't
# care about upstream changes.
force_independent = false

# Per-dish toolchain pin. Overrides bento.toml's [toolchain] for this
# dish only.
[toolchain]
go = "1.22.5"

# Tasks. Adapters supply default `build`, `test`, `lint` recipes per
# language; declare a [tasks.<name>] block here to override or add.
[tasks.build]
run = "go build -o bin/api ./cmd/api"
inputs = ["**/*.go", "go.mod", "go.sum"]
outputs = ["bin/api"]

[tasks.test]
run = "go test ./..."
env = ["DATABASE_URL", "REDIS_URL"]
retry = 1                              # 1 retry → up to 2 attempts

[tasks.lint]
run = "golangci-lint run"

# Optional: hot-reload command for `bento serve` / `bento dev`.
[serve]
run = "air"
```

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | required | Dish handle. Used by CLI flags (`bento build <name>`) and bentos' `dishes` list. Must be unique across the workspace. |
| `language` | string | adapter-detected | Adapter id (`go`, `cargo`, `python`, `ruby`, `php`, `maven`, `gradle`, `node-npm`, `node-pnpm`, `node-yarn`, `bun`, `deno`, or any plugin's id). When omitted, bento auto-detects from the dish dir. |
| `package_manager` | string | unset | Reserved for future use; no behaviour today. |
| `inputs` | `string[]` | `[]` | Glob patterns relative to the dish dir. Files matching are mixed into the cache key for **every** task in the dish. Adapters add their own fingerprint files automatically (lockfiles, toolchain pin files, `.tool-versions`). |
| `outputs` | `string[]` | `[]` | Glob patterns of build artefacts. Listed by `bento artifacts` and the GHA `artifacts` output. |
| `depends_on` | `string[]` | `[]` | Other dish names this dish depends on. Builds upstream first. Changes upstream invalidate this dish (unless `force_independent`). |
| `force_independent` | bool | `false` | Opt out of the pessimistic cascade — only this dish's own inputs go into its cache key. |

### `[toolchain]`

Same shape as `bento.toml`'s `[toolchain]` table — `<tool> = "<version>"` pairs plus optional `use_system`. Per-dish pins override the repo-wide ones.

### `[tasks.<name>]`

Tasks named `build`, `test`, `lint` get **default recipes from the adapter** for the dish's language. You only need a `[tasks.<name>]` block to:
- Override the default command (e.g. add flags)
- Declare a custom task name (e.g. `migrate`, `seed`, `deploy-preview`)
- Add task-specific `inputs` / `outputs` / `env` / `retry` config

Custom-named tasks (anything outside the adapter's lifecycle set) don't get pulled into `bento ci` — they only run when explicitly invoked via `bento run <dish> <task> -- <args>`. That's the escape hatch for ad-hoc CLIs, migrations, and one-off scripts: same dish-dir cwd + toolchain semantics as a cached task, but the run bypasses the content-hash cache so non-deterministic invocations stay correct.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `run` | string | required | Shell command. Runs from the dish dir, with the dish's `[toolchain]` honoured. |
| `inputs` | `string[]` | adapter default | Glob patterns mixed into the cache key for **this task only**. Combined with the dish's `inputs`. Omit to use the adapter's default for the language. |
| `outputs` | `string[]` | none | Glob patterns of artefacts produced by this task. Combined with the dish's `outputs` for `bento artifacts`. |
| `env` | `string[]` | `[]` | Names of env vars whose **values** should mix into the cache key. The names are visible (in `bento why`); the values are hashed only. |
| `retry` | int | `0` | Additional attempts on failure. `retry = 2` → up to 3 attempts. A task that succeeds on attempt > 1 is reported `flaky: true` in the execution report. |

### `[serve]`

Optional. Declares the long-running command for `bento serve <bento>` (every dish in a bento) and `bento dev <dish>` (one dish).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `run` | string | required | Long-running command. Bento spawns it, watches the dish's inputs, and restarts on change. |

### `[integrations.<id>]`

Per-dish config for **integrations** — the second extension point alongside language adapters. Each integration interprets its own block; unknown keys are ignored at load time so fields can be added without bento-config changes. See [deploying.md](./deploying.md) for the full deploy workflow.

#### Railway (`[integrations.railway]`)

```toml
[integrations.railway]
service = "backend"                         # one Railway service to deploy to
# services = ["frontend", "landing-page"]   # OR a list — one deploy task per entry
root = ".."                                 # cd here before `railway up` (monorepo root)
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `service` | string | unset | Railway service name. Injected as `--service <name>` in `railway up`. |
| `services` | `string[]` | unset | Fan out to multiple Railway services that share the same source (e.g. frontend + landing-page with different VITE env vars). One deploy task per entry, named `railway:deploy:<slug>`. Mutually exclusive with `service` (plural wins when both are set). |
| `root` | string | unset | Path (relative to the dish dir) to `cd` to before running `railway up`. Required when your Railway service has `rootDirectory` configured dashboard-side — it needs the full monorepo uploaded. Typically `".."` for top-level dishes. |

Railway service identity is dashboard-side — Railway's own `railway.json` schema has no `name` / `service` / `slug` field (verified against their schema JSON), so bento owns this mapping.

#### Vercel (`[integrations.vercel]`)

Currently read-only — the Vercel integration emits `vercel:deploy` + `vercel:preview` tasks without per-dish config. Future fields (`team`, `project`, `scope`) will land here.

#### Cloudflare Pages (`[integrations.cloudflare_pages]`)

Config-only opt-in — Pages projects rarely ship a `wrangler.toml` at the dish root (project settings live in the Cloudflare dashboard), so the integration only fires when the block is present.

```toml
[integrations.cloudflare_pages]
project = "my-pages-project"   # required — the CF Pages project name
dist    = "dist"               # default "dist" — the build output dir to upload
branch  = "main"               # default "main" — branch label for the deploy
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `project` | string | required | Cloudflare Pages project name (the slug shown in the dashboard URL: `dash.cloudflare.com/<account>/pages/view/<project>`). Required for both deploys and `bento secret put\|list\|delete`. |
| `dist` | string | `"dist"` | Build output directory (relative to the dish dir) that Wrangler uploads. |
| `branch` | string | `"main"` | Branch label attached to the deploy in the Pages dashboard. |

Wrangler is invoked as `wrangler pages deploy <dist> --project-name <project> --branch <branch> --commit-dirty=true`. `--commit-dirty=true` is always on — bento rebuilds artefacts fresh per invocation, so Wrangler's default git-state check is just noise on monorepos.

#### Cloudflare Workers (`[integrations.cloudflare_worker]`)

Detected via `wrangler.toml` or `wrangler.jsonc` at the dish root. Per-dish config is optional — the default environment in `wrangler.toml` covers the common case.

```toml
[integrations.cloudflare_worker]
env = "production"   # optional — adds --env production to wrangler deploy
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `env` | string | unset | Wrangler environment name. Maps to `[env.<name>]` blocks in your `wrangler.toml`; flows through to `wrangler deploy --env <name>` and to `wrangler secret put\|list\|delete --env <name>`. Omit for the default environment. |

The integration ID is `cloudflare_worker` (singular, code style); the product brand is "Cloudflare Workers" (plural). Same convention as `[dependencies.foo]` vs "the foo crate" elsewhere.

#### Slack (`[integrations.slack]`) — garnish

Opt-in Notify-kind integration. Fires after every Deploy task in the dish; posts a templated message to a Slack Incoming Webhook.

```toml
[integrations.slack]
webhook_url_env = "SLACK_WEBHOOK_URL"   # env var holding the https://hooks.slack.com/... URL
channel         = "#deploys"             # optional
username        = "Bento"                # optional
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `webhook_url_env` | string | `"SLACK_WEBHOOK_URL"` | Host env-var name holding the webhook URL. Flows through `[environments.<name>] secrets.*` aliases. |
| `channel` | string | unset | Optional channel override (Slack webhooks pin one at creation; this only takes effect for unpinned webhooks). |
| `username` | string | unset | Optional sender display name. |

#### Linear (`[integrations.linear]`) — garnish

Opt-in Notify-kind integration. On a successful deploy, scans the payload for `[A-Z]{2,}-\d+` issue identifiers and transitions each to a target workflow state via Linear's GraphQL API.

```toml
[integrations.linear]
api_key_env       = "LINEAR_API_KEY"
target_state      = "Deployed"
fallback_issue_id = "ENG-1234"   # optional: comment here if no refs matched
team              = "ENG"        # optional: disambiguate state lookup across teams
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api_key_env` | string | `"LINEAR_API_KEY"` | Host env-var name holding the Personal API key. |
| `target_state` | string | `"Deployed"` | Workflow-state name to transition matched issues to on a successful deploy. |
| `fallback_issue_id` | string | unset | Fallback issue to comment on when no issue refs were discovered. Skipped if unset. |
| `team` | string | unset | Team key (e.g. `"ENG"`). Required only when `target_state` is ambiguous across teams. |

Failed deploys skip transitions entirely — only `fallback_issue_id` comments fire, so a broken release is never marked shipped.

#### Anything else

Plugin integrations read whatever keys they recognise from their own `[integrations.<id>]` block. For custom post-deploy hooks without writing a full `Integration` implementation, use the `[[garnishes]]` block below.

### `[[garnishes]]`

Custom-script Notify-kind tasks declared inline — escape hatch for bespoke post-deploy hooks where writing a full `Integration` is overkill. Each entry becomes a Notify task that fans out after every Deploy in the dish; the script receives the GarnishPayload JSON on stdin (`bento schema garnish-payload`).

```toml
[[garnishes]]
name         = "github-pr-comment"
run          = "./scripts/notify-github.sh"
env          = ["GITHUB_TOKEN"]
required_env = ["GITHUB_TOKEN"]
required_cli = ["gh: https://cli.github.com"]
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Task name in the ExecutionReport. Must be unique within the dish. User-declared `[tasks.<name>]` can override the `run` while keeping Notify semantics intact. |
| `run` | string | yes | Shell command invoked once per Deploy trigger with the GarnishPayload on stdin. |
| `env` | `string[]` | no | Env-var allowlist forwarded to the child (same shape as `[tasks.<name>] env`). |
| `required_env` | `string[]` | no | Env vars that must be set at runtime — preflight fails the garnish with a clear message otherwise. |
| `required_cli` | `string[]` | no | CLI binaries that must be on PATH. Entry form: `"binary"` or `"binary: install hint"`. |

Failures never fail the build — same rule as built-in garnishes (`summary.notify_failures` tracks them; exit code stays 0).

---

## File resolution and overrides

CLI flags > per-dish `dish.toml` > repo-wide `bento.toml` > built-in defaults.

For toolchains specifically, bento walks each adapter's detection chain to discover an *implicit* version pin (e.g. `go.mod`'s `go 1.22` directive, `.nvmrc`, `.tool-versions`, etc.). That implicit pin counts as the bottom of the override stack — `dish.toml` > implicit detection > nothing.

The fully resolved cache-key inputs for any one task are visible via `bento why <hash>`.

---

## Example workspaces

### Single bento, single dish

The simplest valid workspace. No `bento.toml`.

```
my-app/
├── bentos/
│   └── all.toml             #  name = "all"   dishes = ["."]
└── dish.toml                #  name = "my-app"  language = "go"
```

### Logical layers, dish reuse

A dish (`shared`) that belongs to multiple bentos.

```
monorepo/
├── bento.toml               # repo-wide cache + toolchain config
├── bentos/
│   ├── backend.toml         # ["services/api", "services/billing", "lib/shared"]
│   ├── frontend.toml        # ["apps/web", "lib/shared"]
│   └── release.toml         # all of the above, in one bento
├── apps/
│   └── web/dish.toml
├── services/
│   ├── api/dish.toml
│   └── billing/dish.toml
└── lib/
    └── shared/dish.toml
```

`shared` is built once when `bento ci --bento release` runs; its cache key is identical no matter which bento you ask for it via.

### Release stages with dependency cascade

Two bentos modelling a deployment ordering: `core` ships first, `extras` depends on `core`. The dep cascade is enforced by `dish.toml`'s `depends_on`, not by the bento boundaries.

```
project/
├── bentos/
│   ├── core.toml            # ["services/auth", "services/users"]
│   └── extras.toml          # ["services/notifications", "services/billing"]
└── services/
    ├── auth/dish.toml       # depends_on = []
    ├── users/dish.toml      # depends_on = ["auth"]
    ├── notifications/dish.toml  # depends_on = ["users", "auth"]
    └── billing/dish.toml    # depends_on = ["users"]
```

Run `bento ci --bento extras` and bento builds `auth` and `users` first (they're upstream), then `notifications` and `billing` in parallel.
