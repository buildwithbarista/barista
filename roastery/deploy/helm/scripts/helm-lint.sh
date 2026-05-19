#!/usr/bin/env bash
# Lint + schema-validate + render the roastery Helm chart.
#
# Usage:
#   bash roastery/deploy/helm/scripts/helm-lint.sh
#
# Checks:
#
#   (1) `helm lint` against the chart with the default values. Catches
#       template syntax errors, missing required values per
#       values.schema.json, and bad YAML.
#
#   (2) `helm lint` again with each fixture under
#       roastery/deploy/helm/fixtures/. Catches fixtures that have
#       drifted past values.schema.json.
#
#   (3) `helm template` for the default values. Surfaces runtime
#       template errors (uses of values absent from values.yaml, bad
#       `index`/`include` calls) that `helm lint` doesn't always catch.
#
#   (4) Optional `kubeconform -strict` over the rendered output if
#       kubeconform is installed (skipped with a warning otherwise so
#       contributors without the binary can still run the script).
#
# Exits 0 on success. Any failed check exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
CHART_DIR="${REPO_ROOT}/roastery/deploy/helm/roastery"
FIXTURE_DIR="${REPO_ROOT}/roastery/deploy/helm/fixtures"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -d "${CHART_DIR}" ]]; then
    fail "${CHART_DIR} does not exist; the M5.1 T9 Helm chart is missing."
fi

if ! command -v helm >/dev/null 2>&1; then
    echo "::warning::helm not on PATH; skipping all checks (install with: brew install helm)" >&2
    exit 0
fi

echo "=== helm version ==="
helm version --short

# ---------------------------------------------------------------------
# (1) helm lint — default values
# ---------------------------------------------------------------------
echo "=== (1) helm lint (default values) ==="
helm lint "${CHART_DIR}" \
    || fail "helm lint failed against default values"

# ---------------------------------------------------------------------
# (2) helm lint — each fixture
# ---------------------------------------------------------------------
echo "=== (2) helm lint (each fixture) ==="
for fixture in "${FIXTURE_DIR}"/*.values.yaml; do
    [[ -f "${fixture}" ]] || continue
    name=$(basename "${fixture}" .values.yaml)
    echo "  -- fixture: ${name}"
    helm lint "${CHART_DIR}" --values "${fixture}" \
        || fail "helm lint failed for fixture ${name}"
done

# ---------------------------------------------------------------------
# (3) helm template — default + fixtures
# ---------------------------------------------------------------------
echo "=== (3) helm template (default values) ==="
RENDER_DIR="$(mktemp -d)"
trap 'rm -rf "${RENDER_DIR}"' EXIT

helm template roastery "${CHART_DIR}" \
    --namespace roastery \
    --values "${FIXTURE_DIR}/default.values.yaml" \
    > "${RENDER_DIR}/default.yaml" \
    || fail "helm template failed for default values"
echo "  rendered $(wc -l < "${RENDER_DIR}/default.yaml") lines"

for fixture in "${FIXTURE_DIR}"/*.values.yaml; do
    [[ -f "${fixture}" ]] || continue
    name=$(basename "${fixture}" .values.yaml)
    [[ "${name}" == "default" ]] && continue
    echo "  -- template fixture: ${name}"
    helm template roastery "${CHART_DIR}" \
        --namespace roastery \
        --values "${fixture}" \
        > "${RENDER_DIR}/${name}.yaml" \
        || fail "helm template failed for fixture ${name}"
done

# ---------------------------------------------------------------------
# (4) kubeconform (optional)
# ---------------------------------------------------------------------
if command -v kubeconform >/dev/null 2>&1; then
    echo "=== (4) kubeconform -strict (rendered output) ==="
    for rendered in "${RENDER_DIR}"/*.yaml; do
        echo "  -- kubeconform: $(basename "${rendered}")"
        # `-strict` rejects unknown fields. `-ignore-missing-schemas`
        # waves through CRDs (ServiceMonitor, etc.) we don't ship
        # schemas for.
        kubeconform -strict -ignore-missing-schemas "${rendered}" \
            || fail "kubeconform reported violations in ${rendered}"
    done
else
    echo "::warning::kubeconform not on PATH; skipping rendered-output validation"
    echo "  install with: brew install kubeconform"
fi

echo "=== PASS: helm chart lint clean ==="
