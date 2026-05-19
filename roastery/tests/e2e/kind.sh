#!/usr/bin/env bash
# kind.sh — End-to-end reference deployment test for roastery.
#
# What this proves:
#
#   (1) `roastery/Dockerfile` builds end-to-end (linux/amd64).
#   (2) The Helm chart at `roastery/deploy/helm/roastery/` installs
#       cleanly against a real Kubernetes cluster (kind), reaches
#       `Ready` within the timeout, and the chart's own `helm test`
#       hook (templates/tests/test-connection.yaml) probes
#       `/healthz`, `/version`, `/metrics` from in-cluster.
#   (3) A full CAS round-trip succeeds against the chart-rendered
#       Service: PUT a known blob, GET it back, byte-equal.
#   (4) All five always-public endpoints (`/healthz`, `/metrics`,
#       `/version`, `/v1/health`, `/v1/capabilities`) respond 200
#       under the chart-rendered config.
#
# Usage (local):
#
#   bash roastery/tests/e2e/kind.sh
#
# Requires `docker`, `kind`, `helm`, `kubectl`, `curl`, `shasum` on
# PATH. The script never invokes `sudo`.
#
# Env-var knobs (all optional):
#
#   CLUSTER_NAME     kind cluster name. Defaults to `roastery-e2e`.
#                    Set to `chart-testing` (or whatever name your
#                    pre-existing cluster uses) and the script will
#                    reuse it instead of creating a fresh one.
#   IMAGE_TAG        Local image tag. Defaults to `e2e-<short-sha>`.
#   RELEASE_NAME     Helm release name. Defaults to `roastery`.
#   NAMESPACE        Kubernetes namespace. Defaults to `roastery-e2e`.
#   TIMEOUT          Helm/kind wait timeout. Defaults to `5m`.
#   KEEP_CLUSTER     `true` → skip `kind delete cluster` on exit. Useful
#                    when iterating locally; defaults to `false`.
#   SKIP_BUILD       `true` → skip `docker buildx build` (assume the
#                    image `roastery:$IMAGE_TAG` already exists). Useful
#                    when re-running after a successful build.
#
# The CI workflow that wraps this script lives at
# `.github/workflows/e2e-kind.yml`. The workflow installs kind via
# `helm/kind-action` (which creates a cluster named `kind`) and sets
# `CLUSTER_NAME=kind` so this script reuses it instead of provisioning
# a second cluster.
#
# Exits 0 on success. Any failed assertion exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-roastery-e2e}"
IMAGE_TAG="${IMAGE_TAG:-e2e-$(git rev-parse --short HEAD)}"
RELEASE_NAME="${RELEASE_NAME:-roastery}"
NAMESPACE="${NAMESPACE:-roastery-e2e}"
TIMEOUT="${TIMEOUT:-5m}"
KEEP_CLUSTER="${KEEP_CLUSTER:-false}"
SKIP_BUILD="${SKIP_BUILD:-false}"

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "${REPO_ROOT}"

# A scratch dir for fixtures (tokens file, blob, etc.). Cleaned up by
# the EXIT trap below regardless of pass/fail.
WORK_DIR="$(mktemp -d)"
PF_PID=""

fail() {
    echo "::error::$1" >&2
    exit 1
}

cleanup() {
    # Kill the port-forward first so kubectl doesn't leak after the
    # cluster is gone.
    if [[ -n "${PF_PID}" ]]; then
        kill "${PF_PID}" 2>/dev/null || true
        wait "${PF_PID}" 2>/dev/null || true
    fi
    if [[ -d "${WORK_DIR}" ]]; then
        rm -rf "${WORK_DIR}"
    fi
    if [[ "${KEEP_CLUSTER}" != "true" ]] && [[ "${CLUSTER_CREATED:-false}" == "true" ]]; then
        echo "==> Tearing down kind cluster: ${CLUSTER_NAME}"
        kind delete cluster --name "${CLUSTER_NAME}" >/dev/null 2>&1 || true
    fi
}

trap cleanup EXIT

# ---------------------------------------------------------------------
# (0) Pre-flight — every binary we touch is on PATH.
# ---------------------------------------------------------------------
require() {
    command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"
}
require docker
require kind
require helm
require kubectl
require curl
require shasum

echo "==> versions"
docker --version
kind --version
helm version --short
kubectl version --client --output=yaml 2>/dev/null | head -3 || true
echo

