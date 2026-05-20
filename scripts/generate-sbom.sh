#!/usr/bin/env bash
# CycloneDX SBOM generation + validation for the Barista product.
#
# This is the single source of truth for the supply-chain SBOM: the
# same script runs locally (to let a maintainer reproduce the published
# SBOM by hand) and in the release CI pipeline. Keeping the logic in one
# shell script — rather than spreading it across `run:` blocks in the
# workflow — means "what the pipeline publishes" and "what a developer
# can reproduce" never drift apart.
#
# Usage:
#   scripts/generate-sbom.sh [options]
#
# Options:
#   --version <ver>    Version embedded in the output file names.
#                      Defaults to the workspace version from
#                      `cargo metadata` (the `barista-cli` package, which
#                      inherits the workspace version).
#   --out-dir <dir>    Directory for the published SBOM artifacts.
#                      Default: ./dist  (gitignored).
#   --rust-only        Generate + validate the Rust workspace SBOM only.
#                      Use when a JDK / Maven is unavailable (the Java
#                      SBOM step needs both). The product merge is then
#                      a copy of the Rust SBOM.
#   --no-merge         Skip the product merge; co-publish the per-language
#                      SBOMs side by side only.
#   --self-test        Run the testable contract instead of a normal
#                      generation: (a) generate + validate the real SBOM
#                      and assert it is VALID; (b) feed a deliberately
#                      corrupted CycloneDX fixture to `cyclonedx validate`
#                      and assert it is REJECTED. Exits non-zero if either
#                      half does not behave as declared. Honors
#                      --rust-only (skips the Java half of (a)).
#   -h | --help        Print this help and exit.
#
# Tooling (build/CI only — never a runtime dependency of the product):
#
#   cargo-cyclonedx    The `cargo cyclonedx` subcommand. Emits one
#                      CycloneDX JSON SBOM per workspace crate next to
#                      that crate's Cargo.toml (`<crate>.cdx.json`).
#                      Install: cargo install cargo-cyclonedx --version 0.5.9
#   cyclonedx          The CycloneDX CLI (`CycloneDX/cyclonedx-cli`
#                      release binary). Used for `merge` (aggregate the
#                      per-crate + per-language SBOMs) and `validate`
#                      (the external-tool validation gate). Pinned
#                      release: v0.32.0. The script looks for it on PATH
#                      as `cyclonedx`, or at $CYCLONEDX_CLI.
#   cyclonedx-maven-plugin
#                      Fetched by Maven (pinned in barback/pom.xml under
#                      the `sbom` profile). Emits barback/target/bom.json.
#
# Aggregation model (documented choice):
#
#   Rust: cargo-cyclonedx emits a per-crate SBOM; the cyclonedx CLI
#         `merge`s all workspace crates into ONE Rust SBOM.
#   Java: the cyclonedx-maven-plugin emits one SBOM for barback (whose
#         deps ship inside `barback-uber.jar`, part of the product).
#   Product: the two per-language SBOMs are `merge`d into a single
#         product SBOM. All three are published:
#           barista-<version>-sbom.cdx.json        (merged product)
#           barista-<version>-sbom-rust.cdx.json    (Rust workspace)
#           barista-<version>-sbom-java.cdx.json    (barback / Java)
#         The merged product SBOM is the primary artifact; the
#         per-language SBOMs are co-published as provenance / inputs.
#
# Validation (the "validated by external tool" contract):
#   Every emitted SBOM is validated with `cyclonedx validate
#   --fail-on-errors` against its declared spec version (auto-detected
#   from the file's `specVersion`). `--fail-on-errors` is REQUIRED:
#   without it the CLI prints "BOM is not valid" but still exits 0.
#   Any invalid SBOM exits this script non-zero, failing CI.
#
# Exits 0 on success; any failure exits non-zero with a diagnostic to
# stderr.

set -euo pipefail

# ---------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------
VERSION=""
OUT_DIR="dist"
RUST_ONLY=0
DO_MERGE=1
SELF_TEST=0

die() {
    echo "generate-sbom: error: $1" >&2
    exit 1
}

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; s/^#$//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)   VERSION="${2:-}"; shift 2 ;;
        --out-dir)   OUT_DIR="${2:-}"; shift 2 ;;
        --rust-only) RUST_ONLY=1; shift ;;
        --no-merge)  DO_MERGE=0; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        -h|--help)   usage; exit 0 ;;
        *)           die "unknown argument: $1 (try --help)" ;;
    esac
done

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

