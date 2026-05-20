#!/usr/bin/env bash
# Validate the cross-platform reproducible-build release workflow file
# and its determinism helper script are well-formed and free of the
# security anti-patterns zizmor catches.
#
# Usage:
#   bash scripts/test-release-workflow.sh
#
# Checks:
#
#   (1) `actionlint .github/workflows/release.yml` exits 0.
#       Catches workflow-syntax mistakes, expression typos, bad
#       `runs-on` strings, and shell-script violations in inline
#       `run:` blocks.
#
#   (2) `zizmor --offline --min-severity=medium .github/workflows/release.yml`
#       exits 0 with no medium-or-higher findings. The full
#       `Workflow lint` job runs the same zizmor invocation against
#       every workflow; this script provides a focused, fast,
#       locally-runnable check for the release pipeline.
#
#   (3) `shellcheck scripts/build-release.sh` exits 0. The release
#       pipeline's determinism logic lives entirely in that script
#       (the workflow matrix just invokes it once per target), so it
#       is the load-bearing surface: shellcheck catches quoting /
#       globbing / set-e bugs that would otherwise only surface
#       intermittently on a release runner.
#
#   (4) `shellcheck` on this script itself, so the validator can't rot.
#
# This script is wired into `.github/workflows/workflow-lint.yml`'s
# `security-agent-config` job alongside the other `test-*.sh`
# validators (DCO, PR template, perf-gate, parity-check, container,
# helm-chart, e2e).
#
# Exits 0 on success. Any failed check exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
WORKFLOW="${REPO_ROOT}/.github/workflows/release.yml"
BUILD_SCRIPT="${REPO_ROOT}/scripts/build-release.sh"
MAVEN_LIB="${REPO_ROOT}/scripts/lib/maven-bundle.sh"
MAVEN_TEST="${REPO_ROOT}/scripts/test-maven-bundle.sh"
SELF="${REPO_ROOT}/scripts/test-release-workflow.sh"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -f "${WORKFLOW}" ]]; then
    fail "${WORKFLOW} does not exist; the release workflow is missing."
fi
if [[ ! -f "${BUILD_SCRIPT}" ]]; then
    fail "${BUILD_SCRIPT} does not exist; the determinism helper is missing."
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
# (3) + (4) shellcheck on the determinism helper and this validator
# ---------------------------------------------------------------------
if ! command -v shellcheck >/dev/null 2>&1; then
    echo "::warning::shellcheck not on PATH; skipping (CI will run it)"
else
    # `-x` follows `# shellcheck source=` directives so the sourced
    # `scripts/lib/maven-bundle.sh` is analyzed in context (otherwise
    # build-release.sh's `. lib/maven-bundle.sh` trips SC1091).
    echo "=== (3) shellcheck ${BUILD_SCRIPT} ==="
    shellcheck -x "${BUILD_SCRIPT}" \
        || fail "shellcheck reported violations in ${BUILD_SCRIPT}"
    echo "=== (3b) shellcheck ${MAVEN_LIB} ==="
    shellcheck "${MAVEN_LIB}" \
        || fail "shellcheck reported violations in ${MAVEN_LIB}"
    echo "=== (3c) shellcheck ${MAVEN_TEST} ==="
    shellcheck -x "${MAVEN_TEST}" \
        || fail "shellcheck reported violations in ${MAVEN_TEST}"
    echo "=== (4) shellcheck ${SELF} ==="
    shellcheck "${SELF}" \
        || fail "shellcheck reported violations in ${SELF}"
fi

# ---------------------------------------------------------------------
# (5) The Maven-bundle hermetic unit test (sha-verify accept/reject +
#     strip-component extraction + SKIP placeholder).
# ---------------------------------------------------------------------
echo "=== (5) ${MAVEN_TEST} ==="
bash "${MAVEN_TEST}" \
    || fail "Maven-bundle unit test failed"

echo "=== PASS: ${WORKFLOW} + ${BUILD_SCRIPT} pass all checks ==="
