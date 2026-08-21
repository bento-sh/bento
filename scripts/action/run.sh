#!/usr/bin/env bash
# bento action runner — one entry point per phase of the composite
# action (install binary, install toolchains, preflight, execute).
#
# Called from `.github/actions/run` via `runs: using: composite`.
# All phase-specific inputs are routed through environment variables
# (not shell substitution) so injection-shaped inputs can't shell out.
#
# Portability: bash 3.2+ (macOS runners). Namerefs (`local -n`) are
# avoided; functions mutate a shared `BENTO_ARGS` global instead.

set -euo pipefail

PHASE="${1:-}"
if [ -z "$PHASE" ]; then
    echo "usage: run.sh <install-bento|install-toolchains|preflight|execute|summarize|pr-body|pr-comment>" >&2
    exit 2
fi

# ── Shared helpers ─────────────────────────────────────────────────

# Global that build_bento_args / add_secret_from_flags append to.
# Each phase that uses it resets the array at entry.
BENTO_ARGS=()

# Parse $BENTO_SECRET_FROM (newline-delimited DECLARED=SOURCE) and
# append --secret-from flags to BENTO_ARGS. Blank lines + whitespace
# are tolerated; no validation beyond "not empty" — `bento`'s own
# parser rejects malformed values with a clear error.
add_secret_from_flags() {
    local raw line
    while IFS= read -r raw; do
        line="$(printf '%s' "$raw" | awk '{$1=$1};1')"
        [ -z "$line" ] && continue
        BENTO_ARGS+=("--secret-from" "$line")
    done <<< "${BENTO_SECRET_FROM:-}"
}

# "musl" or "gnu" for the running Linux userland. Container jobs on
# Alpine images can't exec a glibc binary (no ld-linux loader) and
# report "musl libc" from `ldd --version`; a box with no ldd at all is
# not glibc either.
linux_libc() {
    if ! command -v ldd >/dev/null 2>&1 || ldd --version 2>&1 | grep -qi musl; then
        echo musl
    else
        echo gnu
    fi
}

# SHA-256 of a file. macOS runners ship `shasum` but not `sha256sum`.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Compare a file against an expected hex digest; abort on mismatch.
verify_sha256() {
    local file="$1" expected="$2" actual
    actual="$(sha256_of "$file")"
    if [ "$expected" != "$actual" ]; then
        echo "::error::checksum mismatch for $(basename "$file") (expected $expected, got $actual)" >&2
        exit 1
    fi
    echo "==> checksum verified: $(basename "$file")"
}

# Write a KEY/VALUE pair to $GITHUB_OUTPUT using the heredoc form —
# safe for multi-line values (JSON reports) that would otherwise
# truncate on the first newline.
publish_output() {
    local key="$1"
    local value="$2"
    {
        printf '%s<<__BENTO_EOF__\n' "$key"
        printf '%s\n' "$value"
        printf '__BENTO_EOF__\n'
    } >> "$GITHUB_OUTPUT"
}

# Populate BENTO_ARGS for the given $BENTO_TASK. Covers argv shared
# across the CI / build / check / test / lint / deploy verbs, plus
# deploy's extra flag set. Callers append anything task-unrelated
# (e.g. --report-file) after this returns.
build_bento_args() {
    BENTO_ARGS=()
    case "${BENTO_TASK:-}" in
        ci)     BENTO_ARGS+=("ci") ;;
        build)  BENTO_ARGS+=("build") ;;
        check)  BENTO_ARGS+=("check") ;;
        test)   BENTO_ARGS+=("test") ;;
        lint)   BENTO_ARGS+=("lint") ;;
        deploy) BENTO_ARGS+=("deploy") ;;
        notify) BENTO_ARGS+=("notify") ;;
        *)
            echo "::error::unknown task '${BENTO_TASK:-}' (expected one of: ci, build, check, test, lint, deploy, notify)" >&2
            exit 1
            ;;
    esac

    if [ "${BENTO_TASK}" = "deploy" ]; then
        if [ "${BENTO_PREVIEW:-false}" = "true" ] && [ "${BENTO_ROLLBACK:-false}" = "true" ]; then
            echo "::error::preview and rollback are mutually exclusive" >&2
            exit 1
        fi
        if [ -n "${BENTO_ENV:-}" ]; then
            BENTO_ARGS+=("--env" "$BENTO_ENV")
        fi
        add_secret_from_flags
        [ "${BENTO_PREVIEW:-false}"   = "true" ] && BENTO_ARGS+=("--preview")
        [ "${BENTO_ROLLBACK:-false}"  = "true" ] && BENTO_ARGS+=("--rollback")
        [ "${BENTO_NO_NOTIFY:-false}" = "true" ] && BENTO_ARGS+=("--no-notify")
    fi

    # `notify` shares deploy's secret surface (Slack webhook tokens
    # etc.) but none of its preview/rollback/no-notify toggles.
    if [ "${BENTO_TASK}" = "notify" ]; then
        if [ -n "${BENTO_ENV:-}" ]; then
            BENTO_ARGS+=("--env" "$BENTO_ENV")
        fi
        add_secret_from_flags
    fi

    # Positional target applies to every non-ci task. `ci` is
    # whole-workspace by design.
    if [ -n "${BENTO_TARGET:-}" ] && [ "$BENTO_TASK" != "ci" ]; then
        BENTO_ARGS+=("$BENTO_TARGET")
    fi

    # --bento filter applies to every verb.
    if [ -n "${BENTO_NAME:-}" ]; then
        BENTO_ARGS+=("--bento" "$BENTO_NAME")
    fi
}

