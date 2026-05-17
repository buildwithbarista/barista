<!--
This is the PR-body template the auto-remediation agent uses when it
opens a security-fix PR. The agent appends `?template=security-auto-remediation.md`
to its `gh pr create` URL so this template — not `default.md` — surfaces
in the PR body.

Human contributors generally land on `default.md`; this template is the
agent's surface. If you are a human triaging an auto-remediation PR,
read the slots below as the agent's structured hand-off: every section
should already be populated, and any "N/A — <reason>" entries should
have explicit rationale.

Every slot is mandatory. If a slot does not apply (e.g., a SAST-only
fix has no Sonatype verification), write `N/A — <reason>` rather than
deleting the slot, so reviewers see at a glance which dimensions the
agent checked.
-->

## Summary

<!-- One-paragraph plain-English statement of the fix. -->

## What the agent did

<!-- Bulleted list of file edits. One bullet per file, with a one-line
description of the change. This is the "diff summary" surface: a human
reading just this block should be able to reconstruct the scope of the
change without opening the Files Changed tab. -->

## Why this is the right fix

<!-- Rationale block. Explicitly:

- Why this specific change resolves the finding (cite the rule id and
  the specific behavior the scanner objected to).
- Why a larger refactor is not in scope (the agent is bounded to the
  smallest reviewable change that closes the finding).
- Reference the remediation hint from the issue body if applicable.

This is the "why this is the right fix" surface: a reviewer should be
able to decide whether to merge based on this block plus the diff,
without re-running the scanner or re-deriving the fix. -->

## Sonatype verification

<!-- For dependency-bump PRs only. Paste the verbatim response from
`mcp__sonatype-mcp__getComponentVersion` for the chosen target version.
Must show `malicious: false` AND `policyCompliance.compliant: true`.
Also list the advisory ID(s) the bump clears.

For non-dep-bump PRs: write `N/A — <fix is a SAST/allowlist/SBOM change>`. -->

## Verification

<!-- Self-tests the agent ran on this branch, with exit codes and the
trailing summary line of each:

- `cargo xtask security` — exit code, scanner summary line
- `bash scripts/test-secret-scan.sh` — exit code, "All tests passed" line
- `mvn -f barback/pom.xml test` (Java-side fixes only) — exit code, test summary

If either of the first two failed on this branch, the PR must not be
opened (the agent's prompt enforces this; this slot is the human-readable
attestation). -->

## Status checks

<!-- The required-status-checks contract this PR must satisfy before
it is merge-eligible. Pasted from `.github/security-agent/guardrails.yaml`
so reviewers can see the gate inline without context-switching:

- Secret scan — gitleaks + trufflehog
- SCA — dependency vulnerability scanning
- SAST — static analysis
- CodeQL
- Workflow lint

A red mark on any of the above blocks merge regardless of human
approval. The agent is not exempt. -->

## Reviewer notes

<!-- Caveats, context, or anything else the agent surfaces for the
human reviewer. Common entries:

- "Sonatype recommended N, latest is M; chose N because it's the
  smallest non-major bump that clears the advisory."
- "Adjacent finding X exists but is out of scope for this PR — see
  issue #<N> for the separate fix."
- "Allowlist entry added with rationale per docs/ci/secret-scan-allowlist.md."

If there are no caveats, write `None — straightforward dep bump` (or
the equivalent). Do not leave this section blank. -->

## Provenance

- Originating finding-issue: #<issue-number>
- Scanner: `<tool>` / `<rule_id>`
- Upstream workflow run: <URL>

Fixes #<issue-number>
