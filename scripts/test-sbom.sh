#!/usr/bin/env bash
# The SBOM testable contract ([T] proof).
#
# Usage:
#   bash scripts/test-sbom.sh
#
# This is a thin wrapper around `scripts/generate-sbom.sh --self-test`,
# which runs the two-part acceptance contract for
# "SBOM published and validated by external tool":
#
#   (a) generate + validate the real SBOM(s) with the CycloneDX CLI
#       (the external tool) -> assert VALID;
#   (b) feed a deliberately-corrupted CycloneDX fixture (derived from a
#       real, just-validated SBOM with its spec-identity fields broken)
#       to `cyclonedx validate --fail-on-errors` -> assert REJECTED.
#
# The contract proves the validation gate distinguishes valid from
# invalid (not merely that the happy path runs): a regression that drops
# `--fail-on-errors` would let the corrupted fixture pass and fail (b).
#
# Environment knobs (forwarded to generate-sbom.sh):
#   SBOM_TEST_RUST_ONLY=1   Run the Rust-only path (skip the Java SBOM
#                           half of (a)). Set this when no JDK / Maven is
#                           available; the corrupted-fixture half (b)
#                           still runs against the Rust/product SBOM.
#   SBOM_TEST_NO_SPDX=1     Skip the SPDX half (generation + the
#                           corrupted-SPDX case (c)). Set this when `syft`
#                           is unavailable locally; the CI pipeline always
#                           runs the SPDX path.
#   SBOM_TEST_OUT_DIR=<dir> Output directory for the generated SBOMs.
#                           Default: a throwaway temp dir (cleaned up).
#   CYCLONEDX_CLI=<path>    Path to the cyclonedx CLI binary if not on
#                           PATH as `cyclonedx`.
#
# Exits 0 on success; non-zero if either half of the contract fails.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
GEN="${REPO_ROOT}/scripts/generate-sbom.sh"

fail() {
    echo "::error::$1" >&2
    exit 1
}

[[ -f "$GEN" ]] || fail "${GEN} does not exist; the SBOM generator is missing."

# Throwaway output dir unless the caller pinned one.
OUT_DIR="${SBOM_TEST_OUT_DIR:-}"
CLEANUP_DIR=""
if [[ -z "$OUT_DIR" ]]; then
    OUT_DIR="$(mktemp -d)"
    CLEANUP_DIR="$OUT_DIR"
fi
cleanup() {
    [[ -n "$CLEANUP_DIR" ]] && rm -rf "$CLEANUP_DIR"
}
trap cleanup EXIT

ARGS=(--self-test --out-dir "$OUT_DIR")
if [[ "${SBOM_TEST_RUST_ONLY:-0}" == "1" ]]; then
    ARGS+=(--rust-only)
fi
if [[ "${SBOM_TEST_NO_SPDX:-0}" == "1" ]]; then
    ARGS+=(--no-spdx)
fi

echo "=== scripts/test-sbom.sh: running generate-sbom.sh ${ARGS[*]} ==="
bash "$GEN" "${ARGS[@]}" \
    || fail "SBOM self-test failed (see output above)"

echo "=== PASS: SBOM self-test (valid accepted, corrupted rejected) ==="
