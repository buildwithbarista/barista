# windows-smoke fixture

Minimal 1-module Java project used by the Windows `barista verify`
smoke-build CI job (D19).

## Why a hand-built fixture instead of a corpus project?

The original task suggestion was to reuse the `version-spec` corpus
project. The five seed corpus projects materialized in M0.1 T14 do not
include `version-spec`, and the seeds that do exist are sized for
resolver / lifecycle stress, not for a per-PR smoke test on a Windows
runner where wall-clock budget is tight. A trivial 1-module project
keeps the smoke job under a minute end-to-end while still exercising
the full chain:

```
barista (Rust) → forked mvn.cmd → maven-compiler-plugin → javac → jar → verify
```

## Shape

- `pom.xml` — single jar module, zero deps, `maven-compiler-plugin`
  3.13.0 pinned, JDK 17 source/target.
- `src/main/java/example/Hello.java` — one class, one static method.

No `target/` directory, no lockfile, no `.tool-versions` — the CI
runner provides JDK 21 + Maven via `actions/setup-java`, and the
fixture compiles cleanly under that toolchain.

## What runs against this fixture

1. **Windows runner (`rust-windows` job in `.github/workflows/ci.yml`):**
   `barista verify --no-daemon --root crates/barista-cli/tests/fixtures/windows-smoke`
   — the `[T]` AC for M4.3 T7.
2. **Cross-platform sanity (`crates/barista-cli/tests/cmd_windows_smoke.rs`):**
   the same command line, run on every host that has `mvn` on
   `$PATH`. Catches fixture / CLI regressions on the maintainer's
   machine before they reach the Windows runner.
