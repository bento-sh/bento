#!/usr/bin/env bash
# Tests for run.sh's `pr-body` + `pr-comment` phases — the sticky PR
# comment. Same shape as test_summary.sh: plain bash + jq, assertions
# are greps. `gh` is stubbed on PATH (argv logged to $GH_LOG, canned
# replies from env), so nothing here touches the network.
#
# Run: bash scripts/action/test_pr_comment.sh

set -euo pipefail

RUN_SH="$(cd "$(dirname "$0")" && pwd)/run.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "jq not found — skipping pr-comment tests"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILURES=0

pass() { echo "ok   $1"; }
fail() {
    echo "FAIL $1: $2"
    FAILURES=$((FAILURES + 1))
}

# ── fixtures ────────────────────────────────────────────────────────
mkdir -p "$TMP/bin" "$TMP/ws"

cat > "$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$GH_LOG"
if [ "${GH_FAIL:-0}" = 1 ]; then
    echo "gh: simulated failure" >&2
    exit 1
fi
case "$*" in
    *"-X PATCH"*|*"-X POST"*) : ;;
    *comments*) [ -n "${GH_EXISTING_ID:-}" ] && printf '%s\n' "$GH_EXISTING_ID" ;;
esac
exit 0
STUB
chmod +x "$TMP/bin/gh"

# 1 of 3 tasks cached, one 2100ms build → ~2s estimated saving.
cat > "$TMP/ws/report.json" <<'JSON'
{
  "bentos": [
    {
      "name": "default",
      "dishes": [
        {
          "name": "api",
          "tasks": [
            { "name": "build", "run": "go build ./...", "key": "abcdef0123456789", "duration_ms": 12, "outcome": { "kind": "cache_hit" } },
            { "name": "test", "run": "go test ./...", "key": "0011223344556677", "duration_ms": 2100, "outcome": { "kind": "built", "exit_code": 0 } }
          ]
        },
        {
          "name": "web",
          "tasks": [
            { "name": "lint", "run": "npm run lint", "key": "ffeeddccbbaa9988", "duration_ms": 400, "outcome": { "kind": "failed", "exit_code": 1, "stderr_excerpt": "ESLint: unexpected token" } }
          ]
        }
      ]
    }
  ],
  "summary": { "dishes": 2, "tasks": 3, "hits": 1, "built": 1, "failed": 1, "flaky": 0, "duration_ms": 2512 }
}
JSON

# run.sh <phase> [args] with the gh stub on PATH, from the workspace dir
# so `pr_body`'s bento.toml lookup sees the fixture.
run_phase() {
    ( cd "$TMP/ws" && PATH="$TMP/bin:$PATH" GH_LOG="$TMP/gh.log" bash "$RUN_SH" "$@" ) \
        > "$TMP/out" 2> "$TMP/err"
}

expect_contains() {
    grep -qF -- "$2" "$TMP/out" || fail "$1" "expected output to contain: $2"
}

expect_missing() {
    grep -qF -- "$2" "$TMP/out" && fail "$1" "expected output NOT to contain: $2"
    return 0
}

# ── body: marker once, headline, reused summarize table ─────────────
before=$FAILURES
run_phase pr-body report.json || fail body "pr-body exited non-zero"
markers="$(grep -cF -- '<!-- bento-summary -->' "$TMP/out" || true)"
[ "$markers" -eq 1 ] || fail body-marker "expected exactly 1 marker, got $markers"
expect_contains body-headline "### bento · 1/3 tasks cached (33%) · ~2s saved · 1 failed"
expect_contains body-summary "### bento — 3 tasks · 1 cached · 1 built · 1 failed"
expect_contains body-table "| api | build | cache_hit |"
expect_contains body-failed "**web · lint**"
expect_missing body-no-cloud "app.bento.build"
[ "$FAILURES" -eq "$before" ] && pass "body carries one marker, a headline, and the job-summary markdown"

# ── body: cloud footer only for a bento:// remote ───────────────────
before=$FAILURES
printf '[cache]\nremote = "bento://cache.bento.build"\n' > "$TMP/ws/bento.toml"
run_phase pr-body report.json || fail cloud "pr-body exited non-zero"
expect_contains cloud-footer "View on bento cloud → https://app.bento.build"

