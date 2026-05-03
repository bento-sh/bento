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
    echo "usage: run.sh <install-bento|install-toolchains|preflight|execute>" >&2
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
        Linux)  triple="${arch}-unknown-linux-gnu" ;;
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

    if [ -f "$tmp/${asset}.tar.gz.sha256" ]; then
        local expected actual
        expected="$(awk '{print $1}' "$tmp/${asset}.tar.gz.sha256")"
        actual="$(sha256sum "$tmp/${asset}.tar.gz" | awk '{print $1}')"
        if [ "$expected" != "$actual" ]; then
            echo "::error::checksum mismatch (expected $expected, got $actual)" >&2
            exit 1
        fi
        echo "==> checksum verified"
    fi

    tar -xzf "$tmp/${asset}.tar.gz" -C "$tmp"
    mv "$tmp/${asset}/bento" "$BENTO_INSTALL_DIR/bento"
    chmod +x "$BENTO_INSTALL_DIR/bento"
    echo "$BENTO_INSTALL_DIR" >> "$GITHUB_PATH"
    "$BENTO_INSTALL_DIR/bento" --version
}

phase_install_toolchains() {
    # Capture stdout + exit code so we can publish the JSON output
    # even on partial failure, then propagate the failure upstream.
    local install_exit=0 json
    json="$(bento toolchain install --json)" || install_exit=$?
    printf '%s\n' "$json"
    publish_output "json" "$json"
    exit "$install_exit"
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
    *)
        echo "::error::unknown phase '$PHASE' (expected: install-bento, install-toolchains, preflight, execute)" >&2
        exit 2
        ;;
esac
