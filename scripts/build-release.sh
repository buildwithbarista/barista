#!/usr/bin/env bash
# Reproducible release builder for a single target.
#
# This is the single source of truth for the deterministic-build logic:
# the same script runs locally (to prove determinism on a developer's
# machine) and in the release CI matrix (one invocation per target).
# Keeping the logic in one shell script — rather than spreading it
# across `run:` blocks in the workflow — means "what the pipeline does"
# and "what a developer can reproduce by hand" never drift apart.
#
# Usage:
#   scripts/build-release.sh --target <triple> [options]
#
# Required:
#   --target <triple>   A Rust target triple, e.g. aarch64-apple-darwin.
#
# Options:
#   --version <ver>     Release version string embedded in artifact
#                       names and the manifest. Defaults to the
#                       workspace `version` from `cargo metadata`
#                       (CARGO_PKG_VERSION equivalent).
#   --out-dir <dir>     Directory for the packaged artifact + manifest
#                       fragment. Default: ./dist
#   --target-dir <dir>  Cargo target directory. Default: ./target
#                       (override to get two independent build trees
#                       for the reproducibility double-build check).
#   --git-sha <sha>     Full git SHA recorded in the manifest. Default:
#                       `git rev-parse HEAD`.
#   --no-build          Skip the cargo build; package an already-built
#                       binary (used by callers that build separately).
#   -h | --help         Print this help and exit.
#
# Environment knobs:
#   SKIP_MAVEN_BUNDLE=1 Skip fetching + staging the bundled Maven 4
#                       distribution. The artifact then ships an empty
#                       share/barista/maven-4/ (a `.keep` placeholder).
#                       Intended for local dev runs that only exercise the
#                       binary build and don't want the ~14 MiB fetch.
#                       DEFAULT (unset): the Maven distribution IS bundled.
#   MAVEN_BUNDLE_CACHE  Optional path to a pre-downloaded
#                       `apache-maven-4.0.0-rc-3-bin.tar.gz`. When set and
#                       the file's sha256 matches the pinned digest, the
#                       fetch is skipped and the cached archive is used.
#
# Determinism contract (see README / the release workflow header for the
# rationale of each knob):
#
#   SOURCE_DATE_EPOCH   Pinned build timestamp (Unix seconds). If unset,
#                       derived from the current HEAD commit's author
#                       date so a local run is still reproducible against
#                       itself. Consumed by:
#                         - roastery/build.rs (embedded /version date),
#                         - the tar/zip mtime normalization below.
#   CARGO_INCREMENTAL=0 Incremental compilation embeds absolute,
#                       machine-specific paths and is inherently
#                       non-reproducible; disabled.
#   RUSTFLAGS           `--remap-path-prefix` entries that rewrite every
#                       absolute build path (the checkout dir, $HOME, the
#                       CARGO_HOME registry) to stable logical prefixes,
#                       plus `-C strip=symbols` for deterministic, lean
#                       binaries. Absolute paths leaking into debuginfo /
#                       panic strings are the single most common repro
#                       breaker, so this is the load-bearing knob.
#
# Archive determinism: members are emitted in a fixed (sorted) order,
# every entry's mtime is pinned to SOURCE_DATE_EPOCH, and ownership is
# normalized to uid/gid 0 with empty owner/group names. gzip runs with
# `-n` so the compressed stream carries no embedded timestamp/filename.
#
# Extension points (intentionally NOT implemented here):
#   * Signing / notarization (macOS codesign, Windows Authenticode) and
#     SLSA provenance / SBOM attach in later milestones. They wrap the
#     artifact this script produces; the manifest's per-artifact sha256
#     is the natural input to a detached signature. Search for
#     "SIGNING HOOK" below for the exact seam.
#   * Auto-download of the Maven distribution at first run (delivery shape
#     "b") is intentionally NOT implemented: the default is to BUNDLE the
#     distribution into the artifact (shape "a", staged below — search for
#     "MAVEN BUNDLE"). A future config opt-in could add an on-demand
#     download for size-constrained environments; that is out of scope here.
#
# Output:
#   <out-dir>/barista-<version>-<target>.tar.gz   (unix targets)
#   <out-dir>/barista-<version>-<target>.zip      (windows targets)
#   <out-dir>/manifest-<target>.json              (per-target fragment)
#
# Exits 0 on success; any failure exits non-zero with a diagnostic to
# stderr.

set -euo pipefail

