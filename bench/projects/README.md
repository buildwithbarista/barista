# bench/projects — perf-bench reference project corpus

This directory hosts the **perf-benchmark** reference projects consumed
by the Tier-2 regression gate (`.github/workflows/perf-gate.yml`) and
the Tier-3 dashboard (`bench.barista.build`). Each entry pairs a real
Maven project with a `Bench.toml` manifest declaring how to run it,
what to measure, and what variance the regression gate should tolerate.

This is a *different* surface from `test-corpus/`:

| | `test-corpus/` | `bench/projects/` (this dir) |
|---|---|---|
| Purpose | Resolver golden tests + network captures | Perf benchmarking |
| Schema | `corpus.lock.toml` | `Bench.toml` (per `crates/barista-bench`) |
| Materialization | On-demand via `scripts/materialize-corpus.sh` | Git submodules or vendored |
| Consumer | `barista_test_fixtures::load_corpus_index`, B.1 traffic capture | `barista-bench run --corpus tier-2`, perf-gate workflow |

The two corpora may pin the *same upstream project* (P02 here is the
same Spring PetClinic as `test-corpus/spring-petclinic/`, pinned to
the same SHA), but they are independent surfaces with independent
schemas. Don't conflate.

## Layout

Every entry follows the same shape:

```
bench/projects/<id>/
├── Bench.toml         # manifest (consumed by barista-bench)
└── checkout/          # the project itself (submodule OR vendored)
```

`Bench.toml` is the on-disk contract documented by the `barista-bench`
crate (see `crates/barista-bench/src/manifest.rs` and the JSON-Schema
sidecar at `crates/barista-bench/schema/manifest.schema.json`).

## Current entries (v0.1 seed: P01–P03)

| ID | Display name | Checkout kind | Shape | Direct deps |
|---|---|---|---|---|
| `p01` | hello-world (synthetic floor case) | vendored | 1 module | 3 |
| `p02` | Spring PetClinic | git submodule | 1 module | ~50 |
| `p03` | Spring Boot starter-web app (tiny target) | vendored | 1 module | 1 (~170 transitive) |

P04–P12 land via the Tier-3 corpus build-out in a later milestone; the
target list is the full §17.5 table from the PRD.

## Submodule vs vendored

Each entry picks one of two materialization strategies:

- **Submodule** (`checkout/` is a git submodule). Pinned to a specific
  upstream commit SHA. Refreshes are a deliberate corpus baseline
  reset and must be called out in the commit message that bumps the
  submodule pointer. Use this when the upstream project is a real
  open-source project with a stable git history.
- **Vendored** (`checkout/` is plain source committed in this repo).
  Use this when the corpus entry is synthetic (P01 hello-world) or
  when the upstream project doesn't exist as a clonable repo (P03,
  which describes a shape — "Spring Boot starter-web app, ~170 deps"
  — rather than any specific upstream project). Vendoring keeps the
  baseline deterministic across upstream point releases.

`Bench.toml`'s `labels.checkout_kind` records which strategy each
entry uses, and (for submodules) `labels.upstream_url` +
`labels.upstream_ref` record the pin metadata.

## Materializing locally

For submodule-backed entries:

```
git submodule update --init bench/projects/
```

That clones each pinned submodule into `checkout/` at the recorded
SHA. Vendored entries are already on-disk and need no extra step.

To run a one-off bench against a single entry:

```
cargo run -p barista-bench -- run --projects P02
```

The harness reads `bench/projects/p02/Bench.toml`, invokes `command`
inside `bench/projects/p02/checkout/`, captures the listed `metrics`,
and writes a `results.json` document per the
`barista.bench.results/v1` schema.

## Adding a new entry

1. Create `bench/projects/<id>/` (e.g. `p04`).
2. Materialize the upstream source:
   - Submodule: `git submodule add <url> bench/projects/<id>/checkout`,
     then `git -C bench/projects/<id>/checkout checkout <pinned-sha>`.
   - Vendored: write the source files directly under `checkout/`.
3. Author `bench/projects/<id>/Bench.toml`. Use one of the existing
   entries as a template; tune `iterations`, `warmup_iterations`, and
   `allowed_variance` to the project's build-time budget.
4. Add a `<id>_manifest_parses` test case to
   `crates/barista-bench/tests/corpus_manifests.rs` so the manifest is
   re-validated on every PR.
5. If the entry should run in the Tier-2 perf-gate (rather than only
   the Tier-3 nightly), also wire the project's ID into
   `--corpus tier-2` in the workflow once the harness supports
   subset-by-ID.

## Build artifacts

`target/` directories created by `mvn` inside `checkout/` are excluded
via `bench/projects/.gitignore`. IDE scratch (`.idea/`, `*.iml`,
`.vscode/`) is also excluded. The repo only commits the source (or
submodule pointer) and the `Bench.toml`.
