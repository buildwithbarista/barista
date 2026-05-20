#!/usr/bin/env bash
# Validate the SBOM workflow file + its scripts are well-formed and free
# of the security anti-patterns zizmor catches.
#
# Usage:
#   bash scripts/test-sbom-workflow.sh
#
# Checks:
#
#   (1) `actionlint .github/workflows/sbom.yml` exits 0.
#       Catches workflow-syntax mistakes, expression typos, bad
#       `runs-on` strings, and shell-script violations in inline
#       `run:` blocks.
#
#   (2) `zizmor --offline --min-severity=medium .github/workflows/sbom.yml`
#       exits 0 with no medium-or-higher findings. The full
#       `Workflow lint` job runs the same zizmor invocation against
#       every workflow; this script provides a focused, fast,
#       locally-runnable check for the SBOM pipeline.
#
#   (3) `shellcheck` on the SBOM scripts — the load-bearing surface:
#       `scripts/generate-sbom.sh` (single source of truth for SBOM
#       generation + the external-tool validation gate),
#       `scripts/test-sbom.sh` (the [T] self-test wrapper), and this
#       validator itself (so it can't rot).
#
# This script is wired into `.github/workflows/workflow-lint.yml`'s
# `security-agent-config` job alongside the other `test-*.sh`
# validators (DCO, PR template, perf-gate, parity-check, container,
# helm-chart, e2e, release).
#
# Exits 0 on success. Any failed check exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
WORKFLOW="${REPO_ROOT}/.github/workflows/sbom.yml"
GENERATE_SCRIPT="${REPO_ROOT}/scripts/generate-sbom.sh"
TEST_SCRIPT="${REPO_ROOT}/scripts/test-sbom.sh"
SELF="${REPO_ROOT}/scripts/test-sbom-workflow.sh"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -f "${WORKFLOW}" ]]; then
    fail "${WORKFLOW} does not exist; the SBOM workflow is missing."
fi
if [[ ! -f "${GENERATE_SCRIPT}" ]]; then
    fail "${GENERATE_SCRIPT} does not exist; the SBOM generator is missing."
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
# (3) shellcheck on the SBOM scripts + this validator
# ---------------------------------------------------------------------
if ! command -v shellcheck >/dev/null 2>&1; then
    echo "::warning::shellcheck not on PATH; skipping (CI will run it)"
else
    echo "=== (3a) shellcheck ${GENERATE_SCRIPT} ==="
    shellcheck "${GENERATE_SCRIPT}" \
        || fail "shellcheck reported violations in ${GENERATE_SCRIPT}"
    echo "=== (3b) shellcheck ${TEST_SCRIPT} ==="
    shellcheck "${TEST_SCRIPT}" \
        || fail "shellcheck reported violations in ${TEST_SCRIPT}"
    echo "=== (3c) shellcheck ${SELF} ==="
    shellcheck "${SELF}" \
        || fail "shellcheck reported violations in ${SELF}"
fi

echo "=== PASS: ${WORKFLOW} + SBOM scripts pass all checks ==="
