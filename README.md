# Barista

[![Security scanning](https://img.shields.io/badge/security%20scanning-active-brightgreen?logo=github)](SECURITY.md)

<!--
  Badge target rationale: the link points to `SECURITY.md` (which ships
  in this repo today) rather than the GitHub Security tab because the
  public repository hasn't been published yet — badges that resolve to a
  404 are worse than badges that link to a local file. When the repo
  goes public, retarget the link to `../../security` so it surfaces the
  GitHub Security tab. The shield image is a static shields.io badge;
  swap it for a live workflow-status badge (e.g.,
  `https://github.com/<org>/<repo>/actions/workflows/sast.yml/badge.svg`)
  once the SAST workflow is running against a public repo.
-->

A fast, fully Maven-compatible build tool for the JVM ecosystem.

## Status

Pre-release. **v0.1 is in active development.** Interfaces, command surfaces, and on-disk
formats may change without notice until the first tagged release.

## Why Barista

Barista is a drop-in replacement for `mvn`. It runs the same lifecycle phases, reads the same
`pom.xml` and `~/.m2/settings.xml`, talks to the same repositories, and works with the same
plugins, IDEs, and CI systems.

What's different is underneath. Barista resolves dependencies with a parallel, lock-aware
resolver, caches build artifacts in a content-addressed store, and keeps plugin JVMs warm
through a background daemon so plugin execution doesn't pay startup cost on every invocation.

It is deliberately frugal with the shared infrastructure the ecosystem depends on — Maven
Central, public mirrors, and corporate repository managers. Cached artifacts are fetched once
and reused; an optional remote cache lets teams share results across machines and CI.

The goal is that an existing Maven project works the day you switch, with no migration step,
no new build file, and no lock-in.

## What's in this repository

| Path | Description |
|---|---|
| `crates/` | The Rust workspace: `barista` CLI, dependency resolver, content-addressed cache, lockfile, POM parser, and supporting crates |
| `barback/` | The Java daemon that executes Maven mojos in long-lived JVMs |
| `roastery/` | The remote artifact cache server (REAPI CAS + native HTTP/2) |
| `proto/` | IPC protocol definitions shared between the CLI, daemon, and cache |
| `schema/` | Published JSON schemas for lockfiles and on-disk formats |
| `docs/` | Architecture notes, Maven compatibility notes, CI integration guides |
| `bench/` | Benchmark harnesses and fixtures (added as the workspace fills out) |
| `test-corpus/` | Real-world Maven projects used for compatibility and regression testing |

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

## Installation

Binary releases are not yet published. Once v0.1 ships, the primary installation path will be
Homebrew:

```sh
brew install buildwithbarista/tap/barista
```

Until then, see [Building from source](#building-from-source).

## Hello world

Once installed, a Barista build of an existing Maven project looks like this:

```sh
cd path/to/your-maven-project
barista pull        # resolve dependencies and warm the cache
barista compile     # compile sources
barista test        # run unit tests
barista package     # produce the jar/war/etc.
```

No `pom.xml` edits, no migration step. If the project builds with `mvn`, it builds with
`barista`.

## Building from source

Requires a recent stable Rust toolchain (and Cargo), plus JDK 17 or 21 and Maven for the
`barback` daemon.

```sh
cargo build --release                  # builds the CLI, resolver, roastery, etc.
mvn -f barback/pom.xml package         # builds the barback uberjar
```

The resulting `barista` binary is at `target/release/barista`.

## Documentation

Project documentation lives under [`docs/`](docs/):

- [`docs/arch/`](docs/arch/) — architecture: the resolver, the cache, the daemon protocol,
  on-disk formats.
- [`docs/compat/`](docs/compat/) — Maven compatibility: which behaviors are honored exactly,
  where Barista deviates, and how to detect each case.
- [`docs/ci/`](docs/ci/) — integration recipes for GitHub Actions, GitLab CI, Jenkins, and
  other common CI systems.
- [`docs/perf/`](docs/perf/) — benchmarking methodology and current numbers against `mvn`.

Some of these directories are still being populated as the corresponding components land.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, the commit and review process,
and how to propose changes.

## Security

If you believe you've found a security issue, please follow the disclosure process in
[SECURITY.md](SECURITY.md) rather than opening a public issue.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
