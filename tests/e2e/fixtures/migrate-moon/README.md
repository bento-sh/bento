# migrate-moon fixture

Source-of-truth fixture for the `bento migrate moon` integration test.

Layout mirrors a typical Moonrepo workspace:

- `.moon/workspace.yml` — discovers projects via `apps/*` and `packages/*`
  globs, plus a `node:` toolchain block that should surface as an
  Inferred note pointing at bento.toml's `[toolchain]`.
- `apps/web/moon.yml` — `language: typescript`, exercises `build` (with
  `inputs` + `outputs`), `test`, and a persistent-style `dev` task with
  `options.cache: false` (surfaces as a Skipped note).
- `apps/api/moon.yml` — `language: rust`, exercises `deps: ["^:build"]`
  (surfaces as Inferred — bento derives ordering from the dish graph).
- `packages/utils/moon.yml` — second TS project, exercises that the
  migrator walks both roots correctly.

The fixture is consumed read-only — the integration test copies it to a
temp dir, runs the migrator, and asserts on the emitted bento config
without mutating the source tree.
