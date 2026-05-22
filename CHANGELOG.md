# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.1] - 2026-05-21

First public alpha. Barista is a fast, Maven-compatible build tool for the JVM — a drop-in
`mvn` replacement that reads the same `pom.xml` and `~/.m2/settings.xml`, runs the same
lifecycle phases, and works with existing plugins, repositories, IDEs, and CI.

This is an early alpha: it has been exercised on a limited set of projects and has not yet
been hardened against the diversity of real-world Maven builds. Expect rough edges; it is not
yet recommended for production use. Feedback and bug reports are welcome.

### Added

- **CLI (`barista`)** with Maven lifecycle drop-ins — `compile`, `test`, `package`, `verify`,
  `install`, `deploy` — alongside Barista-native commands: `pull` (lock-aware resolve +
  fetch), `grind tree` (render the resolved dependency tree), `pour`, `dial-in`, `tap`
  (register / inspect / health-probe remote cache + worker endpoints), and `wrapper`
  (generate `baristaw` project wrapper scripts).
- **Dependency resolution** via a parallel, lock-aware resolver backed by a committed
  lockfile and a content-addressed cache at `~/.barista/cache`. `~/.m2/repository` is still
  populated (hardlinked into the cache where possible) so `mvn` and `barista` coexist cleanly.
- **`barback` daemon** — keeps warm JVMs and class loaders so Maven mojo execution does not
  pay JVM startup cost on every invocation. Bundled into the binary releases at
  `share/barista/barback-uber.jar`.
- **`roastery`** — optional remote artifact-cache server (REAPI CAS over native HTTP/2),
  published as a multi-arch container image and shipped alongside the CLI.
- **Maven compatibility** — `--maven-compat` selects `3.9` / `4.0` / `auto` behavior; reads
  `pom.xml`, profiles, and `~/.m2/settings.xml`. A bundled Maven 4 distribution ships in the
  release archives.
- **Supply-chain-hardened releases** — reproducible Linux binaries (byte-identical across two
  independent builds), macOS binaries signed with a Developer ID certificate and notarized by
  Apple, Sigstore (cosign) keyless signatures on every published artifact, SLSA L3 build
  provenance, and CycloneDX SBOMs (merged + per-ecosystem).

### Known limitations

- **Maven lifecycle execution** runs through the `barback` daemon on macOS and Linux. On
  Windows, run lifecycle commands with `--no-daemon`.
- **Multi-module reactor** support is still maturing; single-module builds are the proven
  path for this release candidate.
- This is an alpha: command surfaces, flags, and on-disk formats may change without notice as
  the tool is validated against more projects on the way to a stable `0.1.0`.
