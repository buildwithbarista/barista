# Barista

A fast, fully Maven-compatible build tool for the JVM ecosystem.

Barista is a drop-in replacement for `mvn`. It runs the same lifecycle phases, reads the
same `pom.xml` and `~/.m2/settings.xml`, talks to the same repositories, and works with the
same plugins, IDEs, and CI — but it resolves dependencies faster, caches build artifacts in a
content-addressed store, and is deliberately frugal with the shared infrastructure (Maven
Central, mirrors, corporate repository managers) that the ecosystem depends on.

## Status

Pre-release. **v0.1 is in active development.** Interfaces and on-disk formats may change.

## What's in this repository

| Path | Description |
|---|---|
| `crates/` | The Rust workspace: `barista` CLI, dependency resolver, content-addressed cache, lockfile, POM parser, and supporting crates |
| `barback/` | The Java daemon that executes Maven mojos in long-lived JVMs |
| `roastery/` | The remote artifact cache server (REAPI CAS + native HTTP/2) |
| `proto/`, `schema/` | IPC protocol definitions and published JSON schemas |
| `docs/` | Architecture notes, Maven compatibility notes, CI integration guides |

## Key concepts

- **`barista`** — the CLI. Run `barista compile`, `barista test`, `barista package`, etc.
- **`baristaw`** — the project wrapper, analogous to `mvnw`.
- **`barback`** — a background daemon that keeps warm JVMs and class loaders so plugin
  execution doesn't pay JVM startup on every invocation.
- **`roastery`** — an optional remote cache server. Artifacts are sourced from a roastery (or
  directly from Maven Central) before they land in the local content-addressed cache at
  `~/.barista/cache`.
- **`~/.m2/repository`** — still populated (as hardlinks into the CAS where possible) so that
  `mvn` and `barista` coexist cleanly.

## Building from source

Requires a recent stable Rust toolchain (and Cargo), plus JDK 17 or 21 and Maven for the
`barback` daemon.

```sh
cargo build --release                 # builds the CLI, resolver, roastery, etc.
mvn -f barback/pom.xml package         # builds the barback uberjar
```

(Detailed build and contribution instructions land alongside the first crates.)

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
