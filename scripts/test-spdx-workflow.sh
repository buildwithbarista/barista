#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Validate the SPDX-header CI gate + its scripts are well-formed and free
# of the security anti-patterns zizmor catches.
#
# Usage:
#   bash scripts/test-spdx-workflow.sh
#
# Checks:
#
#   (1) `actionlint .github/workflows/ci.yml` exits 0. The SPDX gate is a
#       job (`spdx-headers`) inside the main CI workflow, so we lint that
#       file. Catches workflow-syntax mistakes, expression typos, bad
#       `runs-on` strings, and shell-script violations in inline `run:`
#       blocks.
#
#   (2) `zizmor --offline --min-severity=medium .github/workflows/ci.yml`
#       exits 0 with no medium-or-higher findings. The full `Workflow
#       lint` job runs the same zizmor invocation across every workflow;
#       this script is the focused, fast, locally-runnable check for the
#       CI workflow that hosts the SPDX gate.
#
#   (3) `shellcheck` on the SPDX scripts — the load-bearing surface:
#       `scripts/check-spdx-headers.sh` (single source of truth for the
#       first-party include/exclude globs + the stamper), `scripts/
#       test-spdx.sh` (the [T] positive/negative self-test), and this
#       validator itself (so it can't rot).
#
# This script is wired into `.github/workflows/workflow-lint.yml`'s
# `security-agent-config` job alongside the other `test-*.sh` validators
# (DCO, PR template, perf-gate, parity-check, container, helm-chart, e2e,
# release, SBOM).
#
# Exits 0 on success. Any failed check exits non-zero with a diagnostic
# to stderr.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
WORKFLOW="${REPO_ROOT}/.github/workflows/ci.yml"
CHECK_SCRIPT="${REPO_ROOT}/scripts/check-spdx-headers.sh"
TEST_SCRIPT="${REPO_ROOT}/scripts/test-spdx.sh"
SELF="${REPO_ROOT}/scripts/test-spdx-workflow.sh"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -f "${WORKFLOW}" ]]; then
    fail "${WORKFLOW} does not exist; the CI workflow is missing."
fi
if [[ ! -f "${CHECK_SCRIPT}" ]]; then
    fail "${CHECK_SCRIPT} does not exist; the SPDX checker is missing."
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
# (3) shellcheck on the SPDX scripts + this validator
# ---------------------------------------------------------------------
if ! command -v shellcheck >/dev/null 2>&1; then
    echo "::warning::shellcheck not on PATH; skipping (CI will run it)"
else
    echo "=== (3a) shellcheck ${CHECK_SCRIPT} ==="
    shellcheck "${CHECK_SCRIPT}" \
        || fail "shellcheck reported violations in ${CHECK_SCRIPT}"
    echo "=== (3b) shellcheck ${TEST_SCRIPT} ==="
    shellcheck "${TEST_SCRIPT}" \
        || fail "shellcheck reported violations in ${TEST_SCRIPT}"
    echo "=== (3c) shellcheck ${SELF} ==="
    shellcheck "${SELF}" \
        || fail "shellcheck reported violations in ${SELF}"
fi

echo "=== PASS: ${WORKFLOW} (spdx-headers job) + SPDX scripts pass all checks ==="
