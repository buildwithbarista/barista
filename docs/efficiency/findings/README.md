# Efficiency findings catalog

This directory is the source-of-truth catalog for **Resource Efficiency findings** —
structured records of observed inefficiencies in Java build-tool network traffic, each
backed by evidence and a proposed mitigation. The catalog is the public face of the
Resource Efficiency program described in PRD §18: every entry has a stable
`EFF-2026-NNN` identifier, a lifecycle, an impact estimate, and a fix proposal.

Findings are authored two ways:

1. **Pipeline-emitted.** The `barista-netanalyze` crate parses HAR captures produced by
   `barista-netcap`, runs its analyzer registry, and writes draft markdown files to
   [`auto-generated/`](./auto-generated/). Drafts carry the placeholder id
   `EFF-2026-PENDING`. A human reviewer **promotes** a draft into this directory,
   assigning the next free `EFF-2026-NNN` and walking it through the lifecycle.
2. **Hand-authored.** Researchers, contributors, or out-of-band Claude Code analysis
   sessions write a finding directly into this directory with a real id from the start.
   The seed findings (`EFF-2026-001` through `EFF-2026-003`) are hand-authored — they
   document the categories the analyzer registry already covers, so the catalog has a
   non-empty starting state before real captures land in M B.1 T3.

The orthogonal `EFF-PROPOSED-*` track (ecosystem-scale changes that need upstream
cooperation) lives in [`../ecosystem-proposals/`](../ecosystem-proposals/) — see
PRD §18.13.

## Lifecycle

Every finding moves through one of these statuses (PRD §18.10). The `status:` frontmatter
field is the source of truth; CI gates introduced in M B.2 T3 and M B.6 will enforce the
transition graph below.

| Status                | Meaning                                                              |
|-----------------------|----------------------------------------------------------------------|
| `open`                | Discovered, not yet triaged. New findings start here.                |
| `accepted`            | Confirmed real and prioritized for work; an issue/PR is expected.    |
| `resolved`            | Fix is merged in code; the bench harness has not yet confirmed it.   |
| `proven`              | Bench harness (PRD §18.12) confirms the savings against a baseline.  |
| `wontfix`             | Accepted as inherent or out-of-scope; carries a written rationale.   |
| `proposed-ecosystem`  | Requires upstream cooperation; tracked under `../ecosystem-proposals/`. |

Permitted transitions: `open → accepted → resolved → proven`. From `open` or `accepted`
a finding may also move to `wontfix` or `proposed-ecosystem`. Anything else (e.g.
`proven → open`) is treated as a regression and requires opening a new finding.

## ID-assignment policy

Promotion from `auto-generated/` to this directory is the moment an id is allocated. The
canonical procedure:

1. List existing entries:
   ```bash
   ls docs/efficiency/findings/EFF-2026-*.md | sort -V | tail -n 1
   ```
2. Take the trailing `NNN`, add 1, zero-pad to three digits. (`EFF-2026-007` → `EFF-2026-008`.)
3. Rewrite the draft's `id:` frontmatter line, move the file, and commit.

The `cargo xtask findings promote <path>` subcommand automates all three steps
atomically and validates the frontmatter on the way through. Manual promotion is
supported (the procedure is just `mv` + `sed`) but using the xtask is preferred — it
will refuse to overwrite an existing id and produces a uniform commit-ready file.

`EFF-2026-NNN` ids are **never reused**, even if a finding is later deleted. The
year prefix flips to `EFF-2027-*` for findings first discovered in 2027.

## How to add a finding

### Pipeline-emitted (the common case)

1. Capture traffic for a corpus project with `barista-netcap`.
2. Run `barista-netanalyze --input session.har --output-dir docs/efficiency/findings/auto-generated/`.
3. Review the draft files. Discard noise; promote real findings with
   `cargo xtask findings promote docs/efficiency/findings/auto-generated/<file>.md`.

The crate-internal contract for the draft shape (frontmatter + body sections) is
documented in the `barista-netanalyze` crate root; once that crate ships a README it
will mirror the schema here. Until then, the authoritative reference is
`crates/barista-netanalyze/src/finding.rs` (the `Finding::to_markdown` method).

