# Adopting bento in an existing repo

You have a working monorepo. You want bento to plan, cache, and run its tasks without rewriting your build. This guide takes you from cold checkout to green CI in 10 minutes.

**Mental model**: bento doesn't replace your tools. It wraps them, hashes their inputs, and only re-runs what changed. Your `go build`, `npm ci`, `mvn test` are still the things that actually run.

The matching greenfield walkthrough is at [new-project.md](./new-project.md). For complete config detail see [configuration.md](./configuration.md). For the CLI itself: `bento --help`.

## 0. Prerequisites

- bento installed (see [README › Install](../README.md#install))
- A git repo with one or more recognisable language ecosystems. Built-in adapters cover Go, Rust, Python (pip + uv), Ruby, PHP, JVM (Maven, Gradle), Node (npm, pnpm, yarn), Bun, Deno. Anything else needs a [plugin](./plugins.md).

## 1. Run `bento init`

From the repo root:

```console
$ bento init
✓ initialised bento workspace at /home/you/your-repo

detected 4 dish(es):
  ✓ apps/api (go)  go 1.22.3
  ✓ apps/web (node-npm)  node 22.1.0
  ⚠ services/billing (php)  no toolchain pin
  ✓ services/scoring (maven)  java 21

captured toolchain pins in bento.toml:
  go = "1.22.3"
  java = "21"
  node = "22.1.0"

note: 1 dish(es) have no detected toolchain pin (services/billing). bento can't
lock to a specific version. Add a per-tool version file (.nvmrc /
.python-version / .ruby-version / .java-version), a project-wide
.tool-versions (asdf / mise), or the equivalent in package.json
(volta.node, engines.node) for reproducible builds.

files:
  bento.toml
  bentos/release.toml
  apps/api/dish.toml
  apps/web/dish.toml
  services/billing/dish.toml
  services/scoring/dish.toml

next:
  bento plan
  bento ci
```

What happened:

- `bento.toml` — repo-wide defaults. The `[toolchain]` block was populated from each dish's auto-detected pin.
- `bentos/release.toml` — your first bento, listing every detected dish.
- `<dish>/dish.toml` — one per detected dish, with `name` and `language` only. **Sources were not touched.**

The yellow ⚠ on `services/billing` flags that bento couldn't figure out its PHP version. See [Troubleshooting](#7-troubleshooting) for fix-up paths.

If init detected nothing (empty repo, only files at the root, languages bento doesn't know about), see the [new-project.md greenfield guide](./new-project.md) and add dishes manually with `bento dish add <path> --lang <lang>`.

## 2. Review what got generated

```console
$ tree -L 2 -P 'bento.toml|dish.toml|*.toml' --prune
.
├── bento.toml
├── bentos
│   └── release.toml
├── apps
│   ├── api
│   │   └── dish.toml
│   └── web
│       └── dish.toml
└── services
    ├── billing
    │   └── dish.toml
    └── scoring
        └── dish.toml
```

Open one of the generated `dish.toml` files:

```toml
# apps/api/dish.toml
name = "api"
language = "go"

# Adapter defaults for go cover build / test / lint.
# Override them by adding [tasks.<name>] blocks here — see
# `bento schema manifest` for the full input-manifest shape.
```

That's it. The Go adapter supplies default `build`, `test`, `lint` recipes (you don't see them in the file, but bento knows them). You can verify by running `bento plan` next.

## 3. Rename the bento (optional)

The default bento is named `release`. If your team's mental model is something more specific (`backend`, `core`, `staging`), rename:

```console
$ mv bentos/release.toml bentos/backend.toml
```

Then edit `name = "release"` → `name = "backend"` inside the file. (The filename and the `name` field must match.)

If you have multiple deployment groupings, add more bentos — see [configuration.md › Multiple bentos](./configuration.md#multiple-bentos) and the [README › Vocabulary](../README.md#vocabulary) section.

## 4. Plan the run

```console
$ bento plan
plan: backend bento (4 dishes)

  api  (go)
    build  [cache miss]  4c33edbecac0
    lint   [cache miss]  79c74f4a1267
    test   [cache miss]  97c3171912aa

  web  (node-npm)
    build  [cache miss]  78c4ee8bb5dc
    lint   [cache miss]  a017d2f020f8
    test   [cache miss]  e29544641d7f

  billing  (php)
    build  [cache miss]  4f7e1a3c2b9a
    lint   [cache miss]  7c91b5dd4e2a
    test   [cache miss]  9d63a8e1c44b

  scoring  (maven)
    build  [cache miss]  2e8b3f7c1a4d
    lint   [cache miss]  6a2d9c5e8f1b
    test   [cache miss]  b1f4d8c93e6a

summary: 4 dishes · 12 tasks · 12 miss · 0 hit
```

Every task starts as a cache miss because nothing's been built yet. The `[cache miss]`/`[cache hit]` shape is what matters — once you've run `bento ci` once, subsequent plans show hits for unchanged dishes.

`bento plan --json` returns the same data structured for agents. `bento schema plan` prints the JSON Schema.

## 5. Run it

```console
$ bento ci
bento: backend (4 dishes)

  api  (go)
    build  [built    ]  4c33edbecac0   1,820ms
    test   [built    ]  97c3171912aa     460ms
    lint   [built    ]  79c74f4a1267     310ms

  web  (node-npm)
    build  [built    ]  78c4ee8bb5dc   3,610ms
    test   [built    ]  e29544641d7f   1,880ms
    lint   [built    ]  a017d2f020f8     920ms

  ...

summary: 4 dishes · 12 tasks · 12 built · 0 cached · 0 failed · 14,512ms
```

Run `bento ci` again immediately and watch every task become a `[cache hit]` returning in milliseconds.

If a task fails, bento prints the underlying tool's stderr verbatim (the same output you'd get from running `go test` directly) and exits non-zero. The structured failure also appears in `bento ci --json`'s report.

## 6. Iterate

The `dish.toml` files are now your editing surface. Common next steps:

### Add a custom task

```toml
# apps/api/dish.toml
[tasks.migrate]
run = "go run ./cmd/migrate"
inputs = ["**/*.go", "migrations/**"]
env = ["DATABASE_URL"]
```

Run with `bento build api migrate` (or just `bento ci` to run every task on every dish).

### Declare a build artefact

```toml
# apps/api/dish.toml
outputs = ["bin/api"]

[tasks.build]
run = "go build -o bin/api ./cmd/api"
```

After build, `bento artifacts` lists the resolved file path for downstream packaging steps. See [README › Packaging your build artefacts](../README.md#packaging-your-build-artefacts).

### Add a dependency between dishes

```toml
# apps/api/dish.toml
depends_on = ["lib-shared"]
```

bento builds `lib-shared` first; any change to `lib-shared`'s inputs invalidates `api`'s cache (the pessimistic cascade). Opt out per-dish with `force_independent = true`.

### Tweak retry on flaky tests

```toml
# apps/web/dish.toml
[tasks.test]
run = "npm test"
retry = 2  # up to 3 attempts
```

A task that succeeds on attempt > 1 is marked `flaky: true` in the report — easy to grep for.

### Multiple bentos

Add another `bentos/<name>.toml` and list a different (or overlapping) set of dishes. The same dish in two bentos is built once; the cache is shared. See [configuration.md › Example workspaces](./configuration.md#example-workspaces).

## 7. Troubleshooting

### Init didn't pick up a dish I expected

Check the dir:

- Does it have a marker file the adapter recognises? (`go.mod`, `package.json`, `composer.json`, `pom.xml`, etc. — see [configuration.md › `language`](./configuration.md#top-level-fields).)
- Is it more than 4 levels deep below the repo root? init walks bounded.
- Is it inside a directory bento skips by default? `node_modules`, `vendor`, `target`, `build`, `dist`, anything starting with `.` — see [`crates/bento-cli/src/init.rs`](https://github.com/bento-sh/bento/blob/main/crates/bento-cli/src/init.rs) for the full skip list.

Add the missed dish manually:

```console
$ bento dish add path/to/dir --lang go
```

If `--lang` is omitted bento auto-detects from the dir contents.

### "no toolchain pin" warning on a dish

Pick whichever pinning convention your team already uses (or none and accept that the cache key is looser):

| File | Convention |
|------|-----------|
| `.nvmrc`, `.node-version` | nvm / fnm / nodenv (Node) |
| `.python-version` | pyenv (Python) |
| `.ruby-version` | rbenv / rvm / asdf (Ruby) |
| `.java-version` | jenv / asdf-java (Java) |
| `.bun-version` | Bun |
| `.deno-version` | Deno |
| `.tool-versions` | asdf / mise (any tool) |
| `.sdkmanrc` | sdkman (JVM) |
| `engines.node` / `volta.node` in `package.json` | npm publishing / Volta |

Run `bento init` again to confirm bento now picks up the pin.

### A dish was misclassified as the wrong language

Override explicitly in `dish.toml`:

```toml
# apps/web/dish.toml
name = "web"
language = "node-pnpm"   # not the auto-detected "node-npm"
```

### I want a different default for build/test/lint

Override in the dish:

```toml
# apps/api/dish.toml
[tasks.test]
run = "go test -race -timeout 5m ./..."
```

Or run a full task with `bento why <hash>` to see exactly what the adapter is invoking and why.

## 8. Wire into CI

Once `bento ci` is green locally, drop the action into a workflow:

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
          bento: backend
```

That's the whole file. The action installs bento, restores its content cache, fetches every pinned toolchain into `~/.bento/tools/` (cached separately so the second run is hot), and runs the build. No `actions/setup-go` / `setup-node` / `setup-java` chain needed — bento handles all of them via the [toolchain] pins captured during `bento init`.

If you'd rather use the upstream `actions/setup-*` (Volta-style version switching, distribution choice for `setup-java`, anything bento doesn't reproduce): set `install-toolchains: 'false'` on the action and chain the setup-* steps yourself. See [README › Toolchain handling](../README.md#toolchain-handling) for the BYO example.

## What now

- **Agent fix-up loops**: when a `cargo` / `golangci-lint` / `eslint` / `ruff` task fails, the JSON report's `diagnostics` array gives you parsed `{file, line, severity, message, rule}` records — feed them straight to your agent without writing tool-specific parsers. `bento schema diagnostics` for the shape.
- Add more bentos for different deployment groupings — see the [configuration reference](./configuration.md).
- Wire build artefacts into Docker/upload-artifact — see [README › Packaging your build artefacts](../README.md#packaging-your-build-artefacts).
- Add a third-party language via the [plugin protocol](./plugins.md).
- Run `bento doctor` periodically to catch config drift.
