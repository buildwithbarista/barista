#!/usr/bin/env bash
# scripts/test-perf-gate-logic.sh
#
# Smoke test for the Tier-2 perf-gate comparison logic in
# `scripts/compare-perf-results.sh`. Constructs synthetic
# baseline/current results.json pairs with controllable median
# wall-clock deltas, runs the comparison script, and asserts the
# right pass/warn/fail outcomes against PRD §17.10 thresholds.
#
# Cases exercised:
#   1. Identical baseline + current        → PASS, exit 0
#   2. Small improvement (-7% on D1)       → PASS+WINS, exit 0
#   3. Small regression (+5% on D1)        → WARN, exit 0
#   4. Large regression (+15% on D1)       → FAIL, exit 1
#   5. Large regression on D1 + exemption  → WARN, exit 0
#   6. D2 +12% (under D2's +15% cap)       → WARN, exit 0
#   7. Missing dimension → +12% delta hits the conservative D1 fallback → FAIL
#
# Run locally: `bash scripts/test-perf-gate-logic.sh`. Run in CI:
# wired into `.github/workflows/workflow-lint.yml` alongside the
# other config-validation smoke tests.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/compare-perf-results.sh"

if [ ! -x "$SCRIPT" ]; then
  echo "error: $SCRIPT not executable" >&2
  exit 2
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 not on PATH" >&2
  exit 2
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# emit_results <out_path> <manifest_id> <project> <dimension> <median_ms>
emit_results() {
  local out="$1" mid="$2" project="$3" dim="$4" ms="$5"
  python3 - "$out" "$mid" "$project" "$dim" "$ms" <<'PYEOF'
import json, sys
out, mid, project, dim, ms = sys.argv[1:6]
ms_f = float(ms)
doc = {
    "schema": "barista.bench.results/v1",
    "manifest_id": mid,
    "run_id": f"2026-05-17T00:00:00Z-{mid}",
    "timestamp": "2026-05-17T00:00:00Z",
    "git_sha": "0" * 40,
    "barista_version": "0.0.0-test",
    "hardware_tier": 2,
    "runner_id": "test",
    "hardware": {
        "id": "test",
        "cpu": "test",
        "cores_physical": 1,
        "cores_logical": 1,
        "memory_gb": 1,
        "os": "test",
    },
    "iterations": [{"iteration": 0, "wall_ms": int(ms_f), "exit_code": 0}],
    "summary": {
        "avg_wall_ms": ms_f,
        "median_wall_ms": ms_f,
        "p95_wall_ms": ms_f,
        "stddev_wall_ms": 0.0,
    },
    "metadata": {"project": project},
}
if dim != "-":
    doc["metadata"]["dimension"] = dim
with open(out, "w", encoding="utf-8") as f:
    json.dump(doc, f)
PYEOF
}

# write_accepted <out_path> <project> <dimension>
# Writes a minimal accepted-regressions.md with one exemption row.
write_accepted() {
  local out="$1" project="$2" dim="$3"
  cat > "$out" <<EOF
# Test accepted regressions

| Project | Dimension | Date | Baseline | Current | Δ | Rationale | Issue/PR |
|---|---|---|---:|---:|---:|---|---|
| $project | $dim | 2026-05-17 | 100 | 200 | +100% | test | n/a |
EOF
}

# write_empty_accepted <out_path>
write_empty_accepted() {
  local out="$1"
  cat > "$out" <<'EOF'
# Test accepted regressions (no entries)

| Project | Dimension | Date | Baseline | Current | Δ | Rationale | Issue/PR |
|---|---|---|---:|---:|---:|---|---|
| (no entries yet) | — | — | — | — | — | — | — |
EOF
}

# run_case <name> <expected_verdict> <expected_exit> [accepted_md]
run_case() {
  local name="$1" expected_verdict="$2" expected_exit="$3" accepted="${4:-}"
  local out_dir="$WORK/$name"
  mkdir -p "$out_dir/baseline" "$out_dir/current"
  cp "$WORK/cases/$name/baseline/"*.json "$out_dir/baseline/"
  cp "$WORK/cases/$name/current/"*.json "$out_dir/current/"
  if [ -z "$accepted" ]; then
    accepted="$WORK/empty-accepted.md"
  fi

  local report="$out_dir/report.md"
  local summary="$out_dir/summary.md"
  local stdout_log="$out_dir/stdout.log"

  set +e
  bash "$SCRIPT" \
    --baseline "$out_dir/baseline" \
    --current "$out_dir/current" \
    --accepted "$accepted" \
    --summary "$summary" \
    --report "$report" > "$stdout_log" 2>&1
  local rc=$?
  set -e

  local actual_verdict
  actual_verdict="$(grep -m1 '^perf-gate verdict: ' "$stdout_log" | sed 's/^perf-gate verdict: //')"

  if [ "$rc" != "$expected_exit" ]; then
    echo "FAIL [$name]: expected exit $expected_exit, got $rc"
    echo "--- stdout ---"
    cat "$stdout_log"
    echo "--- report ---"
    cat "$report"
    return 1
  fi
  if [ "$actual_verdict" != "$expected_verdict" ]; then
    echo "FAIL [$name]: expected verdict $expected_verdict, got $actual_verdict"
    echo "--- stdout ---"
    cat "$stdout_log"
    return 1
  fi
  echo "ok   [$name]: verdict=$actual_verdict exit=$rc"
}

