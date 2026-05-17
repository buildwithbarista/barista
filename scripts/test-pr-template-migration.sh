#!/usr/bin/env bash
# Verify the PR-template directory-form migration is complete and
# structurally correct.
#
# Usage:
#   bash scripts/test-pr-template-migration.sh
#
# Asserts:
#   1. The legacy single-file template `.github/PULL_REQUEST_TEMPLATE.md`
#      does NOT exist. GitHub's PR-template resolution prefers the
#      single-file form if present, which would defeat the directory
#      migration.
#   2. `.github/PULL_REQUEST_TEMPLATE/default.md` exists. This is the
#      template GitHub auto-loads when a user clicks "Compare & pull
#      request". Its content is the M0.1 Task 9 original, preserved
#      byte-equal via `git mv`.
#   3. `.github/PULL_REQUEST_TEMPLATE/security-auto-remediation.md`
#      exists with every section header the auto-remediation policy
#      requires.
#
# Exits 0 on success. Exits non-zero with a diagnostic on the first
# failure.

set -euo pipefail

LEGACY=".github/PULL_REQUEST_TEMPLATE.md"
DEFAULT=".github/PULL_REQUEST_TEMPLATE/default.md"
AGENT=".github/PULL_REQUEST_TEMPLATE/security-auto-remediation.md"

fail() {
  echo "::error::$1" >&2
  exit 1
}

# (1) The legacy single-file template must NOT exist.
if [[ -f "${LEGACY}" ]]; then
  fail "${LEGACY} still exists — the directory-form migration is incomplete. GitHub prefers the single-file form when present, so this file must be removed (it was renamed to ${DEFAULT} via 'git mv')."
fi

# (2) The directory-form default template must exist.
if [[ ! -f "${DEFAULT}" ]]; then
  fail "${DEFAULT} not found — the M0.1 Task 9 template should have been moved here via 'git mv .github/PULL_REQUEST_TEMPLATE.md ${DEFAULT}'."
fi

# (3) The auto-remediation template must exist with the required sections.
if [[ ! -f "${AGENT}" ]]; then
  fail "${AGENT} not found — the auto-remediation PR template is required by the guardrails YAML."
fi

# Expected section headers in the auto-remediation template. Each must
# appear at the start of a line (Markdown H2). The list mirrors the
# guardrails YAML's `pr_template_migration.agent_template_required_sections`.
expected_sections=(
  "## Summary"
  "## What the agent did"
  "## Why this is the right fix"
  "## Sonatype verification"
  "## Verification"
  "## Status checks"
  "## Reviewer notes"
  "## Provenance"
)

missing=()
for section in "${expected_sections[@]}"; do
  # Match the header at the start of a line. `grep -F` for literal
  # match (no regex), `-q` for quiet, `-x -e ...` would force exact
  # line match but the file uses trailing whitespace stripping so the
  # `-F` line-anchored match is the right surface.
  if ! grep -F -q "^${section}$" "${AGENT}" 2>/dev/null && ! awk -v s="${section}" 'BEGIN{rc=1} $0 == s {rc=0; exit} END{exit rc}' "${AGENT}"; then
    missing+=("${section}")
  fi
done

if [[ ${#missing[@]} -gt 0 ]]; then
  echo "::error::${AGENT} is missing the following required section headers:" >&2
  for s in "${missing[@]}"; do
    echo "  - ${s}" >&2
  done
  exit 1
fi

# (4) Sanity: the auto-remediation template should mention `Fixes` so
# the issue auto-closure pattern is in the template. This is a soft
# guard against future edits accidentally dropping the Fixes line.
if ! grep -q "Fixes #" "${AGENT}"; then
  fail "${AGENT} does not contain a 'Fixes #' placeholder. The agent's PR must auto-close its originating issue on merge; the template is the surface that enforces this convention."
fi

# (5) Sanity: `default.md` must contain the M0.1 Task 9 surface
# (Checklist heading + a Conventional Commits bullet). This is a soft
# guard against the migration accidentally clobbering the original.
if ! grep -q "^## Checklist" "${DEFAULT}"; then
  fail "${DEFAULT} does not contain the '## Checklist' heading from the M0.1 Task 9 original. The 'git mv' should have preserved the content byte-equal."
fi

if ! grep -q "Conventional Commits" "${DEFAULT}"; then
  fail "${DEFAULT} does not mention 'Conventional Commits'. The M0.1 Task 9 original's Checklist section included this bullet; its absence suggests the migration was not a pure rename."
fi

echo "PR template migration verified:"
echo "  - ${LEGACY} absent (legacy form removed)"
echo "  - ${DEFAULT} present (M0.1 Task 9 content preserved)"
echo "  - ${AGENT} present with all 8 required section headers"
