---
id: EFF-PROPOSED-05
title: Standardize ETag/Last-Modified on maven-metadata.xml endpoints
severity: medium
category: conditional_request_missing
status: proposed-ecosystem
discovered_by: human-authored
optimization: O-REQ-02
upstream_party: repo-manager vendors (Nexus, Artifactory, Cloudsmith), Apache Maven team
impact:
  bytes_saved_per_build: 60000
  requests_saved_per_build: 0
  connections_saved_per_build: 0
---

## Evidence

Conditional requests (`If-None-Match` / `If-Modified-Since`) let a client
revalidate a resource and receive a tiny `304 Not Modified` instead of the full
body. Barista issues conditional requests when it has a validator (`O-REQ-02`),
but their effectiveness depends on the *server* emitting `ETag` and/or
`Last-Modified` — and coverage on `maven-metadata.xml` endpoints is inconsistent
across repo managers.

- Maven Central emits validators for artifacts and metadata; some repo-manager
  configurations and proxy layers strip or omit them on metadata endpoints.
- Without a validator, every metadata revalidation degrades to a full re-download
  even when the content is unchanged.
- This interacts with `EFF-PROPOSED-02`: conditional requests are the
  lower-effort, no-new-format baseline; signed delta snapshots are the larger
  redesign.

## Impact estimate

Per build, reliable validators on metadata endpoints turn redundant full pulls
into `304`s, saving on the order of **~60 KiB** of otherwise-redundant metadata
bytes for a metadata-heavy build (representative estimate; depends on how many
metadata resources are revalidated and their sizes). Zero request savings (the
revalidation request still goes out) — the win is payload. At ecosystem scale,
metadata is a high-volume, high-redundancy class, so universal validator support
is a broad egress reduction for operators with no client-format change required.

## Proposed mitigation

Strengthens the ecosystem side of **O-REQ-02** (PRD §18.3). Propose that repo
managers emit stable `ETag` and `Last-Modified` headers on `maven-metadata.xml`
(and avoid stripping them in proxy/CDN layers). This is the lowest-friction
proposal in the track — it standardizes existing HTTP semantics rather than
introducing a new format — and unblocks the conditional-fetch path Barista
already implements.

Proposal form: per-vendor feature requests + a conformance note for proxy/CDN
configurations; a public support matrix tracking which endpoints emit validators.

## References

- PRD §18.3 — `O-REQ-02` (conditional fetches for maven-metadata.xml).
- `EFF-PROPOSED-02` — the larger signed-delta redesign this conditional-request
  baseline complements.
- PRD §18.13 — proposed-ecosystem track and engagement process.
- RFC 9110 §8.8 (ETag), §8.8.2 (Last-Modified) — the HTTP semantics being
  standardized across endpoints.
