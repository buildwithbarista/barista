# Barista worker IPC — protocol v1

This directory hosts the **wire-protocol schema** for the IPC between the
`barista` CLI (Rust) and the `barback` daemon (Java). The schema is the
single source of truth for both sides of the conversation; generated
bindings on each side track this file via build-time codegen.

The protocol's transport, framing, and security are described in PRD §12.
This README and the `.proto` files in this directory cover the **message
shapes only** — transport-level concerns (UDS / named-pipe selection,
socket permissions, length-prefix framing) live in the implementation
crates.

## Layout

| File | Purpose |
| --- | --- |
| `worker.proto` | The full protocol v1 schema — `Envelope`, action submission, streaming output, progress events, cancellation, status, errors, credentials envelope. |
| `README.md` | This file. |

## Package

```
syntax = "proto3";
package barista.v1;
option java_package = "com.bluminal.barista.barback.proto";
option java_multiple_files = true;
```

`Envelope` is the only top-level type; every other message rides inside
`Envelope.body` (a `oneof`). The wire format is a 4-byte big-endian
unsigned length prefix followed by exactly that many bytes of `Envelope`
payload.

## Generated bindings

The schema is shared; the bindings live next to the code that uses them.

| Side | Binding location | Codegen |
| --- | --- | --- |
| Rust (CLI, transport, conformance tests) | `crates/barista-ipc/src/proto.rs` | `prost`/`tonic` via `build.rs` |
| Java (barback daemon, embedded Maven core integration) | `barback/src/main/java/com/bluminal/barista/barback/proto/` | `protoc-gen-java` wired into the barback Maven build |

Neither generated tree exists at the time this README is authored —
binding-generation lands in subsequent tasks of the same milestone. The
schema is intentionally finalized first so the two binding generators
work from a frozen contract.

### Generating Java sources by hand

```
protoc \
  --java_out=/tmp/proto-out-java \
  --proto_path=proto \
  proto/barista/v1/worker.proto
```

### Generating Rust sources by hand

The Rust crate uses `prost-build` from its `build.rs`. To exercise the
schema without invoking the full Cargo build:

```
protoc \
  --descriptor_set_out=/dev/null \
  --proto_path=proto \
  proto/barista/v1/worker.proto
```

Exit code `0` indicates the schema parses cleanly. This is the
syntax-only smoke check used during schema authoring; the
binding-generation tasks add Cargo-driven verification end-to-end.

A reusable script for the syntax-only smoke check lives at
[`tests/verify-schema.sh`](tests/verify-schema.sh).

## Schema-evolution policy

Wire-protocol changes are not "just code changes" — a bad change can
break every previously-shipped CLI talking to every newer daemon (and
vice versa). The rules below govern modifications.

### Stability rules

1. **Field tag numbers are permanent.** A tag, once shipped, MUST NOT
   be reused for a different field. Removed fields move to a `reserved`
   block in their parent message so a future contributor cannot
   accidentally collide.

2. **Names matter, even though the wire doesn't see them.** Renames
   churn downstream Rust prost types and Java POJOs for no wire
   benefit. Prefer adding a new field and deprecating the old one with
   a comment over renaming.

3. **Field types are part of the contract.** Changing `int32` to
   `int64`, or `string` to `bytes`, or swapping a singular for a
   `repeated`, is a backward-incompatible change even if proto3
   technically tolerates it for unset fields. Treat as a v2 change.

4. **Enum zero values are forever.** The `UNSPECIFIED` member of every
   enum is the proto3 default and MUST remain. Removing or renaming it
   silently breaks readers that haven't migrated.

### Forward-compatible changes (allowed on the v1 line)

- Adding a new field with a fresh tag number to an existing message.
- Adding a new variant to `Envelope.body` (fresh tag in the `oneof`).
- Adding a new top-level message type.
- Adding a new enum value (readers default to the zero value on
  unknown).
- Adding a new `map<K, V>` entry type for an existing free-form
  attribute map (the map shape itself is the wire contract).

These changes do not require a protocol-version bump. Mixed-version
peers ignore unknown fields per proto3 semantics; new fields appear as
zero-defaults on older readers.

### Backward-incompatible changes

- Removing a field (move it to `reserved` instead; keep the tag held).
- Changing a field's type (see rule 3 above).
- Reordering or removing `oneof` variants such that an old reader
  cannot tolerate the wire bytes.
- Changing message semantics (e.g. "credentials envelope now also
  carries a master-key reference") even if the wire shape is unchanged.

These changes require a new package — `barista.v2` lives in
`proto/barista/v2/` and is generated alongside v1 until v1 is sunset.
Version negotiation flows through `Envelope.version`; mismatched peers
respond with `Error{code: "BAR-PROTO-001"}` and close the connection
(PRD §12.9).

## Credentials envelope — security contract summary

Full contract lives in the comment block above
`CredentialsEnvelope` in `worker.proto`. Key invariants:

- **Decrypted at the boundary.** Entries hold the *plaintext* of any
  encrypted `<server><password>` from `settings.xml`. Encrypted
  ciphertext never crosses the wire. If decryption fails, the action
  aborts before reaching IPC.
- **Scoped per action.** The envelope is OPTIONAL on `ActionRequest`
  and MUST only be populated for mojos that need it (deploy, release,
  authenticated fetches). Compile, test, package mojos MUST NOT
  receive it.
- **Zero-after-use.** Send and receive buffers are zeroed once the
  message has been parsed. Implementation lives in M4.1 Task 5; the
  schema documents the contract so the binding-generation tasks know
  what they're producing types for.
- **Transport-level protection only.** The schema does not encrypt
  the wire. The 0600 UDS (Unix) / per-user-SID-DACL'd named pipe
  (Windows) is the sole protection in flight.

## Versioning

The current schema version is `v1`. Incompatible changes will spawn
`proto/barista/v2/` rather than mutating this directory.
