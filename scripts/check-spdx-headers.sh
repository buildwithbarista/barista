#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Source-hygiene gate: every FIRST-PARTY source file must carry the
# repository's dual-license SPDX tag near the top.
#
# Usage:
#   bash scripts/check-spdx-headers.sh            # check; non-zero on any violator
#   bash scripts/check-spdx-headers.sh --fix      # idempotently stamp missing headers
#   bash scripts/check-spdx-headers.sh --list     # print the enumerated first-party set
#
# This script is the single source of truth for the include/exclude
# globs that define "first-party source". The CI gate (`ci.yml`) and the
# self-test (`scripts/test-spdx.sh`) both call it; the `--fix` mode is
# what authors run locally to add the header to a new file.
#
# The expected tag is:
#
#   SPDX-License-Identifier: MIT OR Apache-2.0
#
# It must appear within the first few lines of the file. The stamper
# writes it as the FIRST line (a line comment) followed by a blank line,
# above any existing `//!` docs, `#![...]` inner attributes, `package`
# declaration, or copyright block. The stamp is idempotent: a file that
# already carries the tag is left untouched.
#
# --------------------------------------------------------------------------
# SCOPE — first-party only.
#
# The tree contains a large amount of VENDORED third-party source
# (test-corpus Maven projects, materialized bench-project checkouts,
# upstream REAPI/googleapis protos). Stamping Barista's license header
# onto vendored code would be a license violation, so the include set is
# an explicit allow-list and the exclude set defends against vendored
# trees that happen to live under an included prefix.
#
# INCLUDE (first-party):
#   Rust   crates/<crate>/{src,tests,benches,examples}/**/*.rs
#          crates/<crate>/build.rs
#          crates/<crate>/fuzz/**/*.rs           (first-party fuzz targets)
#          roastery/{src,tests}/**/*.rs
#          roastery/build.rs
#          xtask/**/*.rs
#   Java   barback/src/main/java/**/*.java        (com.bluminal.barista.barback.*)
#          barback/src/test/java/**/*.java
#          barback/bench/src/**/*.java
#   Proto  proto/barista/v1/**/*.proto            (Barista's own protocol)
#
# EXCLUDE (vendored / not first-party source — never stamp):
#   - target/ anywhere (build output)
#   - test-corpus/**                              (vendored Maven projects)
#   - bench/projects/**                           (vendored bench checkouts)
#   - **/checkout/**, **/vendor/**, **/fixtures/**, **/fixture/**
#   - roastery/proto/**                           (upstream REAPI/googleapis,
#                                                  Apache-2.0 — preserve upstream)
#   - barback/spike/**                            (throwaway investigation
#                                                  spikes + com.example.* sample
#                                                  projects; not part of the build)
#   - tests/**                                    (top-level SAST trip-wire
#                                                  fixtures; com.example/synthetic)
#
# Enumeration is `git ls-files` based, so untracked build output and
# anything in `.gitignore` is invisible to the scan by construction.
# --------------------------------------------------------------------------

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "${REPO_ROOT}"

# The exact tag every first-party file must carry.
readonly SPDX_TAG="SPDX-License-Identifier: MIT OR Apache-2.0"
# The full comment line the stamper prepends. `.rs`, `.java`, and
# `.proto` all use `//` line comments.
readonly SPDX_LINE="// ${SPDX_TAG}"
# How many leading lines the checker scans for the tag.
readonly HEADER_SCAN_LINES=5

MODE="check"
case "${1:-}" in
  --fix)  MODE="fix" ;;
  --list) MODE="list" ;;
  "")     MODE="check" ;;
  *)
    echo "usage: $0 [--fix|--list]" >&2
    exit 64  # EX_USAGE
    ;;
esac