### Hand-authored

1. Allocate the next free id (see above).
2. Copy an existing finding (e.g. `EFF-2026-001.md`) as your template — preserve the
   frontmatter shape exactly.
3. Set `discovered_by: human-authored` (or `claude-analysis` if the finding came from
   an out-of-band Claude Code session that did **not** use `barista-netanalyze` —
   see the *Provenance* section).
4. Fill the four required body sections: `## Evidence`, `## Impact estimate`,
   `## Proposed mitigation`, `## References`.

`cargo xtask findings list` will pick the new file up immediately — no registry to
update.

## Frontmatter schema

Required fields on every finding:

| Field            | Type    | Notes                                                       |
|------------------|---------|-------------------------------------------------------------|
| `id`             | string  | `EFF-2026-NNN` (drafts carry `EFF-2026-PENDING`).           |
| `title`          | string  | One-line summary; YAML-quote if it contains `:`.            |
| `severity`       | enum    | `low` &#124; `medium` &#124; `high` &#124; `critical`.      |
| `category`       | enum    | Analyzer family — see `Category` in `barista-netanalyze`.   |
| `status`         | enum    | See lifecycle table above.                                  |
| `discovered_by`  | string  | Provenance — see below.                                     |
| `impact`         | mapping | `bytes_saved_per_build`, `requests_saved_per_build`, `connections_saved_per_build`. |

Required body sections (in order, level-2 headings):

1. `## Evidence` — bullet list of HAR entries or other concrete observations.
2. `## Impact estimate` — restate the impact numbers in prose with units.
3. `## Proposed mitigation` — free-form markdown; cite the optimization catalog
   identifier from PRD §18.3–§18.6 (`O-REQ-*`, `O-XFER-*`, `O-PROTO-*`, `O-COMP-*`).
4. `## References` — bullet list of URLs and PRD anchors.

### Provenance: `discovered_by` convention

The `discovered_by` field traces a finding back to its origin. The catalog uses three
classes of value:

| Value pattern                  | Used when                                                              |
|--------------------------------|------------------------------------------------------------------------|
| `<AnalyzerName>`               | An automated analyzer in `barista-netanalyze` emitted the draft.       |
| `human-authored`               | A human wrote the finding directly (the seed cohort uses this).        |
| `claude-analysis`              | An out-of-band Claude Code session produced the finding without going through `barista-netanalyze`. |

Auto-generated drafts use the analyzer's stable id (e.g. `MetadataOverFetchAnalyzer`,
`ConnectionChurnAnalyzer`, `UncompressedTransferAnalyzer`) rather than a generic
`claude-analysis` label, so the catalog can be grepped after a rule change to find
every finding the changed analyzer produced. The PRD §18.10 example uses
`claude-analysis` as illustrative shorthand for the third row; this catalog refines it
into three distinguishable cases for traceability.

## How to advance lifecycle

Today the lifecycle is advanced by editing the `status:` field and committing. The
intent is for the M B.2 T3 catalog-validation CI job (and the broader M B.6 program
gates) to enforce the transition graph: a PR that flips `open → proven` without going
through the intermediate states will be flagged. Until that lands, follow the
transition table above by convention.

When marking a finding `proven`, link the bench result in the `## References` section
so the dashboard can pick it up.

## Tooling

| Command                                           | What it does                                                       |
|---------------------------------------------------|--------------------------------------------------------------------|
| `cargo xtask findings list`                       | Print every catalog entry (id, title, severity, category, status, discovered-by) in a table. |
| `cargo xtask findings promote <path>`             | Move a draft from `auto-generated/` into this directory, allocating the next free id and validating frontmatter + body sections. |

## See also

- PRD §18.10 — finding-catalog format specification.
- PRD §18.9 — the analysis pipeline that produces drafts.
- PRD §18.13 — the ecosystem-proposals track (`EFF-PROPOSED-*`).
- `crates/barista-netanalyze/` — the analyzer registry that emits drafts.
- `crates/barista-netcap/` — the capture harness that produces the HARs the analyzers consume.
- [`../ecosystem-proposals/`](../ecosystem-proposals/) — orthogonal upstream-cooperation track.
