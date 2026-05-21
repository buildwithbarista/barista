# Telemetry privacy contract

> **Status:** awaiting initial sign-off.
> **Audience:** the reviewer who signs the privacy gate before the HTTP
> transport is allowed to fire in any shipped configuration; future
> contributors adding or modifying telemetry events.
> **Scope:** the `barista-telemetry` crate (`crates/barista-telemetry/`)
> and the `[telemetry]` slice of `barista-config`. Nothing else in the
> tree opens sockets on the user's behalf for the purpose of telemetry.

This document is operator-grade. It is meant to be read top-to-bottom
before the [Privacy-review checklist](#privacy-review-checklist) below
is signed off, and re-read whenever any of the
[change-management triggers](#change-management) fire.

---

## 1. Scope and threat model

### 1.1 What "telemetry" means here

There are two completely separate things in the `barista-telemetry`
crate, and confusing them is the most common privacy mistake:

1. **Local structured logs** — the `tracing` subsystem
   (`crates/barista-telemetry/src/tracing.rs`). These are rendered to
   stderr (or another writer the embedding process picks) and **never
   leave the user's machine**. They are read by humans or by an IDE /
   AI agent attached to the user's own terminal stream.
2. **Remote telemetry events** — the `TelemetryEvent` enum
   (`crates/barista-telemetry/src/lib.rs`) and its HTTP transport
   (`crates/barista-telemetry/src/transport.rs`). These are the
   *only* things eligible to leave the process over the network.

Everything in this document is about path (2) unless explicitly called
out. Path (1) is governed by an entirely different rule, documented in
[§4 PII boundary](#4-pii-boundary).

### 1.2 Trust model

The threat model the crate is designed against:

| Trust boundary             | Trusted?           | Notes |
|---|---|---|
| The user's own machine     | yes                | local tracing logs live here; we write whatever is useful |
| The Barista process itself | yes                | `TelemetryEvent` constructed and serialized here |
| The network in between     | partially          | TLS protects in transit (rustls, no plaintext) |
| The receiving endpoint     | developer-controlled | aggregate-only ingestion; raw events not retained per PRD §20.1 |

What this design is **not** trying to defend against:

- A compromised local process — if the binary is malicious, all bets
  are off. Telemetry is not a sandbox.
- A user who modifies their own config to send arbitrary data —
  `TelemetryEvent` is `#[non_exhaustive]` and every textual field is
  `&'static str`, so even the user cannot inject runtime strings
  without recompiling the crate (see [§3](#3-event-catalog-audit)).
- Network observers seeing *that* a request was sent — `User-Agent`
  identifies the binary as `barista/<version>`, so the request is
  attributable on the wire whenever it does fire. Anonymity from a
  passive observer is not a goal.

### 1.3 The default posture

The default-built `TelemetrySettings`
(`crates/barista-telemetry/src/lib.rs:124`) is:

```rust
TelemetrySettings {
    enabled: false,
    endpoint: None,
    client_id: None,
    transport_enabled: false,
}
```

i.e. **fully off and unconfigured**. The crate has no API that flips
any of these implicitly. Out of the box, the transport path is
unreachable; the disabled-path no-op is pinned by
`tests/zero_network.rs::disabled_default_panic_sink_never_fires`.

---

## 2. The three guards

The HTTP transport
(`crates/barista-telemetry/src/transport.rs::HttpTelemetrySink::submit`,
lines 369–409) is gated by **three independent booleans**. All three
must be `true` before any HTTP request is constructed. The exact
short-circuit order is pinned in code:

```rust
if !self.settings.enabled { return; }              // guard 1
if !self.settings.transport_enabled { return; }    // guard 2
let endpoint = match self.settings.endpoint.as_deref() {
    Some(url) => url,
    None => return,                                // guard 3
};
```

### 2.1 Guard 1 — `enabled`

The user-facing opt-in. Documented at
`crates/barista-telemetry/src/lib.rs:134` and mirrored in
`barista_config::TelemetryConfig` at
`crates/barista-config/src/schema.rs:174`. Flipped on by the user via
`~/.barista/config.toml` (`[telemetry] enabled = true`) or via the
`BARISTA_TELEMETRY__ENABLED=1` environment variable
(`crates/barista-config/src/sources.rs:427`).

**What it means:** "the user has consented to telemetry collection
*in principle*." This is the bit a human flips. There is no upstream
mechanism that flips this without an explicit user action; the env-var
override is documented and the config field is `false` by default.

### 2.2 Guard 2 — `transport_enabled`

The **post-privacy-review go-live lever**. Documented at
`crates/barista-telemetry/src/lib.rs:146` and at
`crates/barista-config/src/schema.rs:186`. Configured via
`[telemetry] transport-enabled = true` or
`BARISTA_TELEMETRY__TRANSPORT_ENABLED=1`
(`crates/barista-config/src/sources.rs:431`).

**Why a separate guard exists.** A user who set `enabled = true` and
configured an `endpoint` would, in a naive design, immediately start
shipping events. We do not want that — we want the privacy posture
(what we send, where, when, in what shape) to be reviewed and signed
off *before* the transport is allowed to fire. `transport_enabled`
exists so that:

- The user opting in to telemetry is decoupled from the project
  shipping a ready-to-fire transport.
- The "I want to try telemetry" path can be developed and tested
  end-to-end without ever sending real bytes.
- A future regression that accidentally defaults `enabled = true`
  cannot start exfiltration on its own — the third guard would still
  be `false`.

The intent is that this defaults to `true` (or the field is dropped
entirely) only once **this document** is signed off **and** the
endpoint question (Q2 — see [§9](#9-endpoint-tbd-note)) is resolved.
Until both happen, the field is shipped as `false` in every
configuration source.

### 2.3 Guard 3 — `endpoint.is_some()`

A sanity check. If the user enabled telemetry and approved transport
but did not configure a destination, there is literally nowhere to
send. This is the cheap structural guard that prevents a
`HttpTelemetrySink` from POSTing to the empty string or to a default
URL we baked in (we do not bake one in — there is no compiled-in
fallback endpoint anywhere in the crate).

### 2.4 Why three and not one

If we collapsed these into a single `telemetry_active: bool`, then
*either* the user opting in *or* the maintainer flipping go-live
*or* the config supplying a URL would each be load-bearing on its
own. With three independent guards:

- The user-facing `enabled` lever is what users see and toggle.
- The maintainer-facing `transport_enabled` lever cannot be flipped
  by a config typo or an environment variable in a CI runner —
  unless the *user* has also set it, which is the explicit
  override path.
- Mis-configuration that leaves `endpoint = None` is a no-op, not a
  crash and not a redirect to anything.

Each combination of false bits is regression-tested:

- All-off (default):
  `tests/transport_stub.rs::disabled_sink_makes_zero_calls_across_every_event_variant`
- `enabled=true`, `transport_enabled=false`:
  `tests/transport_stub.rs::enabled_but_transport_off_makes_zero_calls`
- `enabled=true`, `transport_enabled=true`, `endpoint=None`:
  `tests/transport_stub.rs::enabled_no_endpoint_makes_zero_calls`
- All-on:
  `tests/transport_stub.rs::all_guards_open_posts_one_call_per_event_with_json_body`

---

## 3. Event catalog audit

`TelemetryEvent` is defined at
`crates/barista-telemetry/src/lib.rs:211–277`. It is
`#[non_exhaustive]` (forward-compat) and externally tagged on `kind`.
The v0.1 catalog has exactly five variants. Every textual field is
`&'static str`, which is the **type-level guarantee** that no
runtime-built string (no path, no error message, no CLI argument, no
GAV coordinate, no project name) can ever appear in a payload —
because such strings are not `'static`. A reviewer reading any call
site can confirm this at a glance: the value being passed must be a
literal known at compile time.

Catalog exhaustiveness is pinned by
`tests/event_shapes.rs::catalog_is_exhaustive` (lines 220–252). If a
sixth variant lands, that test fails until the fixture and the
expected-kinds list are extended, which forces this document to be
updated as part of [change-management](#8-change-management).

### 3.1 `CommandInvoked { name: &'static str }`

| Field | Type | Why it's safe | What was excluded |
|---|---|---|---|
| `name` | `&'static str` | Static subcommand label like `"pour"`, `"pull"`, `"grind"`. Cannot be derived from `argv` because it isn't `String`. | Full `argv`, flags, subcommand operands (paths, coordinates, configuration values). |

Wire shape (pinned at
`tests/event_shapes.rs::wire_shape_is_stable`):
`{"kind":"command_invoked","name":"pour"}`.

### 3.2 `BuildDuration { phase: &'static str, duration_ms: u64 }`

| Field | Type | Why it's safe | What was excluded |
|---|---|---|---|
| `phase` | `&'static str` | One of a closed set of phase labels (`"resolve"`, `"fetch"`, `"compile"`, `"action-dispatch"`). Compile-time literal. | Module identity, file paths, per-artifact timings. |
| `duration_ms` | `u64` | Aggregate wall-clock milliseconds. No identity. | Per-artifact durations, per-thread breakdowns. |

Wire shape: `{"kind":"build_duration","phase":"resolve","duration_ms":1240}`.

### 3.3 `ArtifactCount { category: &'static str, count: u64 }`

| Field | Type | Why it's safe | What was excluded |
|---|---|---|---|
| `category` | `&'static str` | Closed counter category (`"resolved-deps"`, `"fetched-artifacts"`). | The actual GAVs, file paths, repository origins, cache keys. |
| `count` | `u64` | Aggregate counter. | The list of things counted. |

**Naming note.** The label field is called `category` (not `kind`)
deliberately, so it does not collide with the serde external-tag
discriminator (also `kind`). Pinned by
`tests/event_shapes.rs::artifact_count_category_does_not_shadow_discriminator`.

Wire shape: `{"kind":"artifact_count","category":"resolved-deps","count":42}`.

### 3.4 `CacheHitMiss { hits: u64, misses: u64 }`

| Field | Type | Why it's safe | What was excluded |
|---|---|---|---|
| `hits` | `u64` | Aggregate counter. | Cache keys, GAVs, file paths, request URLs. |
| `misses` | `u64` | Aggregate counter. | Same. |

Wire shape: `{"kind":"cache_hit_miss","hits":100,"misses":3}`.

### 3.5 `ErrorCodeOnly { code: &'static str }`

| Field | Type | Why it's safe | What was excluded |
|---|---|---|---|
| `code` | `&'static str` | Stable `BAR-NNN` identifier from the static error catalog. | Free-form error message, backtrace, error chain, the path or coordinate that triggered it, the underlying cause string. |

Wire shape: `{"kind":"error_code_only","code":"BAR-001"}`.

### 3.6 What is *globally* absent from the catalog

Beyond the per-variant exclusions, these things have **no field
anywhere** to put them in:

- **CLI arguments / flags** (`args`, `argv`, `cli_args`, ...).
- **Error messages** (`message`, `msg`, `error_message`, ...).
- **File paths** (`path`, `file`, `filename`, `file_path`, ...).
- **Dependency identities** (`coord`, `gav`, `coordinates`,
  `groupid`, `artifactid`, ...).
- **Project identities** (`project`, `project_name`, ...).
- **Host identities** (`username`, `hostname`, `ip`, ...).
- **Secrets** (`secret`, `token`, `password`, `credential`, `env`).
- **URLs** (`url`) — endpoints belong in settings, never in event
  payloads.

This list is enforced as a test invariant — not just a code-review
checklist — by
`crates/barista-telemetry/tests/event_shapes.rs::no_event_field_names_carry_pii`.
The test serializes every variant to JSON, walks the key tree, and
case-folded-substring-matches against the forbidden-token list
(`FORBIDDEN_FIELD_TOKENS` at lines 66–88). Adding a new field whose
name contains any of those substrings — even with a prefix or suffix
like `error_message` or `file_path` — fails the test loudly. The
allowlist sanity counterpart
(`forbidden_token_check_accepts_legitimate_names`) prevents the
filter from being silently loosened to "accept everything".

---

## 4. PII boundary

The single most important distinction in this crate. **Local tracing
and remote telemetry have different privacy contracts. Conflating
them is a defect.**

### 4.1 Local tracing (`src/tracing.rs`) — paths/args/errors are OK

The `tracing` subsystem writes to a writer chosen at install time —
stderr by default. These bytes **never leave the user's machine**.
Per the module docstring at
`crates/barista-telemetry/src/tracing.rs:18–33`:

> Because local tracing logs **never leave the user's machine**,
> they MAY contain rich diagnostic context — file paths, full
> error messages, CLI arg values, dependency coordinates — that
> would be inappropriate to attach to a `TelemetryEvent`
> destined for the network transport.

So `tracing::info!(target = "barista::resolver", path = %p, "resolved
artifact")` with `p: &Path` is fine. `tracing::error!(?err, "build
failed")` with `err: anyhow::Error` carrying a full message is fine.
These are local diagnostics, read by the user or by tooling the user
attached to their own stream.

### 4.2 Remote telemetry (`TelemetryEvent`) — none of that

The `TelemetryEvent` catalog is the only thing eligible to traverse
the HTTP transport. Every textual field is `&'static str`, so by
construction:

- A path cannot be passed (paths are `String`/`PathBuf` at runtime).
- An error message cannot be passed (`Display`/`Debug` produce
  `String`).
- A CLI argument cannot be passed (parsed from `argv` into `String`).
- A coordinate cannot be passed (`Coordinates` is a runtime struct).

The test gate:
`crates/barista-telemetry/tests/event_shapes.rs::no_event_field_names_carry_pii`
(see [§3.6](#36-what-is-globally-absent-from-the-catalog)).

### 4.3 What the boundary looks like in practice

| Diagnostic | Where it lives | What can be attached |
|---|---|---|
| "resolved artifact at `/home/alice/.m2/...`" | `tracing::info!` only | full path, full coordinate |
| "build phase `resolve` took 1.24s" | `tracing::info!` AND `TelemetryEvent::BuildDuration` | locally: any context; remotely: only `phase` (static label) + `duration_ms` |
| "BAR-001: failed to fetch `org.springframework:spring-core:6.1.0` from `https://repo1.maven.org/...`: connection reset" | `tracing::error!` only carries the full message; `TelemetryEvent::ErrorCodeOnly { code: "BAR-001" }` is the only thing that may go to the wire | locally: full message + URL + coordinate; remotely: just the static `BAR-001` |

If a new diagnostic does not fit cleanly into one column, the answer
is: emit the rich form via `tracing`, and if you also want it on the
wire, ask whether the existing `TelemetryEvent` variants can carry
the aggregate form — not whether a new field can be added to thread
the rich form through.

---

## 5. Opt-in mechanics

For the transport to actually fire, *all three* of the following must
end up `true` in the resolved `TelemetrySettings` the
`HttpTelemetrySink` is constructed with. Each can be set by the user
in either the config file or the environment; the env-var path is
layered on top by `barista-config` before the telemetry crate sees a
value.

### 5.1 Config file (`~/.barista/config.toml`)

```toml
[telemetry]
enabled            = true            # guard 1
transport-enabled  = true            # guard 2 (post-privacy-review lever)
endpoint           = "https://..."   # guard 3
client-id          = "ci-001"        # OPTIONAL — no value is invented if absent
```

Schema: `barista_config::TelemetryConfig` at
`crates/barista-config/src/schema.rs:170–197`. Kebab-case keys,
`deny_unknown_fields` upstream. The four field names are the *only*
ones accepted in the `[telemetry]` table.

### 5.2 Environment variables

Mirroring `BARISTA_TELEMETRY__<FIELD>` overrides, defined in
`crates/barista-config/src/sources.rs:425–432`:

| Env var | Maps to |
|---|---|
| `BARISTA_TELEMETRY__ENABLED` | `telemetry.enabled` (bool) |
| `BARISTA_TELEMETRY__TRANSPORT_ENABLED` | `telemetry.transport-enabled` (bool) |
| `BARISTA_TELEMETRY__ENDPOINT` | `telemetry.endpoint` (string) |
| `BARISTA_TELEMETRY__CLIENT_ID` | `telemetry.client-id` (string) |

There is no other environment variable read by this crate. The crate
itself does not call `std::env::var` — the env-var layer is in
`barista-config`, and the telemetry crate only sees the resolved
struct.

### 5.3 `client_id` — what it is and is not

- It is an **opaque per-install identifier** that the operator can
  pin if they want to correlate events across runs.
- It is **never invented** by this crate. If `client_id` is `None`,
  no per-install ID is attached to outgoing events; the crate does
  not generate one and does not persist one to disk. This is pinned
  at `crates/barista-telemetry/src/lib.rs:80–82`:
  > **No identifier generation.** If `client_id` is `None`, none is
  > invented or persisted.
- It is **not** in the `TelemetryEvent` payload. (`client_id` is a
  settings field, not an event field.) Whether/how the transport
  surfaces it on the wire is a question to settle when Q2 lands —
  the current transport stub does *not* include it in the POST body
  or in a header. If a future revision wires it in, that change
  triggers re-review per [§8](#8-change-management).

---

## 6. Wire format

Pinned in `crates/barista-telemetry/src/transport.rs:22–30` and
asserted by
`crates/barista-telemetry/tests/transport_stub.rs::all_guards_open_posts_one_call_per_event_with_json_body`.

| Property | Value |
|---|---|
| Method | `POST` |
| URL | `<endpoint>` (from settings; no compiled-in default) |
| `Content-Type` | `application/json` |
| `User-Agent` | `barista/<crate-version>` (const at `transport.rs:56`) |
| Other headers | none (no `Cookie`, no `Authorization`, no `X-*`) |
| Body | JSON serialization of `TelemetryEvent`, externally tagged on `kind` |
| TLS | rustls (no plaintext fallback, no `native-tls`) |
| Timeout | 5 seconds, hard-coded (`REQUEST_TIMEOUT` at `transport.rs:60`) |
| Retries | none in v0.1; failures are counted into `dropped_count` and dropped |
| Batching | none in v0.1; one event = one request |
| Auth | none |
| Cookies | none — the `reqwest::blocking::Client` is built without a cookie store |
| Async surface | none — `reqwest::blocking` so the public API of this crate stays sync |

A typical body on the wire (the only thing in the request beyond the
URL, content-type, and user-agent):

```json
{"kind":"build_duration","phase":"resolve","duration_ms":1240}
```

**Error handling** (`transport.rs:33–38`): transport errors (timeout,
DNS, non-2xx, serialization) are counted via
`HttpTelemetrySink::dropped_count` and otherwise swallowed.
`submit` returns `()`. This is what makes telemetry safe to wire into
hot paths: a flaky endpoint cannot crash a build. Pinned by
`tests/transport_stub.rs::transport_errors_are_swallowed_and_counted`
and `tests/transport_stub.rs::invalid_url_returns_err_no_panic`.

---

## 7. Privacy-review checklist

A reviewer signing off goes through these item-by-item, ticking each
only after independently verifying against the code. Do not tick from
this document's summary — open the cited files and confirm.

- [ ] Confirmed `TelemetrySettings::default()` produces
      `{ enabled: false, endpoint: None, client_id: None,
      transport_enabled: false }`
      (`crates/barista-telemetry/src/lib.rs:161–172`; pinned by
      `default_settings_are_disabled`).
- [ ] Confirmed `TelemetryEvent` is `#[non_exhaustive]` and every
      textual field on every variant is `&'static str` — no `String`,
      no `&str` (non-`'static`), no `PathBuf`, no `Path`
      (`crates/barista-telemetry/src/lib.rs:211–277`).
- [ ] Confirmed the catalog is exactly the five variants
      `CommandInvoked`, `BuildDuration`, `ArtifactCount`,
      `CacheHitMiss`, `ErrorCodeOnly` — no more, no fewer — and that
      `tests/event_shapes.rs::catalog_is_exhaustive` enforces the
      list.
- [ ] Confirmed
      `tests/event_shapes.rs::no_event_field_names_carry_pii` is
      present, runs in `cargo test -p barista-telemetry`, and
      rejects the full `FORBIDDEN_FIELD_TOKENS` list including
      `args`, `message`, `path`, `coord`, `project`, `username`,
      `hostname`, `ip`, `secret`, `token`, `password`, `credential`,
      `env`, `url`.
- [ ] Confirmed all three guards (`enabled`, `transport_enabled`,
      `endpoint.is_some()`) are present and each individually
      short-circuits `HttpTelemetrySink::submit` before any
      serialization or network I/O
      (`crates/barista-telemetry/src/transport.rs:369–409`).
- [ ] Confirmed each guard has a dedicated zero-call test:
      `disabled_sink_makes_zero_calls_across_every_event_variant`,
      `enabled_but_transport_off_makes_zero_calls`,
      `enabled_no_endpoint_makes_zero_calls`
      (`crates/barista-telemetry/tests/transport_stub.rs`).
- [ ] Confirmed no compiled-in default endpoint exists anywhere in
      the crate. (`grep -R "https://" crates/barista-telemetry/src`
      yields nothing other than this doc / comments / test fixtures.)
- [ ] Confirmed `User-Agent` is exactly `barista/<crate-version>` and
      no other identifying headers are attached
      (`crates/barista-telemetry/src/transport.rs:56`, `:173–188`).
- [ ] Confirmed the `reqwest::blocking::Client` is built without a
      cookie store and without auth
      (`crates/barista-telemetry/src/transport.rs:164–171`).
- [ ] Confirmed the request timeout is 5 seconds and there are no
      retries in v0.1
      (`REQUEST_TIMEOUT` at `crates/barista-telemetry/src/transport.rs:60`;
      `submit` does not loop or retry on failure).
- [ ] Confirmed `client_id` is **not invented** when `None` — no
      filesystem write, no UUID generation, no machine-ID read
      (the crate has no `std::fs` calls in the emit chain and no
      `uuid`-style dependency; verified by `cargo tree -p
      barista-telemetry`).
- [ ] Confirmed local `tracing` logs (`src/tracing.rs`) write only to
      a caller-supplied writer (stderr by default) and contain no
      code path that posts to the network. The privacy boundary
      comment at lines 18–33 is present and accurate.
- [ ] Confirmed the `[telemetry]` config schema (kebab-case,
      `deny_unknown_fields` upstream) accepts exactly four keys:
      `enabled`, `endpoint`, `client-id`, `transport-enabled`, and no
      others (`crates/barista-config/src/schema.rs:170–197`).
- [ ] Confirmed the env-var override list is exactly
      `BARISTA_TELEMETRY__ENABLED`,
      `BARISTA_TELEMETRY__ENDPOINT`,
      `BARISTA_TELEMETRY__CLIENT_ID`,
      `BARISTA_TELEMETRY__TRANSPORT_ENABLED` and the telemetry crate
      itself does not call `std::env::var`
      (`crates/barista-config/src/sources.rs:425–432`).
- [ ] Confirmed Q2 (telemetry endpoint choice) is resolved — i.e.
      the value that will be shipped in `endpoint` is known, the
      operator of that endpoint is identified, and the
      retention/aggregation contract on the receiving side matches
      PRD §20.1 ("aggregate only; raw events not retained"). If Q2
      is not resolved, `transport_enabled` must remain `false` in
      every shipped configuration regardless of this doc's sign-off
      status (see [§9](#9-endpoint-tbd-note)).

---

## 8. Change management

Any of the following triggers a re-review of this document **before**
the change merges — not after.

### 8.1 Hard triggers (re-review required)

- A new `TelemetryEvent` variant.
- A new field on any existing `TelemetryEvent` variant.
- Changing the type of any field on a `TelemetryEvent` variant
  (especially `&'static str` → anything else).
- Relaxing or removing any of the three guards
  (`enabled`, `transport_enabled`, `endpoint.is_some()`).
- Adding a fourth field to `TelemetrySettings` that influences what
  is sent (e.g. a header, an auth token, a session ID).
- Adding any new request header to the HTTP transport.
- Adding auth, cookies, or a non-rustls TLS path.
- Introducing batching, retries, or any persistence of events to
  disk before send (each is a new privacy surface — what's queued,
  what's recovered after a crash).
- Adding a compiled-in default endpoint.
- Adding code in this crate (not `barista-config`) that calls
  `std::env::var`.
- Wiring `client_id` into the wire payload or a header.

### 8.2 Soft triggers (re-review recommended)

- Adding a new transport implementation alongside
  `ReqwestTransport`.
- Changing the `User-Agent` value.
- Changing the request timeout (the value is operationally
  significant — a long timeout extends the window in which the
  endpoint can correlate a request with the user's session).
- Loosening any of the test invariants in
  `tests/event_shapes.rs` or `tests/transport_stub.rs`.

### 8.3 The "is this a re-review?" rule of thumb

If your PR changes anything in `crates/barista-telemetry/src/` or in
the `[telemetry]` slice of `crates/barista-config/src/`, the default
answer is **yes, re-review this doc**. The bar to skip re-review is
"the change is provably orthogonal to what's documented here"
(e.g. a comment-only edit, a clippy-only refactor, a
non-telemetry-touching dependency bump).

---

## 9. Endpoint TBD note (Q2)

An open question remains for v0.1 — labelled here **Q2: telemetry
endpoint choice — self-hosted vs. Sentry/Honeycomb**. **Q2 is not
resolved as of this document's first revision.**

The v0.1 default for the shipped configuration is
`transport_enabled = false`. Flipping it to `true` in any shipped
configuration is gated on **both**:

1. **`[H]` sign-off on this document** (a release-gating acceptance
   criterion).
2. **Q2 resolution** — a concrete endpoint URL, an identified
   operator, and a written retention contract on the receiving side
   that matches PRD §20.1.

Either of those being absent means the lever stays at `false`. The
transport stub is fully implemented and tested precisely so that the
day Q2 lands, no new code needs to ship in this crate — only a
config-default change. That default change is itself a re-review
trigger per [§8](#8-change-management).

If Q2 remains unresolved at v0.1 release, this document remains the
gate; the no-op transport remains the default and the `[T]` AC "zero
network calls when telemetry disabled" continues to be the live
assertion.

---

## Appendix A — file map

For the reviewer who wants to read the source rather than this doc:

| Concern | File |
|---|---|
| Settings & three guards (definition) | `crates/barista-telemetry/src/lib.rs` |
| `TelemetryEvent` catalog | `crates/barista-telemetry/src/lib.rs` (lines 211–277) |
| Local tracing (separate from transport) | `crates/barista-telemetry/src/tracing.rs` |
| HTTP transport stub | `crates/barista-telemetry/src/transport.rs` |
| Three-guard short-circuit (the actual gate) | `crates/barista-telemetry/src/transport.rs` (lines 369–409) |
| Catalog PII test | `crates/barista-telemetry/tests/event_shapes.rs` |
| Zero-network tests (trait layer) | `crates/barista-telemetry/tests/zero_network.rs` |
| Zero-network tests (HTTP-sink layer) | `crates/barista-telemetry/tests/transport_stub.rs` |
| Config schema for `[telemetry]` | `crates/barista-config/src/schema.rs` (lines 170–197) |
| Env-var override mapping | `crates/barista-config/src/sources.rs` (lines 425–432, 560–578) |

## Appendix B — relevant commits

| Commit | What landed | Doc reference |
|---|---|---|
| T1 (telemetry plumbing) | Settings, handle, sink trait, `NullSink`, `PanicOnAccessSink`, disabled-path no-op | [§1.3](#13-the-default-posture), [§2.1](#21-guard-1--enabled) |
| `52e6904` (T2) | Expanded `TelemetryEvent` to the five PRD §20.2 variants; `no_event_field_names_carry_pii` | [§3](#3-event-catalog-audit), [§4.2](#42-remote-telemetry-telemetryevent--none-of-that) |
| `f983eb2` (T3) | `tracing` subscriber, JSON/human formats, privacy-boundary docstring | [§4.1](#41-local-tracing-srctracingrs--pathsargserrors-are-ok) |
| `1a1af8c` (T4) | HTTP transport stub, three-guard short-circuit, `transport_enabled` field, `MockHttpTransport` | [§2](#2-the-three-guards), [§6](#6-wire-format) |

## Appendix C — what this document is not

- It is **not** a privacy policy for users. That belongs in
  user-facing docs (the project README / book) and should be
  written for a non-engineer audience. This document is the
  internal contract that makes such a user-facing policy
  truthful.
- It is **not** a substitute for the test suite. Every claim made
  here is intended to be backed by a test cited in-line; if a claim
  here is not testable, that is a defect in the claim or the test
  suite, and either the claim is wrong or a new test is owed.
- It is **not** versioned independently. This doc tracks `main` in
  the `barista` repo; any tagged release should re-link the cited
  line numbers and re-run the [checklist](#privacy-review-checklist).