# ---------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------
TARGET=""
VERSION=""
OUT_DIR="dist"
TARGET_DIR="target"
GIT_SHA=""
DO_BUILD=1

die() {
    echo "build-release: error: $1" >&2
    exit 1
}

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; s/^#$//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)     TARGET="${2:-}"; shift 2 ;;
        --version)    VERSION="${2:-}"; shift 2 ;;
        --out-dir)    OUT_DIR="${2:-}"; shift 2 ;;
        --target-dir) TARGET_DIR="${2:-}"; shift 2 ;;
        --git-sha)    GIT_SHA="${2:-}"; shift 2 ;;
        --no-build)   DO_BUILD=0; shift ;;
        -h|--help)    usage; exit 0 ;;
        *)            die "unknown argument: $1 (try --help)" ;;
    esac
done

[[ -n "$TARGET" ]] || die "--target is required"

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

# ---------------------------------------------------------------------
# Bundled Maven 4 distribution (delivery shape "a": bundle).
#
# The pinned coordinates (version / url / sha256) and the
# fetch/verify/extract helpers live in scripts/lib/maven-bundle.sh so the
# pinned digest and the verification logic have exactly one home (shared
# with scripts/test-maven-bundle.sh). The tarball has a single leading
# `apache-maven-<ver>/` path component; the helper STRIPS it on extraction
# so the launcher finds `share/barista/maven-4/bin/mvn` (+ `lib/`) directly.
# `die` is defined above, before the source, as the library requires.
# ---------------------------------------------------------------------
# shellcheck source=scripts/lib/maven-bundle.sh
. "${REPO_ROOT}/scripts/lib/maven-bundle.sh"
# shellcheck source=scripts/lib/barback-bundle.sh
. "${REPO_ROOT}/scripts/lib/barback-bundle.sh"

# ---------------------------------------------------------------------
# Resolve version + git SHA
# ---------------------------------------------------------------------
if [[ -z "$VERSION" ]]; then
    # `cargo metadata` is the most robust way to read the workspace
    # version without parsing TOML by hand. The `barista-cli` package
    # inherits `version.workspace = true`, so its version is the
    # release version.
    VERSION="$(cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json,sys; m=json.load(sys.stdin); print(next(p["version"] for p in m["packages"] if p["name"]=="barista-cli"))')"
    [[ -n "$VERSION" ]] || die "could not determine version from cargo metadata"
fi

if [[ -z "$GIT_SHA" ]]; then
    GIT_SHA="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
fi

# ---------------------------------------------------------------------
# SOURCE_DATE_EPOCH — pinned build timestamp.
#
# Priority: an already-exported SOURCE_DATE_EPOCH (the release pipeline
# sets it from the tagged commit) wins. Otherwise derive it from the
# current HEAD commit's author date so a local invocation is still
# reproducible against a second local invocation of the same commit.
# ---------------------------------------------------------------------
if [[ -z "${SOURCE_DATE_EPOCH:-}" ]]; then
    if SDE="$(git log -1 --pretty=%at 2>/dev/null)" && [[ -n "$SDE" ]]; then
        SOURCE_DATE_EPOCH="$SDE"
    else
        die "SOURCE_DATE_EPOCH unset and HEAD commit date unavailable; \
set SOURCE_DATE_EPOCH explicitly"
    fi
fi
export SOURCE_DATE_EPOCH

# RFC-3339 UTC rendering of the epoch for the manifest. `date -u -d`
# (GNU) and `date -u -r` (BSD/macOS) take the seconds value differently;
# try GNU form first, fall back to BSD form.
build_timestamp_rfc3339() {
    date -u -d "@${SOURCE_DATE_EPOCH}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
        || date -u -r "${SOURCE_DATE_EPOCH}" +%Y-%m-%dT%H:%M:%SZ
}
BUILD_TIMESTAMP="$(build_timestamp_rfc3339)"

# ---------------------------------------------------------------------
# Deterministic build environment
# ---------------------------------------------------------------------
export CARGO_INCREMENTAL=0
# CARGO_HOME may point at a per-user registry path that leaks into
# debuginfo; remap it. Default to the conventional location if unset.
CARGO_HOME_RESOLVED="${CARGO_HOME:-$HOME/.cargo}"