# ---------------------------------------------------------------------
# (1) Build the image locally, tagged with the e2e tag.
# ---------------------------------------------------------------------
if [[ "${SKIP_BUILD}" == "true" ]]; then
    echo "==> SKIP_BUILD=true — assuming roastery:${IMAGE_TAG} already exists"
    docker image inspect "roastery:${IMAGE_TAG}" >/dev/null \
        || fail "SKIP_BUILD=true but roastery:${IMAGE_TAG} is not present locally"
else
    echo "==> Building roastery:${IMAGE_TAG}"
    # No `--platform` override: build for the host arch. The kindest/node
    # image in `kind-config.yaml` is a multi-arch index, so the kind node
    # and the loaded roastery image stay arch-matched on amd64 CI runners
    # and on arm64 laptops alike. Override via `DOCKER_DEFAULT_PLATFORM`
    # if you need a cross-arch run.
    docker buildx build \
        --tag "roastery:${IMAGE_TAG}" \
        --file roastery/Dockerfile \
        --load \
        "${REPO_ROOT}" \
        || fail "docker buildx build failed"
fi

# ---------------------------------------------------------------------
# (2) Provision (or reuse) the kind cluster.
# ---------------------------------------------------------------------
CLUSTER_CREATED="false"
if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
    echo "==> Reusing existing kind cluster: ${CLUSTER_NAME}"
else
    echo "==> Creating kind cluster: ${CLUSTER_NAME}"
    kind create cluster \
        --name "${CLUSTER_NAME}" \
        --config roastery/tests/e2e/kind-config.yaml \
        --wait "${TIMEOUT}" \
        || fail "kind create cluster failed"
    CLUSTER_CREATED="true"
fi

# Pin the kubectl context explicitly so a stray `kubectl config
# use-context` in the surrounding shell can't redirect our probes to
# another cluster.
kubectl config use-context "kind-${CLUSTER_NAME}" >/dev/null \
    || fail "kubectl config use-context kind-${CLUSTER_NAME} failed"

# ---------------------------------------------------------------------
# (3) Load the locally-built image into the kind node.
# ---------------------------------------------------------------------
echo "==> Loading roastery:${IMAGE_TAG} into kind"
kind load docker-image "roastery:${IMAGE_TAG}" --name "${CLUSTER_NAME}" \
    || fail "kind load docker-image failed"

# ---------------------------------------------------------------------
# (4) Install the chart.
#
# The chart's default `server.bind` is `0.0.0.0:7878`, which means the
# fail-closed BAR-AUTH-005 check requires an auth mechanism. We enable
# bearer-token auth and seed it with a single test token so the CAS
# round-trip in step (6) can authenticate. The always-public ops
# endpoints (`/healthz`, `/metrics`, `/version`, `/v1/health`,
# `/v1/capabilities`) are reachable without the token regardless.
# ---------------------------------------------------------------------
TOKENS_FILE="${WORK_DIR}/tokens.txt"
# Format: <label>:<secret> per line (see roastery/README.md
# "Authentication"). The label is opaque metadata; only the secret
# after the colon is matched against incoming Authorization headers.
TEST_TOKEN="e2e-$(head -c 32 /dev/urandom | shasum -a 256 | awk '{print $1}' | head -c 32)"
echo "e2e:${TEST_TOKEN}" > "${TOKENS_FILE}"

kubectl create namespace "${NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - \
    || fail "kubectl apply namespace failed"

echo "==> Installing chart (release=${RELEASE_NAME}, namespace=${NAMESPACE})"
helm upgrade --install "${RELEASE_NAME}" roastery/deploy/helm/roastery \
    --namespace "${NAMESPACE}" \
    --set "image.repository=roastery" \
    --set "image.tag=${IMAGE_TAG}" \
    --set "image.pullPolicy=Never" \
    --set "auth.bearer.enabled=true" \
    --set "auth.bearer.create=true" \
    --set-file "auth.bearer.tokens=${TOKENS_FILE}" \
    --wait \
    --timeout "${TIMEOUT}" \
    || {
        echo "--- helm install failed; collecting diagnostics ---" >&2
        kubectl get pods -n "${NAMESPACE}" -o wide >&2 || true
        kubectl describe pods -n "${NAMESPACE}" >&2 || true
        kubectl logs -n "${NAMESPACE}" -l "app.kubernetes.io/name=roastery" --tail=200 >&2 || true
        fail "helm upgrade --install failed"
    }

