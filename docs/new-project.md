# Starting a new project with bento

You're about to create a polyglot monorepo and want bento as the orchestrator from day one. This guide takes you from `mkdir` to deployable in 10 minutes.

The matching adoption walkthrough (existing repo) is at [adopt-existing-repo.md](./adopt-existing-repo.md). For complete config detail see [configuration.md](./configuration.md). For the CLI itself: `bento --help`.

## 0. Prerequisites

- bento installed (see [README › Install](../README.md#install))
- A native toolchain for whatever languages you plan to ship. **bento can install the toolchain itself** when you pin a version in `[toolchain]` — Go, Node, and Python (via `uv`) are auto-installed into `~/.bento/tools/` on demand. For other languages (Java, Ruby, PHP, …) bento uses whatever's on `$PATH`. See [README › Toolchain handling](../README.md#toolchain-handling) for the full opt-in / opt-out semantics.

## 1. Bootstrap the workspace

```console
$ mkdir myapp && cd myapp
$ git init
$ bento init
✓ initialised bento workspace at /home/you/myapp

files:
  bento.toml
  bentos/release.toml

next:
  bento dish add apps/api --lang go
  bento plan
```

You now have:

```
myapp/
├── bento.toml          # repo-wide defaults; tweak only what you care about
└── bentos/
    └── release.toml    # name = "release", dishes = []
```

`init` in an empty dir creates the placeholders only — there's nothing to detect yet. As you add dishes, they get wired in automatically.

If you'd rather your bento be called something other than `release` — `backend`, `core`, anything — rename now (`mv bentos/release.toml bentos/<name>.toml` and edit the `name` field inside). You can have multiple bentos for different deployment groupings; see [README › Vocabulary](../README.md#vocabulary) and [configuration.md › Multiple bentos](./configuration.md#multiple-bentos).

## 2. Add your first dish

`bento dish add` scaffolds a compilable starter and wires it into the bento. Pick a language:

```console
$ bento dish add apps/api --lang go
✓ scaffolded apps/api as 'api' (go)

files:
  apps/api/go.mod
  apps/api/main.go
  apps/api/dish.toml
  bentos/release.toml          # 'api' added to dishes list

next:
  bento plan
```

The starter is a working "hello world" that compiles, tests, and lints out of the box. Open `apps/api/main.go`, edit it however you want; bento doesn't care what's inside as long as `go build ./...` succeeds.

The generated `apps/api/dish.toml`:

```toml
name = "api"
language = "go"
```

That's all — the Go adapter supplies the default `build`, `check`, `test`, `lint` task recipes (`check` runs `go vet`, the fast type-check). You add a `[tasks.<name>]` block here only when you want to override or add a custom task.

## 3. Add a second dish

Repeat with whatever else you want to ship. A frontend:

```console
$ bento dish add apps/web --lang node-npm
✓ scaffolded apps/web as 'web' (node-npm)

files:
  apps/web/package.json
  apps/web/index.js
  apps/web/dish.toml
  bentos/release.toml          # 'web' added to dishes list

next:
  bento plan
```

Or a Java service, a Python worker, anything bento knows about. Run `bento dish add --help` or see [configuration.md › `language`](./configuration.md#top-level-fields) for the full set of supported languages.

After two dish-adds your tree looks like:

```
myapp/
├── bento.toml
├── bentos/
│   └── release.toml         # name = "release"; dishes = ["apps/api", "apps/web"]
├── apps/
│   ├── api/
│   │   ├── go.mod
│   │   ├── main.go
│   │   └── dish.toml
│   └── web/
│       ├── package.json
│       ├── index.js
│       └── dish.toml
```

## 4. Plan and run

Same flow as adopting an existing repo:

```console
$ bento plan
plan: release bento (2 dishes)

  api  (go)
    build  [cache miss]  4c33edbecac0
    lint   [cache miss]  79c74f4a1267
    test   [cache miss]  97c3171912aa

  web  (node-npm)
    build  [cache miss]  78c4ee8bb5dc
    lint   [cache miss]  a017d2f020f8
    test   [cache miss]  e29544641d7f

summary: 2 dishes · 6 tasks · 6 miss · 0 hit
```

```console
$ bento ci
bento: release (2 dishes)

  api  (go)
    build  [built    ]  4c33edbecac0     830ms
    test   [built    ]  97c3171912aa     420ms
    lint   [built    ]  79c74f4a1267     280ms

  web  (node-npm)
    build  [built    ]  78c4ee8bb5dc   2,940ms
    test   [built    ]  e29544641d7f   1,610ms
    lint   [built    ]  a017d2f020f8     880ms

summary: 2 dishes · 6 tasks · 6 built · 0 cached · 0 failed · 6,960ms
```

Run `bento ci` again — every task hits the cache and returns in milliseconds.

## 5. Customise per-dish

Once you're past hello-world, you'll want real tasks. Edit a `dish.toml`:

```toml
# apps/api/dish.toml
name = "api"
language = "go"

outputs = ["bin/api"]

[tasks.build]
run = "go build -o bin/api ./cmd/api"

[tasks.test]
run = "go test -race ./..."
env = ["DATABASE_URL", "REDIS_URL"]

[tasks.migrate]
run = "go run ./cmd/migrate"
inputs = ["**/*.go", "migrations/**"]
env = ["DATABASE_URL"]
```

What that does:

- `outputs = ["bin/api"]` — bento knows where the built binary lives (used by `bento artifacts` for downstream packaging — see [README › Packaging your build artefacts](../README.md#packaging-your-build-artefacts)).
- `[tasks.build].run` — overrides the adapter's default `go build ./...` with your specific command.
- `[tasks.test].env = ["DATABASE_URL", ...]` — these env var **values** are mixed into the cache key. The names show up in `bento why`; the values are hashed only.
- `[tasks.migrate]` — a brand-new task, not one of the standard build/test/lint trio. Run with `bento build api migrate`.

For the full field list with defaults, see [configuration.md › `<dish>/dish.toml`](./configuration.md#dishdishtoml).

## 6. Add cross-dish dependencies

When one dish depends on another:

```toml
# apps/api/dish.toml
depends_on = ["lib-shared"]
```

bento builds `lib-shared` first. Any change to `lib-shared`'s inputs cascades down — `api`'s cache key now depends on `lib-shared`'s content, so a `lib-shared` edit invalidates `api`. The pessimistic cascade catches "library changed, but the binary's source files didn't" cases that simpler tools miss.

If you don't want the cascade for a particular dish (say, a utility CLI that genuinely doesn't care about library changes), opt out:

```toml
# apps/cli/dish.toml
force_independent = true
```

Visualise the graph:

```console
$ bento graph
release
├── api
│   └── lib-shared
└── web
```

Or `bento graph --format dot | dot -Tsvg > graph.svg` for something nicer.

## 7. Multiple bentos

For most projects, one bento is fine. When you want logical groupings (deploy backend before frontend, ship a `core` set then `extras`, separate `oss` and `enterprise` builds), add more:

```toml
# bentos/backend.toml
name = "backend"
dishes = ["apps/api", "lib-shared"]
```

```toml
# bentos/frontend.toml
name = "frontend"
dishes = ["apps/web"]
```

```console
$ bento ci --bento backend     # build/test/lint just the backend
$ bento ci --bento frontend    # ... or just the frontend
$ bento ci                     # ... or every bento, deduped
```

A dish in multiple bentos is built once; the content cache is shared. See [configuration.md › Example workspaces](./configuration.md#example-workspaces) for worked layouts.

## 8. Wire into CI

Drop the GitHub Action in:

```yaml
# .github/workflows/ci.yml
name: CI
on: [push, pull_request]

jobs:
  bento:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: bento-sh/bento@v0.1
        with:
          version: '0.1.0'
```

That's the whole file. The action installs bento, restores its content cache, fetches every pinned toolchain into `~/.bento/tools/`, and runs `bento ci`. No `actions/setup-*` chain — bento's adapters fetch the right Go / Node / Java / etc. for you, sourced from the `[toolchain]` pins your dishes captured.

See [README › Toolchain handling](../README.md#toolchain-handling) for the opt-out path if you'd rather chain `actions/setup-*` yourself.

## What now

- **Agent fix-up loops** — when a `cargo` / `golangci-lint` / `eslint` / `ruff` task fails, the JSON report's `diagnostics` array gives you parsed `{file, line, severity, message, rule}` records ready to feed back to an agent. `bento schema diagnostics` for the shape.
- **Package and deploy** — see [README › Packaging your build artefacts](../README.md#packaging-your-build-artefacts) for two patterns (convention vs reading the `artifacts` action output).
- **Diagnose cache surprises** — `bento why <hash>` returns the full input manifest behind any cache key.
- **Add a third-party language** — write a [plugin](./plugins.md) (~200 lines of pure-`std` Rust per the reference example).
- **Health check** — `bento doctor` periodically catches config drift.

For the deep config reference, see [configuration.md](./configuration.md). For commands and flags, run `bento --help` and `bento <command> --help`.