# Resolve the target directory to its absolute path. Cargo embeds the
# absolute path of build-script-generated sources (anything under a
# crate's `$OUT_DIR`, e.g. the prost-generated `barista.v1.rs` from
# barista-ipc's build script) into the binary via the generated code's
# `file!()`/`include!` paths. Because a second independent build may use
# a different target directory, that absolute path is a reproducibility
# breaker unless remapped — so we resolve and remap it explicitly. We
# do this BEFORE the build so the remap is in effect when those sources
# are compiled.
mkdir -p "$TARGET_DIR"
TARGET_DIR_ABS="$(cd "$TARGET_DIR" && pwd)"

# --remap-path-prefix rewrites absolute paths embedded by rustc (in
# debuginfo, in `file!()` / panic messages, and in build-script-
# generated `include!`d sources) to stable logical prefixes. Order
# matters: longer/more-specific prefixes first so a broader rewrite
# doesn't shadow a nested one.
#
#   <TARGET_DIR>           -> /target           (generated OUT_DIR sources)
#   <CARGO_HOME>/registry  -> /cargo-registry   (dependency sources)
#   <REPO_ROOT>            -> /barista           (first-party sources)
#   <HOME>                 -> /home              (anything else under $HOME)
#
# The target dir is listed first because in the double-build PoC it is
# the path that differs between the two builders; it may also live
# under $HOME (so it must win over the broader $HOME rewrite).
#
# `-C strip=symbols` drops the symbol table deterministically (the
# workspace defines no [profile.release] strip setting, so we apply it
# here at the flag level rather than mutating Cargo.toml).
REMAP_FLAGS=(
    "--remap-path-prefix=${TARGET_DIR_ABS}=/target"
    "--remap-path-prefix=${CARGO_HOME_RESOLVED}/registry=/cargo-registry"
    "--remap-path-prefix=${REPO_ROOT}=/barista"
    "--remap-path-prefix=${HOME}=/home"
)

# Linux ELF only: drop the `.note.gnu.build-id`. The GNU linker's
# build-id is the one region that is NOT byte-stable across two
# otherwise-identical Linux builds — it was the sole divergence in the
# two-build reproducibility check (the surrounding code generation is
# deterministic; a macOS host build of the same tree is byte-identical
# run-to-run, and Mach-O has no build-id equivalent). Because we
# `strip=symbols` and ship no separate debuginfo, the note carries no
# value here, so removing it is the minimal fix that makes the Linux
# binary byte-reproducible.
BUILD_ID_FLAG=""
case "$TARGET" in
    *linux*) BUILD_ID_FLAG="-C link-arg=-Wl,--build-id=none" ;;
esac

# Compose RUSTFLAGS, preserving any caller-provided flags.
RELEASE_RUSTFLAGS="${RUSTFLAGS:-} ${REMAP_FLAGS[*]} -C strip=symbols ${BUILD_ID_FLAG}"
# Collapse leading/trailing whitespace for cleanliness.
RELEASE_RUSTFLAGS="$(echo "$RELEASE_RUSTFLAGS" | sed 's/^ *//; s/ *$//')"
export RUSTFLAGS="$RELEASE_RUSTFLAGS"

# GITHUB_SHA is honored by roastery/build.rs for the embedded git SHA;
# pin it to the resolved SHA so a shallow CI clone embeds the right
# commit (and a local build embeds HEAD).
export GITHUB_SHA="${GITHUB_SHA:-$GIT_SHA}"

# ---------------------------------------------------------------------
# Platform-specific naming
# ---------------------------------------------------------------------
case "$TARGET" in
    *windows*) BIN_SUFFIX=".exe"; ARCHIVE_KIND="zip" ;;
    *)         BIN_SUFFIX="";     ARCHIVE_KIND="tar"  ;;
esac

echo "build-release: target=${TARGET} version=${VERSION}"
echo "build-release: SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} (${BUILD_TIMESTAMP})"
echo "build-release: RUSTFLAGS=${RUSTFLAGS}"

# ---------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------
if [[ "$DO_BUILD" -eq 1 ]]; then
    echo "build-release: cargo build --release -p barista-cli -p roastery --target ${TARGET}"
    cargo build \
        --release \
        --locked \
        -p barista-cli \
        -p roastery \
        --target "$TARGET" \
        --target-dir "$TARGET_DIR"
fi

BIN_OUT_DIR="${TARGET_DIR}/${TARGET}/release"
BARISTA_BIN="${BIN_OUT_DIR}/barista${BIN_SUFFIX}"
ROASTERY_BIN="${BIN_OUT_DIR}/roastery${BIN_SUFFIX}"
[[ -f "$BARISTA_BIN" ]]  || die "expected binary not found: ${BARISTA_BIN}"
[[ -f "$ROASTERY_BIN" ]] || die "expected binary not found: ${ROASTERY_BIN}"

