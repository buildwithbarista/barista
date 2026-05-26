# Ecosystem-change proposals (`EFF-PROPOSED-*`)

This directory holds the **proposed-ecosystem** track of the Resource Efficiency
program (PRD §18.13): inefficiencies that Barista *cannot* fix alone because they
need cooperation from Maven Central operators, repo-manager vendors (Sonatype
Nexus, JFrog Artifactory, Cloudsmith, GitHub Packages), or the Apache Maven team.

It is orthogonal to the [`../findings/`](../findings/) catalog, which tracks
inefficiencies Barista *can* fix unilaterally (`EFF-2026-NNN`). Entries here carry
`status: proposed-ecosystem` and a stable `EFF-PROPOSED-NN` id.

## Process

For each proposal (PRD §18.13):

1. **Quantify** the opportunity in concrete, transparently-sourced units —
   ecosystem-scale numbers are order-of-magnitude estimates with stated
   assumptions, never fabricated precision.
2. **Draft** the proposal as an Apache JIRA / dev-list RFC / vendor feature
   request, depending on where the change lands.
3. **Reach out** to the relevant party (operator-side outreach is logged
   out-of-band, not in this public repo).
4. **Prototype** the client side behind a feature flag where the change is
   split client/server; offer to assist with server prototypes.
5. **Track adoption**; once implemented upstream, fold the optimization into
   Barista and update the entry.

## Entries

| id | title | optimization | upstream party |
|----|-------|--------------|----------------|
| [EFF-PROPOSED-01](./EFF-PROPOSED-01.md) | Single-request transitive resolution endpoint | O-PROTO-05 | Sonatype + repo-manager vendors |
| [EFF-PROPOSED-02](./EFF-PROPOSED-02.md) | Signed delta snapshots of maven-metadata.xml | O-PROTO-06 | Apache Maven + Sonatype |
| [EFF-PROPOSED-03](./EFF-PROPOSED-03.md) | Lockfile-aware bulk artifact-fetch endpoint | O-PROTO-07 | Sonatype + repo-manager vendors |
| [EFF-PROPOSED-04](./EFF-PROPOSED-04.md) | Universal zstd/brotli content-encoding | O-XFER-02 | repo-manager vendors |
| [EFF-PROPOSED-05](./EFF-PROPOSED-05.md) | Standardize ETag/Last-Modified on metadata | O-REQ-02 | repo-manager vendors + Apache Maven |

These are aspirational and do **not** gate any release. They are the public
roadmap for ecosystem-scale change.

## Frontmatter

Entries reuse the [findings frontmatter schema](../findings/README.md#frontmatter-schema)
(`id`, `title`, `severity`, `category`, `status`, `discovered_by`, `impact`) plus
two proposal-specific fields: `optimization` (the PRD §18.3–§18.6 catalog id) and
`upstream_party` (who must cooperate). Body sections follow the same four-section
shape: Evidence, Impact estimate, Proposed mitigation, References.

## See also

- PRD §18.13 — proposed ecosystem-changes specification.
- [`../findings/`](../findings/) — the unilateral-fix catalog (`EFF-2026-NNN`).
