#!/usr/bin/env bash
# scripts/test-parity-check.sh
#
# Meta-test for `crates/barista-test-fixtures/scripts/parity-check.sh`.
#
# Asserts the parity-check harness:
#   1. PASSES on byte-equal `target/` trees (baseline).
#   2. FAILS  on a perturbed `target/` (one-byte-divergence fixture).
#   3. FAILS  on a missing-file fixture (artifact present on one side
#      only).
#   4. IGNORES known-non-reproducible paths from the documented ignore
#      list (surefire-reports, *.log, etc.) — divergences inside those
#      paths do NOT cause a FAIL.
#
# This is the test linkage for the M4.3 T5 acceptance criterion:
#
#     [T] `parity-check.sh` exits non-zero on a seeded byte-divergence
#         fixture and zero on the matching baseline.
#
# The meta-test runs without `mvn` or `java` on PATH; it exercises the
# harness's `--compare-only` mode directly. End-to-end parity (full
# corpus build + diff) is opt-in for a nightly job once v0.2 wires the
# daemon path — see `crates/barista-test-fixtures/scripts/parity-check.README.md`.
#
# Run locally:           bash scripts/test-parity-check.sh
# Wired into CI by:      .github/workflows/workflow-lint.yml
#                        (security-agent-config job)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARNESS="$REPO_ROOT/crates/barista-test-fixtures/scripts/parity-check.sh"

if [[ ! -x "$HARNESS" ]]; then
  echo "error: $HARNESS not executable" >&2
  exit 2
fi

WORK="$(mktemp -d -t test-parity-check.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

FAILED=0

# Helper: report a case outcome and update the failure counter.
report() {
  local name="$1" expected="$2" got="$3"
  if [[ "$expected" == "$got" ]]; then
    echo "[PASS] ${name} (exit ${got})"
  else
    echo "[FAIL] ${name}: expected exit ${expected}, got ${got}" >&2
    FAILED=$((FAILED + 1))
  fi
}

# Build a synthetic 1-file target tree. The content is deterministic
# so two independently-built trees are byte-equal.
build_target() {
  local dest="$1" jar_content="$2"
  mkdir -p "$dest/classes/example"
  # `.class` stand-in: a fixed byte sequence we treat as opaque
  # bytecode. The harness hashes every regular file; it doesn't care
  # whether the bytes parse as a real classfile.
  printf 'CAFEBABE-baseline-class-bytes\n' > "$dest/classes/example/Hello.class"
  printf '%s' "$jar_content" > "$dest/parity-baseline-0.1.0.jar"
}

# ---------------------------------------------------------------------
# Case 1: byte-equal trees → exit 0 (PASS).
# ---------------------------------------------------------------------
CASE1_MVN="$WORK/case1/mvn-target"
CASE1_BAR="$WORK/case1/barista-target"
build_target "$CASE1_MVN" "the-quick-brown-fox-jumps-over-the-lazy-dog"
build_target "$CASE1_BAR" "the-quick-brown-fox-jumps-over-the-lazy-dog"

set +e
"$HARNESS" --compare-only "$CASE1_MVN" "$CASE1_BAR" > "$WORK/case1.out" 2>&1
rc=$?
set -e
report "case1: byte-equal trees PASS" 0 "$rc"
if [[ "$rc" -ne 0 ]]; then
  echo "---- case1 output ----" >&2
  cat "$WORK/case1.out" >&2
  echo "---- end case1 output ----" >&2
fi

# ---------------------------------------------------------------------
# Case 2: one-byte divergence in the JAR → exit 3 (FAIL).
# This is the "seeded byte-divergence fixture" called for by the AC.
# ---------------------------------------------------------------------
CASE2_MVN="$WORK/case2/mvn-target"
CASE2_BAR="$WORK/case2/barista-target"
build_target "$CASE2_MVN" "the-quick-brown-fox-jumps-over-the-lazy-dog"
# Same shape, one byte flipped on the barista side (lazy -> lXzy).
build_target "$CASE2_BAR" "the-quick-brown-fox-jumps-over-the-lXzy-dog"

set +e
"$HARNESS" --compare-only "$CASE2_MVN" "$CASE2_BAR" > "$WORK/case2.out" 2>&1
rc=$?
set -e
report "case2: 1-byte JAR divergence FAIL" 3 "$rc"

# Cross-check the output mentions the diverging file. This catches
# regressions where the harness exits non-zero for the wrong reason
# (e.g. usage error misclassified as divergence).
if ! grep -q "hash mismatch on parity-baseline-0.1.0.jar" "$WORK/case2.out"; then
  echo "[FAIL] case2: harness output did not flag the JAR divergence" >&2
  echo "---- case2 output ----" >&2
  cat "$WORK/case2.out" >&2
  echo "---- end case2 output ----" >&2
  FAILED=$((FAILED + 1))
