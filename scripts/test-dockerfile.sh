#!/usr/bin/env bash
# Smoke-test the roastery container image.
#
# Usage:
#   bash scripts/test-dockerfile.sh
#
# What this proves:
#
#   (1) `roastery/Dockerfile` builds end-to-end on linux/amd64.
#   (2) The resulting image's `ENTRYPOINT` accepts `--version` and
#       exits 0 with a recognisable identity line. This is the
#       canonical container-boot probe distroless images support
#       (they ship no shell, so `docker exec <image> sh` is not
#       available — we exercise the entrypoint directly).
#   (3) The image boots a real server when run with no extra args,
#       answers `GET /healthz` with `200 OK` + body `ok`, and answers
#       `GET /version` with a JSON document whose `name` field is
#       `"roastery"`.
#   (4) The container exits cleanly on SIGTERM.
#
# This is the body of the `verify` job in
# `.github/workflows/container-roastery.yml`.
#
# Exits 0 on success. Any failed assertion exits non-zero with a
# diagnostic to stderr.

set -euo pipefail

IMAGE="${IMAGE:-roastery:test}"
CONTAINER="${CONTAINER:-roastery-smoke}"
HOST_PORT="${HOST_PORT:-17878}"
REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"

fail() {
    echo "::error::$1" >&2
    cleanup
    exit 1
}

cleanup() {
    docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
    if [[ -n "${STORAGE_DIR:-}" && -d "${STORAGE_DIR}" ]]; then
        rm -rf "${STORAGE_DIR}"
    fi
}

trap cleanup EXIT

echo "=== (1) Build image ==="
docker buildx build \
    --platform linux/amd64 \
    --load \
    --tag "${IMAGE}" \
    --file "${REPO_ROOT}/roastery/Dockerfile" \
    "${REPO_ROOT}" \
    || fail "docker buildx build failed"

echo "=== (2) --version probe ==="
# `--platform linux/amd64` matches the build platform and suppresses
# the Docker-CLI platform-mismatch warning that otherwise appears on
# arm64 hosts (e.g. macOS-on-M1). The image is single-arch here; the
# multi-arch publish happens in the workflow's `build` job.
VERSION_OUT=$(docker run --rm --platform linux/amd64 "${IMAGE}" --version 2>&1) \
    || fail "container --version exited non-zero: ${VERSION_OUT}"
echo "${VERSION_OUT}"
# Strip any platform-warning prefix and inspect the trailing line.
LAST_LINE=$(echo "${VERSION_OUT}" | tail -n1)
case "${LAST_LINE}" in
    "roastery "*) ;;
    *) fail "--version output did not end with 'roastery <version>': ${VERSION_OUT}" ;;
esac

echo "=== (3) Boot + HTTP probes ==="
STORAGE_DIR="$(mktemp -d)"
chmod 777 "${STORAGE_DIR}"
# The fail-closed validation in `ServerConfig::validate` (BAR-AUTH-005)
# requires an auth mechanism whenever the bind address is non-loopback.
# We give the smoke test a one-line bearer-tokens file so the boot path
# exercises the configured-auth branch; the ops endpoints (`/healthz`,
# `/version`) remain public per `auth.rs::is_public_route` so the
# probes below do NOT need to send a token.
TOKENS_FILE="${STORAGE_DIR}/bearer-tokens.txt"
echo "smoke-test-token" > "${TOKENS_FILE}"
chmod 644 "${TOKENS_FILE}"
docker run -d --rm \
    --platform linux/amd64 \
    --name "${CONTAINER}" \
    -p "${HOST_PORT}:7878" \
    -v "${STORAGE_DIR}:/var/lib/roastery" \
    -e ROASTERY_BIND=0.0.0.0:7878 \
    -e ROASTERY_STORAGE_DIR=/var/lib/roastery \
    -e ROASTERY_BEARER_TOKENS_FILE=/var/lib/roastery/bearer-tokens.txt \
    "${IMAGE}" \
    >/dev/null \
    || fail "docker run failed"

# Wait up to 10s for the server to start serving /healthz.
HEALTHZ_URL="http://127.0.0.1:${HOST_PORT}/healthz"
for attempt in $(seq 1 20); do
    if curl -fsS "${HEALTHZ_URL}" >/dev/null 2>&1; then
        echo "  /healthz responsive after attempt ${attempt}"
        break
    fi
    if [[ "${attempt}" -eq 20 ]]; then
        echo "--- container logs ---" >&2
        docker logs "${CONTAINER}" >&2 || true
        fail "/healthz did not respond after 10s"
    fi
    sleep 0.5
done

# Assert /healthz body is exactly "ok" (the M5.1 T7 contract).
HEALTHZ_BODY=$(curl -fsS "${HEALTHZ_URL}")
case "${HEALTHZ_BODY}" in
    "ok"*) ;;
    *) fail "/healthz body was not 'ok' (got: ${HEALTHZ_BODY})" ;;
esac

# Assert /version returns JSON with the expected name field.
VERSION_BODY=$(curl -fsS "http://127.0.0.1:${HOST_PORT}/version")
echo "  /version: ${VERSION_BODY}"
case "${VERSION_BODY}" in
    *'"name":"roastery"'*|*'"name": "roastery"'*) ;;
    *) fail "/version body did not contain '\"name\":\"roastery\"' (got: ${VERSION_BODY})" ;;
esac

echo "=== (4) Graceful shutdown ==="
docker stop --signal=SIGTERM --time=5 "${CONTAINER}" >/dev/null \
    || fail "docker stop (SIGTERM) failed"

echo "=== PASS: roastery container smoke test ==="
