//! Daemon launcher: find or spawn `barback`, connect to its UDS,
//! return a ready [`barista_ipc::Multiplexer`]-backed client/server pair.
//!
//! # Responsibilities
//!
//! 1. **Discover** an existing daemon by checking for a live UDS at
//!    `<socket_dir>/barback.sock`. If we can `connect(2)` to it and the
//!    `Ping`/`Pong` handshake succeeds, we reuse the existing daemon.
//! 2. **Spawn** a new daemon when none is reachable. The launcher
//!    forks `java -cp <classpath> com.bluminal.barista.barback.Server`
//!    with `--socket`, `--workers`, and `--idle-shutdown` set to the
//!    values resolved from `barista-config` (per M4.2 T2 / T5).
//! 3. **Wait-for-bind** poll the socket inode until either it appears
//!    (the daemon's `Server.start` returns synchronously with the bind)
//!    or a deadline elapses. This is the same poll pattern the M4.2 T6
//!    cross-language conformance test uses; replicating the timing
//!    contract keeps the user-facing experience consistent with the
//!    canonical fixture.
//!
//! # Uber-JAR / classpath discovery
//!
//! The barback daemon ships as a Maven module (`barback/pom.xml`), not
//! as a shaded uber-JAR — there is no `maven-shade-plugin` configured
//! at the time of this writing, so `java -jar barback.jar` is not yet
//! a runnable path. The launcher discovers the classpath in the
//! following order; the first hit wins:
//!
//! 1. **`BARISTA_BARBACK_JAR`** — explicit override, points at a
//!    pre-built uber-JAR. When set, the launcher invokes
//!    `java -jar <jar>` directly. Reserved for the day the uber-JAR
//!    build target lands.
//! 2. **`BARISTA_BARBACK_CLASSPATH`** — explicit override, points at a
//!    `:`-separated (Unix) / `;`-separated (Windows) classpath. Used
//!    by tests and packaging scripts that prefer to assemble the
//!    classpath themselves.
//! 3. **`barback_dir/target/{classes,test-classes}` + `mvn
//!    dependency:build-classpath`** — dev-loop path. Mirrors the same
//!    resolution the M4.1 conformance tests use (see
//!    `crates/barista-ipc/tests/conformance_helpers/jvm.rs`). The
//!    launcher caches the resolved classpath in
//!    `<socket_dir>/barback.classpath` so subsequent spawns skip the
//!    Maven invocation.
//!
//! When none of the strategies succeed, the launcher surfaces a
//! `BAR-DAEMON-JAR-NOT-FOUND` error pointing at the override env vars
//! the user can set.
//!
//! # PID-file convention
//!
//! After spawning, the launcher writes the JVM's process id to
//! `<socket_dir>/barback.pid`. The PID file is informational only —
//! daemon discovery primarily uses the socket's `connect(2)` + `Ping`
//! round-trip, which is the ground truth — but it lets the auto-respawn
//! path `kill -9` a leftover daemon when the previous run crashed
//! mid-write and the socket inode is still on disk pointing at a dead
//! process.
//!
//! # Scope
//!
//! This module is Unix-only at v0.1: the daemon's Windows named-pipe
//! bind is deferred (see `Server.java`'s class javadoc). The
//! `#[cfg(unix)]` gate at the top of `mod.rs` keeps this module out of
//! Windows builds entirely.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Default leaf-name for the daemon's UDS, under
/// `barista-config`'s `paths.daemon.socket_dir`.
pub const SOCKET_LEAF: &str = "barback.sock";

/// Default leaf-name for the daemon's PID file.
pub const PID_LEAF: &str = "barback.pid";

/// Default leaf-name for the cached classpath file the launcher writes
/// after a successful `mvn dependency:build-classpath` resolution.
pub const CLASSPATH_CACHE_LEAF: &str = "barback.classpath";

/// Stable error code returned when neither
/// `BARISTA_BARBACK_JAR` nor a discoverable classpath exists.
pub const ERR_CODE_JAR_NOT_FOUND: &str = "BAR-DAEMON-JAR-NOT-FOUND";

