#!/usr/bin/env bash
# Verification harness for `security-finding-to-issue.yml`.
#
# Runs the filer script (`.github/scripts/security_finding_to_issue.py`)
# against five scenarios, mocking the `gh` CLI via `gh_shim.py`. Each
# scenario asserts on the side effects recorded by the shim:
#
#   Test A — empty alerts list                → 0 issues created
#   Test B — one alert, no prior issues       → 1 issue with structured body
#   Test C — same alert, matching-fp issue    → 0 issues (dedup holds)
#   Test D — same alert, different-fp issue   → 1 issue (different finding)
#   Test E — 30 alerts (cap=25)               → 25 issues + 1 tracker
#
# The shim writes one JSON record per `gh issue create` / `gh issue
# comment` call to $GH_SHIM_ISSUES_OUT, which the test then parses
# with python3.
#
# Local invocation (from the repo root):
#   bash tests/fixtures/security-finding-to-issue/verify.sh
#
# CI invocation: same. The script depends on python3 + bash only.

set -euo pipefail

# Resolve the repo root regardless of where the script is invoked from.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
FILER="$REPO_ROOT/.github/scripts/security_finding_to_issue.py"
SHIM="$HERE/gh_shim.py"

if [ ! -f "$FILER" ]; then
  echo "verify.sh: filer script not found at $FILER" >&2
  exit 2
fi
if [ ! -f "$SHIM" ]; then
  echo "verify.sh: gh shim not found at $SHIM" >&2
  exit 2
fi

# Temp scratch dir. Each test gets its own subdir so the issue-out
# files don't bleed across cases.
TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

# Stub PATH: only the shim, plus the system python. The production
# script is invoked through `python3`, which we keep on PATH.
STUB_BIN="$TMPROOT/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/gh" <<EOF
#!/usr/bin/env bash
exec python3 "$SHIM" "\$@"
EOF
chmod +x "$STUB_BIN/gh"
# Prepend the stub dir to PATH so our `gh` shim wins. We keep the
# inherited PATH intact rather than narrowing it because some local
# python3 setups (asdf, pyenv) need their full shim chain to resolve
# the real interpreter — narrowing the PATH breaks those.
export PATH="$STUB_BIN:$PATH"

PASS=0
FAIL=0

