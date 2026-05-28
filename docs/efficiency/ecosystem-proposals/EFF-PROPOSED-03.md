---
id: EFF-PROPOSED-03
title: Lockfile-aware bulk artifact-fetch endpoint
severity: high
category: connection_churn
status: proposed-ecosystem
discovered_by: human-authored
optimization: O-PROTO-07
upstream_party: Sonatype (Maven Central), repo-manager vendors (Nexus, Artifactory)
impact:
  bytes_saved_per_build: 0
  requests_saved_per_build: 300
  connections_saved_per_build: 0
---

## Evidence

A cold build with a known dependency set downloads every artifact as a separate
GET. With a lockfile, the client knows the *exact* set of coordinates +
checksums it needs before it sends a single request — yet the protocol still
forces one request per artifact. HTTP/2 multiplexing (`O-PROTO-01`) amortizes the
connection cost, but each artifact is still an independent request/response with
its own headers and server-side lookup.

- The full fetch list is known up-front from the lockfile; the server could
  stream it as one response.
- Per-artifact request overhead (headers, routing, auth checks) is paid N times
  for what is logically one "give me this manifest's artifacts" operation.
- CI runners with cold caches are the worst case: thousands of artifacts, every
  build.

## Impact estimate

Per cold build: a manifest of ~300 artifacts collapses from ~300 requests to
**one** multipart/streamed response — **~300 fewer requests** (order of
magnitude; depends on graph size and warm-cache hit rate). Egress bytes are
roughly conserved (the artifacts still ship); the win is request count,
per-request overhead, and server-side lookup amplification. CI fleets that build
cold repeatedly are where this compounds at ecosystem scale.

## Proposed mitigation

Implements **O-PROTO-07** (PRD §18.3). Propose to Sonatype and the repo-manager
vendors a **lockfile-aware fetch endpoint**:

```
POST /v1/fetch-manifest      (body: barista lockfile or a compatible coord+checksum list)
  -> multipart/streamed response of the missing artifacts
```

The client sends the lockfile (or a minimal coordinate+checksum list); the server
streams the artifacts the client doesn't already have. Barista's lockfile
(PRD §7) is the natural input format and is already content-addressed, so
integrity is verifiable on arrival. Prototypable client-side behind a flag; the
server side is the cooperation ask.

## References

- PRD §18.3 — `O-PROTO-07` (proposed lockfile-aware artifact fetch).
- PRD §7 — the lockfile format this endpoint consumes.
- PRD §18.13 — proposed-ecosystem track and engagement process.