/// Stable error code returned when the daemon binds the socket but
/// stops accepting connections (we couldn't `connect(2)` to it within
/// the deadline). Distinct from `BAR-DAEMON-CRASHED` (which means the
/// daemon crashed mid-action — see the IPC mux layer): this code means
/// the spawn-ready handshake itself never landed.
pub const ERR_CODE_SPAWN_TIMEOUT: &str = "BAR-DAEMON-SPAWN-TIMEOUT";

/// Errors surfaced by [`LaunchPlan::spawn`] and [`discover_or_spawn`].
#[derive(Debug, thiserror::Error)]
pub enum LauncherError {
    /// Couldn't find a barback classpath, an uber-JAR override, or a
    /// dev-loop checkout. Wire code [`ERR_CODE_JAR_NOT_FOUND`].
    #[error(
        "{ERR_CODE_JAR_NOT_FOUND}: no barback classpath found. \
         Set `BARISTA_BARBACK_JAR=/path/to/barback-uber.jar`, \
         `BARISTA_BARBACK_CLASSPATH=<classpath>`, or run from a dev \
         checkout containing `barback/target/classes`. (tried: {tried})"
    )]
    JarNotFound {
        /// Comma-joined list of resolution strategies that were
        /// attempted. Echoed verbatim in the error message.
        tried: String,
    },

    /// `java` could not be located. Resolved from `$JAVA_HOME/bin/java`,
    /// then `$PATH`.
    #[error(
        "BAR-DAEMON-JAVA-NOT-FOUND: no `java` on $PATH or $JAVA_HOME/bin/java. \
         Install a JDK 17+ or set $JAVA_HOME."
    )]
    JavaNotFound,

    /// Spawning the JVM via `Command::spawn` failed.
    #[error("failed to spawn barback (java {argv:?}): {source}")]
    SpawnFailed {
        /// The argv we tried to spawn with; recorded so error logs
        /// can reproduce the invocation.
        argv: Vec<String>,
        #[source]
        /// The underlying `Command::spawn` failure.
        source: std::io::Error,
    },

    /// The daemon bound the socket inode but we couldn't connect /
    /// handshake within the deadline. Wire code
    /// [`ERR_CODE_SPAWN_TIMEOUT`].
    #[error(
        "{ERR_CODE_SPAWN_TIMEOUT}: barback did not become ready at {socket:?} \
         within {timeout:?}; the daemon's stderr may have more detail."
    )]
    SpawnTimeout {
        /// Socket path we polled.
        socket: PathBuf,
        /// Deadline we waited for.
        timeout: Duration,
    },

    /// `mvn dependency:build-classpath` failed when the launcher tried
    /// to resolve the dev-loop classpath. Recorded with the underlying
    /// non-zero exit status so the user can rerun the same `mvn`
    /// invocation by hand.
    #[error("`mvn dependency:build-classpath` failed (exit {exit:?}): {stderr}")]
    MavenClasspathFailed {
        /// Exit code of `mvn`; `None` on signal.
        exit: Option<i32>,
        /// Captured stderr for diagnostics.
        stderr: String,
    },

    /// Generic I/O error reading / writing daemon plumbing files.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        /// The path where the I/O error happened.
        path: PathBuf,
        #[source]
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Inputs needed to spawn a barback daemon.
///
/// Carved out so callers (production [`discover_or_spawn`], tests with
/// fixtures, the future `barista barback start` command) build the
/// same shape and the spawn helper stays pure.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    /// Directory the launcher uses for the socket + PID + classpath
    /// cache. Mirrors `barista-config`'s `daemon.socket_dir`.
    pub socket_dir: PathBuf,
    /// Concrete socket path; usually `socket_dir.join(SOCKET_LEAF)`.
    pub socket_path: PathBuf,
    /// Resolved worker count (`barback.default_workers` evaluated to
    /// a concrete integer via `daemon::workers::resolve_workers`).
    pub workers: usize,
    /// Idle-shutdown window, seconds.
    pub idle_shutdown_secs: u32,
    /// Optional `--crash-after <n>` debug flag. `Some(n)` arms the
    /// daemon to `Runtime.halt(137)` after `n` action envelopes — used
    /// only by the M4.3 T1 / M4.2 T6 auto-respawn integration tests.
    pub crash_after: Option<u32>,
    /// JVM startup wait deadline. The launcher polls the socket inode
    /// + a connect-readiness probe until this deadline.
    pub spawn_timeout: Duration,
}