# -----------------------------------------------------------------------------
# Set up fixtures.
# -----------------------------------------------------------------------------
write_empty_accepted "$WORK/empty-accepted.md"

# Case 1: identical
mkdir -p "$WORK/cases/identical/baseline" "$WORK/cases/identical/current"
emit_results "$WORK/cases/identical/baseline/p02-d1.json" "p02-d1-cold" "p02" "D1" 1000
emit_results "$WORK/cases/identical/current/p02-d1.json"  "p02-d1-cold" "p02" "D1" 1000

# Case 2: small improvement (-7% on D1) — should be a win.
mkdir -p "$WORK/cases/improvement/baseline" "$WORK/cases/improvement/current"
emit_results "$WORK/cases/improvement/baseline/p02-d1.json" "p02-d1-cold" "p02" "D1" 1000
emit_results "$WORK/cases/improvement/current/p02-d1.json"  "p02-d1-cold" "p02" "D1" 930

# Case 3: small regression (+5% on D1) — under +10% cap → WARN.
mkdir -p "$WORK/cases/small-regression/baseline" "$WORK/cases/small-regression/current"
emit_results "$WORK/cases/small-regression/baseline/p02-d1.json" "p02-d1-cold" "p02" "D1" 1000
emit_results "$WORK/cases/small-regression/current/p02-d1.json"  "p02-d1-cold" "p02" "D1" 1050

# Case 4: large regression (+15% on D1) — over +10% cap → FAIL.
mkdir -p "$WORK/cases/large-regression/baseline" "$WORK/cases/large-regression/current"
emit_results "$WORK/cases/large-regression/baseline/p02-d1.json" "p02-d1-cold" "p02" "D1" 1000
emit_results "$WORK/cases/large-regression/current/p02-d1.json"  "p02-d1-cold" "p02" "D1" 1150

# Case 5: large regression on D1 + exemption — demoted to WARN.
mkdir -p "$WORK/cases/large-regression-exempted/baseline" "$WORK/cases/large-regression-exempted/current"
emit_results "$WORK/cases/large-regression-exempted/baseline/p02-d1.json" "p02-d1-cold" "p02" "D1" 1000
emit_results "$WORK/cases/large-regression-exempted/current/p02-d1.json"  "p02-d1-cold" "p02" "D1" 1150
write_accepted "$WORK/exempt-p02-d1.md" "p02" "D1"

# Case 6: D2 +12% — under D2's +15% cap → WARN.
mkdir -p "$WORK/cases/d2-warn/baseline" "$WORK/cases/d2-warn/current"
emit_results "$WORK/cases/d2-warn/baseline/p02-d2.json" "p02-d2-warm" "p02" "D2" 1000
emit_results "$WORK/cases/d2-warn/current/p02-d2.json"  "p02-d2-warm" "p02" "D2" 1120

# Case 7: missing dimension → fallback to D1's +10% cap → +12% fails.
mkdir -p "$WORK/cases/missing-dim/baseline" "$WORK/cases/missing-dim/current"
emit_results "$WORK/cases/missing-dim/baseline/p02-nodim.json" "p02-nodim" "p02" "-" 1000
emit_results "$WORK/cases/missing-dim/current/p02-nodim.json"  "p02-nodim" "p02" "-" 1120

# -----------------------------------------------------------------------------
# Run cases.
# -----------------------------------------------------------------------------
fail_count=0
run_case identical                  PASS      0 || fail_count=$((fail_count + 1))
run_case improvement                PASS+WINS 0 || fail_count=$((fail_count + 1))
run_case small-regression           WARN      0 || fail_count=$((fail_count + 1))
run_case large-regression           FAIL      1 || fail_count=$((fail_count + 1))
run_case large-regression-exempted  WARN      0 "$WORK/exempt-p02-d1.md" || fail_count=$((fail_count + 1))
run_case d2-warn                    WARN      0 || fail_count=$((fail_count + 1))
run_case missing-dim                FAIL      1 || fail_count=$((fail_count + 1))

if [ "$fail_count" -gt 0 ]; then
  echo
  echo "perf-gate logic smoke test: $fail_count case(s) failed."
  exit 1
fi

echo
echo "perf-gate logic smoke test: all cases passed."
