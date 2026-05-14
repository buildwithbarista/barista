# `tests/fixtures/secrets/`

This directory holds **deliberately fake** credentials whose only job is to
exercise the secret-scanning toolchain. Every file here is an *input* to a
test, not data the project depends on at runtime.

## Why these files exist

The repo's secret-scan configuration (`.gitleaks.toml`, `.gitleaksignore`)
and the pre-commit hook (`.pre-commit-config.yaml`) are only useful if we
can prove two things continuously:

1. **The scanner is wired up and fires.** A known-bad credential shape
   committed to the tree must be caught.
2. **The allowlist mechanism works.** When an allowlist entry is added for
   a documented fixture, the same scanner invocation must report no
   findings.

`scripts/test-secret-scan.sh` exercises both halves of that round-trip
against the files in this directory.

## Inventory

| File | Rule it targets | Notes |
|---|---|---|
| `synthetic_aws_key.txt` | `aws-access-token` (gitleaks default) | Shaped like an AWS access-key ID (`AKIA` prefix + 16-char alphabet). Used by `scripts/test-secret-scan.sh` Tests A and B. |

## Ground rules

- **Never put a real credential here.** Any value that ever was a real
  credential — even one believed to be revoked — must be rotated before
  use as a fixture, then the fixture must be a freshly-generated
  *non-functional* string.
- **Each fixture targets exactly one rule.** If a fixture starts matching
  more than one rule, split it. Tests that assert on rule IDs become
  brittle otherwise.
- **Allowlisting these fixtures is a runtime concern.** This directory is
  *not* allowlisted globally. The integration test toggles individual
  fingerprints in `.gitleaksignore` to assert both the catch-it and the
  let-it-through paths. Permanent allowlists for these files would defeat
  the test.
- **Changing a fixture rotates its fingerprint.** If you edit a file here
  in a way that changes the matched line or rule, expect existing waivers
  pinned to its fingerprint to stop applying. That is the desired
  behaviour — it forces a fresh review.

## How a fixture is referenced by a test

`scripts/test-secret-scan.sh` runs `gitleaks detect --no-git` against this
directory with `--config=.gitleaks.toml`. The JSON report it parses
contains a `Fingerprint` per finding (shape: `path:rule:line`). The
script writes that fingerprint into a temporary `.gitleaksignore` for the
"is allowlisting respected?" half of the round-trip.

If you add a new fixture, also:
1. Add a row to the inventory table above.
2. Reference it from a test or document why it is unreferenced (an
   orphaned fixture is dead code).
