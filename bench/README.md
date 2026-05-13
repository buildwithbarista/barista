# bench — Barista benchmark harnesses and methodology

This directory hosts the Java-side benchmark module (`barback-bench`)
and shared bench fixtures + the methodology documentation that
governs all Barista benchmarking.

Rust microbenchmarks for individual crates live next to those crates
(e.g. `crates/barista-version/benches/compare.rs`); their baseline
numbers are in `docs/perf/baselines.md`. End-to-end ("Tier 3") benches
that compare Barista against `mvn` and `mvnd` on real corpus projects
will live here.

## Test corpus

Real-world Maven projects exercised by the resolver, POM parser, and
end-to-end builds. The corpus is materialized on-demand:

```
bash scripts/materialize-corpus.sh
```

This clones each entry's pinned ref into `test-corpus/<id>/checkout/`
(gitignored). The current corpus is small and grows incrementally
toward ~100 projects.

The per-project lockfiles at `test-corpus/<id>/corpus.lock.toml`
record the git URL, pinned ref, target JDK, and reference Maven
version. A consumer-facing index lives at
`crates/barista-test-fixtures/data/corpus-100.toml` for programmatic
access from Rust code (see `barista_test_fixtures::load_corpus_index`).

### Adding a project

1. Create `test-corpus/<id>/corpus.lock.toml` with the project's
   pinned ref + toolchain.
2. Run `scripts/materialize-corpus.sh --filter <id>` to verify it
   clones and builds.
3. Add a matching `[[entry]]` to
   `crates/barista-test-fixtures/data/corpus-100.toml`.
4. Run `cargo test -p barista-test-fixtures` to verify the index
   parses.
5. Open a PR. The corpus-baseline CI workflow will exercise the new
   entry on its matrix cell.

## Selection criteria

The corpus should span the shapes of real Maven projects:

- Single-module vs multi-module.
- Library vs application.
- Plugin-heavy builds vs minimal builds.
- Tag-pinned vs commit-pinned (most are tag-pinned for reproducibility;
  commit-pinned entries are reserved for projects without stable tags).
- Build-time budget: ideally each project completes `mvn verify` in
  under 3 minutes on a typical CI runner. Larger projects (Spring
  Boot, Quarkus, Camel) can be added with explicit longer timeouts
  and a separate CI matrix cell.

## Tier 1, 2, 3 layout

(Forward reference — concrete benchmarks land alongside their
respective milestones.)

- **Tier 1 (microbenchmarks):** Rust crate-local benches, near the
  code being measured. Recorded in `docs/perf/baselines.md`.
- **Tier 2 (regression gate):** per-PR benches that compare Barista
  against itself on a fixed corpus, gating PRs on time/efficiency
  regressions.
- **Tier 3 (macro / E2E):** full-corpus runs against `mvn` and `mvnd`.
  Numbers published on `bench.barista.build`.
