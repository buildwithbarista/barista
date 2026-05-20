# barista-cache

Local content-addressed cache for Barista artifacts: the on-disk CAS,
the index/journal, the fetcher, GC, and per-coordinate locking (in-process
async mutexes plus a cross-process advisory file lock).

## Cross-process locking

`FilesystemLock` is an advisory per-coord file lock (`flock(2)` on Unix,
`LockFileEx` on Windows). Acquisition is a **non-blocking poll loop**: each
attempt issues a single non-blocking `try_write()` and returns immediately,
backing off with an async sleep between retries. No OS thread is ever parked
inside the lock call, so `acquire_with_timeout` is a *truthful* timeout —
when it gives up, nothing is left behind fighting for the lock.

Production fetches use `acquire_with_timeout` (120s); a timeout fails the
build loudly with a pointer to the stuck lock file rather than hanging.

## A note on the lock tests

The lock tests are **wall-clock-guarded** (a watchdog thread for the sync
tests, `tokio::time::timeout` for the async ones). If
`cargo test -p barista-cache` ever appears to hang, a `FilesystemLock`
regression is the first suspect — check for orphaned `barista_cache-*`
test processes:

```sh
ps -eo pid,etime,command | grep barista_cache | grep -v grep
```

and bound any local test run with `timeout`, e.g.
`timeout 200 cargo test -p barista-cache --lib`.
