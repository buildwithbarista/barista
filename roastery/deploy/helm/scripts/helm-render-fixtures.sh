#!/usr/bin/env bash
# Render each fixture under roastery/deploy/helm/fixtures/ and diff
# the output against the committed *.golden.yaml file.
#
# Usage:
#   bash roastery/deploy/helm/scripts/helm-render-fixtures.sh
#       check   (default — fail on any diff)
#       update  (rewrite golden files in-place from the current render)
#
# Why golden files: the rendered YAML is the chart's contract with
# operators. A template change that perturbs a label, env-var name,
# or volume mount is something a reviewer should see explicitly. The
# golden diff makes the change visible in code review.
#
# The render is reproducible — the chart never reads --set values
# that vary across runs (no Helm template randomness, no
# `now`/`uuidv4` calls). If a renderer-internal API changes between
# helm versions the goldens may shift; bump the pinned helm version
# in CI alongside the regenerated goldens.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
CHART_DIR="${REPO_ROOT}/roastery/deploy/helm/roastery"
FIXTURE_DIR="${REPO_ROOT}/roastery/deploy/helm/fixtures"

MODE="${1:-check}"
case "${MODE}" in
    check|update) ;;
    *)
        echo "::error::unknown mode '${MODE}' (expected: check, update)" >&2
        exit 2
        ;;
esac

if ! command -v helm >/dev/null 2>&1; then
    echo "::warning::helm not on PATH; skipping fixture render"
    exit 0
fi

fail() {
    echo "::error::$1" >&2
    exit 1
}

# Render with a fixed release name + namespace so the golden output
# is stable across local + CI runs.
RELEASE_NAME="roastery"
NAMESPACE="roastery"

DIFF_COUNT=0

for fixture in "${FIXTURE_DIR}"/*.values.yaml; do
    [[ -f "${fixture}" ]] || continue
    name=$(basename "${fixture}" .values.yaml)
    golden="${FIXTURE_DIR}/${name}.golden.yaml"

    rendered="$(mktemp)"
    # `--no-hooks` skips the test-connection Pod so the render stays
    # focused on the deploy-time manifest set. The Helm test hook is
    # exercised separately by `helm test` on a live cluster.
    helm template "${RELEASE_NAME}" "${CHART_DIR}" \
        --namespace "${NAMESPACE}" \
        --values "${fixture}" \
        --no-hooks \
        > "${rendered}" \
        || {
            rm -f "${rendered}"
            fail "helm template failed for fixture ${name}"
        }

    if [[ "${MODE}" == "update" ]]; then
        mv "${rendered}" "${golden}"
        echo "  updated ${golden}"
        continue
    fi

    if [[ ! -f "${golden}" ]]; then
        rm -f "${rendered}"
        fail "golden file missing: ${golden} (run with 'update' to create it)"
    fi

    if ! diff -u "${golden}" "${rendered}" > /dev/null; then
        echo "::error::golden drift for fixture '${name}':"
        diff -u "${golden}" "${rendered}" || true
        DIFF_COUNT=$((DIFF_COUNT + 1))
    else
        echo "  ok: ${name}"
    fi
    rm -f "${rendered}"
done

if [[ "${DIFF_COUNT}" -gt 0 ]]; then
    fail "${DIFF_COUNT} fixture(s) drifted from their golden output. Inspect the diff above; re-run with 'update' if the change is intentional."
fi

echo "=== PASS: all fixtures match their golden output ==="
