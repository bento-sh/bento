# migrate-nx fixture

Hermetic Nx workspace for the `bento migrate nx` e2e test.

## What's exercised

- Root `packageManager: "yarn@4.1.0"` with no per-project
  `package.json` — an Nx project dir usually has only `project.json`,
  so the language pick has to fall back to the workspace root instead
  of hard-coding `node-npm`.
- `workspaceLayout` capping discovery to `apps/` + `libs/`.
- `@nx/vite:build` / `@nx/jest:jest` / `@nx/js:tsc` /
  `@nx/eslint:lint` → canonical CLI invocations.
- `e2e` with `cache: false` → `cache = false` plus `ci = true`
  (non-lifecycle names are `bento run`-only without it).
- `serve` with `@nx/vite:dev-server` → `[serve]`, not `[tasks.serve]`.
- `targetDefaults` + `namedInputs` merging into per-target inputs.
