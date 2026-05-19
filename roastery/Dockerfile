# syntax=docker/dockerfile:1.7
#
# Multi-stage build for the `roastery` remote artifact-cache server.
#
# Build invocation (from the Cargo workspace root):
#
#   docker buildx build \
#       --platform linux/amd64 \
#       --tag roastery:dev \
#       --file roastery/Dockerfile \
#       --load \
#       .
#
# The `scripts/build-image.sh` helper wraps that invocation.
#
# Build args: none. The image is configured entirely at runtime via
# environment variables (`ROASTERY_BIND`, `ROASTERY_STORAGE_DIR`,
# `ROASTERY_*` — see roastery/README.md for the full list).
#
# Caching strategy:
#
#   Stage 1 ("planner" + "builder") copies the workspace manifests
#   and `Cargo.lock` first, builds a stub crate to populate the cargo
#   registry + target cache, then copies the real source and rebuilds.
#   This keeps the dependency-resolution layer cacheable across source
#   edits — only the final `cargo build` re-runs when an `.rs` file
#   changes, not the entire dep graph.
#
#   We hand-roll the layer split instead of pulling in `cargo-chef`
#   to avoid coupling the container build to a third-party toolchain
#   image. The trade-off is one extra rebuild when a `Cargo.toml`
#   churns; that's acceptable for a server we ship per-release.
#
# Probing:
#
#   Kubernetes probes hit the HTTP `/healthz` endpoint (added in
#   M5.1 T7) directly via `livenessProbe` / `readinessProbe`. We do
#   NOT set a `HEALTHCHECK` directive here — distroless images carry
#   no shell, so the `CMD` form would not execute, and the kube
#   probes supersede the Docker-level healthcheck anyway.
#
# What NOT to add:
#
#   - No shell, no debug tools (`sh`, `bash`, `curl`, `ps`, `cat`).
#     If you need a shell for debugging, base the runtime stage on
#     `gcr.io/distroless/cc-debian12:debug-nonroot` instead — that
#     variant carries busybox. Do not ship it to production.
#   - No additional system packages. The runtime image is intentionally
#     a small read-only filesystem; the only writeable directory is
#     `/var/lib/roastery`, mounted as a volume by the operator.
#   - No `USER root` re-entry. Distroless's `:nonroot` variant runs
#     as uid 65532 / gid 65532; the operator's volume mount must
#     match those ownership semantics.

# -----------------------------------------------------------------------------
# Stage 1 — builder
#
# Pinned to the Debian-bookworm Rust image at the channel declared in
# `.tool-versions` at the workspace root (1.88.0 — the stable
# toolchain CI exercises via `dtolnay/rust-toolchain@stable`). Bookworm
# ships the glibc the distroless `cc-debian12` runtime stage expects,
# so the resulting binary is ABI-compatible across the stage boundary.
#
# Note: `rust-toolchain.toml` declares 1.86.0, but `Cargo.lock` carries
# transitive dependencies (`time` 0.3.47 family) that gate on rustc
# 1.88.0. The container build pins to the higher version to match
# what CI's `cargo check --workspace` already uses; the workspace-
# level toolchain pin is tracked for a follow-up bump.
# -----------------------------------------------------------------------------
FROM rust:1.88.0-bookworm AS builder

# Build for the canonical Linux target. Multi-arch builds run this
# stage once per `--platform` value via buildx; the host emulates the
# target via QEMU when cross-building (the CI workflow wires
# `docker/setup-qemu-action` to enable that).
ARG TARGETPLATFORM
ARG TARGETARCH

WORKDIR /work

# Copy only the manifest files first so the dependency-build layer
# can be cached across source-only edits.
#
# `rust-toolchain.toml` is intentionally NOT copied: the base
# `rust:1.88.0-bookworm` image already carries the toolchain we want
# preinstalled, and a per-repo `rust-toolchain.toml` would trigger
# rustup to fetch and switch to the workspace-pinned channel (which
# is currently behind what `Cargo.lock` requires — see the comment
# at the top of this stage).
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ ./.cargo/

# The workspace declares every crate under `crates/` plus `roastery`,
# `xtask`. Copy every manifest so `cargo` can resolve the workspace
# graph. We copy `Cargo.toml` and any `build.rs` shims; real source
# is staged in the next layer.
COPY crates/ ./crates/
COPY xtask/ ./xtask/
COPY roastery/Cargo.toml roastery/build.rs ./roastery/

# Materialise a stub `roastery/src/main.rs` so cargo can complete a
# release build of the dependency graph. The real source overwrites
# this in the next layer.
RUN mkdir -p roastery/src \
    && echo 'fn main() {}' > roastery/src/main.rs \
    && echo '' > roastery/src/lib.rs

# Build dependencies only. `--locked` forces the build to honour
# `Cargo.lock` exactly (no implicit resolver upgrades inside the
# image build).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --release --locked -p roastery

# Stage real source and rebuild. The stub at `roastery/src/{main,lib}.rs`
# is overwritten by the real files here.
COPY roastery/src/ ./roastery/src/
COPY roastery/tests/ ./roastery/tests/

# Touch the manifest so cargo invalidates the stub-built artifact,
# then rebuild against the real source. The cache mounts re-attach
# the registry + target dirs populated by the previous layer, so
# only the changed crates (roastery + its first-party rebuilds)
# actually recompile.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    touch roastery/Cargo.toml \
    && cargo build --release --locked -p roastery \
    && cp /work/target/release/roastery /usr/local/bin/roastery

# -----------------------------------------------------------------------------
# Stage 2 — runtime
#
# Distroless cc-debian12 carries glibc + ca-certificates and nothing
# else. The `:nonroot` variant runs as uid 65532, which matches the
# uid the operator's `securityContext.runAsUser` should declare in
# the kube manifests.
# -----------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

# OCI image labels per https://github.com/opencontainers/image-spec.
# `org.opencontainers.image.source` is the canonical OSS repository;
# `revision` and `version` are stamped by the CI workflow via
# `docker/metadata-action`.
LABEL org.opencontainers.image.title="roastery" \
      org.opencontainers.image.description="Remote artifact cache server for the Barista build tool." \
      org.opencontainers.image.source="https://github.com/buildwithbarista/barista" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.vendor="buildwithbarista"

COPY --from=builder /usr/local/bin/roastery /usr/local/bin/roastery

# Default storage root. Matches `ServerConfig::from_env`'s default so
# operators can run the image with no env vars set and get a
# functional (in-memory-volume) server.
WORKDIR /var/lib/roastery

# Default barista-protocol port. Override via `ROASTERY_BIND`.
EXPOSE 7878

USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/roastery"]
