# migrate-rush fixture

Mirror of a tiny Rush.js workspace used by the `bento migrate rush`
end-to-end test. Two projects under `apps/`:

- `@migrate-rush/web` (Next.js shape — `build`, `test`, `lint` scripts)
- `@migrate-rush/api` (TypeScript service — `build`, `test`, `start`)

`rush.json` pins `pnpmVersion` so the migrator picks the `node-pnpm`
adapter and emits `run = "pnpm run <task>"` lines.

`common/config/rush/command-line.json` adds one custom bulk command
(`audit`) to exercise the migrator's note for unmappable Rush concepts.

The `rush.json` ships with a couple of `// …` JSONC comments — the
migrator strips those on a fallback parse, and the fixture lets the
e2e harness verify that path.
