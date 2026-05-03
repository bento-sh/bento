#!/usr/bin/env bash
#
# bento-guard.sh — PreToolUse hook for the Bash tool.
#
# Steers the agent to `bento <verb>` instead of native package managers
# whenever the working directory is inside a bento workspace (detected by
# walking up looking for a bento.toml).
#
# Behaviour:
#   - Outside a bento workspace: pass through (exit 0).
#   - Inside a bento workspace: if the command matches a known native-tool
#     pattern (bun install, pnpm test, pip install, pytest, uv sync, cargo
#     build, go test, bunx tsc, npx vite, …), block it (exit 2) with a
#     stderr message naming the bento verb to use.
#   - On parsing trouble (missing jq, malformed input, etc.): fail-safe
#     and allow. Hooks must never silently break the agent's workflow.
#
# Bypass:
#   - Prefix the command with `BENTO_GUARD_BYPASS=1 ` to skip the guard
#     for one shot. Reserve this for genuine emergencies; the right
#     habit is to use the bento verb.
#   - Setting BENTO_GUARD_BYPASS=1 in the parent environment also works.
#
# Install:
#   See ~/.claude/skills/bento/SKILL.md (section "Recommended: install
#   the bento-guard hook"). The short form is to register this script
#   under hooks.PreToolUse[].hooks[] for the Bash matcher in either the
#   project's .claude/settings.json or the user-level ~/.claude/settings.json.

set -uo pipefail

# ---------------------------------------------------------------------------
# Read tool input from stdin. The PreToolUse payload is JSON with at least:
#   { "tool_name": "Bash", "tool_input": { "command": "..." }, "cwd": "..." }
# ---------------------------------------------------------------------------
input=$(cat 2>/dev/null || true)
[[ -z "$input" ]] && exit 0

# Fail-safe: without jq we can't parse the payload reliably. Allow.
if ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

command=$(printf '%s' "$input" | jq -r '.tool_input.command // empty' 2>/dev/null)
cwd=$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null)

[[ -z "$command" ]] && exit 0
[[ -z "$cwd" ]] && cwd=$(pwd 2>/dev/null || echo "")

# ---------------------------------------------------------------------------
# Bypass.
# ---------------------------------------------------------------------------
[[ "${BENTO_GUARD_BYPASS:-}" == "1" ]] && exit 0
# Match any leading env-var assignment that includes BENTO_GUARD_BYPASS=1.
if [[ "$command" =~ (^|[[:space:]])BENTO_GUARD_BYPASS=1([[:space:]]|$) ]]; then
  exit 0
fi

# ---------------------------------------------------------------------------
# Workspace detection: walk up from cwd looking for bento.toml. If not
# found, this isn't a bento workspace — pass through.
# ---------------------------------------------------------------------------
bento_root=""
dir="$cwd"
while [[ -n "$dir" && "$dir" != "/" ]]; do
  if [[ -f "$dir/bento.toml" ]]; then
    bento_root="$dir"
    break
  fi
  dir=$(dirname "$dir")
done
[[ -z "$bento_root" ]] && exit 0

