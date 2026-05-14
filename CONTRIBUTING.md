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
  must remain compatible with.
- Standard build commands:
  - `cargo build --release` for the Rust workspace (`barista` CLI, resolver, cache, lockfile,
    `roastery`).
  - `mvn -f barback/pom.xml package` for the Java daemon.

### Using asdf (recommended)

The `.tool-versions` file at the repo root pins Rust, JDK, and Maven:

    asdf plugin add rust   # if you haven't already
    asdf plugin add java
    asdf plugin add maven
    asdf install

This gives you the default versions used by the project. CI additionally
exercises Maven 4.0.x and JDK 17 — install those manually if you want to
reproduce CI cells locally:

    asdf install java temurin-17.0.12+7
    # (Maven 4 is not yet broadly available via asdf; install from
    # https://maven.apache.org/download.cgi and place on PATH.)

For `mvnd`, download the latest 2.x release from
<https://github.com/apache/maven-mvnd/releases> and put the `mvnd` binary
on your PATH.

## Running tests

- `cargo test --workspace` for the Rust crates.
- `mvn -f barback/pom.xml test` for `barback`.
- A 100-project compatibility corpus will live under `test-corpus/` once that milestone lands;
  it is not yet required for local development.

## Local security checks

The repo ships a small set of security checks that run locally on every commit via the
[pre-commit](https://pre-commit.com/) framework. They are the same checks CI enforces, just
cheaper to run before you push.

### Install the tools

- **gitleaks** — secret scanner.
  - macOS: `brew install gitleaks`
  - Linux: `apt install gitleaks` if your distro packages it, otherwise grab a release
    binary from <https://github.com/gitleaks/gitleaks/releases> and put it on your `PATH`.
- **pre-commit** — hook runner.
  - macOS: `brew install pre-commit`
  - Linux/anywhere with Python: `pip install pre-commit` (or `pipx install pre-commit`).

### Wire up the hooks

Run this once per fresh clone:

    pre-commit install

That installs the git `pre-commit` hook. Every subsequent `git commit` will run the hooks
configured in `.pre-commit-config.yaml` — currently gitleaks, a handful of language-agnostic
hygiene checks, and `cargo fmt --check` / `cargo clippy --workspace -- -D warnings` for any
staged Rust files.

### Verify the secret-scan round-trip

You can sanity-check the whole secret-scan pipeline (config, allowlist mechanism, rule pack)
without touching git history by running:

    bash scripts/test-secret-scan.sh

It scans a synthetic AWS-shaped fixture under `tests/fixtures/secrets/` and asserts that
gitleaks (a) fires on it as configured, and (b) honours `.gitleaksignore` when the matching
fingerprint is listed. Both halves should pass; if either fails, the secret-scan setup is
broken and the surrounding hook + CI workflow cannot be trusted until it is fixed.

### When the hook fires on something you believe is a false positive

The default move is **not** to allowlist. Before reaching for `.gitleaksignore`:

1. Confirm the value is not a real credential. If it is, rotate it before doing anything
   else.
2. Remove or redact the value at source if you can — the best waiver is no waiver.
3. If the value genuinely cannot be removed (e.g., a deliberately-shaped fixture or an
   example token in user-facing docs), capture the fingerprint from the gitleaks JSON
   report and add it to `.gitleaksignore` with a comment naming the file, the rule, and
   the rationale. Explain the trade-off in your PR description. A reviewer from the
   security area-CODEOWNERS group needs to approve the entry.

The full allowlist-hygiene playbook (review cadence, audit process, who reaps stale
entries) lives at `docs/ci/secret-scan-allowlist.md`.

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