# ── Phases ─────────────────────────────────────────────────────────

phase_install_bento() {
    mkdir -p "$BENTO_INSTALL_DIR"
    local tag="v${BENTO_VERSION}"

    local arch triple
    case "$(uname -m)" in
        x86_64|amd64)  arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *)             echo "::error::unsupported arch $(uname -m)" >&2; exit 1 ;;
    esac
    case "$(uname -s)" in
        Linux)  triple="${arch}-unknown-linux-$(linux_libc)" ;;
        Darwin) triple="${arch}-apple-darwin" ;;
        *)      echo "::error::unsupported OS $(uname -s)" >&2; exit 1 ;;
    esac

    local asset="bento-${BENTO_VERSION}-${triple}"
    local tmp
    tmp="$(mktemp -d)"

    echo "==> downloading $asset from release $tag"
    gh release download "$tag" \
        --repo "$BENTO_REPO" \
        --pattern "${asset}.tar.gz" \
        --pattern "${asset}.tar.gz.sha256" \
        --dir "$tmp"

    # No checksum asset means either a tampered release or a broken
    # publish — neither is a reason to run the binary anyway.
    if [ ! -f "$tmp/${asset}.tar.gz.sha256" ]; then
        echo "::error::release $tag has no ${asset}.tar.gz.sha256 asset — refusing to install an unverified binary" >&2
        exit 1
    fi
    verify_sha256 "$tmp/${asset}.tar.gz" "$(awk '{print $1}' "$tmp/${asset}.tar.gz.sha256")"

    tar -xzf "$tmp/${asset}.tar.gz" -C "$tmp"
    mv "$tmp/${asset}/bento" "$BENTO_INSTALL_DIR/bento"
    chmod +x "$BENTO_INSTALL_DIR/bento"
    echo "$BENTO_INSTALL_DIR" >> "$GITHUB_PATH"
    "$BENTO_INSTALL_DIR/bento" --version
}

phase_install_toolchains() {
    # bento-toolchain has built-in installers for Go, Node, Python
    # (delegated to `uv python install`), and uv itself (declared
    # co-required by the python tool, so a `[toolchain] python = "..."`
    # pin lays uv down first automatically). Bun and Deno don't have
    # built-in installers yet — we bootstrap those from their pinned
    # upstream release assets when the workspace pins them.
    bootstrap_external_toolchains

    # Capture stdout + exit code so we can publish the JSON output
    # even on partial failure, then propagate the failure upstream.
    local install_exit=0 json
    json="$(bento toolchain install --json)" || install_exit=$?
    printf '%s\n' "$json"
    publish_output "json" "$json"
    exit "$install_exit"
}