# ---------------------------------------------------------------------------
# Pattern table. Each entry is "<regex>@@@<suggestion>" (using @@@ as a
# separator that won't collide with the `|` literals inside either field).
# The regex is tested against the entire command via [[ =~ ]]; the
# suggestion is the bento verb the agent should use instead.
#
# Order matters — first match wins, so put more-specific patterns above
# more-general ones.
# ---------------------------------------------------------------------------
patterns=(
  # Workspace install (npm/pnpm/yarn/bun/pip/uv/composer/bundle/mvn/gradle/deno).
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+(install|ci|i)\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])pip[[:space:]]+install\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])python[0-9.]*[[:space:]]+-m[[:space:]]+pip[[:space:]]+install\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])python[0-9.]*[[:space:]]+setup\.py[[:space:]]+(install|develop)\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])uv[[:space:]]+(sync|pip)\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])composer[[:space:]]+(install|update)\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])bundle[[:space:]]+(install|update)\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])mvn[[:space:]]+dependency:resolve\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])(\./)?gradlew?[[:space:]]+dependencies\b@@@bento install [--bento <name>]'
  '(^|[;&|[:space:]])deno[[:space:]]+install\b@@@bento install [--bento <name>]'

  # Publish (cargo / npm family / deno) — always destructive, always
  # outside bento. Block them so a release accidentally going through
  # the wrong tool doesn't slip past.
  '(^|[;&|[:space:]])cargo[[:space:]]+publish\b@@@bento release <spec>  (publishes via tag-on-bump CI)'
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+publish\b@@@bento release <spec>  (or use the platform release flow)'
  '(^|[;&|[:space:]])deno[[:space:]]+publish\b@@@bento release <spec>  (or use the platform release flow)'

  # Cargo install of an arbitrary crate — almost always means "I want
  # this binary on PATH for development", which bento doesn't manage.
  # Surface it so the agent can confirm before bypassing toolchain
  # pinning.
  '(^|[;&|[:space:]])cargo[[:space:]]+install[[:space:]]+[a-zA-Z0-9_-]+@@@(this is a host-level install, not a bento op — confirm with the user before running)'

  # Add / remove deps.
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+(add|remove|uninstall)\b@@@bento add <pkg> --dish <d> [--dev]'
  '(^|[;&|[:space:]])uv[[:space:]]+(add|remove)\b@@@bento add <pkg> --dish <d> [--dev]'
  '(^|[;&|[:space:]])composer[[:space:]]+(require|remove)\b@@@bento add <pkg> --dish <d> [--dev]'
  '(^|[;&|[:space:]])cargo[[:space:]]+(add|remove)\b@@@bento add <pkg> --dish <d> [--dev]'
  '(^|[;&|[:space:]])go[[:space:]]+get\b@@@bento add <pkg> --dish <d>'

  # Build / typecheck.
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+run[[:space:]]+(build|compile|typecheck|tsc)\b@@@bento build <dish>'
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+(build)\b@@@bento build <dish>'
  '(^|[;&|[:space:]])cargo[[:space:]]+(build|check)\b@@@bento (build|check) <dish>'
  '(^|[;&|[:space:]])go[[:space:]]+(build|vet)\b@@@bento (build|check) <dish>'
  '(^|[;&|[:space:]])uv[[:space:]]+build\b@@@bento build <dish>'
  '(^|[;&|[:space:]])(\./)?gradlew?[[:space:]]+(build|assemble|compileJava|compileKotlin)\b@@@bento build <dish>'
  '(^|[;&|[:space:]])mvn[[:space:]]+(compile|package|install|verify)\b@@@bento build <dish>'
  '(^|[;&|[:space:]])(bunx|npx)[[:space:]]+tsc\b@@@bento (lint|build) <dish>'
  '(^|[;&|[:space:]])(bunx|npx)[[:space:]]+vite([[:space:]]+build)?\b@@@bento (build|dev) <dish>'
  '(^|[;&|[:space:]])tsc[[:space:]]+--noEmit\b@@@bento lint <dish>'

  # Test.
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+(test|run[[:space:]]+test)\b@@@bento test <dish>'
  '(^|[;&|[:space:]])bun[[:space:]]+test\b@@@bento test <dish>'
  '(^|[;&|[:space:]])pytest\b@@@bento test <dish>'
  '(^|[;&|[:space:]])python[0-9.]*[[:space:]]+-m[[:space:]]+pytest\b@@@bento test <dish>'
  '(^|[;&|[:space:]])uv[[:space:]]+run[[:space:]]+pytest\b@@@bento test <dish>'
  '(^|[;&|[:space:]])cargo[[:space:]]+test\b@@@bento test <dish>'
  '(^|[;&|[:space:]])go[[:space:]]+test\b@@@bento test <dish>'
  '(^|[;&|[:space:]])deno[[:space:]]+test\b@@@bento test <dish>'
  '(^|[;&|[:space:]])bundle[[:space:]]+exec[[:space:]]+(rake[[:space:]]+test|rspec|test-unit|minitest)\b@@@bento test <dish>'
  '(^|[;&|[:space:]])(rspec|rake[[:space:]]+test)\b@@@bento test <dish>'
  '(^|[;&|[:space:]])(\./)?gradlew?[[:space:]]+test\b@@@bento test <dish>'
  '(^|[;&|[:space:]])mvn[[:space:]]+test\b@@@bento test <dish>'
  '(^|[;&|[:space:]])(\./)?vendor/bin/(phpunit|pest)\b@@@bento test <dish>'
  '(^|[;&|[:space:]])composer[[:space:]]+test\b@@@bento test <dish>'

  # Lint.
  '(^|[;&|[:space:]])(bunx|npx)[[:space:]]+(eslint|prettier)\b@@@bento lint <dish>'
  '(^|[;&|[:space:]])uvx[[:space:]]+(ruff|mypy)\b@@@bento lint <dish>'
  '(^|[;&|[:space:]])(ruff|mypy|eslint|prettier|golangci-lint)[[:space:]]+(check|run|--check)\b@@@bento lint <dish>'
  '(^|[;&|[:space:]])python[0-9.]*[[:space:]]+-m[[:space:]]+compileall\b@@@bento lint <dish>'
  '(^|[;&|[:space:]])(\./)?gradlew?[[:space:]]+(check|spotlessCheck|ktlintCheck)\b@@@bento lint <dish>'
  '(^|[;&|[:space:]])bundle[[:space:]]+exec[[:space:]]+rubocop\b@@@bento lint <dish>'
  '(^|[;&|[:space:]])(\./)?vendor/bin/(phpstan|psalm|php-cs-fixer)\b@@@bento lint <dish>'

  # Dev / serve.
  '(^|[;&|[:space:]])(bun|npm|pnpm|yarn)[[:space:]]+run[[:space:]]+dev\b@@@bento dev <dish>  (or bento serve <bento>)'
  '(^|[;&|[:space:]])(bunx|npx)[[:space:]]+wrangler[[:space:]]+dev\b@@@bento dev <dish>  (declare a [serve] block in the dish.toml)'
  '(^|[;&|[:space:]])vite([[:space:]]+--port|[[:space:]]+dev)\b@@@bento dev <dish>'
  '(^|[;&|[:space:]])deno[[:space:]]+(task[[:space:]]+dev|run[[:space:]]+--watch)\b@@@bento dev <dish>'

  # Deploy / publish.
  '(^|[;&|[:space:]])(bunx|npx)[[:space:]]+wrangler[[:space:]]+(deploy|publish)\b@@@bento deploy --env <env>'
  '(^|[;&|[:space:]])railway[[:space:]]+up\b@@@bento deploy --env <env>'
  '(^|[;&|[:space:]])vercel[[:space:]]+(deploy|--prod)\b@@@bento deploy --env <env>'
)

# ---------------------------------------------------------------------------
# Match.
# ---------------------------------------------------------------------------
matched=""
suggestion=""
for entry in "${patterns[@]}"; do
  regex="${entry%@@@*}"
  hint="${entry##*@@@}"
  if [[ "$command" =~ $regex ]]; then
    matched="${BASH_REMATCH[0]}"
    suggestion="$hint"
    break
  fi
done

[[ -z "$matched" ]] && exit 0

# ---------------------------------------------------------------------------
# Block. The stderr text is what the agent reads — make it instruction-
# shaped so future calls go through bento.
# ---------------------------------------------------------------------------
cat >&2 <<EOF
[bento-guard] Blocked native-tool invocation inside a bento workspace.

  Workspace:  $bento_root
  Command:    $(printf '%s' "$command" | head -c 200)
  Matched:    ${matched# }

This monorepo is managed by bento. Native package-manager invocations
bypass bento's content-addressed cache, toolchain pinning, and per-dish
scoping. Use the bento verb instead:

  ${suggestion}

Reference:    ~/.claude/skills/bento/SKILL.md
Bypass once:  prefix the command with 'BENTO_GUARD_BYPASS=1 ' (only for
              genuine one-offs that bento doesn't cover).
EOF
exit 2