# ---------------------------------------------------------------------
# Stage the artifact tree.
#
# Layout (v0.1):
#   barista-<version>-<target>/
#     bin/barista[.exe]              the CLI (primary artifact)
#     bin/roastery[.exe]             the cache server (shipped alongside
#                                    so a single download stands up both
#                                    the client and a local cache)
#     share/barista/maven-4/         BUNDLED Maven 4 distribution
#       bin/mvn, lib/, boot/, conf/  (default; .keep when SKIP_MAVEN_BUNDLE)
#     LICENSE-APACHE
#     LICENSE-MIT
#     README.md
#     CHANGELOG.md
#
# MAVEN BUNDLE: the pinned Maven 4 distribution is staged into
# share/barista/maven-4/ so end-user installs (Homebrew, release tarball,
# container image) have a working Maven without any host configuration.
# The launcher's bundled-home fallback (maven_home.rs) derives this path
# from its own executable location. SKIP_MAVEN_BUNDLE=1 stages an empty
# share/barista/maven-4/ (a `.keep`) for fast local binary-only builds.
# ---------------------------------------------------------------------
STAGE_PARENT="$(mktemp -d)"
trap 'rm -rf "$STAGE_PARENT"' EXIT
PKG_NAME="barista-${VERSION}-${TARGET}"
STAGE="${STAGE_PARENT}/${PKG_NAME}"

mkdir -p "${STAGE}/bin" "${STAGE}/share/barista/maven-4"
install -m 0755 "$BARISTA_BIN"  "${STAGE}/bin/barista${BIN_SUFFIX}"
install -m 0755 "$ROASTERY_BIN" "${STAGE}/bin/roastery${BIN_SUFFIX}"

# Bundle the Maven 4 distribution (default), or leave a `.keep` placeholder
# when the caller opts out for a fast local binary-only build.
MAVEN_BUNDLE_VERSION=""
MAVEN_BUNDLE_SHA256=""
if [[ "${SKIP_MAVEN_BUNDLE:-0}" == "1" ]]; then
    echo "build-release: SKIP_MAVEN_BUNDLE=1 — staging empty share/barista/maven-4/"
    : > "${STAGE}/share/barista/maven-4/.keep"
else
    stage_maven_bundle "${STAGE}/share/barista/maven-4"
    MAVEN_BUNDLE_VERSION="${MAVEN_VERSION}"
    MAVEN_BUNDLE_SHA256="${MAVEN_ARCHIVE_SHA256}"
fi

# Bundle the barback daemon uber-JAR (default), or skip it for a fast local
# binary-only build (the launcher then falls back to a dev checkout). The jar
# ships at share/barista/barback-uber.jar — a sibling of the Maven
# distribution — where the launcher's bundled-jar discovery finds it. Without
# it, a binary install can `barista pull` but the daemon build path can't run.
BARBACK_BUNDLE_SHA256=""
if [[ "${SKIP_BARBACK_BUNDLE:-0}" == "1" ]]; then
    echo "build-release: SKIP_BARBACK_BUNDLE=1 — not staging share/barista/barback-uber.jar"
else
    stage_barback_bundle "${STAGE}/share/barista"
    BARBACK_BUNDLE_SHA256="$(sha256_of "${STAGE}/share/barista/${BARBACK_UBER_LEAF}")"
fi

for doc in LICENSE-APACHE LICENSE-MIT README.md CHANGELOG.md; do
    if [[ -f "$doc" ]]; then
        install -m 0644 "$doc" "${STAGE}/${doc}"
    fi
done

# macOS code signing (Developer ID + hardened runtime + secure timestamp).
# Runs only when an identity is provided AND `codesign` is on PATH (i.e. on
# a macOS runner with the signing keychain already set up by the workflow);
# a no-op on the Linux/Windows legs and on local builds without an identity.
# Only OUR Mach-O binaries are signed — the bundled Maven distribution
# (Apache's) and barback-uber.jar (a JVM jar) are not codesign subjects.
#
# Signing embeds a signature + a secure timestamp, so a signed binary is NOT
# byte-reproducible across builds. That is expected and acceptable: the
# repro-verify gate runs on the `x86_64-unknown-linux-gnu` target only (which
# is never signed), so reproducibility is proven on the unsigned Linux build.
if [[ -n "${BARISTA_CODESIGN_IDENTITY:-}" ]] && command -v codesign >/dev/null 2>&1; then
    echo "build-release: codesigning Mach-O binaries (Developer ID, hardened runtime)"
    for bin in "${STAGE}/bin/barista${BIN_SUFFIX}" "${STAGE}/bin/roastery${BIN_SUFFIX}"; do
        codesign --force --options runtime --timestamp \
            --sign "${BARISTA_CODESIGN_IDENTITY}" "$bin" \
            || die "codesign failed for $bin"
        codesign --verify --strict --verbose=2 "$bin" \
            || die "codesign verification failed for $bin"
    done
