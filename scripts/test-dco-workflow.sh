#!/usr/bin/env bash
# Exercise the DCO check (`scripts/check-dco.sh`) against synthetic
# commit-history fixtures.
#
# Usage:
#   bash scripts/test-dco-workflow.sh
#
# Each test case builds a throwaway git repository in a temp dir,
# constructs a known commit history (some commits signed off, some
# not), then invokes `scripts/check-dco.sh <base> <head>` and asserts
# the exit code matches the expected PASS/FAIL.
#
# Exits 0 if every case behaves as expected. Exits non-zero on the
# first divergence, with a diagnostic.
#
# Why a self-contained shell test? The DCO check itself is shell + git
# plumbing; a shell-based test exercises it through the same surface
# CI will. A unit test in another language would have to spawn `git`
# and `bash` anyway, so the shell form is the smallest faithful
# reproduction.

set -euo pipefail

# Resolve the repo root so the test can be invoked from anywhere.
REPO_ROOT="$(git rev-parse --show-toplevel)"
CHECK_DCO="${REPO_ROOT}/scripts/check-dco.sh"

if [[ ! -x "${CHECK_DCO}" ]]; then
  echo "::error::${CHECK_DCO} not found or not executable" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Test harness.
# ---------------------------------------------------------------------------

PASS_COUNT=0
FAIL_COUNT=0
FAILURES=()

# Create a fresh git repo in a temp dir and echo its path. Pinning the
# author identity here keeps every test deterministic and decouples
# the test from the host's `~/.gitconfig`.
make_repo() {
  local dir
  dir="$(mktemp -d)"
  git -C "${dir}" init --quiet --initial-branch=main
  git -C "${dir}" config user.name "Test Author"
  git -C "${dir}" config user.email "test@example.com"
  git -C "${dir}" config commit.gpgsign false
  echo "${dir}"
}

# Make an empty commit with the given message + author email in the
# given repo. The author email is parameterized so we can simulate the
# "signed off by a different person" case.
make_commit() {
  local repo="$1"
  local message="$2"
  local author_email="${3:-test@example.com}"
  GIT_AUTHOR_NAME="Test Author" \
  GIT_AUTHOR_EMAIL="${author_email}" \
  GIT_COMMITTER_NAME="Test Author" \
  GIT_COMMITTER_EMAIL="${author_email}" \
    git -C "${repo}" commit --allow-empty --quiet -m "${message}"
}

# Run check-dco.sh against a repo over the range <base>..<head>.
# `expected` is "PASS" or "FAIL"; the test asserts the script's exit
# code matches.
assert_dco() {
  local name="$1"
  local repo="$2"
  local base="$3"
  local head="$4"
  local expected="$5"

  # Run from inside the repo so check-dco.sh's `git rev-parse` resolves
  # against the right working tree.
  local rc=0
  ( cd "${repo}" && bash "${CHECK_DCO}" "${base}" "${head}" ) >/dev/null 2>&1 || rc=$?

  local got
  if [[ ${rc} -eq 0 ]]; then
    got="PASS"
  else
    got="FAIL"
  fi

  if [[ "${got}" == "${expected}" ]]; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "  OK    ${name}  (expected ${expected}, got ${got})"
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    FAILURES+=("${name}: expected ${expected}, got ${got} (exit ${rc})")
    echo "  FAIL  ${name}  (expected ${expected}, got ${got})"
  fi
}

cleanup() {
  # Reap any temp repos. They're under /tmp anyway, but tidy up.
  if [[ -n "${TMP_REPOS:-}" ]]; then
    for repo in ${TMP_REPOS}; do
      rm -rf "${repo}"
    done
  fi
}
trap cleanup EXIT
TMP_REPOS=""

# ---------------------------------------------------------------------------
# Case 1 — all commits signed off → PASS.
# ---------------------------------------------------------------------------

echo "Case 1: all commits signed off"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"  # base commit
BASE="$(git -C "${REPO}" rev-parse HEAD)"

make_commit "${REPO}" "$(printf 'feat: first change\n\nSigned-off-by: Test Author <test@example.com>')"
make_commit "${REPO}" "$(printf 'feat: second change\n\nSigned-off-by: Test Author <test@example.com>')"
HEAD="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "all-signed" "${REPO}" "${BASE}" "${HEAD}" "PASS"

# ---------------------------------------------------------------------------
# Case 2 — one commit lacks sign-off → FAIL.
# ---------------------------------------------------------------------------

echo "Case 2: one commit lacks sign-off"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"
BASE="$(git -C "${REPO}" rev-parse HEAD)"