printf '[cache]\nremote = "s3://bucket/prefix"\n' > "$TMP/ws/bento.toml"
run_phase pr-body report.json || fail cloud-s3 "pr-body exited non-zero"
expect_missing cloud-s3-footer "app.bento.build"
rm -f "$TMP/ws/bento.toml"
[ "$FAILURES" -eq "$before" ] && pass "cloud footer appears only for a bento:// remote"

# comment <expect-exit> — run the pr-comment phase with a fresh gh log.
comment() {
    : > "$TMP/gh.log"
    run_phase pr-comment || fail pr-comment "phase exited non-zero (must never fail the job)"
}

export REPORT_FILE="$TMP/ws/report.json"
export GITHUB_REPOSITORY="acme/widgets"
export GITHUB_EVENT_NAME="pull_request"
export PR_NUMBER=7
export GH_TOKEN="t0ken"

# ── never: no gh calls, no output ───────────────────────────────────
before=$FAILURES
BENTO_PR_COMMENT=never comment
[ -s "$TMP/gh.log" ] && fail never "gh was invoked with pr-comment: never"
[ -s "$TMP/out" ] && fail never "expected no output with pr-comment: never"
[ "$FAILURES" -eq "$before" ] && pass "pr-comment: never posts nothing"

# ── auto outside a pull_request event: nothing ──────────────────────
before=$FAILURES
GITHUB_EVENT_NAME=push comment
[ -s "$TMP/gh.log" ] && fail auto-push "gh was invoked on a push event"
[ "$FAILURES" -eq "$before" ] && pass "pr-comment: auto skips non-pull_request events"

# ── always: comments on a push too ──────────────────────────────────
before=$FAILURES
GITHUB_EVENT_NAME=push BENTO_PR_COMMENT=always comment
grep -q -- "-X POST" "$TMP/gh.log" || fail always "expected a POST on a push event with pr-comment: always"
[ "$FAILURES" -eq "$before" ] && pass "pr-comment: always comments off a pull_request event"

# ── no token: warning, no gh call, exit 0 ───────────────────────────
before=$FAILURES
GH_TOKEN='' GITHUB_TOKEN='' comment
expect_contains no-token "::warning::"
expect_contains no-token-hint "pull-requests: write"
[ -s "$TMP/gh.log" ] && fail no-token "gh was invoked without a token"
[ "$FAILURES" -eq "$before" ] && pass "missing token warns and skips instead of failing"

# ── create path: no existing comment → POST ─────────────────────────
before=$FAILURES
comment
grep -q "repos/acme/widgets/issues/7/comments --paginate" "$TMP/gh.log" \
    || fail create "expected a marker lookup on the PR's comments"
grep -q -- "-X POST repos/acme/widgets/issues/7/comments" "$TMP/gh.log" \
    || fail create "expected a POST creating the comment"
grep -q -- "-X PATCH" "$TMP/gh.log" && fail create "unexpected PATCH with no existing comment"
[ "$FAILURES" -eq "$before" ] && pass "no existing comment → creates one"

# ── update path: marker found → PATCH that comment ──────────────────
before=$FAILURES
GH_EXISTING_ID=4242 comment
grep -q -- "-X PATCH repos/acme/widgets/issues/comments/4242" "$TMP/gh.log" \
    || fail update "expected a PATCH against the existing comment"
grep -q -- "-X POST" "$TMP/gh.log" && fail update "unexpected POST when a comment already exists"
[ "$FAILURES" -eq "$before" ] && pass "existing comment → edited in place (sticky)"

# ── gh failure: warning, exit 0 ─────────────────────────────────────
before=$FAILURES
GH_FAIL=1 comment
expect_contains gh-fail "::warning::"
[ "$FAILURES" -eq "$before" ] && pass "gh failure is a warning, not a job failure"

# ── missing report file: silent no-op ───────────────────────────────
before=$FAILURES
REPORT_FILE="$TMP/nope.json" comment
[ -s "$TMP/gh.log" ] && fail no-report "gh was invoked with no report file"
[ "$FAILURES" -eq "$before" ] && pass "missing report file is a silent no-op"

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES assertion(s) failed"
    exit 1
fi
echo "all pr-comment tests passed"