else
  echo "[PASS] case2: output flagged the JAR file by name"
fi

# ---------------------------------------------------------------------
# Case 3: file present on one side only → exit 3 (FAIL), output names
# the missing-on-X side.
# ---------------------------------------------------------------------
CASE3_MVN="$WORK/case3/mvn-target"
CASE3_BAR="$WORK/case3/barista-target"
build_target "$CASE3_MVN" "the-quick-brown-fox-jumps-over-the-lazy-dog"
build_target "$CASE3_BAR" "the-quick-brown-fox-jumps-over-the-lazy-dog"
# Add an extra file only on the barista side: simulates a stray
# artifact that the reference build didn't produce.
printf 'rogue-artifact\n' > "$CASE3_BAR/stray.jar"

set +e
"$HARNESS" --compare-only "$CASE3_MVN" "$CASE3_BAR" > "$WORK/case3.out" 2>&1
rc=$?
set -e
report "case3: missing-on-mvn-side artifact FAIL" 3 "$rc"
if ! grep -q "missing on mvn side: *stray.jar" "$WORK/case3.out"; then
  echo "[FAIL] case3: harness output did not flag the missing stray.jar" >&2
  echo "---- case3 output ----" >&2
  cat "$WORK/case3.out" >&2
  echo "---- end case3 output ----" >&2
  FAILED=$((FAILED + 1))
else
  echo "[PASS] case3: output flagged the missing-side artifact"
fi

# ---------------------------------------------------------------------
# Case 4: divergence inside ignore-list paths → exit 0 (PASS).
# Asserts the ignore globs ARE honored — divergences in surefire
# reports, *.log, *.tmp, maven-status/ must not cause a FAIL.
# ---------------------------------------------------------------------
CASE4_MVN="$WORK/case4/mvn-target"
CASE4_BAR="$WORK/case4/barista-target"
build_target "$CASE4_MVN" "the-quick-brown-fox-jumps-over-the-lazy-dog"
build_target "$CASE4_BAR" "the-quick-brown-fox-jumps-over-the-lazy-dog"

# Each of these paths is in IGNORE_GLOBS; the harness should treat
# divergences here as expected and not flag them.
mkdir -p "$CASE4_MVN/surefire-reports" "$CASE4_BAR/surefire-reports"
printf 'mvn-time-2026-05-17\n'  > "$CASE4_MVN/surefire-reports/TEST-Foo.xml"
printf 'mvn-time-2026-05-18\n'  > "$CASE4_BAR/surefire-reports/TEST-Foo.xml"

mkdir -p "$CASE4_MVN/maven-status" "$CASE4_BAR/maven-status"
printf 'staging-A\n' > "$CASE4_MVN/maven-status/compile.txt"
printf 'staging-B\n' > "$CASE4_BAR/maven-status/compile.txt"

printf 'old-log\n' > "$CASE4_MVN/diagnostic.log"
printf 'new-log\n' > "$CASE4_BAR/diagnostic.log"

set +e
"$HARNESS" --compare-only "$CASE4_MVN" "$CASE4_BAR" > "$WORK/case4.out" 2>&1
rc=$?
set -e
report "case4: ignore-list divergences PASS" 0 "$rc"
if [[ "$rc" -ne 0 ]]; then
  echo "---- case4 output ----" >&2
  cat "$WORK/case4.out" >&2
  echo "---- end case4 output ----" >&2
fi

# ---------------------------------------------------------------------
# Case 5: usage error → exit 1.
# Asserts wrong-flag exits with the documented usage-error code, not
# silently with 0 or with the divergence code.
# ---------------------------------------------------------------------
set +e
"$HARNESS" --bogus-flag > "$WORK/case5.out" 2>&1
rc=$?
set -e
report "case5: unknown-flag usage error" 1 "$rc"

# ---------------------------------------------------------------------
# Case 6: missing compare-only target dir → exit 2 (env error).
# ---------------------------------------------------------------------
set +e
"$HARNESS" --compare-only "$WORK/does-not-exist" "$CASE1_BAR" \
  > "$WORK/case6.out" 2>&1
rc=$?
set -e
report "case6: missing --compare-only dir env error" 2 "$rc"

# ---------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------
echo
if [[ "$FAILED" -eq 0 ]]; then
  echo "test-parity-check.sh: all cases passed"
  exit 0
else
  echo "test-parity-check.sh: ${FAILED} case(s) failed" >&2
  exit 1
fi