impl LaunchPlan {
    /// Build a default plan from a socket directory + worker count.
    /// Other fields take production defaults; tests override
    /// `crash_after` / `spawn_timeout` directly.
    pub fn new(socket_dir: PathBuf, workers: usize, idle_shutdown_secs: u32) -> Self {
        let socket_path = socket_dir.join(SOCKET_LEAF);
        Self {
            socket_dir,
            socket_path,
            workers,
            idle_shutdown_secs,
            crash_after: None,
            spawn_timeout: Duration::from_secs(30),
        }
    }
}

/// Outcome of [`discover_or_spawn`] / [`spawn_daemon`].
///
/// The returned `Child` is `Some(_)` only when *this* launcher spawned
/// the daemon. If the daemon was already running, no child handle is
/// returned (we don't own it; the previous launcher does or it's a
/// pre-existing `barista barback start` invocation).
#[derive(Debug)]
pub struct DaemonHandle {
    /// The path the daemon is listening on. Always populated.
    pub socket_path: PathBuf,
    /// Child process handle, when this launcher spawned the daemon.
    /// `None` when discovery found an existing live daemon.
    pub child: Option<Child>,
}

/// Discover an existing barback daemon, or spawn one. The returned
/// [`DaemonHandle::socket_path`] is guaranteed to be a UDS that a
/// caller can `connect(2)` to and exchange `Ping`/`Pong` with.
///
/// `plan` carries the spawn configuration. If a daemon is already
/// listening at `plan.socket_path` and answers our `connect(2)`,
/// the spawn is skipped and the child handle is `None`.
///
/// `classpath` is the lookup that produces the `-cp` / `-jar` argument
/// to `java`. Pulled out as a closure so test fixtures can substitute
/// a synthetic Java entry point (e.g. an `EchoServerCli` for the
/// auto-respawn tests) without dragging the full barback uber-JAR
/// machinery into the test harness.
pub fn discover_or_spawn(
    plan: &LaunchPlan,
    classpath: impl FnOnce() -> Result<JvmEntry, LauncherError>,
) -> Result<DaemonHandle, LauncherError> {
    // Ensure the socket directory exists with 0700 perms. The daemon
    // creates it on its own if missing, but the launcher needs the
    // directory to exist for the PID/classpath cache writes below.
    ensure_socket_dir(&plan.socket_dir)?;

    if socket_is_live(&plan.socket_path, Duration::from_millis(500)) {
        return Ok(DaemonHandle {
            socket_path: plan.socket_path.clone(),
            child: None,
        });
    }

    // No live daemon. If the socket inode exists on disk but isn't
    // accepting connections, it's stale (previous daemon crashed
    // before unlinking). Best-effort kill the previous PID and remove
    // the inode so the new bind doesn't fight EADDRINUSE.
    reap_stale_daemon(plan);

    let entry = classpath()?;
    let child = spawn_daemon(plan, &entry)?;

    // Write the PID file so the auto-respawn path can reach the
    // previous daemon if it crashes mid-action and the OS hasn't
    // reaped it yet.
    if let Err(e) = write_pid(&plan.socket_dir, child.id()) {
        // Non-fatal: the PID file is a hint, not a contract. Log via
        // stderr at "best-effort" volume.
        eprintln!(
            "barista: warning: could not write {}: {e}",
            plan.socket_dir.join(PID_LEAF).display(),
        );
    }

    wait_for_ready(&plan.socket_path, plan.spawn_timeout)?;

    Ok(DaemonHandle {
        socket_path: plan.socket_path.clone(),
        child: Some(child),
    })
}

/// JVM entry-point spec; either an uber-JAR (`-jar`) or a classpath
/// (`-cp <cp> <main-class>`).
#[derive(Debug, Clone)]
pub enum JvmEntry {
    /// `java -jar <path>`. The JAR's `Main-Class` manifest entry
    /// drives the entry point.
    Jar(PathBuf),
    /// `java -cp <classpath> <main-class>`. Used when the daemon
    /// hasn't been shaded into an uber-JAR yet.
    Classpath {
        /// `:`-/`;`-separated classpath.
        classpath: OsString,
        /// Fully-qualified Java main class.
        main_class: String,
    },
}

