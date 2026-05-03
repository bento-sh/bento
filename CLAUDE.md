# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository purpose

**bento** is a polyglot monorepo orchestrator — one CLI that wraps every dish's native package manager (npm / pnpm / yarn / bun / cargo / go / composer / pip / bundle / mvn / gradle / deno) behind uniform verbs. Positioned as "built for agents first, first-class for humans."

**Language:** Rust (edition 2021, MSRV 1.75).

**Distribution:** published binaries via GitHub releases; floating minor tags (`v0.1`) and pinned patches (`v0.1.0`). GitHub Action at the repo root re-exports the CLI so CI users can `uses: bento-sh/bento@v0.1`.

## Structure

```
/
├── crates/
│   ├── bento-cli/               clap-based entrypoint
│   ├── bento-core/              plan, execute, cache-key compute, cascade
│   ├── bento-config/            bento.toml parser + schema
│   ├── bento-cache/             local + remote caches; blake3 CAS
│   ├── bento-cas-protocol/      shared wire types for hosted remote cache
│   ├── bento-adapters/          per-language adapters (go, cargo, node, …)
│   ├── bento-toolchain/         embedded mini-mise toolchain manager
│   ├── bento-watch/              dev-mode file watcher
│   ├── bento-plugin/             subprocess plugin JSON-RPC client
│   └── bento-mcp/                MCP server — typed tool surface for agents
├── examples/
│   └── bento-adapter-noop/      reference plugin
├── docs/                        user + agent-facing docs (configuration, agents, deploying, plugins…)
├── tests/e2e/                   polyglot end-to-end fixtures + harness
├── action.yml                   GitHub Action wrapper
└── CHANGELOG.md
```

Agent-facing docs live in `docs/agents.md` — that's the user-visible guide for wiring Claude Code / Cursor / etc. into a bento-managed repo. Keep it current; it's the headline of the "built for agents" pitch.

## Quality gates

Every change must pass, before `git push` or a PR lands:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

The pre-push hook runs all three. Fast e2e tests are opt-out (skip ecosystems whose toolchain isn't on PATH); fat e2e tests need `BENTO_E2E_NETWORK=1` and all toolchains installed.

## Release workflow

1. Bump `workspace.package.version` + `workspace.dependencies` path refs + `bento-adapter-noop` dev-dep pin.
2. Update `CHANGELOG.md` with post-last-tag commits.
3. Commit, tag `vX.Y.Z`, update floating `vX.Y` tag, push tags.
4. Release workflow builds + publishes prebuilt binaries automatically.

## Writing style

Concise and detailed. No fluff, no filler — get to the point but don't skip important detail. Follow existing `docs/*` patterns when adding documentation.

## Comments + commits

- Default to no comments. Add one only when the *why* is non-obvious (hidden constraint, subtle invariant, workaround rationale). Never comment the *what* — well-named identifiers cover that.
- Commit messages: lowercase, imperative, scope-prefixed (`fix(railway): …`, `test(e2e): …`). Body paragraphs explain the why + what changed, not the how.
- Keep commits atomic so bisect stays useful.

## Do not commit

- Generated binaries, `.env` files, `node_modules/`, target/release output.
