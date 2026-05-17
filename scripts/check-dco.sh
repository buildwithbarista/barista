#!/usr/bin/env bash
# Developer Certificate of Origin (DCO) check.
#
# Walks every commit in the range `<base>..<head>` and asserts that
# each one carries a `Signed-off-by:` trailer whose email matches the
# commit's author email. This is the same property the DCO GitHub App
# (`dcoapp/app`) enforces; reimplementing it as a shell script keeps
# the check observable in the workflow YAML and removes the App-install
# step from the operator onboarding flow.
#
# Usage:
#   bash scripts/check-dco.sh <base-sha> <head-sha>
#
# Exits 0 when every commit is signed off. Exits non-zero with a
# diagnostic listing every offending commit. The diagnostic is the
# `::error::` GitHub-Actions annotation format so failures surface at
# the workflow-summary level.
#
# Merge commits (those with more than one parent) are skipped: their
# sign-off requirement is on the merged commits, not on the merge
# commit itself. This matches the `dcoapp/app` default behavior.
#
# The check is deliberately strict on email match: a trailer like
#   Signed-off-by: Random Person <other@example.com>
# on a commit authored by `aj@ajbrown.org` fails, because the DCO is a
# per-commit attestation and the signer must be the author. This is
# also the `dcoapp/app` default; relaxing it to "any signoff trailer"
# would defeat the contributor-attestation property.

set -euo pipefail

# ---------------------------------------------------------------------------
# Arg parsing.
# ---------------------------------------------------------------------------

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <base-sha> <head-sha>" >&2
  exit 64  # EX_USAGE
fi

BASE_SHA="$1"
HEAD_SHA="$2"

# ---------------------------------------------------------------------------
# Compute the commit range.
#
# `<base>..<head>` is "commits reachable from head but not from base".
# That's the right range for a PR: every commit the PR introduces, and
# no commits that already existed on the base branch.
# ---------------------------------------------------------------------------

if ! git rev-parse --verify "${BASE_SHA}^{commit}" >/dev/null 2>&1; then
  echo "::error::base SHA ${BASE_SHA} is not reachable in this checkout" >&2
  exit 1
fi
if ! git rev-parse --verify "${HEAD_SHA}^{commit}" >/dev/null 2>&1; then
  echo "::error::head SHA ${HEAD_SHA} is not reachable in this checkout" >&2
  exit 1
fi

# `--no-merges` filters merge commits out of the range. We could also
# do this in the loop body via `git rev-list --parents` and a count
# check, but `--no-merges` is the canonical surface.
COMMITS="$(git rev-list --no-merges "${BASE_SHA}..${HEAD_SHA}")"

if [[ -z "${COMMITS}" ]]; then
  echo "DCO check: no commits in range ${BASE_SHA}..${HEAD_SHA} — nothing to verify."
  exit 0
fi

# ---------------------------------------------------------------------------
# Walk the range and check each commit.
# ---------------------------------------------------------------------------

# Collect failing commits so we can print all of them at once rather
# than bailing on the first failure. A contributor with 5 unsigned
# commits wants the full list, not 5 separate force-push iterations.
FAILED=()

while IFS= read -r sha; do
  [[ -z "${sha}" ]] && continue

  # Pull the author email and the full commit message body. Using `git
  # show -s --format=...` keeps us out of the working tree.
  author_email="$(git show -s --format='%ae' "${sha}")"
  subject="$(git show -s --format='%s' "${sha}")"

  # `git interpret-trailers --parse` extracts trailers from the commit
  # message in `Key: Value` form, one per line. We then look for any
  # `Signed-off-by:` entry whose value contains `<author_email>`.
  trailers="$(git show -s --format='%B' "${sha}" | git interpret-trailers --parse)"

  # Match `Signed-off-by: ... <email>` (case-insensitive on the key).
  # `grep -i` for case-insensitive; `-F` would be wrong here because we
  # do want pattern matching on the email shape.
  if echo "${trailers}" | grep -qi "^Signed-off-by:.*<${author_email}>"; then
    continue
  fi

  # Fallback: some workflows author commits as a bot but sign off as a
  # human, or vice versa. The DCO spec's intent is "the author attests";
  # require a literal author-email match. Failure path:
  FAILED+=("${sha}  ${author_email}  ${subject}")
done <<< "${COMMITS}"

# ---------------------------------------------------------------------------
# Report.
# ---------------------------------------------------------------------------

if [[ ${#FAILED[@]} -eq 0 ]]; then
  count="$(echo "${COMMITS}" | grep -c .)"
  echo "DCO check: PASS (${count} commit(s) verified)."
  exit 0
fi

echo "::error::DCO check FAILED — the following commit(s) lack a 'Signed-off-by: <author-name> <author-email>' trailer:" >&2
for entry in "${FAILED[@]}"; do
  echo "  - ${entry}" >&2
done
cat >&2 <<'EOF'

How to fix:

  1. Configure your git identity (one-time, per-clone):

       git config user.name "Your Name"
       git config user.email "you@example.com"

  2. Sign off your most-recent commit:

       git commit --amend --signoff --no-edit

     Or sign off every commit on the PR branch interactively:

       git rebase --signoff <base-branch>

  3. Force-push the rewritten branch:

       git push --force-with-lease

Going forward, sign off every commit by default:

       git commit -s -m "your message"

The DCO trailer attests that you have the right to submit the work
under the project's license. See <https://developercertificate.org/>
for the full text.
EOF

exit 1