make_commit "${REPO}" "$(printf 'feat: first change\n\nSigned-off-by: Test Author <test@example.com>')"
make_commit "${REPO}" "feat: second change WITHOUT sign-off"
make_commit "${REPO}" "$(printf 'feat: third change\n\nSigned-off-by: Test Author <test@example.com>')"
HEAD="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "one-unsigned" "${REPO}" "${BASE}" "${HEAD}" "FAIL"

# ---------------------------------------------------------------------------
# Case 3 — no commits signed off → FAIL.
# ---------------------------------------------------------------------------

echo "Case 3: zero commits signed off"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"
BASE="$(git -C "${REPO}" rev-parse HEAD)"

make_commit "${REPO}" "feat: change one (unsigned)"
make_commit "${REPO}" "feat: change two (unsigned)"
HEAD="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "none-signed" "${REPO}" "${BASE}" "${HEAD}" "FAIL"

# ---------------------------------------------------------------------------
# Case 4 — empty range (head == base) → PASS (vacuously true).
# ---------------------------------------------------------------------------

echo "Case 4: empty range (head == base)"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"
SHA="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "empty-range" "${REPO}" "${SHA}" "${SHA}" "PASS"

# ---------------------------------------------------------------------------
# Case 5 — sign-off email does NOT match author email → FAIL.
#
# The DCO requires the *author* to attest. A `Signed-off-by:` trailer
# from a different person doesn't satisfy the per-commit attestation
# property; this case asserts the check enforces that.
# ---------------------------------------------------------------------------

echo "Case 5: sign-off email != author email"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"
BASE="$(git -C "${REPO}" rev-parse HEAD)"

# Author email is test@example.com, but the sign-off claims other@example.com.
make_commit \
  "${REPO}" \
  "$(printf 'feat: mismatched sign-off\n\nSigned-off-by: Different Person <other@example.com>')" \
  "test@example.com"
HEAD="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "email-mismatch" "${REPO}" "${BASE}" "${HEAD}" "FAIL"

# ---------------------------------------------------------------------------
# Case 6 — case-insensitive trailer key (`signed-off-by:` vs `Signed-off-by:`) → PASS.
#
# `git interpret-trailers` normalises trailer key casing; the DCO check
# must therefore accept either case. This asserts no contributor is
# tripped up by a lowercase trailer.
# ---------------------------------------------------------------------------

echo "Case 6: lowercase trailer key"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"
BASE="$(git -C "${REPO}" rev-parse HEAD)"

make_commit "${REPO}" "$(printf 'feat: lowercase sign-off trailer\n\nsigned-off-by: Test Author <test@example.com>')"
HEAD="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "lowercase-trailer" "${REPO}" "${BASE}" "${HEAD}" "PASS"

# ---------------------------------------------------------------------------
# Case 7 — merge commits in range are skipped (only parents are checked) → PASS.
#
# `--no-merges` in check-dco.sh filters merge commits out of the
# range. Construct a history where the only unsigned commit IS the
# merge commit itself; both parents are signed. The check should pass.
# ---------------------------------------------------------------------------

echo "Case 7: merge commit unsigned but parents signed → merge skipped"
REPO="$(make_repo)"; TMP_REPOS+=" ${REPO}"

make_commit "${REPO}" "initial: base commit on main"
BASE="$(git -C "${REPO}" rev-parse HEAD)"

# Side branch.
git -C "${REPO}" checkout -b feature --quiet
make_commit "${REPO}" "$(printf 'feat: side branch change\n\nSigned-off-by: Test Author <test@example.com>')"

# Back to main, advance it.
git -C "${REPO}" checkout main --quiet
make_commit "${REPO}" "$(printf 'feat: main-line change\n\nSigned-off-by: Test Author <test@example.com>')"

# Merge with --no-ff to force a merge commit. The merge commit itself
# has no Signed-off-by trailer.
git -C "${REPO}" merge --no-ff --no-edit feature --quiet
HEAD="$(git -C "${REPO}" rev-parse HEAD)"

assert_dco "merge-skipped" "${REPO}" "${BASE}" "${HEAD}" "PASS"

# ---------------------------------------------------------------------------
# Report.
# ---------------------------------------------------------------------------

echo
echo "test-dco-workflow.sh summary: ${PASS_COUNT} passed, ${FAIL_COUNT} failed"

if [[ ${FAIL_COUNT} -gt 0 ]]; then
  echo
  echo "Failures:"
  for entry in "${FAILURES[@]}"; do
    echo "  - ${entry}"
  done
  exit 1
fi