# ---------------------------------------------------------------------
# (5) helm test — exercises templates/tests/test-connection.yaml.
# ---------------------------------------------------------------------
echo "==> helm test ${RELEASE_NAME}"
helm test "${RELEASE_NAME}" --namespace "${NAMESPACE}" --timeout "${TIMEOUT}" \
    || {
        echo "--- helm test failed; collecting diagnostics ---" >&2
        kubectl logs -n "${NAMESPACE}" \
            "${RELEASE_NAME}-test-connection" --tail=200 >&2 || true
        fail "helm test failed"
    }

# ---------------------------------------------------------------------
# (6) End-to-end CAS round-trip via port-forward.
#
# The chart's own `helm test` hook covers the ops endpoints from
# in-cluster; this step proves the storage path actually works
# end-to-end: PUT a blob, read it back byte-equal. That's the
# milestone-level AC ("`helm install` against kind cluster produces a
# working roastery") in mechanical form.
# ---------------------------------------------------------------------
SVC_PORT=$(kubectl get svc -n "${NAMESPACE}" "${RELEASE_NAME}" \
    -o jsonpath='{.spec.ports[0].port}') \
    || fail "could not read Service port from kubectl"

LOCAL_PORT=17878
echo "==> port-forward svc/${RELEASE_NAME} ${LOCAL_PORT} -> ${SVC_PORT}"
kubectl port-forward -n "${NAMESPACE}" "svc/${RELEASE_NAME}" \
    "${LOCAL_PORT}:${SVC_PORT}" >/dev/null 2>&1 &
PF_PID=$!

# Wait up to 10s for the port-forward to start accepting connections.
HEALTHZ_URL="http://127.0.0.1:${LOCAL_PORT}/healthz"
for attempt in $(seq 1 20); do
    if curl -fsS "${HEALTHZ_URL}" >/dev/null 2>&1; then
        echo "  port-forward up after attempt ${attempt}"
        break
    fi
    if [[ "${attempt}" -eq 20 ]]; then
        fail "/healthz did not respond after 10s via port-forward"
    fi
    sleep 0.5
done

echo "==> CAS round-trip"
BLOB="${WORK_DIR}/blob.bin"
# A non-trivial blob with timestamp + randomness so we never collide
# with a pre-existing entry from a previous re-use of the same kind
# cluster.
printf 'roastery e2e %s %s\n' "$(date -u +%s)" "$RANDOM" > "${BLOB}"
DIGEST="$(shasum -a 256 "${BLOB}" | awk '{print $1}')"
echo "  digest: ${DIGEST}"

# PUT — expect 201 Created. The CAS PUT handler is auth-required, so
# the test token from step (4) is sent on every CAS request.
PUT_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' \
    -X PUT \
    -H "Authorization: Bearer ${TEST_TOKEN}" \
    -H 'Content-Type: application/octet-stream' \
    --data-binary "@${BLOB}" \
    "http://127.0.0.1:${LOCAL_PORT}/v1/cas/sha256/${DIGEST}")" \
    || fail "curl PUT failed"
if [[ "${PUT_STATUS}" != "201" ]]; then
    fail "CAS PUT expected 201, got ${PUT_STATUS}"
fi

# GET — expect 200 + byte-equal body.
GET_BODY_FILE="${WORK_DIR}/blob.get"
GET_STATUS="$(curl -sS -o "${GET_BODY_FILE}" -w '%{http_code}' \
    -H "Authorization: Bearer ${TEST_TOKEN}" \
    "http://127.0.0.1:${LOCAL_PORT}/v1/cas/sha256/${DIGEST}")" \
    || fail "curl GET failed"
if [[ "${GET_STATUS}" != "200" ]]; then
    fail "CAS GET expected 200, got ${GET_STATUS}"
fi
if ! cmp -s "${BLOB}" "${GET_BODY_FILE}"; then
    fail "CAS GET body did not match PUT body"
fi
echo "  PUT + GET byte-equal round-trip: OK"

# ---------------------------------------------------------------------
# (7) Always-public endpoints — every one must return 200 with no
# Authorization header.
# ---------------------------------------------------------------------
echo "==> Probing always-public endpoints"
for ep in /healthz /metrics /version /v1/health /v1/capabilities; do
    code="$(curl -sS -o /dev/null -w '%{http_code}' \
        "http://127.0.0.1:${LOCAL_PORT}${ep}")" \
        || fail "curl ${ep} failed"
    if [[ "${code}" != "200" ]]; then
        fail "${ep} returned ${code}, expected 200"
    fi
    echo "  ${ep}: 200"
done

echo "==> PASS: roastery kind e2e"
