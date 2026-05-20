# Barista Release-Candidate (RC) program

This document describes how Barista runs its pre-1.0 release-candidate
program: the 30-day observation window, how critical bugs are tracked and
triaged, the severity SLAs, and the criteria that gate the `v0.1.0` GA tag.

It is the public companion to the release-build pipeline
([`release.yml`](../../.github/workflows/release.yml)) and the reproducible-
build helper ([`scripts/build-release.sh`](../../scripts/build-release.sh)).

## 1. What the RC program is

Before `v0.1.0` ships as a general-availability (GA) release, Barista runs a
**time-boxed release-candidate program**: one or more pre-release builds
(`v0.1.0-rc.1`, `v0.1.0-rc.2`, …) are published for a self-selected preview
cohort to exercise against real projects, with a structured channel for
reporting build-breaking and correctness-critical bugs.

The goal is to surface the failures that only appear under real-world build
graphs — JDK/OS permutations, large multi-module reactors, unusual
`settings.xml` and mirror configurations — that the test corpus and CI matrix
do not cover, **before** committing to the compatibility promises a GA tag
implies.

## 2. The 30-day clock (non-restarting)

The RC program runs for a **fixed 30 calendar days**:

- The clock **starts** when `v0.1.0-rc.1` is published.
- The clock **ends** 30 calendar days later, regardless of how many RC builds
  are cut in between.
- **Subsequent RC cuts do NOT restart the clock.** If a P0/P1 bug forces a
  `v0.1.0-rc.2` (or `-rc.3`, …), the fix ships, the cohort re-tests, but the
  30-day window keeps counting from `rc.1`.

This is deliberate. A "restart the clock on every fix" policy lets a steady
trickle of RC churn defer GA indefinitely; a fixed window forces the program
to converge. The trade-off — that a bug found late in the window gets less
post-fix soak time — is accepted and is exactly why the GA gate (§5) requires
the window to **end clean**, not merely to elapse.

## 3. Preview cohort

The cohort is **opt-in and public**. Anyone can participate:

- Install a published RC build (see the release notes attached to each
  `v0.1.0-rc.N` pre-release on GitHub) and run it against your own Maven
  project(s).
- The most valuable testing is `barista verify` and full `mvn`-vocabulary
  builds (`clean`/`compile`/`test`/`package`/`verify`/`install`) on projects
  that already build cleanly under upstream Maven, so any divergence is a real
  Barista bug rather than a pre-existing project issue.

There is no registration step and no NDA — RC builds are public pre-releases.
Participants are encouraged (not required) to report the shape of their
workload (module count, JDK, OS, reference Maven version) on the issues they
file so triage can reproduce.

## 4. Reporting channel and the `rc-critical` label

All RC bugs are filed as **GitHub issues** on this repository.

- Build-breaking or correctness-critical reports are labeled
  **`rc-critical`** (see §6 for who applies it and how it is created).
- The `rc-critical` label is the single queue the maintainers watch during the
  RC window; an issue without it is triaged on the normal cadence.
- Include: the RC version (`barista --version`), the OS + arch, the JDK
  (`java -version`), the reference Maven version the project builds under, and
  a minimal reproduction (ideally a public repo + the failing command).

## 5. Severity, SLAs, and the GA gate

Two severities gate the program:

| Severity | Definition | Acknowledge | Fix target |
| --- | --- | --- | --- |
| **P0** | Build-breaking for a workload that builds cleanly under upstream Maven (wrong artifacts, corruption, daemon crash, data loss), with no workaround. | 1 business day | Next RC cut |
| **P1** | Significant correctness or compatibility divergence with a workaround, or a P0 that has a documented workaround. | 3 business days | Next RC cut |

Lower-severity issues (P2/P3) are tracked normally and do not gate GA.

**GA exit criteria for `v0.1.0`:**

1. The 30-day clock (§2) has elapsed.
2. **Zero open `rc-critical` P0 or P1 issues** at the close of the window — the
   window must end *clean*, not merely expire.
3. The reproducible-build pipeline produces the GA artifacts identically to the
   final RC (the GA build is the same commit, re-tagged; see `release.yml`).

If the window elapses with open P0/P1s, GA slips until they are resolved and a
clean RC build has been observed by the cohort — the clock does not extend, but
the *exit gate* is unconditional.

## 6. Operator setup — create the `rc-critical` label

The `rc-critical` label must exist on the repository before `rc.1` is cut.
Create it once with the GitHub CLI (requires `repo` scope on the canonical
repository):

```bash
# Resolve owner/repo from the local git remote (assumes `origin` is the
# canonical GitHub remote).
read -r OWNER REPO < <(
  gh repo view --json owner,name \
    --jq '"\(.owner.login) \(.name)"'
)

gh label create rc-critical \
  --repo "${OWNER}/${REPO}" \
  --color B60205 \
  --description "RC-blocking P0/P1 bug — gates the v0.1.0 GA tag" \
  --force
```

`--force` makes the command idempotent (updates the label if it already
exists), so it is safe to re-run.

## 7. Cutting an RC build

Cutting `v0.1.0-rc.N` is a **deliberate, human-gated action** — the
release pipeline builds and publishes only on a pushed `v*` tag, and tagging
is not automated:

1. Confirm the RC program scope and cohort (this document) are approved.
2. Ensure the `rc-critical` label exists (§6).
3. Tag the release commit on `main` and push the tag:
   ```bash
   git tag -s v0.1.0-rc.1 -m "Barista v0.1.0-rc.1"
   git push origin v0.1.0-rc.1
   ```
   The `release.yml` workflow builds all platform artifacts + the build
   manifest and publishes them as a GitHub **pre-release** (the workflow sets
   `prerelease: true` automatically for `-rc.` / `-alpha.` / `-beta.` tags).
4. Announce the RC and the reporting channel (§4) to the cohort.

The first `rc.1` cut starts the 30-day clock (§2). Subsequent cuts to address
`rc-critical` fixes reuse the same procedure and **do not** restart it.