# ---------------------------------------------------------------------
# Locate the cyclonedx CLI.
#
# Order: explicit $CYCLONEDX_CLI, then `cyclonedx` on PATH. The CLI is
# the external validation tool; it is not optional.
# ---------------------------------------------------------------------
CYCLONEDX="${CYCLONEDX_CLI:-}"
if [[ -z "$CYCLONEDX" ]]; then
    if command -v cyclonedx >/dev/null 2>&1; then
        CYCLONEDX="cyclonedx"
    else
        die "cyclonedx CLI not found (set \$CYCLONEDX_CLI or put 'cyclonedx' on PATH). \
Install the pinned CycloneDX/cyclonedx-cli v0.32.0 release binary."
    fi
fi

command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH"

# ---------------------------------------------------------------------
# Resolve version
# ---------------------------------------------------------------------
if [[ -z "$VERSION" ]]; then
    VERSION="$(cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json,sys; m=json.load(sys.stdin); print(next(p["version"] for p in m["packages"] if p["name"]=="barista-cli"))')"
    [[ -n "$VERSION" ]] || die "could not determine version from cargo metadata"
fi

mkdir -p "$OUT_DIR"

PRODUCT_SBOM="${OUT_DIR}/barista-${VERSION}-sbom.cdx.json"
RUST_SBOM="${OUT_DIR}/barista-${VERSION}-sbom-rust.cdx.json"
JAVA_SBOM="${OUT_DIR}/barista-${VERSION}-sbom-java.cdx.json"

# ---------------------------------------------------------------------
# validate_sbom <path> — run the external-tool validation gate.
#
# `--fail-on-errors` makes the CLI return non-zero on an invalid SBOM
# (without it the CLI prints the diagnosis but exits 0). The spec
# version is auto-detected from the file's `specVersion`.
# ---------------------------------------------------------------------
validate_sbom() {
    local path="$1"
    [[ -f "$path" ]] || die "expected SBOM not found: $path"
    echo "=== validate: ${path} ==="
    "$CYCLONEDX" validate \
        --input-file "$path" \
        --input-format json \
        --fail-on-errors \
        || die "cyclonedx validate rejected ${path}"
}

# ---------------------------------------------------------------------
# generate_rust_sbom — per-crate cargo-cyclonedx, then merge to one.
#
# cargo-cyclonedx writes `<crate>.cdx.json` next to every workspace
# Cargo.toml. We discover the per-crate outputs from `cargo metadata`
# (workspace members only) rather than globbing so a stray *.cdx.json
# elsewhere in the tree can't sneak into the merge. spec 1.5 is the
# highest the pinned cargo-cyclonedx (0.5.9) emits; the CLI `merge`
# up-converts to its native spec for the merged file.
# ---------------------------------------------------------------------
generate_rust_sbom() {
    command -v cargo-cyclonedx >/dev/null 2>&1 \
        || die "cargo-cyclonedx not found (cargo install cargo-cyclonedx --version 0.5.9)"

    echo "=== cargo cyclonedx (per-crate, spec 1.5) ==="
    cargo cyclonedx --format json --spec-version 1.5 --describe crate \
        || die "cargo cyclonedx failed"

    # Collect the per-crate SBOM paths from the workspace member set.
    # Each member's manifest dir holds `<crate>.cdx.json`.
    local members
    members="$(cargo metadata --no-deps --format-version 1 \
        | python3 -c '
import json, os, sys
m = json.load(sys.stdin)
for p in m["packages"]:
    d = os.path.dirname(p["manifest_path"])
    f = os.path.join(d, p["name"] + ".cdx.json")
    if os.path.isfile(f):
        print(f)
')"
    [[ -n "$members" ]] || die "no per-crate SBOMs were produced by cargo cyclonedx"

    echo "=== cyclonedx merge (Rust workspace) -> ${RUST_SBOM} ==="
    # shellcheck disable=SC2086
    # $members is a newline list of paths with no spaces (crate names +
    # repo paths); word-splitting into separate --input-files args is
    # intended.
    "$CYCLONEDX" merge \
        --input-files $members \
        --output-format json \
        --output-file "$RUST_SBOM" \
        || die "cyclonedx merge of the Rust per-crate SBOMs failed"

    # Clean up the transient per-crate files (they are gitignored, but
    # leaving them in the tree is noise).
    echo "$members" | while IFS= read -r f; do
        [[ -n "$f" ]] && rm -f "$f"
    done

    validate_sbom "$RUST_SBOM"
}

