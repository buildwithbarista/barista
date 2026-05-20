#!/usr/bin/env bash
# Hermetic unit test for the bundled-Maven supply-chain logic in
# scripts/lib/maven-bundle.sh.
#
# Usage:
#   bash scripts/test-maven-bundle.sh
#
# This test is fully offline + deterministic: it never reaches the network.
# It builds a tiny synthetic "Maven distribution" tarball (same leading
# `apache-maven-<ver>/` shape as the real one), feeds it to the real
# `stage_maven_bundle` via MAVEN_BUNDLE_CACHE, and asserts:
#
#   (1) `verify_sha256` ACCEPTS a file whose digest matches.
#   (2) `verify_sha256` REJECTS (dies) when the digest differs — the
#       wrong-sha → rejected proof for the bundled distribution.
#   (3) `stage_maven_bundle` strips the leading path component so the
#       distribution lands at <dest>/bin/mvn (+ <dest>/lib/), matching the
#       launcher's bundled-home probe.
#   (4) `stage_maven_bundle` REJECTS a cached archive whose sha256 does not
#       match the pinned `MAVEN_ARCHIVE_SHA256` — never silently accepted.
#   (5) The pinned coordinates are the documented, expected values (guards
#       against an accidental edit to the constant / version).
#
# Exits 0 on success; any failed assertion exits non-zero with a diagnostic.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
MAVEN_LIB="${REPO_ROOT}/scripts/lib/maven-bundle.sh"

# The library requires the sourcing script to define `die`.
die() {
    echo "test-maven-bundle: error: $1" >&2
    exit 1
}

[[ -f "${MAVEN_LIB}" ]] || die "library not found: ${MAVEN_LIB}"
# shellcheck source=scripts/lib/maven-bundle.sh
. "${MAVEN_LIB}"

PASS=0
ok() {
    echo "  ok: $1"
    PASS=$((PASS + 1))
}

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

# ---------------------------------------------------------------------
# Build a synthetic Maven distribution tarball with the real shape:
#   apache-maven-<ver>/bin/mvn
#   apache-maven-<ver>/bin/mvn.cmd
#   apache-maven-<ver>/lib/maven-core.jar   (placeholder)
#   apache-maven-<ver>/boot/classworlds.jar (placeholder)
# ---------------------------------------------------------------------
DIST="${WORK}/apache-maven-${MAVEN_VERSION}"
mkdir -p "${DIST}/bin" "${DIST}/lib" "${DIST}/boot" "${DIST}/conf"
printf '#!/bin/sh\necho fake-mvn\n' > "${DIST}/bin/mvn"
chmod 0755 "${DIST}/bin/mvn"
printf '@echo off\r\n' > "${DIST}/bin/mvn.cmd"
printf 'placeholder\n' > "${DIST}/lib/maven-core.jar"
printf 'placeholder\n' > "${DIST}/boot/classworlds.jar"

FIXTURE_TGZ="${WORK}/fixture-maven.tar.gz"
( cd "${WORK}" && tar -czf "${FIXTURE_TGZ}" "apache-maven-${MAVEN_VERSION}" )
FIXTURE_SHA="$(sha256_of "${FIXTURE_TGZ}")"

echo "=== (1) verify_sha256 accepts a matching digest ==="
# Run in a subshell so a `die` (which would `exit 1`) is caught.
if ( verify_sha256 "${FIXTURE_TGZ}" "${FIXTURE_SHA}" >/dev/null 2>&1 ); then
    ok "verify_sha256 accepted the matching digest"
else
    die "verify_sha256 rejected a digest that matched"
fi

echo "=== (2) verify_sha256 rejects a wrong digest ==="
WRONG_SHA="0000000000000000000000000000000000000000000000000000000000000000"
if ( verify_sha256 "${FIXTURE_TGZ}" "${WRONG_SHA}" >/dev/null 2>&1 ); then
    die "verify_sha256 ACCEPTED a wrong digest (should have rejected)"
else
    ok "verify_sha256 rejected the wrong digest"
fi

echo "=== (3) stage_maven_bundle strips the leading component ==="
# Override the pinned digest for THIS staging call so the fixture passes
# verification (the real pinned constant is asserted separately in (5)).
DEST_OK="${WORK}/dest-ok"
(
    # The local override is INTENTIONALLY scoped to this subshell so the
    # parent's pinned constant (asserted in step 5) is untouched.
    # shellcheck disable=SC2030
    MAVEN_ARCHIVE_SHA256="${FIXTURE_SHA}"
    MAVEN_BUNDLE_CACHE="${FIXTURE_TGZ}"
    export MAVEN_BUNDLE_CACHE
    stage_maven_bundle "${DEST_OK}" >/dev/null 2>&1
)
[[ -f "${DEST_OK}/bin/mvn" ]] \
    || die "expected ${DEST_OK}/bin/mvn after strip-component extraction"
[[ -d "${DEST_OK}/lib" ]] \
    || die "expected ${DEST_OK}/lib/ after strip-component extraction"
# The leading apache-maven-<ver>/ directory must NOT survive.
[[ ! -d "${DEST_OK}/apache-maven-${MAVEN_VERSION}" ]] \
    || die "leading apache-maven-* component was not stripped"
ok "stage_maven_bundle produced bin/mvn + lib/ with the leading component stripped"

echo "=== (4) stage_maven_bundle rejects a wrong-sha cached archive ==="
DEST_BAD="${WORK}/dest-bad"
# Here the pinned digest is the (wrong, real) constant while the cached
# archive is the fixture → mismatch → must die, leaving no staged tree.
if (
    MAVEN_BUNDLE_CACHE="${FIXTURE_TGZ}"
    export MAVEN_BUNDLE_CACHE
    stage_maven_bundle "${DEST_BAD}" >/dev/null 2>&1
); then
    die "stage_maven_bundle ACCEPTED an archive whose sha256 != pinned digest"
else
    ok "stage_maven_bundle rejected the wrong-sha cached archive"
fi
[[ ! -f "${DEST_BAD}/bin/mvn" ]] \
    || die "a rejected bundle should not have produced a staged bin/mvn"

echo "=== (5) pinned coordinates match the documented values ==="
[[ "${MAVEN_VERSION}" == "4.0.0-rc-3" ]] \
    || die "MAVEN_VERSION drifted: ${MAVEN_VERSION} (expected 4.0.0-rc-3)"
EXPECTED_SHA="ef86d972e52a04866f5f78b457d21d7ecd96efa99c696998f2bd4b86ee020bcd"
# Reads the parent-shell constant from the sourced library; the step-3
# override was confined to its subshell, so this is the real pinned value.
# shellcheck disable=SC2031
[[ "${MAVEN_ARCHIVE_SHA256}" == "${EXPECTED_SHA}" ]] \
    || die "MAVEN_ARCHIVE_SHA256 drifted from the pinned, upstream-confirmed digest"
[[ "${MAVEN_ARCHIVE}" == "apache-maven-4.0.0-rc-3-bin.tar.gz" ]] \
    || die "MAVEN_ARCHIVE name drifted: ${MAVEN_ARCHIVE}"
ok "pinned version + archive name + sha256 are the expected values"

echo "=== PASS: ${PASS} assertions ==="