# --------------------------------------------------------------------------
# enumerate_first_party
#
# Prints, one per line, the repo-relative path of every tracked
# first-party source file in scope. Sorted + de-duplicated.
#
# Implementation note: git pathspec `*` does NOT cross `/` and git has no
# portable recursive `**` glob, so we enumerate every tracked source file
# by extension and classify each path with shell `case` globs (where `*`
# DOES cross `/`). The include allow-list is matched first; the vendored
# exclude patterns are matched first within the loop and win, so a file
# under an included prefix that also matches an exclude is dropped.
# --------------------------------------------------------------------------
enumerate_first_party() {
  git ls-files -z -- '*.rs' '*.java' '*.proto' \
  | while IFS= read -r -d '' path; do
      # EXCLUDE (vendored / not first-party) — checked first; wins.
      case "${path}" in
        target/*|*/target/*)        continue ;;
        test-corpus/*)              continue ;;
        bench/projects/*)           continue ;;
        */checkout/*)               continue ;;
        */vendor/*)                 continue ;;
        */fixtures/*|*/fixture/*)   continue ;;
        roastery/proto/*)           continue ;;
        barback/spike/*)            continue ;;
        tests/*)                    continue ;;
      esac
      # INCLUDE (first-party) — explicit allow-list.
      case "${path}" in
        # Rust: per-crate source, tests, benches, examples, fuzz, build.rs.
        crates/*/src/*.rs)          ;;
        crates/*/tests/*.rs)        ;;
        crates/*/benches/*.rs)      ;;
        crates/*/examples/*.rs)     ;;
        crates/*/fuzz/*.rs)         ;;
        crates/*/build.rs)          ;;
        # roastery: source, tests, build.rs.
        roastery/src/*.rs)          ;;
        roastery/tests/*.rs)        ;;
        roastery/build.rs)          ;;
        # xtask: the workspace task runner.
        xtask/*.rs)                 ;;
        # barback: its OWN Java only (com.bluminal.barista.barback.*).
        barback/src/main/java/*.java)  ;;
        barback/src/test/java/*.java)  ;;
        barback/bench/src/*.java)      ;;
        # Barista's own protocol.
        proto/barista/v1/*.proto)   ;;
        # Anything else is out of scope.
        *) continue ;;
      esac
      printf '%s\n' "${path}"
    done \
  | LC_ALL=C sort -u
}

# --------------------------------------------------------------------------
# has_header <path>
#
# True (exit 0) iff the SPDX tag appears within the first
# HEADER_SCAN_LINES lines of the file.
# --------------------------------------------------------------------------
has_header() {
  head -n "${HEADER_SCAN_LINES}" "$1" | grep -qF "${SPDX_TAG}"
}

# --------------------------------------------------------------------------
# stamp <path>
#
# Idempotently prepend the SPDX comment line + a blank line to the file.
# No-op if the tag is already present in the header window. Preserves all
# existing content verbatim (byte-for-byte after the inserted prefix).
# --------------------------------------------------------------------------
stamp() {
  local path="$1"
  if has_header "${path}"; then
    return 0
  fi
  # Build the stamped content in a temp file, then copy it back over the
  # ORIGINAL file in place. Copying content (rather than `mv`-ing the
  # temp over the path) leaves the original inode — and therefore its
  # permission bits, including the executable bit git tracks — untouched.
  local tmp
  tmp="$(mktemp "${TMPDIR:-/tmp}/spdx.XXXXXX")"
  {
    printf '%s\n\n' "${SPDX_LINE}"
    cat "${path}"
  } > "${tmp}"
  cat "${tmp}" > "${path}"
  rm -f "${tmp}"
}

# --------------------------------------------------------------------------
# Dispatch.
# --------------------------------------------------------------------------
if [[ "${MODE}" == "list" ]]; then
  enumerate_first_party
  exit 0
fi

violators=()
stamped=0
total=0
while IFS= read -r path; do
  total=$((total + 1))
  if [[ "${MODE}" == "fix" ]]; then
    if ! has_header "${path}"; then
      stamp "${path}"
      stamped=$((stamped + 1))
      echo "stamped: ${path}"
    fi
  else
    if ! has_header "${path}"; then
      violators+=("${path}")
    fi
  fi
done < <(enumerate_first_party)

if [[ "${MODE}" == "fix" ]]; then
  echo "=== --fix: ${stamped}/${total} file(s) stamped (rest already had the header) ==="
  exit 0
fi

if [[ "${#violators[@]}" -gt 0 ]]; then
  echo "::error::${#violators[@]} first-party file(s) of ${total} are missing the SPDX header (${SPDX_TAG}):" >&2
  printf '  %s\n' "${violators[@]}" >&2
  echo "" >&2
  echo "Run 'bash scripts/check-spdx-headers.sh --fix' to add the header." >&2
  exit 1
fi

echo "=== PASS: all ${total} first-party source file(s) carry '${SPDX_TAG}' ==="