fi

# Normalize every file's mtime to SOURCE_DATE_EPOCH so the archive's
# embedded timestamps are stable. `find | sort` gives a deterministic
# traversal order. Use the BSD/GNU touch epoch form that's available.
touch_epoch() {
    # GNU touch: -d @epoch. BSD touch: -t [[CC]YY]MMDDhhmm[.SS].
    if touch -d "@${SOURCE_DATE_EPOCH}" "$1" 2>/dev/null; then
        return 0
    fi
    local stamp
    stamp="$(date -u -r "${SOURCE_DATE_EPOCH}" +%Y%m%d%H%M.%S)"
    touch -t "$stamp" "$1"
}
while IFS= read -r f; do
    touch_epoch "$f"
done < <(find "$STAGE" -print | LC_ALL=C sort)

mkdir -p "$OUT_DIR"
# Resolve OUT_DIR to an absolute path now — we cd into the staging
# parent for archive creation so member paths are clean (no leading
# temp-dir component), and a relative OUT_DIR would otherwise resolve
# against the wrong cwd.
OUT_DIR_ABS="$(cd "$OUT_DIR" && pwd)"

# ---------------------------------------------------------------------
# Package deterministically.
# ---------------------------------------------------------------------
ARCHIVE_NAME=""
if [[ "$ARCHIVE_KIND" == "tar" ]]; then
    ARCHIVE_NAME="${PKG_NAME}.tar.gz"
    ARCHIVE_PATH="${OUT_DIR_ABS}/${ARCHIVE_NAME}"
    # Build a sorted member list (paths relative to STAGE_PARENT) so the
    # tar stream is byte-stable regardless of filesystem traversal order.
    MEMBER_LIST="$(mktemp)"
    ( cd "$STAGE_PARENT" && find "$PKG_NAME" -print | LC_ALL=C sort ) > "$MEMBER_LIST"

    # Detect tar flavor: GNU tar and bsdtar (libarchive) both ship the
    # determinism knobs we need but spell them differently.
    #
    #   ownership   GNU: --owner=0 --group=0 --numeric-owner
    #               bsdtar: --uid=0 --gid=0 --uname= --gname= --numeric-owner
    #   mtime       GNU honors --mtime="@<epoch>". bsdtar's --mtime does
    #               NOT accept the `@<seconds>` form (it wants a parseable
    #               date string and rejects `@…` outright), so for bsdtar
    #               we rely on the per-file mtimes already pinned to
    #               SOURCE_DATE_EPOCH via `touch` above — bsdtar archives
    #               the on-disk mtime, which we've made deterministic.
    #
    # Both honor --format=ustar and reading a pre-sorted member list from
    # a file, so neither relies on its own (differing) default ordering.
    # `--no-recursion` is essential here: the member list already names
    # every directory AND file (in sorted order), so without it tar would
    # also recurse into each listed directory and emit its contents a
    # second time — duplicating every file in the archive. With it, each
    # listed path is archived exactly once, in our sorted order. Both GNU
    # tar and bsdtar support the flag.
    TAR_VERSION="$(tar --version 2>&1 | head -1)"
    TAR_COMMON=( --format=ustar --no-recursion )
    if echo "$TAR_VERSION" | grep -qi 'GNU tar'; then
        TAR_COMMON+=( --mtime="@${SOURCE_DATE_EPOCH}" )
        TAR_OWNER=( --owner=0 --group=0 --numeric-owner )
    else
        # bsdtar / libarchive: omit --mtime (per-file touch handles it).
        TAR_OWNER=( --uid=0 --gid=0 --uname= --gname= --numeric-owner )
    fi

    # `gzip -n` strips the gzip header's mtime + original-filename so the
    # compressed stream is reproducible. We pipe through it explicitly
    # rather than relying on tar's `-z` (which may embed a timestamp).
    ( cd "$STAGE_PARENT" \
        && tar --create \
            "${TAR_COMMON[@]}" \
            "${TAR_OWNER[@]}" \
            --files-from "$MEMBER_LIST" \
            --file - \
        ) | gzip -9 -n > "$ARCHIVE_PATH"
    rm -f "$MEMBER_LIST"
