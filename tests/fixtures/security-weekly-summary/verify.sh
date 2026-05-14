#!/usr/bin/env bash
# Verification harness for `security-weekly-summary.yml`.
#
# Runs the script (`.github/scripts/security_weekly_summary.py`) against
# four scenarios, mocking the `gh` CLI via `gh_shim.py`. Each scenario
# asserts on the side effects recorded by the shim:
#
#   Test A — empty world                       → 1 summary issue, "no findings"
#   Test B — mixed findings + prior summary    → 1 summary, body counts match
#   Test C — mixed PRs                         → success rate body section accurate
#   Test D — retention sweep                   → an old prior summary gets closed
#
# Local invocation (from the repo root):
#   bash tests/fixtures/security-weekly-summary/verify.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
SCRIPT="$REPO_ROOT/.github/scripts/security_weekly_summary.py"
SHIM="$HERE/gh_shim.py"

if [ ! -f "$SCRIPT" ]; then
  echo "verify.sh: script not found at $SCRIPT" >&2
  exit 2
fi
if [ ! -f "$SHIM" ]; then
  echo "verify.sh: gh shim not found at $SHIM" >&2
  exit 2
fi

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

STUB_BIN="$TMPROOT/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/gh" <<EOF
#!/usr/bin/env bash
exec python3 "$SHIM" "\$@"
EOF
chmod +x "$STUB_BIN/gh"
export PATH="$STUB_BIN:$PATH"

PASS=0
FAIL=0

# Anchor `now` at 2026-05-14 so the fixture timestamps are deterministic.
ANCHOR="2026-05-14T13:00:00"

run_test() {
  local name="$1"; shift
  local findings="$1"; shift
  local summaries="$1"; shift
  local prs="$1"; shift
  local retention="$1"; shift
  local expect_creates="$1"; shift
  local expect_closes="$1"; shift
  local body_grep="$1"; shift   # extended regex, or "-" to skip

  local tdir="$TMPROOT/$name"
  mkdir -p "$tdir"
  local out="$tdir/calls.jsonl"
  : > "$out"
  : > "$tdir/trace.log"

  GH_SHIM_FINDINGS="$findings" \
  GH_SHIM_SUMMARIES="$summaries" \
  GH_SHIM_PRS="$prs" \
  GH_SHIM_OUT="$out" \
  GH_SHIM_TRACE="$tdir/trace.log" \
  REPORT_DATE="$ANCHOR" \
  WEEKLY_SUMMARY_RETENTION_WEEKS="$retention" \
  REMEDIATION_BOT_LOGIN="security-bot[bot]" \
  python3 "$SCRIPT" > "$tdir/stdout.log" 2> "$tdir/stderr.log" || {
    echo "[$name] FAIL: script exited non-zero"
    echo "--- stdout ---"; cat "$tdir/stdout.log"
    echo "--- stderr ---"; cat "$tdir/stderr.log"
    FAIL=$((FAIL + 1))
    return
  }

  local creates closes
  creates=$(python3 -c "
import json
n = 0
for line in open('$out'):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get('action') == 'create':
        n += 1
print(n)
")
  closes=$(python3 -c "
import json
n = 0
for line in open('$out'):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get('action') == 'close':
        n += 1
print(n)
")

  local ok=1
  if [ "$creates" != "$expect_creates" ]; then
    echo "[$name] FAIL: expected $expect_creates create(s); got $creates"
    ok=0
  fi
  if [ "$closes" != "$expect_closes" ]; then
    echo "[$name] FAIL: expected $expect_closes close(s); got $closes"
    ok=0
  fi
  if [ "$body_grep" != "-" ] && [ "$expect_creates" -gt 0 ]; then
    # Walk the first create record's body and require all `;`-separated
    # regexes to match.
    if ! python3 - "$out" "$body_grep" <<'PY'
import json, re, sys
out_path, patterns = sys.argv[1], sys.argv[2].split(";")
body = None
for line in open(out_path):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get("action") == "create":
        body = rec.get("body", "")
        break
if body is None:
    print("no create record found", file=sys.stderr)
    sys.exit(1)
for pat in patterns:
    if not re.search(pat, body):
        print(f"body missing pattern: {pat!r}", file=sys.stderr)
        sys.exit(1)
PY
    then
      echo "[$name] FAIL: body pattern mismatch"
      ok=0
    fi
  fi

  if [ "$ok" = "1" ]; then
    echo "[$name] PASS"
    PASS=$((PASS + 1))
  else
    echo "  trace: $tdir/trace.log"
    echo "  stdout: $tdir/stdout.log"
    FAIL=$((FAIL + 1))
  fi
}

FINDINGS_EMPTY="$HERE/findings_empty.json"
FINDINGS_MIXED="$HERE/findings_mixed.json"
SUMMARIES_EMPTY="$HERE/summaries_empty.json"
SUMMARIES_PRIOR_ONLY="$HERE/summaries_prior_only.json"
SUMMARIES_PRIOR="$HERE/summaries_prior_plus_old.json"
PRS_EMPTY="$HERE/prs_empty.json"
PRS_MIX="$HERE/prs_two_merged_one_closed.json"

# Test A — empty world. The script still creates a summary issue; it
# reports zero findings, zero PRs, no delta.
run_test "A-empty-world" \
  "$FINDINGS_EMPTY" "$SUMMARIES_EMPTY" "$PRS_EMPTY" \
  6 1 0 \
  "Open findings by severity;\\*\\*total\\*\\* \\| \\*\\*0\\*\\*;The tracker is empty"

# Test B — mixed findings with a prior summary; expect severity counts
# and the week-over-week delta to render. The fixture has 4 OPEN
# findings (1 critical + 1 high + 1 medium + 1 low); prior open-total
# was 2, so the delta is +2.
run_test "B-mixed-findings" \
  "$FINDINGS_MIXED" "$SUMMARIES_PRIOR_ONLY" "$PRS_EMPTY" \
  6 1 0 \
  "\\| critical \\| 1 \\|;\\| high \\| 1 \\|;\\| medium \\| 1 \\|;\\| low \\| 1 \\|;\\*\\*total\\*\\* \\| \\*\\*4\\*\\*;changed by \\*\\*\\+2\\*\\*"

# Test C — mixed PRs (2 merged, 1 closed); expect 66.7% success rate.
run_test "C-mixed-prs" \
  "$FINDINGS_EMPTY" "$SUMMARIES_EMPTY" "$PRS_MIX" \
  6 1 0 \
  "opened \\*\\*3\\*\\* PR\\(s\\);\\*\\*2\\*\\* merged;66\\.7%"

# Test D — retention sweep. Two prior summaries: one from last week,
# one from January (older than 6 weeks). Expect 1 close.
run_test "D-retention" \
  "$FINDINGS_EMPTY" "$SUMMARIES_PRIOR" "$PRS_EMPTY" \
  6 1 1 \
  "-"

echo
echo "=========================="
echo "PASSED: $PASS"
echo "FAILED: $FAIL"
echo "=========================="

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
