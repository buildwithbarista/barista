#!/usr/bin/env bash
# Validate the roastery kind-e2e workflow file + its supporting shell
# scripts are well-formed and free of the security anti-patterns
# zizmor catches.
#
# Usage:
#   bash scripts/test-e2e-workflow.sh
#
# Checks:
#
#   (1) `actionlint .github/workflows/e2e-kind.yml` exits 0.
#       Catches workflow-syntax mistakes, expression typos, and
#       shell-script violations in inline `run:` blocks.
#
#   (2) `zizmor --offline --min-severity=medium .github/workflows/e2e-kind.yml`
#       exits 0 with no medium-or-higher findings. The full
#       `Workflow lint` job runs the same zizmor invocation against
#       every workflow; this script provides a focused, fast,
#       locally-runnable check for the file added by M5.1 T10.
#
#   (3) `shellcheck roastery/tests/e2e/kind.sh` exits 0. The script
#       is the e2e's load-bearing surface; shellcheck catches
#       quoting / globbing / set-e bugs that would otherwise only
#       surface intermittently on the runner.
#
# This script is wired into `.github/workflows/workflow-lint.yml`'s
# `security-agent-config` job alongside the other `test-*.sh`
# validators (DCO, PR template, perf-gate, parity-check, container,
# helm-chart).
#
# Exits 0 on success. Any failed check exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
WORKFLOW="${REPO_ROOT}/.github/workflows/e2e-kind.yml"
SCRIPT="${REPO_ROOT}/roastery/tests/e2e/kind.sh"
KIND_CONFIG="${REPO_ROOT}/roastery/tests/e2e/kind-config.yaml"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -f "${WORKFLOW}" ]]; then
    fail "${WORKFLOW} does not exist; the M5.1 T10 kind-e2e workflow is missing."
fi
if [[ ! -f "${SCRIPT}" ]]; then
    fail "${SCRIPT} does not exist; the M5.1 T10 e2e harness script is missing."
fi
if [[ ! -f "${KIND_CONFIG}" ]]; then
    fail "${KIND_CONFIG} does not exist; the M5.1 T10 kind cluster config is missing."
fi

# ---------------------------------------------------------------------
# (1) actionlint
# ---------------------------------------------------------------------
if ! command -v actionlint >/dev/null 2>&1; then
    echo "::warning::actionlint not on PATH; skipping syntax check (CI will run it)"
else
    echo "=== (1) actionlint ${WORKFLOW} ==="
    actionlint "${WORKFLOW}" \
        || fail "actionlint reported violations in ${WORKFLOW}"
fi

# ---------------------------------------------------------------------
# (2) zizmor
# ---------------------------------------------------------------------
if ! command -v zizmor >/dev/null 2>&1; then
    echo "::warning::zizmor not on PATH; skipping security scan (CI will run it)"
else
    echo "=== (2) zizmor ${WORKFLOW} ==="
    zizmor \
        --offline \
        --min-severity=medium \
        "${WORKFLOW}" \
        || fail "zizmor reported medium-or-higher findings in ${WORKFLOW}"
fi

# ---------------------------------------------------------------------
# (3) shellcheck on the e2e script
# ---------------------------------------------------------------------
if ! command -v shellcheck >/dev/null 2>&1; then
    echo "::warning::shellcheck not on PATH; skipping (CI will run it)"
else
    echo "=== (3) shellcheck ${SCRIPT} ==="
    shellcheck "${SCRIPT}" \
        || fail "shellcheck reported violations in ${SCRIPT}"
fi

echo "=== PASS: ${WORKFLOW} + ${SCRIPT} pass all checks ==="