# Read a `<key> = "<value>"` line out of a `[<section>]` block in
# bento.toml. Echoes the value (no quotes) or nothing if absent.
# Tolerates whitespace; ignores commented-out lines. Pure bash so it
# works on both Linux + macOS runners (no GNU-awk dependency).
read_toml_value() {
    local section="$1"
    local key="$2"
    local file="bento.toml"
    [ -f "$file" ] || return 0

    local in_block=0 line
    while IFS= read -r line; do
        if [[ "$line" =~ ^[[:space:]]*\[${section}\][[:space:]]*$ ]]; then
            in_block=1
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*\[ ]]; then
            in_block=0
            continue
        fi
        [ "$in_block" -eq 1 ] || continue
        # Skip commented lines (anywhere a # appears with only whitespace
        # before it, the line is a comment).
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        if [[ "$line" =~ ^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"([^\"]*)\" ]]; then
            printf '%s\n' "${BASH_REMATCH[1]}"
            return 0
        fi
    done < "$file"
}

bootstrap_external_toolchains() {
    # Bento's built-in installer covers go, node, python, and uv. Bun
    # and Deno fall through to upstream release assets for now —
    # tracked separately for proper BunTool / DenoTool support in
    # bento-toolchain (needs zip-archive support).
    local bun_version
    bun_version="$(read_toml_value toolchain bun || true)"

    if [ -n "$bun_version" ]; then
        if ! command -v bun >/dev/null 2>&1; then
            install_bun "$bun_version"
        fi
    fi
}

# Install bun from its pinned GitHub release asset, checksum-verified
# against the release's SHASUMS256.txt. Deliberately NOT `curl
# bun.sh/install | bash`: that fetches an unpinned script over the
# network and runs it, so whoever controls bun.sh controls this runner.
install_bun() {
    local version="$1"
    local install_dir="${HOME}/.bun"
    local tag="bun-v${version}"
    local base="https://github.com/oven-sh/bun/releases/download/${tag}"

    # bun publishes separate musl builds; same Alpine constraint as bento.
    local os arch libc=""
    case "$(uname -s)" in
        Linux)
            os=linux
            if [ "$(linux_libc)" = musl ]; then
                libc="-musl"
            fi
            ;;
        Darwin) os=darwin ;;
        *)      echo "::error::unsupported OS for bun bootstrap: $(uname -s)" >&2; exit 1 ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64)  arch=x64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *)             echo "::error::unsupported arch for bun bootstrap: $(uname -m)" >&2; exit 1 ;;
    esac
    local asset="bun-${os}-${arch}${libc}"

    echo "==> bootstrapping bun (pinned: $version) from oven-sh/bun@${tag}"
    local tmp
    tmp="$(mktemp -d)"
    curl -fsSL -o "$tmp/${asset}.zip" "${base}/${asset}.zip"
    curl -fsSL -o "$tmp/SHASUMS256.txt" "${base}/SHASUMS256.txt"

    local expected
    expected="$(awk -v want="${asset}.zip" '$2 == want || $2 == "*" want {print $1}' "$tmp/SHASUMS256.txt")"
    if [ -z "$expected" ]; then
        echo "::error::${asset}.zip missing from ${tag} SHASUMS256.txt" >&2
        exit 1
    fi
    verify_sha256 "$tmp/${asset}.zip" "$expected"

    mkdir -p "$install_dir/bin"
    unzip -q -o "$tmp/${asset}.zip" -d "$tmp"
    mv "$tmp/${asset}/bun" "$install_dir/bin/bun"
    chmod +x "$install_dir/bin/bun"
    echo "$install_dir/bin" >> "$GITHUB_PATH"
    export PATH="$install_dir/bin:$PATH"
    "$install_dir/bin/bun" --version
}

