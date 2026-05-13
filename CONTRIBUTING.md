# Contributing to Barista

## Welcome

Thanks for your interest in Barista. Contributions are welcome. Barista is pre-release software
under active development; expect breaking changes to APIs, on-disk formats, and CLI surface
until the first stable release.

## Code of conduct

This project follows the terms of the [Code of Conduct](CODE_OF_CONDUCT.md). By participating,
you agree to uphold it.

## Before you contribute

- For non-trivial work, please file an issue first (or pick up an existing one) so we can agree
  on scope and approach before code is written. Small fixes, typos, and obvious bugs can go
  straight to a pull request.
- **License of contributions.** By submitting a contribution, you agree that your work is
  dual-licensed under either of the [MIT license](LICENSE-MIT) or the
  [Apache License, Version 2.0](LICENSE-APACHE), at the user's option. See the final section
  of this file.
- **Sign-off.** Please sign your commits with `git commit -s` (DCO-style). A formal CLA may be
  introduced later if the project is donated to a foundation; until then, the sign-off is
  sufficient.

## Development setup

- A recent stable Rust toolchain. The exact version is pinned in `rust-toolchain.toml` once it
  lands; until then, use the latest stable release.
- **JDK 17 and JDK 21.** The `barback` daemon runs on both via a runtime-detected branch, and
  CI exercises both. Install both if you intend to work on `barback`.
- **Maven 3.9.x and Maven 4.0.x**, plus **mvnd 2.x**. These are the embedder targets Barista
  must remain compatible with. An `.tool-versions` file for `asdf` will be added in a later
  milestone.
- Standard build commands:
  - `cargo build --release` for the Rust workspace (`barista` CLI, resolver, cache, lockfile,
    `roastery`).
  - `mvn -f barback/pom.xml package` for the Java daemon.

## Running tests

- `cargo test --workspace` for the Rust crates.
- `mvn -f barback/pom.xml test` for `barback`.
- A 100-project compatibility corpus will live under `test-corpus/` once that milestone lands;
  it is not yet required for local development.

## Coding conventions

- **Rust.** `cargo fmt` must be clean. `cargo clippy --workspace --all-targets -- -D warnings`
  must be clean.
- **Java.** `barback` follows [Google Java Format](https://github.com/google/google-java-format).
- **Public APIs are documented.** `cargo doc --no-deps` must build without warnings.

## Commit conventions

- [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/) is preferred
  (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`, `build:`, `ci:`).
- Use the imperative mood ("add foo", not "added foo"). Keep subject lines at or below 72
  characters. Use the body to explain the "why".
- Reference issues and pull requests by number where relevant (e.g. `Fixes #123`).

## Pull requests

- One topic per pull request. Smaller PRs are easier to review and easier to land.
- Include tests for any behavioral change.
- Update any documentation affected by your change.
- CI must be green before a PR is merged.

## Reporting bugs and requesting features

Bug reports and feature requests go in [GitHub Issues](../../issues). Issue templates live
under `.github/ISSUE_TEMPLATE/` and will guide you through the fields we need.

## Security issues

Do **not** open a public issue for a security vulnerability. See [SECURITY.md](SECURITY.md)
for the private disclosure process.

## License of your contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in Barista by you shall be dual-licensed as **MIT OR Apache-2.0**, without any additional terms
or conditions.
