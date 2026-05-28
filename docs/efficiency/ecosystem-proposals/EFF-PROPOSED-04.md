---
id: EFF-PROPOSED-04
title: Universal zstd/brotli content-encoding support across repo-manager vendors
severity: medium
category: compression_absent
status: proposed-ecosystem
discovered_by: human-authored
optimization: O-XFER-02
upstream_party: JFrog (Artifactory), Cloudsmith, GitHub Packages, other repo-manager vendors
impact:
  bytes_saved_per_build: 120000
  requests_saved_per_build: 0
  connections_saved_per_build: 0
---

## Evidence

Text resources — POMs, `maven-metadata.xml`, and repo-manager REST/JSON
responses — are highly compressible (~70–85% with gzip, more with zstd/brotli).
gzip is universally supported and Barista now negotiates it (`EFF-2026-003`,
resolved). The *ecosystem* gap is the better codecs: zstd and brotli compress
this content materially more than gzip, but support is uneven across vendors.

- Sonatype Nexus 3.71+ already supports brotli and zstd content-encoding.
- JFrog Artifactory, Cloudsmith, GitHub Packages, and others do not uniformly
  advertise zstd/brotli for compressible content types.
- A client that advertises `Accept-Encoding: zstd, br, gzip` falls back to gzip
  against non-supporting servers — correct, but leaving the better-codec savings
  on the table for a large slice of ecosystem traffic.

## Impact estimate

Per build, the *incremental* win of zstd/brotli over gzip on the text-resource
portion is on the order of **another ~120 KiB** for a metadata/POM-heavy build
(zstd typically beats gzip by a further 10–30% on this XML/JSON shape). This is
on top of the gzip baseline already captured by `EFF-2026-003`. The number is a
representative estimate, not a measured figure; actual savings depend on payload
shapes and how much of a build's traffic hits a zstd-capable server. At
Maven-Central / large-mirror egress scale (petabytes/month per PRD §1), a
better-codec default on the compressible-content class is a meaningful egress
reduction for operators.

## Proposed mitigation

Completes the ecosystem side of **O-XFER-02** (PRD §18.4). The client side is a
unilateral Barista change (advertise `zstd, br, gzip`, sequenced after the
HTTP/2-default work — see `EFF-2026-003`). This proposal is the *vendor* ask:
track and request zstd (and brotli) content-encoding support for compressible
content types across the major repo managers, using Nexus 3.71+ as the existence
proof. Maintain a support matrix in this track and file per-vendor feature
requests.

Proposal form: per-vendor feature requests (Artifactory, Cloudsmith, GitHub
Packages product teams), plus a public support matrix so the ecosystem can track
adoption.

## References

- PRD §18.4 — `O-XFER-02` (zstd/brotli content negotiation).
- `EFF-2026-003` — the client-side gzip baseline (resolved); zstd/brotli is the
  follow-up this proposal tracks on the server side.
- PRD §18.13 — proposed-ecosystem track and engagement process.
