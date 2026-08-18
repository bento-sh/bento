#!/usr/bin/env bash
# Tests for run.sh's `summarize` phase — the markdown the action appends
# to $GITHUB_STEP_SUMMARY. Plain bash + jq, no test framework: the thing
# under test is 20 lines of jq and the assertions are greps.
#
# Run: bash scripts/action/test_summary.sh

set -euo pipefail

RUN_SH="$(cd "$(dirname "$0")" && pwd)/run.sh"

if ! command -v jq >/dev/null 2>&1; then
    echo "jq not found — skipping summarize tests"
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

# summarize <fixture.json> -> $TMP/out
summarize() {
    if ! bash "$RUN_SH" summarize "$1" > "$TMP/out" 2>"$TMP/err"; then
        echo "--- stderr ---"
        cat "$TMP/err"
        return 1
    fi
}

expect_contains() {
    grep -qF -- "$2" "$TMP/out" || fail "$1" "expected output to contain: $2"
}

expect_missing() {
    grep -qF -- "$2" "$TMP/out" && fail "$1" "expected output NOT to contain: $2"
    return 0
}

# ── headline + table + failed excerpt ───────────────────────────────
cat > "$TMP/mixed.json" <<'JSON'
{
  "bentos": [
    {
      "name": "default",
      "dishes": [
        {
          "name": "api",
          "tasks": [
            { "name": "build", "run": "go build ./...", "key": "abcdef0123456789deadbeef", "duration_ms": 12, "outcome": { "kind": "cache_hit" } },
            { "name": "test", "run": "go test ./...", "key": "0011223344556677", "duration_ms": 2100, "outcome": { "kind": "built", "exit_code": 0 } }
          ]
        },
        {
          "name": "web",
          "tasks": [
            { "name": "lint", "run": "npm run lint", "key": "ffeeddccbbaa9988", "duration_ms": 400, "outcome": { "kind": "failed", "exit_code": 1, "stderr_excerpt": "ESLint: MARKER_START unexpected token" } }
          ]
        }
      ]
    }
  ],
  "summary": { "dishes": 2, "tasks": 3, "hits": 1, "built": 1, "failed": 1, "flaky": 0, "duration_ms": 2512 }
}
JSON

summarize "$TMP/mixed.json" || fail headline "summarize exited non-zero"
expect_contains headline "3 tasks · 1 cached · 1 built · 1 failed · 2.5s"
expect_contains table-header "| dish | task | outcome | key | duration_ms |"
expect_contains row-cache-hit "| api | build | cache_hit | \`abcdef012345\` | 12 |"
expect_contains row-built "| api | test | built | \`001122334455\` | 2100 |"
expect_missing key-untruncated "abcdef0123456789"
expect_contains failed-heading "**web · lint**"
expect_contains failed-excerpt "MARKER_START unexpected token"
[ "$FAILURES" -eq 0 ] && pass "headline, table, key truncation, failed excerpt"

# ── stderr excerpt is truncated ─────────────────────────────────────
before=$FAILURES
jq -n --arg err "$(printf 'x%.0s' $(seq 1 1200))MARKER_TAIL" '
  { bentos: [ { name: "default", dishes: [ { name: "d", tasks: [
      { name: "t", run: "x", key: "k", duration_ms: 1,
        outcome: { kind: "failed", exit_code: 1, stderr_excerpt: $err } } ] } ] } ],
    summary: { tasks: 1, hits: 0, built: 0, failed: 1, duration_ms: 5 } }' > "$TMP/long.json"

summarize "$TMP/long.json" || fail truncation "summarize exited non-zero"
expect_missing excerpt-tail-dropped "MARKER_TAIL"
[ "$FAILURES" -eq "$before" ] && pass "failed stderr_excerpt truncated"

# ── row cap at 50 ───────────────────────────────────────────────────
before=$FAILURES
jq -n '
  { bentos: [ { name: "default", dishes: [ { name: "d", tasks: [
      range(0; 60) | { name: "t\(.)", run: "x", key: "0123456789abcdef",
                       duration_ms: 1, outcome: { kind: "cache_hit" } } ] } ] } ],
    summary: { tasks: 60, hits: 60, built: 0, failed: 0, duration_ms: 1000 } }' > "$TMP/big.json"

summarize "$TMP/big.json" || fail rowcap "summarize exited non-zero"
rows="$(grep -c '^| d | t' "$TMP/out" || true)"
[ "$rows" -eq 50 ] || fail rowcap "expected 50 table rows, got $rows"
expect_contains rowcap-more "…and 10 more"
[ "$FAILURES" -eq "$before" ] && pass "table capped at 50 rows with overflow note"

# ── empty / degenerate reports don't crash ──────────────────────────
before=$FAILURES
echo '{"bentos":[],"summary":{"dishes":0,"tasks":0,"hits":0,"built":0,"failed":0,"duration_ms":0}}' > "$TMP/empty.json"
summarize "$TMP/empty.json" || fail empty "summarize exited non-zero on empty report"
expect_contains empty-headline "0 tasks · 0 cached · 0 built · 0 failed · 0s"
expect_missing empty-no-table "| dish | task |"

echo '{}' > "$TMP/bare.json"
summarize "$TMP/bare.json" || fail bare "summarize exited non-zero on {}"
expect_contains bare-headline "0 tasks · 0 cached"
[ "$FAILURES" -eq "$before" ] && pass "empty + bare reports render a headline and no table"

if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES assertion(s) failed"
    exit 1
fi
echo "all summarize tests passed"
