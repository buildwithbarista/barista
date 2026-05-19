#!/usr/bin/env bash
# Build the roastery container image locally via buildx.
#
# Usage:
#   roastery/scripts/build-image.sh
#
# Environment overrides:
#   TAG       — image tag (default: roastery:dev)
#   PLATFORM  — buildx platform string (default: linux/amd64)
#   REPO_ROOT — context root (default: `git rev-parse --show-toplevel`)
#
# This script is the local mirror of the `verify` job in
# `.github/workflows/container-roastery.yml`. Both invocations pass
# `--load` so the resulting image can be `docker run`-d immediately
# afterwards.

set -euo pipefail

TAG="${TAG:-roastery:dev}"
PLATFORM="${PLATFORM:-linux/amd64}"
REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"

echo "Building ${TAG} for ${PLATFORM} from context ${REPO_ROOT}"

exec docker buildx build \
    --platform "${PLATFORM}" \
    --tag "${TAG}" \
    --file "${REPO_ROOT}/roastery/Dockerfile" \
    --load \
    "${REPO_ROOT}"
