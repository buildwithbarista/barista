# `parity-check.sh` — artifact-divergence test harness

This harness compares the `target/` artifacts produced by upstream
`mvn` against those produced by `barista verify --no-daemon` on each
project in the Maven-compatibility test corpus. Any byte-divergence
(modulo a small documented ignore list) is a failure.

## What this proves today

In v0.1, `barista verify --no-daemon` delegates the entire build to the
upstream `mvn` binary. Reference-mvn and barista-mvn therefore produce
byte-equal `target/` trees by construction. That makes the harness a
**regression gate**: it locks in the v0.1 guarantee that `--no-daemon`
is a transparent fallback that never perturbs build outputs (no stray
env-var leakage, no stripped flags, no `-Dmaven.repo.local` confusion).

The harness's full value comes online in v0.2, when the daemon path is
wired for arbitrary corpus projects. At that point the same script
exercises the real cross-tool comparison.

## Running it

The harness expects:

- `mvn` on `$PATH`, at the version pinned in `.tool-versions`
  (`asdf install` from the repo root takes care of this).
- A built `barista` binary, or a working `cargo run -p barista-cli`
  toolchain. CI passes `BARISTA_BIN=<path>` so the harness skips the
  compile.
- The corpus materialized at `test-corpus/<id>/checkout/`. The harness
  calls `scripts/materialize-corpus.sh --filter <id>` on demand for
  any entry that isn't materialized yet.

Typical local invocation:

```bash
cargo build --release -p barista-cli
BARISTA_BIN="$(pwd)/target/release/barista" \
  crates/barista-test-fixtures/scripts/parity-check.sh
```

Filter to one project (useful during development):

```bash
BARISTA_BIN="$(pwd)/target/release/barista" \
  crates/barista-test-fixtures/scripts/parity-check.sh \
  --filter commons-lang
```

## Output

Per-project status line followed by a summary footer:

```
[commons-lang] PASS
[commons-io] PASS
[jackson-core] FAIL artifact divergence (see lines above)
  [jackson-core] hash mismatch on target/jackson-core-2.18.0.jar
      mvn:     ab12...
      barista: cd34...
[slf4j] PASS

summary: 4 project(s) checked, 3 PASS, 1 FAIL, 0 SKIP
```

Exit codes:

| Code | Meaning                                                 |
| ---- | ------------------------------------------------------- |
| 0    | All checked projects byte-equal across both paths.      |
| 1    | Usage error (bad flag, missing required argument).      |
| 2    | Environment error (no `mvn`/`barista`, missing corpus). |
| 3    | At least one project diverged.                          |

## Ignore list

A small set of paths under `target/` are excluded from byte-comparison
because they're known not to be reproducible across runs even on a
fully deterministic toolchain. Each entry in `IGNORE_GLOBS` at the top
of the script carries a one-line comment justifying its inclusion.
Current entries:

- `surefire-reports/*`, `failsafe-reports/*` — wall-clock build times
  and per-test elapsed-ms numbers; `SOURCE_DATE_EPOCH` doesn't flow
  through here. Documented as a known gap in the M4.3 `--ci`
  reproducibility test.
- `maven-status/*` — plugin-internal staging state that doesn't affect
  the produced JAR (we hash the JAR itself).
- `*.log`, `*.tmp` — per-execution diagnostic logs from various
  plugins; not artifacts.

Adding to this list requires a one-line comment in the script
explaining why the path is non-reproducible, and a corresponding
note in this README.

## Compare-only mode

For meta-testing the comparison logic, pass two pre-built `target/`
trees instead of running builds:

```bash
parity-check.sh --compare-only path/to/mvn-target path/to/barista-target
```

Exit 0 on byte-equality, 3 on divergence. This is what
`scripts/test-parity-check.sh` exercises: it builds one corpus project
once, copies the resulting `target/` tree, perturbs a byte, and
re-invokes the harness to assert the divergence is caught.

## Adding a corpus project

The harness reuses `test-corpus/<id>/corpus.lock.toml` for its
project list. Adding a project to the corpus automatically opts it
in to the parity check. See `test-corpus/README.md` for the lockfile
schema.

## Open follow-ups

- **Daemon-path parity** (v0.2): once `barista verify` (no
  `--no-daemon`) is wired for arbitrary corpus projects, run the same
  harness without `--no-daemon` and assert byte-equality there too.
  This is the load-bearing case — the `--no-daemon` baseline is
  trivially-equal by construction.
- **Nightly CI integration**: the full corpus takes 30-60s per project
  (~5 minutes for the v0.1 seed set, more as the corpus grows). The
  meta-test under `scripts/test-parity-check.sh` runs on every PR,
  but full-corpus runs should land in a nightly job once the v0.2
  daemon path makes them worth the wall-clock cost.
- **Multi-module reporting**: today the harness walks every dir named
  `target/` under the project root, but it doesn't emit per-module
  rollups for multi-module reactors. Output could be tightened with a
  per-module sub-summary on FAIL for projects like `slf4j`.
