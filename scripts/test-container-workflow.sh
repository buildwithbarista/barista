#!/usr/bin/env bash
# Validate the roastery container-build workflow file is well-formed
# and free of the security anti-patterns zizmor catches.
#
# Usage:
#   bash scripts/test-container-workflow.sh
#
# Checks:
#
#   (1) `actionlint .github/workflows/container-roastery.yml` exits 0.
#       Catches workflow-syntax mistakes, expression typos, and
#       shell-script violations in inline `run:` blocks.
#
#   (2) `zizmor --offline --persona=default .github/workflows/container-roastery.yml`
#       exits 0 with no medium-or-higher findings. The full
#       `Workflow lint` job runs the same zizmor invocation against
#       every workflow; this script provides a focused, fast,
#       locally-runnable check for the file added by M5.1 T8.
#
# This script is wired into `.github/workflows/workflow-lint.yml`'s
# `security-agent-config` job alongside the other `test-*.sh`
# validators (DCO, PR template, perf-gate, parity-check).
#
# Exits 0 on success. Any failed check exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

WORKFLOW=".github/workflows/container-roastery.yml"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -f "${WORKFLOW}" ]]; then
    fail "${WORKFLOW} does not exist; the M5.1 T8 container-build workflow is missing."
fi

# ---------------------------------------------------------------------
# (1) actionlint
# ---------------------------------------------------------------------
if ! command -v actionlint >/dev/null 2>&1; then
    echo "::warning::actionlint not on PATH; skipping syntax check (CI will run it)"
else
    echo "=== actionlint ${WORKFLOW} ==="
    actionlint "${WORKFLOW}" \
        || fail "actionlint reported violations in ${WORKFLOW}"
fi

# ---------------------------------------------------------------------
# (2) zizmor
# ---------------------------------------------------------------------
if ! command -v zizmor >/dev/null 2>&1; then
    echo "::warning::zizmor not on PATH; skipping security scan (CI will run it)"
else
    echo "=== zizmor ${WORKFLOW} ==="
    # `--offline` skips network-backed audits so the test is hermetic
    # and fast. `--min-severity=medium` matches the gate the
    # `workflow-lint.yml` job uses for production scans.
    zizmor \
        --offline \
        --min-severity=medium \
        "${WORKFLOW}" \
        || fail "zizmor reported medium-or-higher findings in ${WORKFLOW}"
fi

echo "=== PASS: ${WORKFLOW} passes actionlint + zizmor ==="
