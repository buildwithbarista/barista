# Test-coverage report

Per-crate test-coverage for the Barista workspace, measured against the
project quality targets, with a gap analysis and a prioritized follow-up
list. The numbers below are produced by `scripts/coverage.sh`, which is
also the CI gate (see [Gate policy](#gate-policy)).

Like the [performance baselines](baselines.md), these numbers go stale as
the code changes. Re-run `bash scripts/coverage.sh` (or
`bash scripts/coverage.sh --report-only` to skip the gate) to refresh
them, then update the table and the run header below.

## Targets

Per crate:

| Metric | Target |
|---|---:|
| Line coverage | ≥ 80% |
| Branch coverage | ≥ 70% |
| Function coverage | ≥ 80% |

**Priority modules** — the core correctness surface — must hit target and
are a **hard gate**: `barista-resolver`, `barista-cache`,
`barista-lockfile`. Non-priority crates are **advisory** at v0.1 (a miss
is reported but does not fail the gate). See [Gate policy](#gate-policy).

## Methodology

- **Tool:** [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov)
  `0.8.7` (pinned). Install with
  `cargo install cargo-llvm-cov --version 0.8.7 --locked`. It drives
  rustc's `-C instrument-coverage` (source-based coverage) and LLVM's
  `llvm-cov` / `llvm-profdata`, which ship inside the pinned toolchain.
- **Command (the exact one `scripts/coverage.sh` runs):**

  ```sh
  cargo llvm-cov --workspace --summary-only      # bounded; instrumented build + all tests
  cargo llvm-cov report --json --output-path …   # re-export from the captured profdata
  ```

  The JSON export is aggregated per crate (sum of covered / total across
  each crate's source files) to produce the table below.
- **Bounding:** the run is wrapped in `timeout` (default 1200s / 20 min).
  Coverage is an instrumented build of every crate plus the full test
  run, so it is several times slower than an ordinary `cargo test`.
- **Toolchain:** the workspace pins **stable** rust
  (`rust-toolchain.toml`). On a rustup-less toolchain (e.g. asdf), the
  script points `cargo-llvm-cov` at the `llvm-cov` / `llvm-profdata`
  binaries inside the active toolchain's sysroot.
- **What is excluded / why:**
  - **Branch coverage** is **not measured on stable.** True branch
    coverage requires rustc's nightly-only `-Z coverage-options=branch`
    flag; on the pinned stable toolchain that flag is rejected. The 70%
    branch target is recorded as a forward-looking goal; measuring it
    needs a nightly run and is out of scope for the stable CI gate.
    **Region coverage** — a finer-grained, stable-toolchain measure of
    control-flow coverage — is reported in its place as the closest
    available proxy.
  - **Docker-gated integration tests** (the `roastery` and
    `barista-roastery-client` `container_roundtrip` tests) are
    `#[ignore]`d and do not run under coverage. The code paths they
    exercise therefore read as uncovered here.
  - **Binary entrypoints** (`*/src/main.rs`, `src/bin/*.rs`) read at or
    near 0%: an instrumented `--tests` run builds them but does not
    execute the `main`/`run` shell, which is exercised by end-to-end CLI
    tests rather than the unit/integration suite.
  - **Workstream-scaffold crates** — `barista-bench`, `barista-netcap`,
    `barista-netanalyze` — are early-stage and thin; their numbers are
    informative, not gating (and none are priority modules).
  - **Vendored / generated code** is not first-party and is excluded from
    the crate roll-up by construction (the workspace's first-party set is
    the same one enumerated by `scripts/check-spdx-headers.sh --list`).

### Run header

- Tool: `cargo-llvm-cov 0.8.7`
- Rust: `rustc 1.88.0 (6b00bc388 2025-06-23)` (stable; workspace pin)
- Host: `aarch64-apple-darwin` — macOS (Darwin 25.2.0 arm64)
- Tests: 104 test binaries, all passing; 33 `#[ignore]`d (Docker) not run
- Date: 2026-05-20

## Per-crate coverage

`*` marks a priority module. **Branch%** is `n/a` everywhere — branch
coverage is nightly-only and not measured on the stable toolchain (see
[Methodology](#methodology)); region% is shown as the stable-toolchain
proxy. "Meets target?" is line ≥ 80% **and** function ≥ 80%.

| Crate | Line% | Branch% | Function% | Region% | Meets target? |
|---|---:|---:|---:|---:|:--|
| barista-resolver `*` | 92.40 | n/a | 91.59 | 93.60 | ✅ |
| barista-cache `*` | 87.53 | n/a | 82.75 | 89.67 | ✅ |
| barista-lockfile `*` | 95.99 | n/a | 91.67 | 96.62 | ✅ |
| barista-coords | 99.33 | n/a | 100.00 | 95.47 | ✅ |
| barista-version | 95.94 | n/a | 99.17 | 96.28 | ✅ |
| barista-tap | 94.68 | n/a | 96.15 | 94.39 | ✅ |
| barista-pom | 93.58 | n/a | 93.33 | 90.82 | ✅ |
| barista-netanalyze | 90.70 | n/a | 85.71 | 90.26 | ✅ |
| barista-config | 89.74 | n/a | 88.89 | 86.35 | ✅ |
| barista-ipc | 87.91 | n/a | 91.03 | 85.78 | ✅ |
| barista-test-fixtures | 85.59 | n/a | 94.44 | 84.95 | ✅ |
| roastery | 81.34 | n/a | 82.31 | 84.60 | ✅ |
| barista-roastery-client | 80.12 | n/a | 74.23 | 80.63 | ❌ (func) |
| barista-netcap | 78.02 | n/a | 82.86 | 78.61 | ❌ (line) |
| xtask | 77.67 | n/a | 65.79 | 78.58 | ❌ (line, func) |
| barista-telemetry | 75.96 | n/a | 73.47 | 70.00 | ❌ (line, func) |
| barista-cli | 72.92 | n/a | 74.22 | 72.70 | ❌ (line, func) |
| barista-bench | 21.83 | n/a | 27.59 | 23.34 | ❌ (scaffold) |
| **Workspace TOTAL** | **84.49** | **n/a** | **83.77** | **82.82** | — |

## Gap analysis

**Priority modules: all three meet target.** No priority-module gap to
close. The hard gate is green.

| Priority module | Line% | Func% | Status |
|---|---:|---:|:--|
| barista-resolver | 92.40 | 91.59 | ✅ comfortably above target |
| barista-cache | 87.53 | 82.75 | ✅ above target |
| barista-lockfile | 95.99 | 91.67 | ✅ comfortably above target |

The remaining gaps are all in **non-priority** crates and are advisory at
v0.1. The dominant cause of each miss, lowest-covered files first:

- **barista-bench** (21.83% line) — workstream scaffold. The bulk is the
  `src/bin/barista-bench.rs` entrypoint (15.40%, 813/961 lines uncovered);
  the library shell is thin. Expected for an early-stage bench harness.
- **barista-cli** (72.92% line, 74.22% func) — the misses concentrate in
  command/daemon code exercised by end-to-end flows rather than the unit
  suite: `src/daemon/respawn.rs` (0%, 108 lines), `src/cmd/shot.rs`
  (29.34%, 301 missed), `src/cmd/verify.rs` (32.56%, 437 missed),
  `src/daemon/launcher.rs` (39.29%, 221 missed).
- **barista-telemetry** (75.96% line, 73.47% func) — `src/transport.rs`
  (68.00%, 40 missed) and `src/tracing.rs` (64.34%) carry the gap; the
  uncovered paths are exporter/transport error branches.
- **xtask** (77.67% line, 65.79% func) — `src/main.rs` (0%, the CLI
  dispatch shell) and `src/findings.rs` (70.83%, 119 missed) drive the
  miss. `xtask` is a developer task runner, not shipped code.
- **barista-netcap** (78.02% line) — `src/session.rs` (73.29%) and
  `src/ca.rs` (75.71%) carry the gap; workstream-scaffold crate.
- **barista-roastery-client** (line 80.12% passes, **func 74.23% misses**)
  — `src/tls.rs` (48.51% line) is the largest hole; several of its paths
  are only reachable through the `#[ignore]`d Docker round-trip test.
  `src/types.rs` (0%, 6 lines) is a trivial type-only module.

### Prioritized follow-up

Writing the missing tests is **out of scope for this report** (the
deliverable is the report + the gate). The follow-ups below are ordered
priority-first per policy:

1. **Priority modules — none.** All three are above target; no follow-up
   required. (If a future change drops one below target, the hard gate
   fails and the gap must be closed before merge.)
2. **barista-cli** — the largest absolute gap in shipped code. Add
   integration coverage for `cmd/shot`, `cmd/verify`, and the daemon
   `respawn`/`launcher` paths (or mark genuinely-end-to-end-only code so
   it is not counted against the unit target).
3. **barista-telemetry** — cover the `transport.rs` / `tracing.rs`
   exporter error branches.
4. **barista-roastery-client** — raise function coverage by covering
   `tls.rs` (some paths need the Docker round-trip de-`#[ignore]`d or a
   non-container TLS unit test) and the trivial `types.rs`.
5. **xtask / barista-netcap / barista-bench** — lowest priority
   (dev-tooling and early scaffolds); revisit as those workstreams mature.

## Gate policy

`scripts/coverage.sh` runs the bounded coverage measurement, prints the
per-crate table above, and enforces:

- **Priority modules are a HARD gate.** If `barista-resolver`,
  `barista-cache`, or `barista-lockfile` is below target on line **or**
  function coverage, the script exits non-zero (CI fails).
- **Non-priority crates are ADVISORY** at v0.1. A miss is printed and
  flagged `FAIL(advisory)` in the table but does not fail the gate. This
  keeps the report honest about the whole tree while only blocking on the
  crates whose correctness matters most today. The intent is to tighten
  non-priority crates to a hard gate as their coverage reaches target.
- **Branch coverage is not gated** on the stable toolchain (it is
  nightly-only); the 70% target is recorded as a goal. Region coverage is
  reported as the stable-toolchain proxy.

Run `bash scripts/coverage.sh --report-only` to print the table without
enforcing the gate.
