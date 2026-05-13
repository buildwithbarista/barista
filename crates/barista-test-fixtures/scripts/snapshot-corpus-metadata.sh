#!/usr/bin/env bash
#
# snapshot-corpus-metadata.sh — refresh resolver test fixtures.
#
# Walks every materialized corpus project under test-corpus/, reads its
# declared <dependencies>, and snapshots each one's POM +
# maven-metadata.xml into
#
#     crates/barista-resolver/tests/fixtures/<groupId>/<artifactId>/...
#
# The pinned reference Maven (set by .tool-versions; currently 3.9.9 on
# JDK 21) is used so the snapshot matches CI's reference behaviour.
#
# Status: v0.1 PLACEHOLDER. The fixture set is tiny today (a handful of
# coords) and is maintained by hand following the workflow documented
# in crates/barista-resolver/tests/fixtures/README.md. Full automation
# is deferred until the corpus grows large enough to make manual
# maintenance painful.
#
# Run from the repo root:
#     bash crates/barista-test-fixtures/scripts/snapshot-corpus-metadata.sh
#
# Requires: mvn 3.9.x on PATH (from .tool-versions), curl, network.

set -euo pipefail

# Resolve repo root from this script's location so it can be invoked
# from anywhere.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/crates/barista-resolver/tests/fixtures"
CORPUS_DIR="${REPO_ROOT}/test-corpus"

cat <<EOF
snapshot-corpus-metadata.sh — v0.1 placeholder

This script is a stub. Fixture maintenance is currently manual.

To add or refresh a fixture by hand:

  GROUP_SLASHED=org/apache/commons      # dotted groupId, '.' -> '/'
  ARTIFACT=commons-lang3
  VERSION=3.14.0
  DEST="${FIXTURES_DIR}/\${GROUP_SLASHED//\//.}/\${ARTIFACT}"

  mkdir -p "\${DEST}/\${VERSION}"

  curl -fsSL -o "\${DEST}/\${VERSION}/pom.xml" \\
    "https://repo.maven.apache.org/maven2/\${GROUP_SLASHED}/\${ARTIFACT}/\${VERSION}/\${ARTIFACT}-\${VERSION}.pom"

  curl -fsSL -o "\${DEST}/maven-metadata.xml" \\
    "https://repo.maven.apache.org/maven2/\${GROUP_SLASHED}/\${ARTIFACT}/maven-metadata.xml"

Then:

  cargo test -p barista-resolver

Paths:
  REPO_ROOT     = ${REPO_ROOT}
  FIXTURES_DIR  = ${FIXTURES_DIR}
  CORPUS_DIR    = ${CORPUS_DIR}

Reference toolchain (from .tool-versions):
EOF

if [[ -f "${REPO_ROOT}/.tool-versions" ]]; then
    sed 's/^/  /' "${REPO_ROOT}/.tool-versions"
else
    echo "  (.tool-versions not present)"
fi

echo
echo "Full automation is tracked for a post-v0.1 milestone."
exit 0