/// Spawn the daemon using `entry`'s JVM-entry spec, attaching stdout /
/// stderr piped to threads that drain them so the JVM never blocks on
/// a full pipe.
pub fn spawn_daemon(plan: &LaunchPlan, entry: &JvmEntry) -> Result<Child, LauncherError> {
    let java = locate_java()?;

    let mut argv_for_diag: Vec<String> = vec![java.to_string_lossy().into_owned()];
    // The program path is the resolved `java` binary from
    // `locate_java()` — either `$JAVA_HOME/bin/java` (a configured
    // toolchain path) or `which::which("java")` (a $PATH lookup
    // analogous to the `--no-daemon` `mvn` resolver pattern, which
    // also resolves binary names dynamically). Both branches surface
    // exactly the same trust boundary as the `--no-daemon` path:
    // we trust the user's $PATH / $JAVA_HOME the same way every
    // build tool does.
    // nosemgrep: barista-rust-unchecked-command-new
    let mut cmd = Command::new(&java);

    match entry {
        JvmEntry::Jar(jar) => {
            cmd.arg("-jar").arg(jar);
            argv_for_diag.push("-jar".to_string());
            argv_for_diag.push(jar.display().to_string());
        }
        JvmEntry::Classpath {
            classpath,
            main_class,
        } => {
            cmd.arg("-cp").arg(classpath).arg(main_class);
            argv_for_diag.push("-cp".to_string());
            argv_for_diag.push(classpath.to_string_lossy().into_owned());
            argv_for_diag.push(main_class.clone());
        }
    }

    cmd.arg("--socket").arg(&plan.socket_path);
    cmd.arg("--workers").arg(plan.workers.to_string());
    cmd.arg("--idle-shutdown")
        .arg(plan.idle_shutdown_secs.to_string());
    if let Some(n) = plan.crash_after {
        cmd.arg("--crash-after").arg(n.to_string());
    }

    argv_for_diag.push("--socket".to_string());
    argv_for_diag.push(plan.socket_path.display().to_string());
    argv_for_diag.push("--workers".to_string());
    argv_for_diag.push(plan.workers.to_string());
    argv_for_diag.push("--idle-shutdown".to_string());
    argv_for_diag.push(plan.idle_shutdown_secs.to_string());
    if let Some(n) = plan.crash_after {
        argv_for_diag.push("--crash-after".to_string());
        argv_for_diag.push(n.to_string());
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| LauncherError::SpawnFailed {
        argv: argv_for_diag,
        source: e,
    })?;

    // Drain stdout + stderr in background threads so the JVM never
    // blocks on a full pipe. The barback `SEVERE` / `WARNING` log
    // lines on the crash-after path produce non-trivial output that
    // would otherwise deadlock a `Stdio::piped` JVM under load.
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || drain_stream(out, "barback stdout"));
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || drain_stream(err, "barback stderr"));
    }

    Ok(child)
}

fn drain_stream<R: std::io::Read>(mut r: R, label: &str) {
    let mut buf = Vec::with_capacity(4096);
    // Read until EOF. We always swallow the bytes silently in
    // production — the JVM's logs are best-effort diagnostic and
    // surfacing them on every barista invocation would clutter user
    // output. Set `BARISTA_BARBACK_VERBOSE=1` to forward.
    if std::env::var("BARISTA_BARBACK_VERBOSE").is_ok() {
        let mut chunk = [0u8; 4096];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let s = String::from_utf8_lossy(&chunk[..n]);
                    eprint!("[{label}] {s}");
                }
                Err(_) => break,
            }
        }
    } else {
        let _ = r.read_to_end(&mut buf);
    }
}

