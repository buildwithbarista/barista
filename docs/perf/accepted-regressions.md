# Accepted performance regressions

Each entry below documents a known-and-accepted performance regression:
when it was introduced, why the slowdown is acceptable, and the
metric + workload + threshold that the gate should exempt.

The Tier-2 perf-gate workflow
(`.github/workflows/perf-gate.yml`) parses this file via
`scripts/compare-perf-results.sh` and treats any `(Project,
Dimension)` pair in the table below as exempt: a regression that
would otherwise fail the gate on that pair is demoted to a warning
instead. The gate does NOT consult the `Δ` column — exemption is
binary per `(Project, Dimension)` pair. The `Δ` column is for human
review.

## Format

A single markdown table. The first two columns are load-bearing
(they are what the parser reads); the remaining columns are
human-review context.

- **Project** — corpus project ID. Matches the `metadata.project`
  field on the results.json document the gate compares.
- **Dimension** — one of `D1`..`D7` per PRD §17.10. Matches the
  `metadata.dimension` field.
- **Date** — when the regression was introduced (YYYY-MM-DD).
- **Baseline / Current / Δ** — the median wall-clock before, after,
  and percent change at the moment the entry was filed.
- **Rationale** — why the slowdown is acceptable. Should reference an
  issue, ADR, or PR that justifies the trade-off.
- **Issue/PR** — link to the discussion that ratified the exemption.

Rows whose `Project` cell starts with `(` are placeholder rows and
are ignored by the parser. This lets a freshly-created file say
something useful without producing a phantom exemption.

## Adding an entry

1. Open a PR that adds a row to the `## Entries` table.
2. Link the issue or ADR that explains the trade-off in the
   `Rationale` and `Issue/PR` columns.
3. The PR description should call out which Tier-2 perf-gate run
   first failed on the regression (the gate's `Files changed`
   annotations include the manifest_id + percent delta).
4. Reviewer applies the `performance-ok` label to acknowledge the
   regression is intentional. See PRD §17.10 for the workflow.

Removing an entry is the reverse: a PR that removes the row, with a
note that the regression has been fixed (or the relevant code path
removed).

## Entries

| Project | Dimension | Date | Baseline (ms) | Current (ms) | Δ | Rationale | Issue/PR |
|---|---|---|---:|---:|---:|---|---|
| (no entries yet — first will land via A.2 T2) | — | — | — | — | — | — | — |
