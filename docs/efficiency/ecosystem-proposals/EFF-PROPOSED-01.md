---
id: EFF-PROPOSED-01
title: Single-request transitive dependency-resolution endpoint
severity: high
category: request_amplification
status: proposed-ecosystem
discovered_by: human-authored
optimization: O-PROTO-05
upstream_party: Sonatype (Maven Central), repo-manager vendors (Nexus, Artifactory, Cloudsmith)
impact:
  bytes_saved_per_build: 0
  requests_saved_per_build: 400
  connections_saved_per_build: 0
---

## Evidence

Resolving a dependency graph today is inherently chatty: a client must fetch a
POM, parse it, discover its dependencies and parent, fetch each of those POMs,
and repeat until the graph closes. A medium application (Spring Boot web app,
~40 direct dependencies) closes a transitive graph of several hundred POM +
`maven-metadata.xml` requests, each a separate HTTP round-trip. The work is
fundamentally a graph traversal the *server* could do in one pass over data it
already holds, but the protocol forces the *client* to drive it one node at a
time.

- Every POM fetch is a dependency edge the server could have followed itself.
- The traversal is strictly sequential at each depth level — latency compounds.
- The same popular POMs (`spring-boot-dependencies`, BOM imports) are re-fetched
  by every client in the ecosystem, billions of times over.

## Impact estimate

Per build: a graph of N nodes costs ~N POM requests plus metadata polls;
collapsing the traversal to **one request** removes the other N−1. For a
representative medium build that is **~400 fewer requests** (order of magnitude;
exact count is graph-dependent). The byte payload is roughly conserved (the
graph data still ships), so the win is request-count and handshake/latency, not
egress volume.

At ecosystem scale the lever is large: Maven Central serves on the order of
**1.5 trillion requests/year** (Sonatype, *2024 State of the Software Supply
Chain*). Resolution traffic is a substantial fraction of that. Even a
conservative reduction in the resolution-request class is a multi-hundred-billion
request/year reduction — framed as an order-of-magnitude opportunity, not a
measured figure, and contingent on adoption.

## Proposed mitigation

Implements **O-PROTO-05** (PRD §18.3). Work with Sonatype and the repo-manager
vendors to define and standardize a resolution endpoint:

```
GET /v1/dep-graph/{group}/{artifact}/{version}?scope=compile&strategy=nearest-wins
```

returning the full transitive closure (coordinates, POM metadata needed for
resolution, checksums) in a single response. The server performs the traversal
over its own index. The client side is prototypable behind a feature flag in
Barista's resolver (Barista already builds the closure internally, so the client
can validate a server response against its own traversal during rollout).

Proposal form: an RFC shared with the Maven Central / Sonatype infrastructure
team and the Nexus/Artifactory product teams; reference implementation offered
for the server side where feasible.

## References

- PRD §18.3 — `O-PROTO-05` (proposed dependency-graph batch endpoint).
- PRD §18.13 — proposed-ecosystem track and engagement process.
- Sonatype, *2024 State of the Software Supply Chain* — Maven Central request
  volume (~1.5T requests/year), the ecosystem-scale denominator for this estimate.
