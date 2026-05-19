#!/usr/bin/env bash
# Validate the roastery Helm chart + the workflow that lints it.
#
# Usage:
#   bash scripts/test-helm-chart.sh
#
# Combines five focused checks into one entry point so the
# `security-agent-config` job in workflow-lint.yml can call it
# alongside the other `test-*.sh` validators:
#
#   (1) `helm lint` against the chart with default values + each
#       fixture under roastery/deploy/helm/fixtures/. Catches template
#       syntax errors and schema violations.
#
#   (2) Golden-fixture diff. `roastery/deploy/helm/scripts/helm-render-fixtures.sh check`
#       renders every fixture and fails on any drift from the
#       committed *.golden.yaml files. The golden files are the
#       chart's regression net — a template change that perturbs a
#       label, env var, or volume mount is visible in code review as
#       a diff against the goldens.
#
#   (3) `kubeconform -strict` against the rendered output, when the
#       binary is on PATH (skipped with a warning otherwise so
#       contributors without it can still run this script).
#
#   (4) `actionlint .github/workflows/helm.yml` — workflow syntax +
#       shell-script linting on the chart's own CI workflow.
#
#   (5) `zizmor --offline --min-severity=medium .github/workflows/helm.yml`
#       — security analysis of the workflow.
#
# This script is wired into `.github/workflows/workflow-lint.yml`'s
# `security-agent-config` job alongside the other `test-*.sh`
# validators (DCO, PR template, perf-gate, parity-check, container).
#
# Exits 0 on success. Any failed check exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
WORKFLOW="${REPO_ROOT}/.github/workflows/helm.yml"
CHART_DIR="${REPO_ROOT}/roastery/deploy/helm/roastery"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -d "${CHART_DIR}" ]]; then
    fail "${CHART_DIR} does not exist; the M5.1 T9 Helm chart is missing."
fi
if [[ ! -f "${WORKFLOW}" ]]; then
    fail "${WORKFLOW} does not exist; the M5.1 T9 helm workflow is missing."
fi

# ---------------------------------------------------------------------
# (1) + (2) + (3): helm lint + golden diff + kubeconform
# ---------------------------------------------------------------------
echo "=== (1) helm lint + (2) golden diff + (3) kubeconform ==="
bash "${REPO_ROOT}/roastery/deploy/helm/scripts/helm-lint.sh"
bash "${REPO_ROOT}/roastery/deploy/helm/scripts/helm-render-fixtures.sh" check

# ---------------------------------------------------------------------
# (4) actionlint
# ---------------------------------------------------------------------
if ! command -v actionlint >/dev/null 2>&1; then
    echo "::warning::actionlint not on PATH; skipping syntax check (CI will run it)"
else
    echo "=== (4) actionlint ${WORKFLOW} ==="
    actionlint "${WORKFLOW}" \
        || fail "actionlint reported violations in ${WORKFLOW}"
fi

# ---------------------------------------------------------------------
# (5) zizmor
# ---------------------------------------------------------------------
if ! command -v zizmor >/dev/null 2>&1; then
    echo "::warning::zizmor not on PATH; skipping security scan (CI will run it)"
else
    echo "=== (5) zizmor ${WORKFLOW} ==="
    zizmor \
        --offline \
        --min-severity=medium \
        "${WORKFLOW}" \
        || fail "zizmor reported medium-or-higher findings in ${WORKFLOW}"
fi

echo "=== PASS: helm chart + workflow pass all checks ==="
