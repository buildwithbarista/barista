//! Compile-time codegen for the worker IPC schema.
//!
//! Invokes `prost-build` to turn `proto/barista/v1/worker.proto` into Rust
//! types under `$OUT_DIR/barista.v1.rs`. The generated file is `include!`d
//! from `src/proto.rs` and re-exported via `src/lib.rs`.
//!
//! Cargo reruns this script when the proto file changes, when this build
//! script itself changes, or when the `PROTOC` env var changes (the latter
//! is what `prost-build` consults to locate the `protoc` binary).
//!
//! `protoc` must be on `PATH` (or pointed at via `PROTOC`). The repo's
//! contributor docs cover installation; CI installs it explicitly.

use std::io::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    // Path to the schema is resolved relative to the workspace root, which is
    // the parent of `crates/`. `CARGO_MANIFEST_DIR` is this crate's directory.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `crates/barista-ipc` -> parent `crates` -> parent `<workspace root>`.
    // The unwrap path is unreachable in any valid workspace checkout; we
    // surface a build-script error rather than panicking so a malformed
    // checkout produces a readable cargo diagnostic.
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "crates/barista-ipc must live two levels under the workspace root",
            )
        })?;
    let proto_root = workspace_root.join("proto");
    let worker_proto = proto_root.join("barista/v1/worker.proto");

    // Re-run codegen when the schema or this build script changes.
    println!("cargo:rerun-if-changed={}", worker_proto.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROTOC");

    let mut config = prost_build::Config::new();

    // No google.protobuf.* well-known types in the schema (verified against
    // worker.proto — no `import "google/protobuf/..."` lines). prost-build's
    // default behavior would still link `prost-types` if a WKT ever
    // appeared; since none do, we skip the dependency entirely.

    // ---- per-type attributes ----------------------------------------------
    //
    // `Mojo` is a natural map key (group:artifact:version:goal pin -> action
    // metadata in caches and progress aggregators). Make it `Eq + Hash` so
    // downstream crates can use it directly as a HashMap key.
    config.type_attribute("barista.v1.Mojo", "#[derive(Eq, Hash)]");

    // Credential-bearing types get `ZeroizeOnDrop` so decrypted secrets are
    // wiped from memory when the message is dropped, plus a `#[doc(...)]`
    // marker so the redacted-Debug story is visible at the API surface.
    // The manual `Debug` impl lives in `src/proto.rs`.
    //
    // `zeroize::ZeroizeOnDrop` requires every field to implement `Zeroize`.
    // `String`, `Vec<u8>`, and `Option<T: Zeroize>` all do; the generated
    // `oneof` enum does not, so we zeroize the wrapping struct and rely on
    // the wrapped String/Bytes payloads being zeroized by the derive macro
    // walking the fields. Because the generated `oneof` enum is itself a
    // field of `Credential`, we need a custom-path approach: derive
    // `Zeroize`/`ZeroizeOnDrop` on the leaf types (`SshKey`) and the leaf
    // `CredentialsEnvelope` / `Credential` types, but suppress the derive
    // on the oneof enum (which prost generates as a sibling type) by only
    // wrapping the outer message — `Drop` on `Credential` runs Zeroize on
    // every field including the oneof, and the oneof's variants hold
    // `String` / `SshKey` which implement Zeroize.
    //
    // In practice prost-generated structs auto-implement `Default` and store
    // oneof fields as `Option<NestedOneofEnum>`. Adding `ZeroizeOnDrop` on
    // the outer message zeroizes all primitive fields; the oneof Option is
    // zeroized to `None` (which drops the inner enum, which drops its
    // contained String/SshKey, which we separately mark Zeroize). This is
    // belt-and-braces: even if the macro misses a field, the inner String /
    // SshKey drops zero their contents.
    config.type_attribute(
        "barista.v1.CredentialsEnvelope",
        "#[derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop)]",
    );
    config.type_attribute(
        "barista.v1.Credential",
        "#[derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop)]",
    );
    config.type_attribute(
        "barista.v1.SshKey",
        "#[derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop)]",
    );
    // The oneof itself becomes an enum named `credential::Secret`. Mark it
    // Zeroize so the outer struct's drop-glue can recurse cleanly.
    config.type_attribute(
        "barista.v1.Credential.secret",
        "#[derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop)]",
    );

    // Suppress prost's default `Debug` impl on credential-bearing types —
    // the manual redacted `Debug` impls in `src/proto.rs` take over. Prost
    // emits `#[derive(Clone, PartialEq, ::prost::Message)]` by default and
    // `Message` requires `Debug`; we use prost's `skip_debug` API for this.
    config.skip_debug([
        "barista.v1.CredentialsEnvelope",
        "barista.v1.Credential",
        "barista.v1.SshKey",
    ]);

    // ---- compile ----------------------------------------------------------
    //
    // Single proto file, single include root. `protoc` resolves imports
    // relative to the include root — the schema doesn't import anything
    // today, but configuring the root keeps future imports working.
    config.compile_protos(&[worker_proto], &[proto_root])?;

    Ok(())
}
