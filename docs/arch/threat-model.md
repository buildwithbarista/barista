# Barista threat model

This document is the security baseline for Barista v0.1. It enumerates the
assets Barista protects, the trust boundaries it draws, and — for each of the
five threat classes that matter most to a build tool — the **implemented**
mitigation, the attack scenario it defends against, and the residual risk that
remains for a later release.

The emphasis is on *grounding*: every claim below names the file and the
mechanism that backs it, so a reviewer can read the code and confirm the
behavior. Where v0.1 does **not** defend against something, that is stated
explicitly. A threat model that only lists wins is not useful.

The five threat classes covered:

1. [Cache poisoning](#1-cache-poisoning)
2. [Lockfile drift](#2-lockfile-drift)
3. [Dependency confusion](#3-dependency-confusion)
4. [Plugin trust](#4-plugin-trust)
5. [Credentials](#5-credentials)

A sixth section covers [roastery-specific threats](#6-roastery-specific-threats)
(the shared cache server), and the document closes with a [findings
table](#findings) and a list of [known deferrals](#known-deferrals-v02).

---

## Scope and assets

Barista is a Maven-compatible build tool. Its security-relevant job is to take
a project's declared dependency graph and produce **the same bytes Maven would
have produced**, without letting an attacker substitute different bytes anywhere
along the way. The assets it protects:

| Asset | What it is | Where it lives |
|---|---|---|
| Build-artifact integrity | The exact bytes of every JAR/POM/metadata file a build consumes | Local content-addressed store; the lockfile pins the expected digest |
| The lockfile as source of truth | The pinned, reviewable resolution result (`barista.lock`) | Project repo, under version control |
| The local cache | Per-developer content-addressed store of all fetched artifacts | `~/.barista/` (or configured cache root) |
| The shared cache (roastery) | Optional team-wide CAS + Maven mirror | An operator-run server |
| Developer credentials | Repository-deploy / SCM-push secrets used by `mvn deploy` and release goals | `settings.xml`; in-flight only across the CLI↔daemon IPC channel |

### Trust boundaries

```
                          UNTRUSTED NETWORK
   +---------------------------------------------------------------+
   |   Maven Central / mirrors / authenticated repositories        |
   +---------------------------------------------------------------+
        ^                                   ^
        | (B) HTTPS fetch + checksum        | (B) HTTPS fetch + checksum
        |     verify                        |     verify (server-side)
        |                                   |
   +----+-----------------------+     +-----+-------------------------------+
   |  barista CLI (Rust)        |     |  roastery (shared CAS server)       |
   |                            |     |  - bearer / mTLS auth on /v1/cas/*  |
   |  - content-addressed cache | (C) |  - digest-keyed storage (no path   |
   |  - lockfile validation     +<--->|    traversal)                       |
   |  - resolver                |     |  - upstream-on-miss fetch + verify  |
   +----+-----------------------+     +-------------------------------------+
        |
        | (A) framed IPC over a 0600 UDS (Unix) /
        |     per-user-SID DACL'd named pipe (Windows)
        |     -- carries CredentialsEnvelope for deploys
        v
   +----------------------------+
   |  barback daemon (Java)     |
   |  - embedded Maven core     |
   |  - executes mojos          |
   +----------------------------+

   TRUST BOUNDARIES:
     (A) CLI <-> barback daemon  -- local, same-user; OS perms are the only authn
     (B) CLI / roastery <-> upstream repos  -- untrusted bytes, integrity by digest
     (C) CLI <-> roastery  -- authenticated (when configured), integrity by digest
```

Boundary (A) is **same-host, same-user** by design: the daemon runs as the
developer and the only thing crossing the wire that the local filesystem
doesn't already expose is repository credentials. Boundaries (B) and (C) carry
**untrusted bytes** — the defense is that nothing is trusted until its SHA-256
matches an expectation.

---

## 1. Cache poisoning

**Threat.** An attacker substitutes malicious bytes for a legitimate artifact —
on disk in the local cache, in transit from an upstream repository, or relayed
through the roastery — so that a build links code the developer never reviewed.

**Attack scenarios.**

- A compromised or man-in-the-middled upstream serves a trojaned JAR under a
  legitimate coordinate.
- A local process with write access to the cache root rewrites a cached object
  in place.
- A buggy or malicious roastery returns bytes that don't match the digest the
  client asked for.

**Implemented mitigation.** Everything in the cache is **content-addressed by
SHA-256**, and the address *is* the integrity check.

- `crates/barista-cache/src/cas.rs` — the content-addressed store. `Cas::put`
  hashes the bytes with `sha2::Sha256`, derives the `ContentHash`, and stores
  the blob at `objects/<aa>/<full-hex>` keyed by that digest. Writes are
  atomic: bytes land in `tmp/<random>`, are hashed, then `rename(2)`-d into
  place; a concurrent writer of the same bytes loses the race harmlessly
  (identical bytes either way). Because the on-disk path is derived from the
  content hash, a tampered object simply ceases to be addressable under its old
  name — the next lookup misses rather than serving poisoned bytes.
- `crates/barista-cache/src/checksum.rs` — sidecar verification. Maven
  publishes `.sha256` (authoritative) and `.sha1` (advisory) sidecars.
  `verify` recomputes both over the downloaded bytes and aborts with
  `ChecksumError::Mismatch` on any disagreement. Conflicting sidecars (SHA-256
  matches but SHA-1 doesn't) are an error, never a silent SHA-256-wins.
  Missing sidecars yield `Verification::Unverified` so the layer can warn.
- `crates/barista-cache/src/source.rs` — the fetch→verify→persist pipeline.
  `fetch_and_cache` fetches the artifact and both sidecars concurrently, calls
  `checksum::verify`, and only then calls `Cas::put`. The roastery path
  (`try_roastery`) re-verifies the bytes against the sidecars *locally even
  though the roastery already verified server-side* (defense in depth — see the
  comment at the `checksum::verify` call), and additionally asserts that the
  digest the local CAS computed equals the digest it asked the roastery for. A
  `BAR-CAS-001` digest mismatch from the roastery is mapped to
  `RoasteryOutcome::DigestMismatch`, logged at ERROR, and the pipeline *falls
  through to direct upstream* rather than persisting the bad bytes.
- `roastery/src/storage/fs.rs` + `roastery/src/proto/barista.rs` — server-side
  verification on PUT. `FsCas::put` hashes the streamed body and compares it to
  the client-claimed `expected_digest`; on mismatch it returns
  `StorageError::DigestMismatch` and the `tempfile::NamedTempFile` `Drop`
  deletes the staging file, so **the store never gains a poisoned entry**. The
  HTTP layer (`cas_put_inner`) surfaces this as `400 BAR-CAS-001` with both
  digests populated.

**Residual risk / deferred.**

- **No consumer-side cryptographic signature verification.** Integrity is
  anchored to the SHA-256 the *upstream* published in its sidecar. If an
  upstream is compromised at the source — it serves a trojaned JAR *and* a
  matching `.sha256` — Barista will accept it: the bytes match the published
  digest. Defending against that requires verifying a publisher signature
  (e.g. PGP / Sigstore) against an out-of-band trust root; that is a v0.2 item
  (see [Known deferrals](#known-deferrals-v02)).
- **The local cache root is trusted to the OS.** Barista assumes the cache
  directory is only writable by the developer. It does not detect an attacker
  who has filesystem write access and rewrites an object *and* updates the
  index to match. The lockfile (next section) is the cross-check that catches
  on-disk drift at build time for *lockfile-pinned* artifacts.

---

## 2. Lockfile drift

**Threat.** The resolved dependency set silently diverges from what was
reviewed and committed — either because the source tree changed without the
lockfile being regenerated, or because a build host resolves something
different from what CI locked.

**Attack scenarios.**

- A dependency or version is added/changed in a POM but the committed
  `barista.lock` is stale, so a reviewer approves a graph that no longer
  matches reality.
- A CI run resolves against a different effective POM than the one the lockfile
  was generated from, producing a different artifact set than was reviewed.

**Implemented mitigation.** The lockfile carries a **project signature** and a
`--frozen` mode that turns any drift into a hard error.

- `crates/barista-lockfile/src/signature.rs` — `compute_signature` produces a
  SHA-256 over the canonicalized effective POMs of every reactor module.
  Modules are sorted by `groupId:artifactId`, each module's `RawPom` is encoded
  with deterministic bincode, and each is **length-prefixed** before hashing so
  two different partitions of the same byte stream can't collide. Any change to
  a coordinate, version, property, or dependency moves the signature (the test
  module exercises each of these).
- `crates/barista-lockfile/src/mode.rs` — `validate` / `validate_strict`
  compare the freshly-computed signature against the one stamped in the on-disk
  lockfile. `ValidationMode::Default` silently refreshes on mismatch;
  `ValidationMode::Frozen` (the `--frozen` / `--locked` CI mode) returns
  `ValidationError::Stale` so the build **fails loudly** instead of resolving
  something new.
- `crates/barista-lockfile/src/schema.rs` — the lockfile pins, per artifact,
  the `sha256` (and optional `sha1`), `size_bytes`, and `source_url`
  (`LockfileEntry`). The schema docstring states the design intent directly:
  the lockfile is "metadata sufficient to re-fetch every artifact (URL +
  checksums)" and to "validate that the same lockfile applies to the current
  source." It also snapshots the relevant `settings.xml` `mirrors` /
  `repositories` (`SettingsSnapshot`) so environment drift between the lock host
  and the build host is detectable.

Together, lockfile drift collapses to the cache-poisoning case: a `--frozen`
build that resolves to a lockfile-pinned artifact will reject any byte stream
whose SHA-256 doesn't match the pinned `sha256`.

**Residual risk / deferred.**

- **The lockfile itself is not signed.** Its authenticity is anchored to code
  review + version control, not a cryptographic signature. A reviewer who
  approves a malicious lockfile change (the digests are opaque hex) gets no
  automated warning beyond the human-readable diff
  (`crates/barista-lockfile/src/diff.rs`). This is the standard lockfile trust
  model (the same as Cargo/npm) and is intentional for v0.1.
- **`--frozen` is opt-in.** The default mode auto-refreshes a stale lockfile.
  Teams that want drift to be fatal must run CI with `--frozen`; this is the
  documented CI posture, not the default for local iteration.

---

## 3. Dependency confusion

**Threat.** An attacker publishes a malicious artifact under a coordinate the
victim intends to resolve from a *trusted* source (e.g. an internal repository),
to a *public* source the victim also consults — and gets the build to prefer
the attacker's copy.

**Attack scenarios.**

- An internal library `com.acme:secret-lib:1.0.0` is published to Maven Central
  by an attacker; the victim's build, configured to consult both Central and an
  internal repo, fetches the public (malicious) one.
- An attacker injects a higher-priority repository or mirror into resolution so
  that *all* coordinates resolve through an attacker-controlled host.

**Implemented mitigation.** Resolution source is **operator-configured, not
attacker-influenced**, and the integrity check is digest-based regardless of
which source served the bytes.

- Repository / mirror configuration comes from a fixed precedence chain, not
  from the artifacts being resolved. `crates/barista-config/src/lib.rs`
  documents the six-layer precedence (compiled defaults → user config → project
  config → `~/.m2/settings.xml` servers/mirrors/proxies → `BARISTA_*` env →
  CLI flags). An artifact's POM cannot inject a new higher-priority repository
  into this chain — a downloaded POM is data, not configuration.
- `crates/barista-cache/src/fetch.rs` — the fetcher resolves a coordinate to a
  concrete URL via `url_for_artifact` against a configured `default_upstream`
  (Maven Central by default), with per-call overrides supplied by the
  resolver/config, not by the artifact. The mirror/repository set is captured
  in `SettingsSnapshot` at lock time, so a build can detect when the configured
  source topology has changed since the lockfile was written.
- `crates/barista-cache/src/source.rs` — the upstream/mirror ordering. On a
  local-CAS miss the pipeline tries the configured roastery first (when one is
  set), then falls through to the direct upstream `Fetcher`. Crucially, the
  roastery path is **keyed by the SHA-256 from the upstream sidecar**: the
  client fetches the small `.sha256` sidecar from the upstream repository, asks
  the roastery for *that exact digest*, and re-verifies locally. A roastery
  that serves different bytes for the requested digest is caught
  (`RoasteryOutcome::DigestMismatch`) and bypassed. So even a malicious shared
  cache cannot substitute a different artifact for a coordinate whose digest the
  client already learned from the trusted upstream.
- `roastery/src/upstream/` — the roastery's own upstream-on-miss path. When the
  roastery is asked for a digest it doesn't have, it consults its configured
  upstream repositories *in operator-configured order*
  (`roastery/src/upstream/fetch.rs`: `for repo in &self.repos`) and verifies
  each candidate against the requested digest via `Cas::put`'s in-flight hash
  check; a repository that serves the wrong bytes is logged and the next is
  tried (`UpstreamError::DigestMismatch` → "falling through to next repo"). The
  attacker cannot make the roastery prefer a malicious source because the
  requested digest pins the acceptable bytes.

**Residual risk / deferred.**

- **No namespace/ownership policy.** Barista does not (yet) enforce "this group
  prefix must only resolve from this repository." If an operator configures
  both an internal repo and a public repo with overlapping coordinate
  namespaces and *no lockfile pin exists yet for that coordinate*, first
  resolution follows the configured precedence — it is the operator's
  responsibility to order sources and (preferably) commit a lockfile.
  Coordinate-scoped repository pinning is a candidate v0.2 hardening.
- **First-resolution trust-on-first-use.** The digest cross-check protects
  *re-resolution* of a coordinate whose digest is already known (from a sidecar
  or a lockfile). The very first resolution of a brand-new coordinate trusts the
  configured upstream's sidecar. This is the same TOFU posture as the
  cache-poisoning section and is mitigated in practice by committing the
  lockfile.

---

## 4. Plugin trust

**Threat.** Maven plugins execute arbitrary code inside the build. A malicious
or tampered plugin artifact is a direct path to code execution on the
developer's machine and in CI.

**Attack scenario.** An attacker substitutes a trojaned `maven-*-plugin` JAR,
or confuses a plugin coordinate the way [§3](#3-dependency-confusion) describes,
and the daemon loads it into the plugin classloader and runs it.

**Implemented mitigation (current posture — stated honestly).** In v0.1, **a
Maven plugin is an ordinary content-addressed artifact**, and it receives
exactly the same integrity guarantees as any other dependency — no more, no
less:

- A plugin's JAR is resolved, fetched, checksum-verified
  (`crates/barista-cache/src/checksum.rs`), and stored in the same
  content-addressed store as compile dependencies
  (`crates/barista-cache/src/cas.rs`). Its digest can be pinned in the lockfile
  (`crates/barista-lockfile/src/schema.rs`), so a `--frozen` build detects a
  changed plugin JAR exactly as it detects a changed library.
- The plugin's classpath is carried to the daemon as a distinct field —
  `plugin_classpath` on `ActionRequest` (`proto/barista/v1/worker.proto`), built
  by the CLI's action graph (`crates/barista-cli/src/action_graph/mod.rs`) —
  pointing at JARs in the content-addressed cache. The daemon constructs the
  Maven plugin classloader from those paths. (In the v0.1 happy path the action
  graph leaves `plugin_classpath` empty pending the M4.3 classpath wiring; the
  field and its CAS-backed integrity contract are in place.)

So: plugin **integrity** (the bytes are what their digest says) is defended by
the CAS + checksum + lockfile machinery. Plugin **authenticity** (the bytes
came from a publisher you trust) is **not** defended in v0.1.

**Residual risk / deferred — explicitly NOT defended in v0.1.**

- **No signed-plugin verification.** Barista does not verify a publisher
  signature on plugin artifacts. A plugin whose trojaned bytes ship with a
  matching upstream `.sha256` will be accepted and executed. This is a known
  v0.2 item.
- **No plugin sandboxing or capability restriction.** A plugin runs with the
  full privileges of the build (filesystem, network, the developer's
  environment). `ActionRequest.jvm_args` is validated against an allowlist
  (per the schema comment) but plugin code itself is unconstrained. Mojo
  scope is *trusted* once resolved.
- **Plugins inherit the dependency-confusion residual risk** from
  [§3](#3-dependency-confusion): no coordinate-scoped repository pinning for
  plugin coordinates either.

---

## 5. Credentials

**Threat.** Repository-deploy and SCM-push secrets (used by `mvn deploy`,
`maven-deploy-plugin`, `maven-release-plugin`) are exposed — read off the IPC
channel by another local user, left in memory, or leaked into logs.

**Attack scenarios.**

- Another local user reads the CLI↔daemon IPC channel while a `deploy` action
  carries credentials.
- A credential lingers in a reusable buffer and is later served to an unrelated
  request as "uninitialized" memory.
- A credential ends up in a log line or an error message.

**Implemented mitigation.**

**Finding #4 — the cross-referenced credential finding.** The **sole in-flight
protection** for credentials is the operating system's access control on the
IPC endpoint:

- **Unix:** a **mode-0600 (owner-only) Unix-domain socket**.
  `crates/barista-ipc/src/transport/uds.rs` — `bind_secure` binds the listener
  under a `0700` per-user directory and `chmod(2)`s the socket inode to `0600`;
  `connect_secure` runs a three-step ceremony: a pre-connect `stat(2)`
  verifying the inode is a socket owned by us with mode bits exactly `0600`
  (`crates/barista-ipc/src/auth/socket_path.rs`: `SOCKET_MODE = 0o600`,
  `RUN_DIR_MODE = 0o700`), then `connect(2)`, then a post-connect
  `getsockopt(SO_PEERCRED)` peer-UID check (`verify_peer_uid`) that closes the
  TOCTOU window if the inode is swapped between the `stat` and the `connect`.
- **Windows:** the equivalent is a **per-user-SID DACL'd named pipe**.
  `crates/barista-ipc/src/transport/pipe.rs` — `bind_secure` creates the pipe
  via `create_with_security_attributes_raw` with a hand-rolled DACL
  (`crates/barista-ipc/src/auth/dacl.rs`: `PipeDacl`) granting `FILE_ALL_ACCESS`
  to exactly the current process token's user SID plus `NT AUTHORITY\SYSTEM`.
  Every other principal — including other interactive users and other admins —
  gets `ERROR_ACCESS_DENIED` from `CreateFileW`. The explicit DACL is the
  security-bearing call (a NULL SD would inherit a default DACL that lets
  `Authenticated Users` probe or open the pipe).

The protocol layer is explicit that this is the *only* authentication it relies
on (`proto/barista/v1/worker.proto`, transport comment): "The kernel-enforced
permission is the *only* authentication this protocol relies on."

The `CredentialsEnvelope` contract (`proto/barista/v1/worker.proto`) layers
several additional rules on top of the OS-enforced channel:

- **Decrypt-at-boundary.** Entries carry the *decrypted* secret. If a
  `settings.xml` password was `{...}`-wrapped, it is run through the Maven
  master-password pipeline *before* the envelope is constructed; if decryption
  fails the action is aborted before IPC — **ciphertext never crosses the
  wire**. The CLI side enforces this: `build_deploy_credentials`
  (`crates/barista-cli/src/cmd/verify.rs`) refuses an encrypted password with
  `VerifyError::DeployAuthEncrypted`.
- **Scoped per action.** The envelope is OPTIONAL and is populated **only** for
  actions that demonstrably need it (`deploy` / `deploy-file` /
  `release:perform` / authenticated-mirror fetches). Compile/test/package mojos
  MUST NOT receive credentials. The CLI is the sole authority on scope; the
  daemon does not infer it.
- **Zero-after-use.** `crates/barista-ipc/src/auth/zeroize.rs` — the wire buffer
  is scrubbed after decode. `Transport::recv`
  (`crates/barista-ipc/src/transport/uds.rs` and `.../pipe.rs`) calls
  `BytesMut::zeroize_buffer` on **every** receive (not just credential-bearing
  frames — branching on contents would add latency and risk a discrimination
  bypass) so a credential-bearing buffer cannot be re-served from the codec's
  allocation pool as fresh memory. The generated `Credential` /
  `CredentialsEnvelope` / `SshKey` types derive `zeroize::ZeroizeOnDrop`, and
  `zeroize_envelope` is a belt-and-braces walk that scrubs the credential field
  even if a caller `mem::take`s it out before the message drops.
- **No logging.** Neither side may log envelope contents; diagnostics identify
  a credential by `server_id` only.

**Residual risk / deferred.**

- **No wire-level encryption on the IPC channel.** The schema explicitly states
  it carries no TLS-equivalent envelope encryption: "The 0600 UDS /
  per-user-SID-DACL'd named pipe ... is the sole protection in flight." This is
  acceptable because the channel is same-host, same-user. **Off-host transport
  of credentials (roastery deploys, taps) is out of scope for v0.1 and requires
  a different envelope shape with wire-level encryption** (called out in the
  schema and in [Known deferrals](#known-deferrals-v02)).
- **`bytes::Bytes` cannot be scrubbed in place.** The zeroizer's `Bytes` impl is
  a documented no-op (reference-counted shared memory cannot be safely
  overwritten). The recv path therefore uses `BytesMut` (uniquely owned); a
  conformance test pins that the production codec yields `BytesMut`, not
  `Bytes`, so the no-op branch is unreachable in production today. If a future
  streaming layer surfaces `Bytes` to the zeroizer, it fails *closed* (buffer
  not scrubbed) rather than silently — this is a known gap to watch when the
  streaming multiplex layer evolves.
- **Master-password decryption is itself a follow-up.** `decrypt_password`
  handles the un-wrapped and simple cases; full Maven master-password support is
  a documented follow-up. Encrypted credentials that can't be decrypted are
  *refused*, not sent — failing closed.

---

## 6. Roastery-specific threats

The roastery is the optional team-shared CAS + Maven mirror. It is the one
Barista component that listens on the network, so it gets its own threat
surface. It is the focus of a planned external penetration test (treat the
findings table's `pentest` rows as the tracking target for that engagement).

**Unauthenticated access.** The route topology splits public from protected
explicitly (`roastery/src/proto/barista.rs`): `public_router()` mounts only
`/v1/health` and `/v1/capabilities`; `protected_router()` mounts every CAS
endpoint (`/v1/cas/sha256/{digest}` GET/HEAD/PUT and `/v1/cas/missing`). The
operational `/healthz` probe is likewise public. The auth layer
(`roastery/src/auth/layer.rs`) wraps only the protected sub-router. When neither
bearer nor mTLS is configured the layer accepts requests as
`Principal::Anonymous` — but `ServerConfig::validate` (`roastery/src/config.rs`)
guarantees that anonymous mode is only reachable on a loopback bind, so a
network-exposed roastery must have auth configured.

**mTLS bypass.** `roastery/src/auth/mtls.rs` loads the operator CA bundle into a
rustls `WebPkiClientVerifier`; a client that presents no cert, or a cert that
doesn't chain to a configured root, is rejected *at the TLS layer* before the
request reaches axum. The auth layer only re-parses the already-validated leaf
to extract a subject string for logging.

- **Residual risk:** when *both* bearer and mTLS are configured, a valid bearer
  token is accepted without consulting the client cert
  (`roastery/src/auth/layer.rs`, `decide`). This is intentional (it lets a
  client transit a TLS-terminating load balancer with a bearer token) but means
  the two mechanisms are OR-ed, not AND-ed. Operators wanting strict mutual
  auth should configure mTLS only.
- **Residual risk / deferred:** the mTLS subject is captured for logging but
  **per-call principal-based authorization is not enforced** in v0.1. The auth
  layer attaches a `Principal` extension but handlers are identity-blind; RBAC
  reading that principal is a documented v0.2 item.

**Bearer-token attacks.** `roastery/src/auth/bearer.rs` stores only SHA-256
digests of tokens (plaintext is dropped at load), compares via
`subtle::ConstantTimeEq` (no byte-by-byte timing recovery), and returns a
single uniform `BAR-AUTH-001 "unauthorized"` body for every failure mode (no
header, wrong scheme, wrong token) so the response shape doesn't distinguish
"no token" from "bad token" (`roastery/src/auth/layer.rs`,
`unauthorized_response`).

- **Residual risk / deferred:** tokens load once at startup; revocation requires
  a restart (SIGHUP reload is a documented v0.2 follow-up).

**Path traversal.** The CAS is **digest-keyed**, and digests are validated as
canonical lowercase hex before they ever touch a path. `Digest::from_hex` (used
by `parse_digest` in `roastery/src/proto/barista.rs`) rejects anything that
isn't 64 lowercase hex chars; `FsCas::path_for`
(`roastery/src/storage/fs.rs`) then derives `<root>/cas/<2>/<62>` purely from
that validated hex. There is no attacker-controlled path segment — a request
like `../../etc/passwd` fails digest validation with a 400 long before any
filesystem access. The `list` prefix is validated the same way (lowercase hex,
length-capped).

**Replay.** CAS operations are **idempotent and content-addressed**, which
neutralizes replay at the data layer: a replayed PUT of bytes that already exist
is a no-op success (a SHA-256 collision is definitionally the same bytes); a
replayed GET returns the same immutable blob. There is no mutable state a replay
could corrupt. Authentication replay (a captured bearer token reused by a
network attacker) is mitigated by running over TLS; this is part of the planned
external pentest's scope.

- **Residual risk / deferred:** REAPI `GetTree` is intentionally
  **`UNIMPLEMENTED`** (`roastery/src/proto/reapi.rs`) — roastery is a flat CAS,
  not a Merkle store. Bytestream resumable writes and the REAPI compressed-blob
  variants are likewise `UNIMPLEMENTED` (`roastery/src/proto/reapi/resource.rs`).
  These are honest "not supported in v0.1" answers, not silent failures, but a
  client depending on them gets an error rather than a fallback.

---

## Findings

This table is the artifact the milestone gate ("no Critical/High findings
unresolved at release") tracks against. It is seeded with the credential finding
cross-referenced by the milestone (finding #4). The red-team tasks
(cache-poisoning, lockfile-drift) and the external penetration test append rows
here with a severity and a status.

**Severity:** Critical / High / Medium / Low.
**Status:** mitigated / accepted / deferred / open.

| # | Threat class | Finding | Severity | Status | Mitigation (file + mechanism) |
|---|---|---|---|---|---|
| 1 | Cache poisoning | Tampered/MITM'd artifact bytes substituted for a legitimate coordinate | High | mitigated | Content-addressed SHA-256 store (`crates/barista-cache/src/cas.rs`) + sidecar verify (`crates/barista-cache/src/checksum.rs`) + server-side PUT verify (`roastery/src/storage/fs.rs`, `BAR-CAS-001`) |
| 2 | Cache poisoning | No publisher-signature check; a compromised upstream serving JAR+matching sidecar is accepted | Medium | deferred (v0.2: consumer-side signature verification) | — |
| 3 | Lockfile drift | Resolved graph diverges from the reviewed/committed lockfile | High | mitigated | Project signature (`crates/barista-lockfile/src/signature.rs`) + `--frozen` reject (`crates/barista-lockfile/src/mode.rs`) + per-entry pinned `sha256` (`crates/barista-lockfile/src/schema.rs`) |
| 4 | Credentials | In-flight `mvn deploy` credentials readable by another local principal on the CLI↔daemon channel | High | mitigated | 0600 owner-only UDS (`crates/barista-ipc/src/transport/uds.rs`) / per-user-SID DACL'd named pipe (`crates/barista-ipc/src/transport/pipe.rs`, `crates/barista-ipc/src/auth/dacl.rs`); decrypt-at-boundary + scoped + zero-after-use `CredentialsEnvelope` (`proto/barista/v1/worker.proto`, `crates/barista-ipc/src/auth/zeroize.rs`) |
| 5 | Dependency confusion | Malicious artifact preferred over the intended trusted source | High | mitigated | Operator-configured source precedence (`crates/barista-config/src/lib.rs`) + digest-keyed roastery/upstream ordering (`crates/barista-cache/src/source.rs`, `roastery/src/upstream/fetch.rs`) |
| 6 | Dependency confusion | No coordinate-scoped repository pinning; first-resolution TOFU on a brand-new coordinate | Medium | accepted (mitigated in practice by committing a lockfile) | Lockfile pin (`crates/barista-lockfile/src/schema.rs`) |
| 7 | Plugin trust | Trojaned plugin JAR executed in the build | High | partially mitigated (integrity) / deferred (authenticity) | Integrity via CAS + checksum + lockfile (`crates/barista-cache/src/cas.rs`, `crates/barista-cache/src/checksum.rs`); signed-plugin verification deferred to v0.2 |
| 8 | Roastery | Unauthenticated network access to CAS endpoints | High | mitigated | Public/protected route split + auth layer; anonymous mode loopback-only (`roastery/src/proto/barista.rs`, `roastery/src/auth/layer.rs`, `roastery/src/config.rs`) |
| 9 | Roastery | Path traversal via the digest path segment | High | mitigated | Validated lowercase-hex digest before any path use (`roastery/src/proto/barista.rs` `parse_digest`, `roastery/src/storage/fs.rs` `path_for`) |
| 10 | Roastery | Bearer-token timing / oracle leakage | Medium | mitigated | SHA-256-only storage + constant-time compare + uniform 401 body (`roastery/src/auth/bearer.rs`, `roastery/src/auth/layer.rs`) |
| 11 | Roastery | mTLS principal not enforced for per-call authz; bearer OR mTLS (not AND) | Low | deferred (v0.2 RBAC) | Principal captured but handlers identity-blind (`roastery/src/auth/layer.rs`) |
| 12 | Roastery | External penetration test of the full network surface | — | open (planned external pentest) | — |

---

## Known deferrals (v0.2)

Collected here so the gaps are in one place. Each is a deliberate v0.1 scope
decision, grounded in a code comment or an unimplemented stub:

- **Consumer-side dependency signature verification.** v0.1 anchors integrity to
  the upstream-published SHA-256 sidecar, not a publisher signature. (Finding #2.)
- **Signed-plugin verification.** Plugins get artifact-integrity guarantees but
  no authenticity check. (Finding #7.)
- **Coordinate-scoped repository pinning** (namespace ownership policy) to harden
  dependency confusion beyond source precedence + lockfile TOFU. (Finding #6.)
- **Off-host credential transport with wire-level encryption.** The v0.1
  `CredentialsEnvelope` carries no envelope encryption and is valid only over the
  same-host 0600 UDS / DACL'd pipe; roastery deploys and taps need a different,
  encrypted envelope shape (`proto/barista/v1/worker.proto`).
- **Roastery RBAC.** A `Principal` is attached per request but handlers are
  identity-blind in v0.1; per-route ACLs reading the principal are v0.2
  (`roastery/src/auth/layer.rs`). (Finding #11.)
- **Bearer-token reload without restart** (SIGHUP) — v0.1 loads tokens once at
  startup (`roastery/src/auth/bearer.rs`).
- **REAPI `GetTree`, resumable bytestream writes, and compressed-blob variants**
  are `UNIMPLEMENTED` by design — roastery v0.1 is a flat CAS, not a Merkle
  store (`roastery/src/proto/reapi.rs`, `roastery/src/proto/reapi/resource.rs`).
- **`bytes::Bytes` zeroization** is a documented no-op; revisit if a future
  streaming layer surfaces `Bytes` (rather than the uniquely-owned `BytesMut`)
  to the credential-scrubbing path (`crates/barista-ipc/src/auth/zeroize.rs`).
