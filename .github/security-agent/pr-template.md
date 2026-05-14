<!--
This is the PR-body template the auto-remediation agent populates when
it opens a security-fix PR. The agent loads it, fills each slot, and
passes the result to `gh pr create --body-file -`.

A follow-up milestone migrates this into the directory-form PR
templates at `.github/PULL_REQUEST_TEMPLATE/security-auto-remediation.md`
so it can be surfaced by appending `?template=security-auto-remediation.md`
to the PR-create URL. Until that migration lands, the agent supplies
the body directly via `--body-file`.

Every slot is mandatory. If a slot does not apply (e.g., a SAST-only
fix has no Sonatype verification), write `N/A — <reason>` rather than
deleting the slot, so reviewers see at a glance which dimensions the
agent checked.
-->

## Summary

<!-- One-paragraph plain-English statement of the fix. -->

## What the agent did

<!-- Bulleted list of file edits. One bullet per file, with a one-line
description of the change. -->

## Why this is the right fix

<!-- Reasoning. Explicitly: why this specific change resolves the
finding, and why a larger refactor is not in scope. Reference the
remediation hint from the issue body if applicable. -->

## Sonatype verification

<!-- For dependency-bump PRs only. Paste the verbatim response from
`mcp__sonatype-mcp__getComponentVersion` for the chosen target version.
Must show `malicious: false` AND `policyCompliance.compliant: true`.
Also list the advisory ID(s) the bump clears.

For non-dep-bump PRs: write `N/A — <fix is a SAST/allowlist/SBOM change>`. -->

## Self-test results

<!-- Exit codes and trailing output of the mandatory self-tests run on
this branch:

- `cargo xtask security` — exit code, scanner summary line
- `bash scripts/test-secret-scan.sh` — exit code, "All tests passed" line
- `mvn -f barback/pom.xml test` (Java-side fixes only) — exit code, test summary

If either of the first two failed on this branch, the PR must not be opened. -->

## Provenance

<!-- Link to the originating issue and the upstream scanner run. -->

- Originating finding-issue: #<issue-number>
- Scanner: `<tool>` / `<rule_id>`
- Upstream workflow run: <URL>

Fixes #<issue-number>
