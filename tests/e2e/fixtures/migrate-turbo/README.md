# migrate-turbo fixture

Hermetic Turborepo workspace for the `bento migrate turbo` e2e test.
The vendored `vercel/turbo` fixture covers real-world shape but only
runs with `BENTO_E2E_NETWORK=1`; this one runs on every `cargo test`.

## What's exercised

- `packageManager: "pnpm@8.10.0"` + `pnpm-workspace.yaml` — the
  emitted dishes must say `language = "node-pnpm"` and
  `run = "pnpm run <task>"`. A hard-coded `node-npm` makes the first
  `bento install` run `npm ci` against a pnpm lockfile.
- `typecheck` with `cache: false` — must translate to `cache = false`
  plus `ci = true` (a non-lifecycle name is `bento run`-only without
  it, so it would silently vanish from `bento ci`).
- `dev` with `persistent: true` — belongs in `[serve]`, not
  `[tasks.dev]`.
- JSONC `//` and `/* */` comments in `turbo.json`.
