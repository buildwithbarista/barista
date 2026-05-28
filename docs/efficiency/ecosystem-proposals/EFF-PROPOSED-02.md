---
id: EFF-PROPOSED-02
title: Signed delta snapshots of maven-metadata.xml
severity: medium
category: metadata_overfetch
status: proposed-ecosystem
discovered_by: human-authored
optimization: O-PROTO-06
upstream_party: Apache Maven team (metadata format), Sonatype (Maven Central)
impact:
  bytes_saved_per_build: 0
  requests_saved_per_build: 30
  connections_saved_per_build: 0
---

## Evidence

`maven-metadata.xml` describes the available versions of an artifact. Clients
poll it to discover `LATEST`/`RELEASE` and snapshot timestamps. The file is
re-fetched whole on every poll even when nothing has changed, and the polling is
unconditional in many client/plugin paths (see `EFF-2026-001`,
`EFF-2026-004`). There is no ecosystem-wide mechanism to learn *that* metadata
changed without re-pulling it.

- Metadata for popular artifacts changes rarely relative to how often it is
  polled across the ecosystem.
- A client that built yesterday re-pulls full metadata today to discover, in the
  common case, that nothing relevant changed.
- Conditional requests (`EFF-PROPOSED-05`) help per-endpoint, but a subscribe-to-
  changes model removes the poll entirely for unchanged artifacts.

## Impact estimate

Per build: avoiding redundant full-metadata pulls saves on the order of **tens of
requests** (graph-dependent) and the associated XML bytes. The bigger lever is
ecosystem-scale: metadata polling is a large, highly-redundant request class on
Maven Central (billions of requests/month per PRD §1). A delta/snapshot model
that lets clients fetch only changed entries — or skip the poll entirely — turns
a per-build, per-artifact poll into an occasional delta fetch. Order-of-magnitude
opportunity, contingent on client + server adoption; not a measured figure.

## Proposed mitigation

Implements **O-PROTO-06** (PRD §18.3). Propose to the Apache Maven team and
Sonatype an hourly/daily **signed snapshot of metadata deltas**: a compact,
signed manifest of which artifacts' metadata changed in a window, so clients can
(a) skip polling artifacts absent from the delta and (b) fetch only changed
entries. Signing preserves the integrity guarantees clients depend on.

Proposal form: an Apache Maven JIRA / dev-list RFC for the metadata-delta format,
co-developed with Sonatype for the Central serving side. Client side prototypable
in Barista behind a flag once a delta format exists.

## References

- PRD §18.3 — `O-PROTO-06` (proposed signed metadata snapshots).
- PRD §18.13 — proposed-ecosystem track and engagement process.
- `EFF-2026-001`, `EFF-2026-004` — the redundant-metadata-poll findings this
  proposal addresses at ecosystem scale.
