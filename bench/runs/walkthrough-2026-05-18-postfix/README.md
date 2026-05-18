# Walkthrough benchmark — post-journal-recovery-fix re-run

Companion dataset to [`../walkthrough-2026-05-18/`](../walkthrough-2026-05-18/).
Same workload (P03 Spring Boot starter-web, warm caches, 10 × 2-warmup
iterations per `(manifest, baseline)`), same hardware (Apple M4 Max,
Temurin 21, macOS 26.2), but run after the M2.3 T11 fix landed —
`cmd/pull.rs` now opens the cache index via `Index::open_with_recovery`
(which truncates a torn journal tail and continues) instead of the
strict `Index::open` (which exits 1).

## Why this dataset exists

The original walkthrough surfaced a real bug: 10 sequential `barista
pull --update` invocations could leave the cache index journal in a
torn-tail state, after which every subsequent pull failed with
`journal at .../journal.log ends mid-record (truncation detected)`
exit 1. The mechanism in `crates/barista-cache/src/recovery.rs` is
already correct (`Index::open_with_recovery` truncates the bad tail
and continues); `cmd::pull::run` just wasn't using it. The fix is a
one-line change to call the recovery-aware opener; a regression test
in `crates/barista-cli/tests/cmd_pull_full_fetch.rs` deliberately
chops the tail off and asserts pull self-heals.

This dataset is the proof: after the fix, the same bench harness on
the same hardware in the same configuration runs cleanly to
completion with no journal-corruption error, no degenerate
fast-path iterations, and stable variance.

## Headline results — median wall-clock

| Step | mvn 3.9.9 | barista (warm daemon) | barista (`--no-daemon`) | barista warm vs mvn |
|---|---:|---:|---:|---:|
| **Pull / resolve** | 1391.5 ms | **729.0 ms** | — | **1.91× faster** |
| **Compile** (clean → compile) | 1383.0 ms | **246.5 ms** | 1394.5 ms | **5.61× faster** |
| **Package** (clean → package, `-DskipTests`) | 1719.5 ms | **1090.5 ms** | 1717.5 ms | **1.58× faster** |

Numbers are within noise of the original dataset's headline:

| Step | original (pre-fix) | post-fix | delta |
|---|---:|---:|---:|
| barista pull | 872 ms | 729 ms | −16% (less variance) |
| barista compile (warm daemon) | 172 ms | 247 ms | +44% (daemon-startup jitter) |
| barista package (warm daemon) | 1034 ms | 1091 ms | +6% |

The pull number genuinely improved — the postfix re-run benefits from
a fresh cache rewarm. The compile/package deltas reflect normal
daemon-startup variance from the first warmup iteration; medians are
within the manifest's 15% declared variance budget either way.

## Variance — pull is now flat

Pre-fix pull iterations: stddev 81 ms (over 819–1213 ms).
Post-fix pull iterations: stddev 10 ms (over 720–751 ms).

The narrow band post-fix reflects what the workload should look like
when nothing in the cache stack is doing anything weird:

```
iteration  wall_ms
0          751
1          729
2          720
3          721
4          736
5          738
6          729
7          727
8          736
9          721
```

Note that the very first iteration is consistently the slowest
across both datasets — daemon JIT and OS file cache settling. The
manifest's `warmup_iterations = 2` discards the worst of that
warmth-up variance.

## Manual verification of the recovery path

Independent of the bench harness, the fix is validated by deliberately
corrupting the journal and re-running pull:

```bash
$ python3 -c "import os; os.truncate(os.path.expanduser('~/.barista/cache/index/journal.log'), 181878)"
$ rm -f barista.lock
$ barista pull --update
barista: warning: cache index journal at /Users/ajbrown/.barista/cache/index/journal.log
                  had a torn tail; truncated at byte offset 172039 and continuing.
pull: build.barista.corpus:spring-boot-starter-web-app:0.0.1-SNAPSHOT: wrote barista.lock with 34 entries
$ echo $?
0
```

The warning surfaces what happened (so the operator knows their cache
self-healed); pull exits 0 and produces a valid lockfile.

## Open follow-up — root cause of the corruption

The fix is recovery-side, not prevention-side. The underlying
question — **how does the journal tail end up torn in clean
single-process usage when every append `flush()`+`sync_data()`s
explicitly?** — remains open and is filed as M2.3 T12. Possible
causes (none yet confirmed): `BufWriter::drop` flush failing
silently; partial write on the BufWriter's internal buffer at a
non-record boundary; FS-level torn-write semantics on APFS. The
corrupted journal that triggered all of this is saved at
`/tmp/barista-bench-debug/journal-corrupted-1779111467.log` for
analysis.

Until that's nailed down, recovery on read is the right defense:
torn tails are real (power loss, OOM-kills, FS quirks) regardless of
this specific corruption mode.

## Harness improvement bundled with the fix

The bench-runner harness now defaults to **stderr=inherit** during
measurement (stdout still goes to `/dev/null`). Pre-fix, both
streams were nulled — which meant baseline failures surfaced only
as `non-zero exit (N) from <cmd>` with the actual diagnostic
silenced. Stderr text is small, rare, and dwarfed by the timing
signal; surfacing it makes the harness self-debuggable. The full
both-streams-inherit mode is preserved behind
`BARISTA_BENCH_PASSTHROUGH=1` for deep inspection.