# Put the remote-cache JWT where bento looks for it, and warn about the
# silent-no-op case: a `bento://` remote with no token resolvable makes
# bento disable the remote tier with a `tracing::warn!` nobody reads, so
# laptops (which have a `bento login` keychain entry) get hits and CI
# gets none.
#
# The `cache-token` input can't bind straight to `BENTO_CACHE_TOKEN` in
# action.yml: a step-level `env:` entry with an empty value shadows the
# caller's own export, so an unset input would *unset* a token the
# workflow already provided. It arrives under `_INPUT` instead and only
# overrides when non-empty.
setup_cache_token() {
    local token_env
    token_env="$(read_toml_value cache remote_token_env)"
    # Unset or not an identifier → the default. `export "$name=…"` and
    # `${!name}` both abort the shell on a malformed name, and a typo in
    # bento.toml shouldn't take the job down with it.
    [[ "$token_env" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || token_env="BENTO_CACHE_TOKEN"

    if [ -n "${BENTO_CACHE_TOKEN_INPUT:-}" ]; then
        export BENTO_CACHE_TOKEN="$BENTO_CACHE_TOKEN_INPUT"
        # A repo that renamed the var via `remote_token_env` reads only
        # that name — exporting the default alone would be another
        # silent miss.
        export "$token_env=$BENTO_CACHE_TOKEN_INPUT"
    fi

    local remote
    remote="$(read_toml_value cache remote)"
    case "$remote" in
        bento://*) ;;
        *) return 0 ;;
    esac

    if [ -z "${!token_env:-}" ]; then
        echo "::warning::[cache] remote = \"$remote\" is configured but \$${token_env} is empty — bento will skip the remote cache tier for this run. Pass cache-token: \${{ secrets.BENTO_CACHE_TOKEN }} to the action (get the JWT from \`bento login\` or the dashboard)."
    fi
}

# Render a markdown job summary for an ExecutionReport (the
# `--report-file` JSON, pretty-printed) on stdout. Split out as its own
# phase so scripts/action/test_summary.sh can exercise it without a
# runner.
summarize() {
    jq -r '
      def secs: (. / 100 | round) / 10;
      (.summary // {}) as $s
      | [ .bentos[]?.dishes[]? | .name as $dish | .tasks[]?
          | { dish: $dish,
              task: (.name // ""),
              outcome: (.outcome.kind // "unknown"),
              key: (.key // ""),
              ms: (.duration_ms // 0),
              err: (.outcome.stderr_excerpt // "") } ] as $rows
      | [ "### bento — \($s.tasks // 0) tasks · \($s.hits // 0) cached · \($s.built // 0) built · \($s.failed // 0) failed · \(($s.duration_ms // 0) | secs)s", "" ]
      + ( if ($rows | length) == 0 then []
          else [ "| dish | task | outcome | key | duration_ms |", "|---|---|---|---|---|" ]
             + ( $rows[:50] | map("| \(.dish) | \(.task) | \(.outcome) | `\(.key[:12])` | \(.ms) |") )
             + ( if ($rows | length) > 50
                 then [ "", "…and \(($rows | length) - 50) more" ]
                 else [] end )
          end )
      + ( [ $rows[] | select(.outcome == "failed") ]
          | map([ "", "**\(.dish) · \(.task)**", "", "```", (.err | .[0:1000]), "```" ])
          | add // [] )
      | .[]
    ' "$1"
}

# HTML marker that makes the PR comment sticky: the update path finds
# the previous comment by grepping bodies for it.
PR_MARKER='<!-- bento-summary -->'

# One-line headline for the PR comment. "Saved" is an estimate, not a
# measurement — a cache hit's duration_ms is its restore cost, not the
# build it replaced — so it extrapolates from the mean duration of the
# tasks that did run this session.
# ponytail: mean-of-built heuristic; needs per-task historical durations
# from the cache server to do better.
pr_headline() {
    jq -r '
      def dur: (. / 1000 | floor) as $s
        | if $s >= 60 then "\($s / 60 | floor)m\($s % 60)s" else "\($s)s" end;
      (.summary // {}) as $s
      | ($s.tasks // 0) as $t
      | ($s.hits // 0) as $h
      | [ .bentos[]?.dishes[]?.tasks[]? | select(.outcome.kind == "built") | (.duration_ms // 0) ] as $built
      | (if ($built | length) > 0 and $h > 0 then ($built | add) / ($built | length) * $h else 0 end) as $saved
      | "### bento · \($h)/\($t) tasks cached (\(if $t > 0 then ($h * 100 / $t | round) else 0 end)%)"
        + (if $saved > 0 then " · ~\($saved | dur) saved" else "" end)
        + (if ($s.failed // 0) > 0 then " · \($s.failed) failed" else "" end)
    ' "$1"
}

# Full PR comment body: marker, headline, the same markdown the job
# summary gets, and a cloud link for workspaces on the hosted cache.
# Reads bento.toml from the cwd, so callers run it in the workspace.
pr_body() {
    printf '%s\n\n' "$PR_MARKER"
    pr_headline "$1"
    printf '\n'
    summarize "$1"
    case "$(read_toml_value cache remote)" in
        bento://*) printf '\n[View on bento cloud → https://app.bento.build](https://app.bento.build)\n' ;;
    esac
}

# Post (or update) the sticky PR comment. Every failure here is a
# `::warning::` and a zero exit: a missing token or a job scoped to
# `pull-requests: read` must not turn a green bento run red.
phase_pr_comment() {
    case "${BENTO_PR_COMMENT:-auto}" in
        never)  return 0 ;;
        always) ;;
        auto)   [ "${GITHUB_EVENT_NAME:-}" = "pull_request" ] || return 0 ;;
        *)
            echo "::warning::pr-comment: unknown value '${BENTO_PR_COMMENT}' (expected auto, always, never) — skipping the PR comment."
            return 0
            ;;
    esac

    [ -f "${REPORT_FILE:-}" ] || return 0
    command -v jq >/dev/null 2>&1 || return 0

    if [ -z "${GH_TOKEN:-}" ] && [ -z "${GITHUB_TOKEN:-}" ]; then
        echo "::warning::pr-comment: no GH_TOKEN / GITHUB_TOKEN in the environment — skipping the PR comment. Grant the job 'permissions: pull-requests: write' (the action's github-token input defaults to the workflow token)."
        return 0
    fi
    export GH_TOKEN="${GH_TOKEN:-$GITHUB_TOKEN}"

    local pr="${PR_NUMBER:-}"
    if [ -z "$pr" ]; then
        pr="$(gh pr view --json number --jq .number 2>/dev/null || true)"
    fi
    if [ -z "$pr" ]; then
        echo "::warning::pr-comment: no pull request found for this ref — skipping the PR comment."
        return 0
    fi

    local body
    body="$(mktemp)"
    if ! pr_body "$REPORT_FILE" > "$body"; then
        echo "::warning::pr-comment: rendering the comment body failed — skipping the PR comment."
        rm -f "$body"
        return 0
    fi

    # --paginate applies --jq per page, so the id stream can span pages:
    # take the first and tolerate the SIGPIPE that closing early gives gh.
    local id
    id="$(gh api "repos/${GITHUB_REPOSITORY}/issues/${pr}/comments" --paginate \
            --jq ".[] | select(.body | contains(\"$PR_MARKER\")) | .id" 2>/dev/null | head -1 || true)"

    if [ -n "$id" ]; then
        gh api -X PATCH "repos/${GITHUB_REPOSITORY}/issues/comments/${id}" -F "body=@${body}" --silent \
            || echo "::warning::pr-comment: updating comment ${id} failed."
    else
        gh api -X POST "repos/${GITHUB_REPOSITORY}/issues/${pr}/comments" -F "body=@${body}" --silent \
            || echo "::warning::pr-comment: posting the comment failed — does the job grant 'permissions: pull-requests: write'?"
    fi
    rm -f "$body"
    return 0
}

phase_preflight() {
    BENTO_ARGS=("doctor")
    if [ -n "${BENTO_ENV:-}" ]; then
        BENTO_ARGS+=("--env" "$BENTO_ENV")
    fi
    add_secret_from_flags
    bento "${BENTO_ARGS[@]}"
}

phase_execute() {
    setup_cache_token
    build_bento_args

    # --report-file always set so the `report` step output is
    # populated regardless of the human-vs-JSON stdout choice.
    BENTO_ARGS+=("--report-file" "$REPORT_FILE")

    local bento_exit=0
    if [ "${BENTO_JSON:-false}" = "true" ]; then
        local json
        json="$(bento "${BENTO_ARGS[@]}" --json)" || bento_exit=$?
        printf '%s\n' "$json"
        publish_output "json" "$json"
    else
        bento "${BENTO_ARGS[@]}" || bento_exit=$?
    fi

    # `report` output: read from --report-file, may be absent on crash.
    if [ -f "$REPORT_FILE" ]; then
        local report
        report="$(cat "$REPORT_FILE")"
        publish_output "report" "$report"
    fi

    # Counters + job summary. Best-effort: jq ships on every GitHub
    # runner but self-hosted boxes may lack it, and an unparseable
    # report must not turn a green run red.
    if [ -f "$REPORT_FILE" ] && command -v jq >/dev/null 2>&1; then
        publish_output "cache-hits"   "$(jq -r '.summary.hits   // 0' "$REPORT_FILE" 2>/dev/null || echo 0)"
        publish_output "cache-misses" "$(jq -r '.summary.built  // 0' "$REPORT_FILE" 2>/dev/null || echo 0)"
        publish_output "failed"       "$(jq -r '.summary.failed // 0' "$REPORT_FILE" 2>/dev/null || echo 0)"
        if [ "${BENTO_JOB_SUMMARY:-true}" = "true" ] && [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
            summarize "$REPORT_FILE" >> "$GITHUB_STEP_SUMMARY" || true
        fi
    fi

    # `artifacts` output: best-effort. Never fail the build; always
    # publish valid JSON so downstream `jq` doesn't choke.
    local artifacts
    if ! artifacts="$(bento artifacts --json 2>/dev/null)"; then
        artifacts='{}'
    fi
    publish_output "artifacts" "$artifacts"

    exit "$bento_exit"
}

# ── Dispatch ───────────────────────────────────────────────────────

case "$PHASE" in
    install-bento)       phase_install_bento ;;
    install-toolchains)  phase_install_toolchains ;;
    preflight)           phase_preflight ;;
    execute)             phase_execute ;;
    summarize)           summarize "${2:?usage: run.sh summarize <report.json>}" ;;
    pr-body)             pr_body "${2:?usage: run.sh pr-body <report.json>}" ;;
    pr-comment)          phase_pr_comment ;;
    *)
        echo "::error::unknown phase '$PHASE' (expected: install-bento, install-toolchains, preflight, execute, summarize, pr-body, pr-comment)" >&2
        exit 2
        ;;
esac
