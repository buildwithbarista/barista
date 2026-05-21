# shellcheck shell=bash
# Bundled barback uber-JAR: build (or accept a prebuilt) + stage helper.
# Sourced by scripts/build-release.sh (the release packager).
#
# This is a LIBRARY: it defines a variable + a function and runs no top-level
# work. It does not `set -e` (the sourcing script owns shell options) and
# does not define `die` or `sha256_of` — the sourcing script provides `die`,
# and `sha256_of` comes from the already-sourced scripts/lib/maven-bundle.sh.
#
# ── Why the uber-JAR ships in the product ─────────────────────────────────
# The barback daemon executes the Maven lifecycle (`barista verify`, etc.).
# Its runnable uber-JAR (`barback-uber.jar`, produced by maven-shade-plugin)
# must be present for the CLI to spawn the daemon. End-user installs have no
# dev checkout, so the release artifacts SHIP the jar under
# `share/barista/barback-uber.jar` — a sibling of the bundled Maven 4
# distribution — where the launcher's bundled-jar discovery
# (crates/barista-cli/src/daemon/launcher.rs, `bundled_barback_jar`) finds it
# relative to the executable. Without it a binary install can `barista pull`
# (resolve + cache) but the daemon BUILD path cannot start.
#
# ── Reproducibility ───────────────────────────────────────────────────────
# The jar is built with `-Dproject.build.outputTimestamp=$SOURCE_DATE_EPOCH`
# so maven-shade normalizes the zip entries' timestamps + order: two builds
# at the same epoch produce a byte-identical jar. This preserves
# build-release.sh's determinism contract — the `repro-verify` gate compares
# whole archives, and the staged jar is part of the archive.

BARBACK_UBER_LEAF="barback-uber.jar"

# stage_barback_bundle <dest_dir>
#   Places <dest_dir>/barback-uber.jar. Resolution order:
#     1. $BARISTA_BARBACK_UBER_JAR — a prebuilt jar (a CI build-once artifact
#        or a local fast path); copied verbatim.
#     2. otherwise build it reproducibly via `mvn -f barback/pom.xml package`.
#   Requires SOURCE_DATE_EPOCH and REPO_ROOT from the sourcing script, and a
#   `die` function. Builds with whatever `mvn` + JDK are on PATH.
stage_barback_bundle() {
    local dest="$1"
    mkdir -p "$dest"
    local jar
    if [[ -n "${BARISTA_BARBACK_UBER_JAR:-}" ]]; then
        [[ -f "${BARISTA_BARBACK_UBER_JAR}" ]] \
            || die "BARISTA_BARBACK_UBER_JAR set but not a file: ${BARISTA_BARBACK_UBER_JAR}"
        jar="${BARISTA_BARBACK_UBER_JAR}"
    else
        command -v mvn >/dev/null 2>&1 \
            || die "mvn not found; needed to build barback-uber.jar (set BARISTA_BARBACK_UBER_JAR to a prebuilt jar, or SKIP_BARBACK_BUNDLE=1)"
        echo "build-release: building barback-uber.jar (reproducible; outputTimestamp=${SOURCE_DATE_EPOCH})"
        mvn -B -q -f "${REPO_ROOT}/barback/pom.xml" -DskipTests \
            "-Dproject.build.outputTimestamp=${SOURCE_DATE_EPOCH}" \
            package \
            || die "barback build failed"
        jar="${REPO_ROOT}/barback/target/${BARBACK_UBER_LEAF}"
    fi
    [[ -f "${jar}" ]] || die "barback uber-JAR not found at: ${jar}"
    install -m 0644 "${jar}" "${dest}/${BARBACK_UBER_LEAF}"
}
