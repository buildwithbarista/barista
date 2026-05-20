# shellcheck shell=bash
# Bundled Maven 4 distribution: pinned coordinates + fetch/verify/extract
# helpers. Sourced by `scripts/build-release.sh` (the release packager) and
# by `scripts/test-maven-bundle.sh` (its hermetic unit test), so the pinned
# digest and the verification logic have exactly one home.
#
# This is a LIBRARY: it defines variables + functions and runs no top-level
# work. It does not `set -e`; the sourcing script owns shell options. It
# does not define `die` — the sourcing script must provide one (both
# `build-release.sh` and the test do).
#
# ── The bundle, and why it exists ─────────────────────────────────────────
# The barback daemon's embedded Maven core refuses to start without a Maven 4
# distribution directory (configured via `BARISTA_MAVEN_HOME` /
# `-Dbarista.maven.home`). End-user installs have no Maven on the host, so
# the release tarballs SHIP the pinned distribution under
# `share/barista/maven-4/`; the `barista` launcher's bundled-home fallback
# (crates/barista-cli/src/daemon/maven_home.rs) points barback at it.
#
# ── Supply-chain pin ──────────────────────────────────────────────────────
# The version is pinned to the Maven 4 version the daemon + the
# conformance/bench harnesses target. The archive's sha256 is pinned to a
# hard-coded constant and verified before extraction: the pipeline NEVER
# silently accepts whatever downloads. The pinned sha256 was confirmed
# against the upstream `.sha512` sidecar published alongside the archive at
# the canonical Apache dist/archive mirrors; `MAVEN_ARCHIVE_SHA256` below is
# the SHA-256 of that same byte-identical archive.
#
#   archive:  apache-maven-4.0.0-rc-3-bin.tar.gz
#   url:      https://archive.apache.org/dist/maven/maven-4/4.0.0-rc-3/binaries/
#   sha512:   cbc1cd352929685d72d8b64099e92591947ecb65dfcf79db50c6d2ccdbdfd410\
#             be257240028bbc7032481b2744465b691f46d596c3b7294f38b9744732aaea63
#   sha256:   ef86d972e52a04866f5f78b457d21d7ecd96efa99c696998f2bd4b86ee020bcd

MAVEN_VERSION="4.0.0-rc-3"
MAVEN_ARCHIVE="apache-maven-${MAVEN_VERSION}-bin.tar.gz"
# Canonical immutable source: the Apache archive mirror. The primary
# `dlcdn.apache.org` and `repo.maven.apache.org` mirrors serve the same
# byte-identical archive, but the `archive.` host is the durable one for a
# pinned RC.
MAVEN_BASE_URL="https://archive.apache.org/dist/maven/maven-4/${MAVEN_VERSION}/binaries"
MAVEN_URL="${MAVEN_BASE_URL}/${MAVEN_ARCHIVE}"
# Pinned SHA-256 of ${MAVEN_ARCHIVE}. Verified before extraction.
MAVEN_ARCHIVE_SHA256="ef86d972e52a04866f5f78b457d21d7ecd96efa99c696998f2bd4b86ee020bcd"

# sha256_of <file> — emit the file's lowercase hex SHA-256. Works with both
# GNU coreutils (`sha256sum`) and the BSD/macOS `shasum`.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# verify_sha256 <file> <expected-hex> — die unless <file>'s sha256 matches.
# This is the load-bearing supply-chain check for the bundled Maven
# distribution: a tampered or wrong-version archive is rejected here, never
# silently accepted. Requires the sourcing script to define `die`.
verify_sha256() {
    local file="$1" expected="$2" actual
    actual="$(sha256_of "$file")"
    if [[ "$actual" != "$expected" ]]; then
        die "sha256 mismatch for ${file}:
  expected ${expected}
  actual   ${actual}
Refusing to bundle an archive that does not match the pinned digest."
    fi
}

# stage_maven_bundle <dest-dir> — fetch + verify + extract the pinned Maven 4
# distribution into <dest-dir>, stripping the leading `apache-maven-<ver>/`
# path component so the launcher finds <dest-dir>/bin/mvn (+ lib/) directly.
#
# Honors MAVEN_BUNDLE_CACHE: when it names an existing file, that archive is
# used in place of a network fetch (it is still sha-verified). Requires the
# sourcing script to define `die`.
stage_maven_bundle() {
    local dest="$1" archive
    local workdir
    workdir="$(mktemp -d)"
    # shellcheck disable=SC2064  # expand $workdir now, not at trap time.
    trap "rm -rf '${workdir}'" RETURN

    if [[ -n "${MAVEN_BUNDLE_CACHE:-}" && -f "${MAVEN_BUNDLE_CACHE}" ]]; then
        echo "maven-bundle: using cached archive ${MAVEN_BUNDLE_CACHE}"
        archive="${MAVEN_BUNDLE_CACHE}"
    else
        archive="${workdir}/${MAVEN_ARCHIVE}"
        echo "maven-bundle: fetching ${MAVEN_URL}"
        # `--fail` turns an HTTP 4xx/5xx into a non-zero exit (otherwise curl
        # would write the error page to the output file and we'd fail the sha
        # check with a confusing diagnostic — failing on the fetch is clearer).
        curl --fail --location --silent --show-error \
            --output "${archive}" "${MAVEN_URL}" \
            || die "failed to download ${MAVEN_URL}"
    fi

    echo "maven-bundle: verifying ${MAVEN_ARCHIVE} sha256"
    verify_sha256 "${archive}" "${MAVEN_ARCHIVE_SHA256}"

    # Strip the leading `apache-maven-<ver>/` component so the distribution
    # lands directly under <dest>. Both GNU tar and bsdtar honor
    # --strip-components.
    mkdir -p "${dest}"
    tar -xzf "${archive}" -C "${dest}" --strip-components=1

    # Fail fast if the extracted layout isn't what the launcher's
    # bundled-home probe validates (bin/mvn + lib/).
    [[ -f "${dest}/bin/mvn" ]] \
        || die "bundled Maven missing bin/mvn after extraction into ${dest}"
    [[ -d "${dest}/lib" ]] \
        || die "bundled Maven missing lib/ after extraction into ${dest}"
    echo "maven-bundle: staged Maven ${MAVEN_VERSION} into ${dest}"
}