else
    # Windows: produce a .zip. `zip -X` excludes extra file attributes
    # (uid/gid, extended attrs); combined with the normalized mtimes set
    # above and a sorted input list, the archive is byte-stable. zip
    # embeds per-entry mtime from the filesystem, which we've already
    # pinned to SOURCE_DATE_EPOCH via touch.
    ARCHIVE_NAME="${PKG_NAME}.zip"
    ARCHIVE_PATH="${OUT_DIR_ABS}/${ARCHIVE_NAME}"
    rm -f "$ARCHIVE_PATH"
    ( cd "$STAGE_PARENT" \
        && find "$PKG_NAME" -print | LC_ALL=C sort \
        | zip -X -9 "@" "$ARCHIVE_PATH" >/dev/null )
fi

# ---------------------------------------------------------------------
# Hashes (sha256_of is defined once near the top, shared with the
# Maven-bundle verification).
# ---------------------------------------------------------------------
BIN_SHA256="$(sha256_of "${STAGE}/bin/barista${BIN_SUFFIX}")"
ARCHIVE_SHA256="$(sha256_of "$ARCHIVE_PATH")"

# rustc version string for the manifest. Strip to the canonical
# `rustc X.Y.Z (hash date)` line.
RUSTC_VERSION="$(rustc --version)"

# ---------------------------------------------------------------------
# Per-target manifest fragment.
#
# The aggregate manifest (assembled by the workflow from every target's
# fragment) is a JSON object:
#
#   {
#     "schema_version": 1,
#     "barista_version": "<ver>",
#     "git_sha": "<full sha>",
#     "build_timestamp": "<RFC3339, == SOURCE_DATE_EPOCH>",
#     "source_date_epoch": <unix seconds>,
#     "artifacts": [ <fragment>, <fragment>, ... ]
#   }
#
# Each fragment (this file) is one element of "artifacts":
#
#   {
#     "target":          "<rust target triple>",
#     "binary_sha256":   "<sha256 of the barista binary>",
#     "archive":         "<archive filename>",
#     "archive_sha256":  "<sha256 of the .tar.gz / .zip>",
#     "build_timestamp": "<RFC3339>",
#     "rustc_version":   "<rustc --version output>",
#     "barista_version": "<ver>",
#     "git_sha":         "<full sha>",
#     "maven_bundle":    { "version": "<ver>", "sha256": "<archive sha256>" }
#                        OR null when SKIP_MAVEN_BUNDLE was set.
#     "barback_bundle":  { "sha256": "<barback-uber.jar sha256>" }
#                        OR null when SKIP_BARBACK_BUNDLE was set.
#   }
# ---------------------------------------------------------------------
FRAGMENT_PATH="${OUT_DIR_ABS}/manifest-${TARGET}.json"
python3 - "$FRAGMENT_PATH" <<PYEOF
import json, sys
maven_version = "${MAVEN_BUNDLE_VERSION}"
maven_sha256 = "${MAVEN_BUNDLE_SHA256}"
maven_bundle = (
    {"version": maven_version, "sha256": maven_sha256}
    if maven_version
    else None
)
barback_sha256 = "${BARBACK_BUNDLE_SHA256}"
barback_bundle = {"sha256": barback_sha256} if barback_sha256 else None
fragment = {
    "target": "${TARGET}",
    "binary_sha256": "${BIN_SHA256}",
    "archive": "${ARCHIVE_NAME}",
    "archive_sha256": "${ARCHIVE_SHA256}",
    "build_timestamp": "${BUILD_TIMESTAMP}",
    "rustc_version": "${RUSTC_VERSION}",
    "barista_version": "${VERSION}",
    "git_sha": "${GIT_SHA}",
    "maven_bundle": maven_bundle,
    "barback_bundle": barback_bundle,
}
with open(sys.argv[1], "w") as f:
    json.dump(fragment, f, indent=2, sort_keys=True)
    f.write("\n")
PYEOF

echo "build-release: wrote ${ARCHIVE_PATH}"
echo "build-release:   binary_sha256  ${BIN_SHA256}"
echo "build-release:   archive_sha256 ${ARCHIVE_SHA256}"
echo "build-release: wrote ${FRAGMENT_PATH}"