run_test() {
  local name="$1"; shift
  local alerts="$1"; shift
  local issues_in="$1"; shift
  local max_per_run="$1"; shift
  local expect_creates="$1"; shift
  local expect_comments="$1"; shift
  local expect_tracker="$1"; shift  # 0 or 1
  local expect_first_labels="$1"; shift  # ; -separated, or "-"

  local tdir="$TMPROOT/$name"
  mkdir -p "$tdir"
  local out="$tdir/calls.jsonl"
  : > "$out"
  : > "$tdir/trace.log"

  GH_SHIM_ALERTS="$alerts" \
  GH_SHIM_ISSUES="$issues_in" \
  GH_SHIM_ISSUES_OUT="$out" \
  GH_SHIM_TRACE="$tdir/trace.log" \
  GH_SHIM_LABELS="security,security-bot" \
  MAX_ISSUES_PER_RUN="$max_per_run" \
  UPSTREAM_WORKFLOW="SAST — static analysis" \
  UPSTREAM_RUN_ID="123456" \
  UPSTREAM_RUN_URL="https://example.invalid/run/123456" \
  UPSTREAM_HEAD_SHA="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" \
  python3 "$FILER" > "$tdir/stdout.log" 2> "$tdir/stderr.log" || {
    echo "[$name] FAIL: filer exited non-zero"
    echo "--- stdout ---"
    cat "$tdir/stdout.log"
    echo "--- stderr ---"
    cat "$tdir/stderr.log"
    FAIL=$((FAIL + 1))
    return
  }

  # Tally calls.
  local creates comments
  creates=$(python3 -c "
import json, sys
n = 0
for line in open('$out'):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get('action') == 'create' and 'rate-limited' not in rec.get('labels', []):
        n += 1
print(n)
")
  comments=$(python3 -c "
import json
n = 0
for line in open('$out'):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get('action') == 'comment':
        n += 1
print(n)
")
  local trackers
  trackers=$(python3 -c "
import json
n = 0
for line in open('$out'):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get('action') == 'create' and 'rate-limited' in rec.get('labels', []):
        n += 1
print(n)
")

  local ok=1
  if [ "$creates" != "$expect_creates" ]; then
    echo "[$name] FAIL: expected $expect_creates non-tracker create(s); got $creates"
    ok=0
  fi
  if [ "$comments" != "$expect_comments" ]; then
    echo "[$name] FAIL: expected $expect_comments comment(s); got $comments"
    ok=0
  fi
  if [ "$trackers" != "$expect_tracker" ]; then
    echo "[$name] FAIL: expected $expect_tracker tracker issue(s); got $trackers"
    ok=0
  fi

  # When a creation is expected and labels were specified, assert the
  # first non-tracker create carries them and a fingerprint comment.
  if [ "$expect_creates" -gt 0 ] && [ "$expect_first_labels" != "-" ]; then
    python3 - "$out" "$expect_first_labels" <<'PY' || ok=0
import json, sys, re
out_path, expected_labels = sys.argv[1], sys.argv[2].split(";")
first = None
for line in open(out_path):
    line = line.strip()
    if not line: continue
    rec = json.loads(line)
    if rec.get("action") == "create" and "rate-limited" not in rec.get("labels", []):
        first = rec
        break
if first is None:
    print("expected at least one non-tracker create")
    sys.exit(1)
missing = [lab for lab in expected_labels if lab not in first.get("labels", [])]
if missing:
    print(f"first create missing labels: {missing}; got {first.get('labels')}")
    sys.exit(1)
body = first.get("body", "")
if not re.search(r"<!--\s*sec-fingerprint:\s*[0-9a-f]{64}\s*-->", body):
    print("first create body missing sec-fingerprint HTML comment")
    sys.exit(1)
required_sections = ["## ", "### Location", "### Scanner", "### Auto-remediation"]
missing_sec = [s for s in required_sections if s not in body]
if missing_sec:
    print(f"first create body missing sections: {missing_sec}")
    sys.exit(1)
PY
    if [ $? -ne 0 ]; then
      echo "[$name] FAIL: body/labels assertion failed"
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

# ---------------------------------------------------------------------------
# Generate the 30-alert fixture for Test E. Each alert is identical to
# the alerts_single.json template except for path + start_line, so the
# fingerprints diverge and the script sees them as distinct findings.

THIRTY="$TMPROOT/alerts_thirty.json"
python3 - <<PY > "$THIRTY"
import json
alerts = []
for i in range(30):
    alerts.append({
        "number": 1000 + i,
        "state": "open",
        "html_url": f"https://github.com/example/example/security/code-scanning/{1000 + i}",
        "rule": {
            "id": "rust/unsafe-block",
            "severity": "warning",
            "security_severity_level": "medium",
            "description": "Use of unsafe block requires a SAFETY justification comment.",
            "name": "Unjustified unsafe block",
            "help": "Add a SAFETY comment above the unsafe block."
        },
        "tool": {"name": "semgrep", "version": "1.162.0"},
        "most_recent_instance": {
            "ref": "refs/heads/main",
            "analysis_key": ".github/workflows/sast.yml:semgrep",
            "message": {"text": f"Unsafe block without justification (#{i})."},
            "location": {
                "path": f"crates/barista-coords/src/finding_{i}.rs",
                "start_line": 100 + i,
                "end_line": 105 + i,
                "snippet": {"text": f"unsafe {{ /* finding {i} */ }}"}
            }
        }
    })
print(json.dumps(alerts))
PY

ALERTS_EMPTY="$HERE/alerts_empty.json"
ALERTS_SINGLE="$HERE/alerts_single.json"
ISSUES_EMPTY="$HERE/issues_empty.json"
ISSUES_MATCH="$HERE/issues_with_matching_fingerprint.json"
ISSUES_DIFF="$HERE/issues_with_different_fingerprint.json"

# Test A — no findings → no issues.
run_test "A-empty-alerts" "$ALERTS_EMPTY" "$ISSUES_EMPTY" 25 0 0 0 "-"

# Test B — single new finding → exactly one structured issue.
run_test "B-single-new"   "$ALERTS_SINGLE" "$ISSUES_EMPTY" 25 1 0 0 "security;security-bot;semgrep"

# Test C — same finding, matching-fingerprint issue exists → no creates.
run_test "C-dedup"        "$ALERTS_SINGLE" "$ISSUES_MATCH" 25 0 0 0 "-"

# Test D — same finding, different existing issue → still a create.
run_test "D-distinct"     "$ALERTS_SINGLE" "$ISSUES_DIFF"  25 1 0 0 "security;security-bot;semgrep"

# Test E — 30 alerts, cap=25 → 25 creates + 1 tracker issue (no prior tracker).
run_test "E-rate-limit"   "$THIRTY"        "$ISSUES_EMPTY" 25 25 0 1 "security;security-bot;semgrep"

# Test F — missing scanner label auto-created; existing labels skipped.
# Reuses Test B's recorded calls: GH_SHIM_LABELS reported security +
# security-bot as existing, so the filer must create only `semgrep`.
F_OUT="$TMPROOT/B-single-new/calls.jsonl"
python3 - "$F_OUT" <<'PY'
import json, sys
created = set()
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    rec = json.loads(line)
    if rec.get("action") == "label":
        created.add(rec.get("name"))
errs = []
if "semgrep" not in created:
    errs.append("expected the missing 'semgrep' label to be created")
if "security" in created or "security-bot" in created:
    errs.append(f"existing labels should not be re-created; got {sorted(created)}")
if errs:
    for e in errs:
        print(e)
    sys.exit(1)
PY
if [ $? -eq 0 ]; then
  echo "[F-label-autocreate] PASS"
  PASS=$((PASS + 1))
else
  echo "[F-label-autocreate] FAIL: see above"
  FAIL=$((FAIL + 1))
fi

echo
echo "=========================="
echo "PASSED: $PASS"
echo "FAILED: $FAIL"
echo "=========================="

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
