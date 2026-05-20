# Security deferrals and accepted residual risks (v0.1)

This document records the disposition of every entry in the
[threat model](./threat-model.md) findings table as of the v0.1 release
gate: which findings are **resolved**, which are **accepted residual risks**
with a documented rationale and a v0.2 remediation plan, and which work is
**deferred** (notably the external penetration test).

It is the companion to the threat model's findings table — the threat model
says *what* the findings are; this document records the *release decision*
for each.

## Release-gate summary

The v0.1 security gate is **"no Critical or High finding left unresolved
without an accepted, documented disposition."**

- **Critical findings:** none.
- **High findings:** six are mitigated and red-team-verified or
  mechanism-verified (#1, #3, #4, #5, #8, #9). One — plugin **authenticity**
  (#7) — is an **accepted residual** for v0.1: artifact *integrity* is
  mitigated, *authenticity* (signed-plugin verification) is scheduled for
  v0.2 and disclosed in the release notes (see below).
- **Medium / Low findings:** all are accepted residuals with rationale
  (#2, #6, #10, #11).
- **Open work:** the external penetration test (#12) is pending vendor
  engagement; the in-house red-team suites (findings #1–#3) stand in for it
  at v0.1 and the external test is a disclosed deferral.

No finding is silently closed. Every High-or-above either has a verified
mitigation or appears below with an explicit accept/defer decision.

## Resolved (no further v0.1 action)

| # | Finding | Why it is resolved for v0.1 |
|---|---|---|
| 1 | Cache poisoning — tampered artifact bytes | Content-addressed SHA-256 store + sidecar verify + server-side PUT verify; red-team-verified (`crates/barista-cache/tests/redteam_cache_poison.rs`) that a rejected fetch persists nothing. |
| 3 | Lockfile drift | `--frozen` rejects source-tree drift + forged/corrupted/missing project signature; red-team-verified (`crates/barista-lockfile/tests/redteam_lockfile_drift.rs`). |
| 4 | In-flight credentials on the CLI↔daemon channel | 0600 owner-only UDS / per-user-SID DACL'd named pipe + decrypt-at-boundary, scoped, zero-after-use `CredentialsEnvelope`. |
| 5 | Dependency confusion — malicious source preferred | Operator-configured source precedence + digest-keyed roastery/upstream ordering. |
| 8 | Roastery — unauthenticated CAS access | Public/protected route split + auth layer; anonymous mode is loopback-only and a non-loopback bind without auth fails closed at startup. |
| 9 | Roastery — path traversal via digest segment | Digest is validated lowercase-hex before any filesystem path is constructed. |

## Accepted residual risks (documented; carried into v0.1)

### #7 — Plugin trust: authenticity deferred (High)
**Decision: accept for v0.1, remediate in v0.2, disclose in release notes.**

Maven plugins are resolved and executed like any other artifact, so they
inherit the same **integrity** guarantees as every dependency: content-
addressed storage, checksum verification, and lockfile pinning
(`crates/barista-cache/src/cas.rs`, `crates/barista-cache/src/checksum.rs`).
What v0.1 does **not** provide is plugin **authenticity** — there is no
publisher-signature verification, so a plugin that is byte-for-byte what the
(possibly compromised) upstream published will execute. This is the same
trust-on-first-use boundary as finding #2, applied to executable plugin code,
which is why it is rated High rather than Medium.

- **v0.1 mitigations in force:** integrity (CAS + checksum + lockfile pin),
  and the operator-controlled source precedence (#5) that limits *where*
  plugins can come from.
- **Residual:** a compromised-but-self-consistent upstream can ship a
  trojaned plugin.
- **v0.2 remediation:** signed-plugin verification (consumer-side signature
  checking), tracked alongside the consumer-side signature work for #2.
- **Disclosure:** called out in the v0.1 release notes so operators can
  decide their plugin-source trust posture (e.g. pin plugins to a vetted
  internal mirror).

### #2 — Cache poisoning: coordinated upstream artifact+sidecar swap (Medium)
Checksum verification is trust-on-first-use against the upstream-published
sidecar, so an upstream that swaps **both** the artifact and its `.sha256`
to a self-consistent attacker blob is accepted. The real defenses are the
committed lockfile pin plus downstream content-addressed re-verify (#1). The
boundary is asserted honestly by
`redteam_cache_poison.rs::coordinated_upstream_swap_is_accepted_documented_tofu_residual`.
**v0.2 remediation:** consumer-side publisher-signature verification.

### #6 — Dependency confusion: no coordinate-scoped repo pinning (Medium)
First resolution of a brand-new coordinate is trust-on-first-use; there is no
per-coordinate namespace-to-repository binding. **Mitigated in practice** by
committing a lockfile (which pins the resolved source + digest). v0.2 may add
explicit coordinate-scoped repository pinning.

### #10 — Roastery: bearer-token timing/oracle leakage (Medium)
Mitigated: tokens are stored only as SHA-256 hashes, compared in constant time
(`subtle::ConstantTimeEq`), and all failures return a uniform 401 body. Listed
as a residual only because token auth over a network is inherently a higher-
exposure surface than the loopback IPC channel; no further v0.1 action.

### #11 — Roastery: mTLS principal not enforced per-call (Low)
The authenticated `Principal` is captured but handlers are identity-blind, and
bearer/mTLS are OR-ed rather than AND-ed. There is no per-call RBAC in v0.1.
**v0.2 remediation:** RBAC using the already-captured `Principal`.

## Deferred work

### #12 — External penetration test of the roastery network surface
The external penetration test (mTLS bypass, path traversal, replay, unauth
access) requires a third-party security firm. For v0.1:

- The **in-house red-team suites** (findings #1–#3, plus the roastery auth +
  path-traversal mechanism tests in the roastery crate) provide adversarial
  coverage of the highest-severity surfaces.
- The **external penetration test is a disclosed deferral** in the v0.1
  release notes; it remains scheduled and its sign-off is a release gate that
  is tracked separately from this document.

Until the external review is signed off, v0.1 should be described as having
had an **internal** security review with a **pending** external penetration
test — not as having passed an independent audit.

## How to extend this document

When the external penetration test or any future review produces a finding:

1. Add it to the [threat model](./threat-model.md) findings table with a
   severity and status.
2. If it is Critical/High, it must be either fixed before the release tag or
   given an explicit accept/defer decision **here** with a rationale and a
   remediation plan.
3. Medium/Low findings are recorded here as accepted residuals.

A release tag must not be cut while a Critical or High finding has neither a
verified mitigation nor an accepted, documented disposition in this file.
