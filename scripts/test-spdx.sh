#!/usr/bin/env bash
# Self-test for the SPDX header gate (`scripts/check-spdx-headers.sh`).
#
# Usage:
#   bash scripts/test-spdx.sh
#
# Two assertions:
#
#   (a) POSITIVE — the real check over the (already-stamped) tree exits 0.
#       Proves the gate accepts a correctly-headered tree, i.e. every
#       enumerated first-party file carries the dual-license SPDX tag.
#
#   (b) NEGATIVE — a synthetic first-party-SHAPED file with NO header,
#       placed at a path the enumerator scans, makes the check exit
#       non-zero and names the offending file. Proves the gate actually
#       rejects a missing header (a check that can never fail is worse
#       than no check). The synthetic file is created in a temp git
#       worktree clone of the repo so the real tree is never mutated;
#       cleanup is unconditional via an EXIT trap.
#
# This is the [T] "prove it" harness for the M6.3 T5 SPDX gate. It is
# also a `shellcheck`-clean shell script, so the workflow validator
# (`scripts/test-spdx-workflow.sh`) lints it alongside the checker.
#
# Exits 0 when both assertions hold; non-zero with a diagnostic
# otherwise.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
CHECK="${REPO_ROOT}/scripts/check-spdx-headers.sh"

fail() {
    echo "::error::$1" >&2
    exit 1
}

if [[ ! -x "${CHECK}" && ! -f "${CHECK}" ]]; then
    fail "${CHECK} not found; cannot self-test the SPDX gate."
fi

# -------------------------------------------------------------------------
# (a) POSITIVE — the real tree passes.
# -------------------------------------------------------------------------
echo "=== (a) positive: check over the real (stamped) tree must PASS ==="
if ! ( cd "${REPO_ROOT}" && bash "${CHECK}" >/dev/null ); then
    fail "(a) check-spdx-headers.sh exited non-zero on the real tree; \
the stamped tree should be 100% covered. Run it directly to see the \
violators."
fi
echo "    PASS: real tree is fully covered."

# -------------------------------------------------------------------------
# (b) NEGATIVE — a missing-header first-party file is rejected.
#
# We build a minimal throwaway git repo in a temp dir: a copy of the
# checker script plus ONE first-party-SHAPED Rust file with NO header,
# at a path the enumerator includes (crates/<crate>/src/*.rs). The
# enumerator is `git ls-files` based and rooted at the repo top-level, so
# the synthetic file must be `git add`-ed for the scan to see it.
#
# A self-contained scratch repo (rather than a clone of the real tree) is
# hermetic, fast, independent of the real tree's commit state, and never
# touches the working tree under test. Cleanup is unconditional via the
# EXIT trap.
# -------------------------------------------------------------------------
echo "=== (b) negative: a header-less first-party file must be REJECTED ==="

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/spdx-selftest.XXXXXX")"
cleanup() { rm -rf "${SCRATCH}"; }
trap cleanup EXIT

CLONE="${SCRATCH}/repo"
mkdir -p "${CLONE}/scripts"
cp "${CHECK}" "${CLONE}/scripts/check-spdx-headers.sh"
git -C "${CLONE}" init --quiet
# Identity is required for `git add`/commit machinery on some hosts even
# though we never commit; set throwaway local config.
git -C "${CLONE}" config user.email selftest@example.invalid
git -C "${CLONE}" config user.name "spdx-selftest"

# A first-party-SHAPED Rust file with NO SPDX header, at a path the
# enumerator includes (crates/<crate>/src/*.rs). It must be `git add`-ed
# so `git ls-files` sees it.
BAD_REL="crates/barista-coords/src/__spdx_selftest_missing_header.rs"
BAD_ABS="${CLONE}/${BAD_REL}"
mkdir -p "$(dirname "${BAD_ABS}")"
cat > "${BAD_ABS}" <<'EOF'
// Deliberately header-less file used by scripts/test-spdx.sh to prove
// the SPDX gate rejects a missing header. Never committed to the real
// tree.
pub fn answer() -> u32 {
    42
}
EOF
git -C "${CLONE}" add "scripts/check-spdx-headers.sh" "${BAD_REL}"

# The check MUST now fail and MUST name the offending file.
set +e
OUT="$( cd "${CLONE}" && bash scripts/check-spdx-headers.sh 2>&1 )"
rc=$?
set -e

if [[ "${rc}" -eq 0 ]]; then
    echo "${OUT}" >&2
    fail "(b) check-spdx-headers.sh exited 0 with a header-less first-party \
file present; the gate is not actually enforcing anything."
fi
if ! grep -qF "${BAD_REL}" <<<"${OUT}"; then
    echo "${OUT}" >&2
    fail "(b) check exited non-zero (good) but did not name the offending \
file '${BAD_REL}' in its output."
fi
echo "    PASS: header-less first-party file rejected (exit ${rc}); \
offender named in output."

# -------------------------------------------------------------------------
# Done.
# -------------------------------------------------------------------------
echo "=== PASS: SPDX gate self-test (positive + negative) ==="