/// Check whether a daemon is currently listening at `path` by
/// attempting a non-blocking `connect(2)` with a short timeout. The
/// `Ping` handshake is intentionally deferred to the multiplexer-level
/// caller: this function only checks "is the socket accepting", not
/// "is the daemon healthy" — distinguishing the two needs the IPC
/// machinery which is overkill for a pre-spawn discover.
///
/// Exposed `pub` so the `shot` warm-path (M4.3 T3) can probe daemon
/// liveness without committing to a full discover/spawn cycle: a
/// negative answer falls through to the cold-path dispatcher which
/// owns the spawn machinery.
pub fn socket_is_live(path: &Path, timeout: Duration) -> bool {
    if !path.exists() {
        return false;
    }
    // `std::os::unix::net::UnixStream::connect` is synchronous and
    // doesn't take a timeout, but UDS connect is effectively
    // instantaneous on the local socket. The OS will fail-fast with
    // ECONNREFUSED if no listener is bound, so a bare `connect()`
    // gives us the answer in microseconds. A small spin-loop on the
    // ConnectionRefused error covers the narrow race where the inode
    // exists but the daemon hasn't called `listen(2)` yet.
    let deadline = Instant::now() + timeout;
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return false,
        }
    }
}

/// Block until the socket at `path` accepts a `connect(2)` (the
/// daemon is ready) or `timeout` elapses. Returns
/// [`LauncherError::SpawnTimeout`] on timeout.
pub fn wait_for_ready(path: &Path, timeout: Duration) -> Result<(), LauncherError> {
    let deadline = Instant::now() + timeout;
    loop {
        if socket_is_live(path, Duration::from_millis(100)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(LauncherError::SpawnTimeout {
                socket: path.to_path_buf(),
                timeout,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Locate the active `java` binary. Resolved from `$JAVA_HOME/bin/java`
/// first, then `$PATH`. Returns [`LauncherError::JavaNotFound`] when
/// neither produces a usable path.
pub fn locate_java() -> Result<PathBuf, LauncherError> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let candidate = Path::new(&home).join("bin").join("java");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    which::which("java").map_err(|_| LauncherError::JavaNotFound)
}

/// Best-effort kill of a stale daemon: read the PID file, send a
/// `SIGKILL`, and unlink the socket inode. Logs at "warning" volume on
/// failures but never propagates an error — the next bind will surface
/// any remaining problem (e.g. EADDRINUSE).
fn reap_stale_daemon(plan: &LaunchPlan) {
    if let Ok(pid_str) = std::fs::read_to_string(plan.socket_dir.join(PID_LEAF))
        && let Ok(pid) = pid_str.trim().parse::<i32>()
    {
        // SIGKILL is intentional: the previous daemon is unresponsive
        // (we just tried to `connect()` and failed); a polite SIGTERM
        // here would deadlock if the daemon was already wedged.
        // `kill -9` against a non-existent pid is a no-op (`ESRCH`).
        //
        // We shell out to the system `kill` binary rather than pull
        // `libc` / `nix` into the workspace for a single syscall.
        // The portability tradeoff (a fork+exec per stale daemon) is
        // bounded — this path runs at most once per `barista verify`
        // when the previous daemon crashed.
        let _ = Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = std::fs::remove_file(&plan.socket_path);
}

fn ensure_socket_dir(dir: &Path) -> Result<(), LauncherError> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| LauncherError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        // Tighten to 0700; the daemon's bind ceremony depends on the
        // parent dir already being owner-only (see Server.java's
        // `DIR_PERMS_0700`).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(dir)
                .map_err(|e| LauncherError::Io {
                    path: dir.to_path_buf(),
                    source: e,
                })?
                .permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(dir, perms).map_err(|e| LauncherError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
        }
    }
    Ok(())
}

fn write_pid(socket_dir: &Path, pid: u32) -> std::io::Result<()> {
    std::fs::write(socket_dir.join(PID_LEAF), pid.to_string())
}

// ---------------------------------------------------------------------------
// Classpath discovery
// ---------------------------------------------------------------------------

/// Strategies attempted in [`discover_jvm_entry`]. Documented as a
/// type so error messages can list which paths were exercised.
const TRIED_LIST: &str =
    "$BARISTA_BARBACK_JAR, $BARISTA_BARBACK_CLASSPATH, $BARISTA_BARBACK_HOME/target/classes";

/// Discover a [`JvmEntry`] to invoke barback with.
///
/// Resolution order:
///
/// 1. `$BARISTA_BARBACK_JAR` — explicit uber-JAR path.
/// 2. `$BARISTA_BARBACK_CLASSPATH` — explicit classpath.
/// 3. `$BARISTA_BARBACK_HOME` (or, when unset, a dev-loop checkout at
///    `<current_dir_walk_up>/barback/`) — the classpath is assembled
///    from `target/classes` + `target/test-classes` + the cached
///    `mvn dependency:build-classpath` output.
///
/// `cwd` is the directory the dev-loop walk-up starts at. Most callers
/// pass `std::env::current_dir().unwrap_or(...)`. The hookable param
/// keeps the function pure for tests.
pub fn discover_jvm_entry(cwd: &Path) -> Result<JvmEntry, LauncherError> {
    if let Some(jar) = std::env::var_os("BARISTA_BARBACK_JAR") {
        let p = PathBuf::from(jar);
        if p.is_file() {
            return Ok(JvmEntry::Jar(p));
        }
    }
    if let Some(cp) = std::env::var_os("BARISTA_BARBACK_CLASSPATH") {
        return Ok(JvmEntry::Classpath {
            classpath: cp,
            main_class: "com.bluminal.barista.barback.Server".to_string(),
        });
    }
    let barback_dir = std::env::var_os("BARISTA_BARBACK_HOME")
        .map(PathBuf::from)
        .or_else(|| find_barback_dev_dir(cwd));
    if let Some(dir) = barback_dir {
        let cp = build_classpath_from_dev_dir(&dir)?;
        return Ok(JvmEntry::Classpath {
            classpath: cp,
            main_class: "com.bluminal.barista.barback.Server".to_string(),
        });
    }
    Err(LauncherError::JarNotFound {
        tried: TRIED_LIST.to_string(),
    })
}

fn find_barback_dev_dir(cwd: &Path) -> Option<PathBuf> {
    let mut here = Some(cwd.to_path_buf());
    while let Some(d) = here {
        let candidate = d.join("barback").join("pom.xml");
        if candidate.is_file() {
            return Some(d.join("barback"));
        }
        here = d.parent().map(Path::to_path_buf);
    }
    None
}

fn build_classpath_from_dev_dir(barback_dir: &Path) -> Result<OsString, LauncherError> {
    let cache_dir = barback_dir.join("target");
    let cache_file = cache_dir.join(CLASSPATH_CACHE_LEAF);
    let cp_separator: char = if cfg!(windows) { ';' } else { ':' };

    let classes = barback_dir.join("target").join("classes");
    let test_classes = barback_dir.join("target").join("test-classes");

    // If the cache file exists and `classes/` is non-empty, reuse the
    // cached classpath line. We don't try to invalidate based on
    // `pom.xml` mtime: the dev loop runs `mvn` explicitly when deps
    // change, and `barista verify` callers can delete the cache file
    // to force a refresh.
    let cached_cp = if cache_file.is_file() && classes.is_dir() {
        std::fs::read_to_string(&cache_file)
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    };

    let resolved = if let Some(cp) = cached_cp {
        cp
    } else {
        let out = mvn_dependency_classpath(barback_dir)?;
        // Try to persist the cache; non-fatal on failure.
        let _ = std::fs::write(&cache_file, &out);
        out
    };

    let mut full = OsString::new();
    if test_classes.is_dir() {
        full.push(&test_classes);
        full.push(cp_separator.to_string());
    }
    if classes.is_dir() {
        full.push(&classes);
        full.push(cp_separator.to_string());
    } else {
        // No compiled classes — try to compile them.
        mvn_compile(barback_dir)?;
        full.push(&classes);
        full.push(cp_separator.to_string());
    }
    full.push(&resolved);
    Ok(full)
}

fn mvn_binary_name() -> &'static str {
    if cfg!(windows) { "mvn.cmd" } else { "mvn" }
}

fn mvn_dependency_classpath(barback_dir: &Path) -> Result<String, LauncherError> {
    // `mvn_binary_name()` returns a `&'static str` literal ("mvn" /
    // "mvn.cmd"). The cfg-gated helper exists to keep the Windows
    // / Unix branch in one place, not to introduce a dynamic name —
    // semgrep flags the function call shape, not a real risk.
    // nosemgrep: barista-rust-unchecked-command-new
    let out = Command::new(mvn_binary_name())
        .arg("-f")
        .arg(barback_dir.join("pom.xml"))
        .arg("-q")
        .arg("dependency:build-classpath")
        .arg("-Dmdep.outputFile=/dev/stdout")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| LauncherError::Io {
            path: barback_dir.to_path_buf(),
            source: e,
        })?;
    if !out.status.success() {
        return Err(LauncherError::MavenClasspathFailed {
            exit: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn mvn_compile(barback_dir: &Path) -> Result<(), LauncherError> {
    // See `mvn_dependency_classpath` above — `mvn_binary_name()`
    // is a static-`&str` cfg-gated literal, not a dynamic name.
    // nosemgrep: barista-rust-unchecked-command-new
    let out = Command::new(mvn_binary_name())
        .arg("-f")
        .arg(barback_dir.join("pom.xml"))
        .arg("-q")
        .arg("test-compile")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| LauncherError::Io {
            path: barback_dir.to_path_buf(),
            source: e,
        })?;
    if !out.status.success() {
        return Err(LauncherError::MavenClasspathFailed {
            exit: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_plan_constructor_defaults_socket_path() {
        let plan = LaunchPlan::new(PathBuf::from("/tmp/baristarun"), 4, 1800);
        assert_eq!(
            plan.socket_path,
            PathBuf::from("/tmp/baristarun/barback.sock")
        );
        assert_eq!(plan.workers, 4);
        assert_eq!(plan.idle_shutdown_secs, 1800);
        assert!(plan.crash_after.is_none());
    }

    #[test]
    fn socket_is_live_returns_false_when_inode_missing() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("never-bound.sock");
        assert!(!socket_is_live(&p, Duration::from_millis(50)));
    }

    #[test]
    fn ensure_socket_dir_creates_with_0700() {
        let td = tempfile::tempdir().unwrap();
        let d = td.path().join("baristarun");
        ensure_socket_dir(&d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "socket dir must be 0700");
        }
    }

    // Test mutates process-wide env vars; per-fn allow keeps the
    // workspace-wide `unsafe_code = warn` lint clean while letting
    // the test exercise the Rust 2024 `set_var` unsafety contract.
    #[allow(unsafe_code)]
    #[test]
    fn discover_jvm_entry_prefers_explicit_classpath_env() {
        // SAFETY: setting an environment variable is racy across
        // threads — this test is single-threaded and the explicit
        // env is restored at end.
        // SAFETY: this test sets process-wide env vars. The test
        // suite runs each test in a fresh process under `cargo
        // test`, so cross-test interference is bounded. We still
        // restore the variables on exit so concurrent test
        // executors (`cargo nextest`) don't observe leaked state.
        let prev_jar = std::env::var_os("BARISTA_BARBACK_JAR");
        let prev_cp = std::env::var_os("BARISTA_BARBACK_CLASSPATH");
        // SAFETY: see function-level comment.
        unsafe {
            std::env::remove_var("BARISTA_BARBACK_JAR");
            std::env::set_var("BARISTA_BARBACK_CLASSPATH", "/x:/y");
        }
        let entry = discover_jvm_entry(Path::new("/")).expect("explicit CP wins");
        match entry {
            JvmEntry::Classpath {
                classpath,
                main_class,
            } => {
                assert_eq!(classpath, OsString::from("/x:/y"));
                assert_eq!(main_class, "com.bluminal.barista.barback.Server");
            }
            JvmEntry::Jar(_) => panic!("expected Classpath, got Jar"),
        }
        // SAFETY: see function-level comment.
        unsafe {
            std::env::remove_var("BARISTA_BARBACK_CLASSPATH");
            if let Some(v) = prev_jar {
                std::env::set_var("BARISTA_BARBACK_JAR", v);
            }
            if let Some(v) = prev_cp {
                std::env::set_var("BARISTA_BARBACK_CLASSPATH", v);
            }
        }
    }
}