# ---------------------------------------------------------------------
# generate_java_sbom — cyclonedx-maven-plugin via the `sbom` profile.
#
# The plugin (pinned in barback/pom.xml) emits barback/target/bom.json
# (CycloneDX 1.6). Requires a JDK + the offline-resolvable Maven deps;
# the `--rust-only` flag skips this whole step when neither is present.
# ---------------------------------------------------------------------
generate_java_sbom() {
    local mvn_bin="${MAVEN_BIN:-mvn}"
    command -v "$mvn_bin" >/dev/null 2>&1 \
        || die "Maven ('$mvn_bin') not found; use --rust-only or set \$MAVEN_BIN"

    echo "=== mvn -P sbom package (barback Java SBOM) ==="
    "$mvn_bin" -f barback/pom.xml -P sbom -DskipTests package \
        || die "Maven SBOM generation failed"

    local mvn_bom="barback/target/bom.json"
    [[ -f "$mvn_bom" ]] || die "expected ${mvn_bom} was not produced by the cyclonedx-maven-plugin"

    cp "$mvn_bom" "$JAVA_SBOM"
    validate_sbom "$JAVA_SBOM"
}

# ---------------------------------------------------------------------
# merge_product_sbom — combine the per-language SBOMs into one product
# SBOM. With --rust-only / when no Java SBOM exists, the product SBOM is
# a copy of the Rust SBOM (still a valid, complete-as-available SBOM).
# ---------------------------------------------------------------------
merge_product_sbom() {
    if [[ "$DO_MERGE" -eq 0 ]]; then
        echo "=== --no-merge: co-publishing per-language SBOMs side by side ==="
        return 0
    fi
    if [[ -f "$JAVA_SBOM" ]]; then
        echo "=== cyclonedx merge (product = Rust + Java) -> ${PRODUCT_SBOM} ==="
        "$CYCLONEDX" merge \
            --input-files "$RUST_SBOM" "$JAVA_SBOM" \
            --output-format json \
            --output-file "$PRODUCT_SBOM" \
            || die "cyclonedx merge of the product SBOM failed"
    else
        echo "=== product SBOM = Rust SBOM (no Java SBOM present) -> ${PRODUCT_SBOM} ==="
        cp "$RUST_SBOM" "$PRODUCT_SBOM"
    fi
    validate_sbom "$PRODUCT_SBOM"
}

# ---------------------------------------------------------------------
# run_generation — the normal path.
# ---------------------------------------------------------------------
run_generation() {
    generate_rust_sbom
    if [[ "$RUST_ONLY" -eq 0 ]]; then
        generate_java_sbom
    else
        echo "=== --rust-only: skipping the Java SBOM step ==="
    fi
    merge_product_sbom

    echo ""
    echo "=== SBOM artifacts written to ${OUT_DIR}/ ==="
    ls -1 "$OUT_DIR"/barista-"${VERSION}"-sbom*.cdx.json
}

# ---------------------------------------------------------------------
# run_self_test — the testable [T] contract.
#
#   (a) generate + validate the real SBOM(s) -> assert VALID.
#   (b) feed a deliberately-corrupted CycloneDX fixture to
#       `cyclonedx validate` -> assert REJECTED (non-zero).
#
# Honors --rust-only for (a).
# ---------------------------------------------------------------------
run_self_test() {
    echo "########################################################"
    echo "# SBOM self-test (a): real SBOM generates + validates  #"
    echo "########################################################"
    run_generation
    echo "self-test (a) PASS: real SBOM(s) generated and validated."

    echo ""
    echo "########################################################"
    echo "# SBOM self-test (b): corrupted SBOM is REJECTED       #"
    echo "########################################################"
    local tmp corrupt
    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" RETURN
    corrupt="${tmp}/corrupt.cdx.json"

    # Derive the corrupt fixture from a real, just-validated SBOM so the
    # ONLY thing wrong with it is the spec-identity fields. This proves
    # the validator distinguishes valid from invalid (not merely that it
    # rejects malformed JSON).
    python3 - "$PRODUCT_SBOM" "$corrupt" <<'PY'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
with open(src) as f:
    bom = json.load(f)
# Break the spec identity: an unknown specVersion + bogus bomFormat.
bom["specVersion"] = "9.9"
bom["bomFormat"] = "NotACycloneDX"
with open(dst, "w") as f:
    json.dump(bom, f)
PY

    set +e
    "$CYCLONEDX" validate \
        --input-file "$corrupt" \
        --input-format json \
        --fail-on-errors
    rc=$?
    set -e
    echo "cyclonedx validate exit on corrupted fixture: ${rc} (expect non-zero)"
    if [[ "$rc" -eq 0 ]]; then
        die "self-test (b) FAILED: cyclonedx validate accepted a corrupted SBOM. \
The validation gate is broken (missing --fail-on-errors?)."
    fi
    echo "self-test (b) PASS: corrupted SBOM was rejected."

    echo ""
    echo "=== SBOM self-test PASS: valid accepted, corrupted rejected ==="
}

if [[ "$SELF_TEST" -eq 1 ]]; then
    run_self_test
else
    run_generation
fi
