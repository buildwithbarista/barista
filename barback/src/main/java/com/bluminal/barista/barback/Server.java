// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;
import com.bluminal.barista.barback.proto.Envelope;
import com.bluminal.barista.barback.proto.Error;
import com.bluminal.barista.barback.proto.Ping;
import com.bluminal.barista.barback.proto.Pong;
import com.bluminal.barista.barback.proto.RedactedCredential;
import com.bluminal.barista.barback.proto.Shutdown;
import com.bluminal.barista.barback.core.EmbeddedMaven;
import com.bluminal.barista.barback.core.EmbeddedMavenActionDispatcher;
import com.bluminal.barista.barback.core.EmbeddedMavenFactory;
import com.bluminal.barista.barback.lifecycle.IdleTimer;
import com.bluminal.barista.barback.workers.WorkerPool;
import com.google.protobuf.InvalidProtocolBufferException;

import java.io.IOException;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermission;
import java.time.Clock;
import java.time.Duration;
import java.util.EnumSet;
import java.util.Locale;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * Production socket server for the {@code barback} worker daemon.
 *
 * <p>Listens on a Unix domain socket (Linux + macOS) and dispatches
 * length-prefixed {@code Envelope} messages from the {@code barista}
 * CLI to per-connection handlers. The wire format matches the contract
 * established in {@code proto/barista/v1/worker.proto} and validated
 * end-to-end by the Rust&harr;Java conformance harness landed in
 * milestone 4.1: each frame is a {@code u32} big-endian length prefix
 * followed by exactly that many bytes of protobuf-encoded
 * {@link Envelope} payload.
 *
 * <h2>Dispatch shell &mdash; scope of this version</h2>
 *
 * <p>The server handles three {@link Envelope.BodyCase} variants
 * inline today; the rest are routed to the canonical
 * {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} placeholder so downstream
 * CLI tests can pin to the contract while the embedded Maven core is
 * still under construction:
 *
 * <ul>
 *   <li>{@link Envelope.BodyCase#PING} &mdash; answered inline with a
 *       {@link Pong} carrying the daemon identifier and JDK metadata.
 *       This is the connection-liveness handshake from the CLI side
 *       and lets clients confirm the daemon is reachable before
 *       submitting work.</li>
 *   <li>{@link Envelope.BodyCase#SHUTDOWN} &mdash; flips the server's
 *       shutdown flag, drains in-flight connections, and closes the
 *       listener. The connection that delivered the {@link Shutdown}
 *       message is allowed to read further envelopes from any queued
 *       data already on the socket; subsequent {@code accept()} calls
 *       return null. Pairs with the {@link IdleTimer}-driven
 *       self-shutdown path: the server tears down on either the
 *       explicit {@link Shutdown} envelope or the idle window
 *       elapsing without any inbound activity, whichever fires
 *       first.</li>
 *   <li>{@link Envelope.BodyCase#ACTION} &mdash; routed to a stub
 *       handler that returns an {@link ActionResult} with
 *       {@link Error#getCode()} = {@code "BAR-DAEMON-NOT-YET-IMPLEMENTED"}
 *       and a non-zero exit code. This placeholder will be replaced
 *       when the embedded Maven 4 core lands in milestone 4.2 Task 3.
 *       The CLI side can wire its dispatch + error rendering today
 *       against this stable wire shape.</li>
 * </ul>
 *
 * <p>Every other envelope variant currently produces the same
 * {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} error reply scoped by
 * {@code request_id} so connections do not hang on an unknown
 * dispatch case.
 *
 * <h2>Threading model</h2>
 *
 * <p>The accept loop runs on a single dedicated thread. Each accepted
 * connection is handed to a {@link com.bluminal.barista.barback.workers.WorkerPool},
 * which picks its backing executor based on the running JVM: virtual
 * threads on Java 21+ (with a {@link java.util.concurrent.Semaphore}
 * enforcing the configured concurrency budget), and a bounded
 * platform-thread {@link java.util.concurrent.ThreadPoolExecutor} on
 * Java 17. The budget is the {@code workers} field of
 * {@link SocketConfig}, sourced from the {@code --workers} flag or
 * the host-default {@code 1C} fallback. {@link #startWith(SocketConfig,
 * com.bluminal.barista.barback.workers.WorkerPool)} accepts a
 * caller-built pool so tests and benches can drive either backend
 * directly.
 *
 * <h2>Socket-permission contract</h2>
 *
 * <p>Immediately after binding the Unix domain socket inode, the
 * server calls {@link Files#setPosixFilePermissions(Path, Set)} to
 * narrow the file mode to {@code 0600}. This is the only access
 * control the IPC layer relies on per the security note in
 * {@code worker.proto}: any process able to read the socket inode is
 * implicitly trusted on the wire, so the inode is owner-only.
 *
 * <h2>Default socket path</h2>
 *
 * <p>The default path follows the platform freedesktop convention:
 *
 * <ul>
 *   <li>{@code $XDG_RUNTIME_DIR/barista/barback.sock} when
 *       {@code XDG_RUNTIME_DIR} is set;</li>
 *   <li>{@code $HOME/.barista/run/barback.sock} otherwise.</li>
 * </ul>
 *
 * <p>The parent directory is created with {@code 0700} permissions if
 * it does not already exist, so the bind cannot race a permissive
 * mode.
 *
 * <h2>Windows</h2>
 *
 * <p>{@link #start(SocketConfig)} throws
 * {@link UnsupportedOperationException} on Windows: the production
 * server-side named-pipe binding (with a DACL pinned to the current
 * user SID + {@code NT AUTHORITY\\SYSTEM}) is deferred to a follow-up
 * task scoped explicitly within milestone 4.2. The Windows CLI can
 * still exercise the conformance-validated wire format against a
 * remote daemon over the same protocol; only the local-daemon bind
 * path is missing. See the TODO immediately above {@link #bindUnix}
 * for the follow-up scope.
 *
 * <h2>Robustness</h2>
 *
 * <p>A single misbehaving connection (garbage bytes, oversized length
 * prefix, malformed protobuf, mid-frame EOF) is logged at
 * {@link Level#WARNING} and the offending {@link SocketChannel} is
 * closed; the accept loop continues. The connection-level crash
 * detection on the CLI side is wired in milestone 4.2 Task 6 (the
 * Rust side maps a mid-action socket close to a structured
 * {@code BAR-DAEMON-CRASHED} retryable error; see the Rust
 * {@code barista_ipc::TransportError::DaemonCrashed} variant).
 *
 * <h3>Debug failure-model fixture (M4.2 T6)</h3>
 *
 * <p>The hidden {@code --crash-after <n>} flag arms the daemon for
 * deliberate self-termination via {@link Runtime#halt(int)} after
 * exactly {@code n} action envelopes have been dispatched. The exit
 * code is {@link #CRASH_EXIT_CODE} (137 = {@code 128 + SIGKILL}). This
 * is the fixture path the cross-language integration tests use to
 * exercise the CLI-side failure-model wiring against a real daemon
 * — see {@code crates/barista-ipc/tests/crash_recovery_conformance.rs}
 * for the Rust side. The flag is intentionally absent from the
 * {@link #main} usage string and the {@code SocketConfig} constructor
 * chain defaults it to {@code 0} (disabled).
 *
 * <h2>Logging</h2>
 *
 * <p>Uses {@link java.util.logging} so the daemon does not pull in a
 * logging framework dependency. All log lines that mention an
 * envelope route any {@code Credential} instances through
 * {@link RedactedCredential} so credential bytes never reach the
 * log stream.
 */
public final class Server implements AutoCloseable {

    private static final Logger LOG = Logger.getLogger(Server.class.getName());

    /**
     * Width in bytes of the length-prefix field that precedes every
     * envelope on the wire. Matches the Rust side's
     * {@code LengthDelimitedCodec.length_field_length(4).big_endian()}
     * configuration and the {@code u32} length-prefix described in
     * {@code worker.proto} &sect;Framing.
     */
    static final int LENGTH_FIELD_BYTES = 4;

    /**
     * Hard cap on the size of a single envelope. Matches the Rust
     * side's {@code MAX_FRAME_BYTES = 16 MiB} constant in
     * {@code crates/barista-ipc/src/transport/mod.rs}. Larger frames
     * trigger a clean connection close so a peer cannot trick the
     * daemon into allocating 4 GiB because it announced a 4 GiB
     * length.
     */
    static final int MAX_FRAME_BYTES = 16 * 1024 * 1024;

    /** Wire protocol version number the daemon speaks. Mirrors the
     *  constant the CLI uses to populate {@link Envelope#getVersion}. */
    static final int PROTOCOL_VERSION = 1;

    /**
     * Canonical placeholder error code returned for every action that
     * reaches the daemon before the embedded Maven core (milestone 4.2
     * Task 3) lands. CLI-side tests can pin to this code.
     */
    static final String NOT_YET_IMPLEMENTED_CODE = "BAR-DAEMON-NOT-YET-IMPLEMENTED";

    /**
     * POSIX permission set for the socket inode: owner read+write,
     * everyone else nothing.
     */
    private static final Set<PosixFilePermission> SOCKET_PERMS_0600 =
            EnumSet.of(PosixFilePermission.OWNER_READ, PosixFilePermission.OWNER_WRITE);

    /**
     * POSIX permission set for the socket's parent directory: owner
     * full access (read+write+execute), everyone else nothing.
     */
    private static final Set<PosixFilePermission> DIR_PERMS_0700 = EnumSet.of(
            PosixFilePermission.OWNER_READ,
            PosixFilePermission.OWNER_WRITE,
            PosixFilePermission.OWNER_EXECUTE);

    /**
     * Default worker-pool size when {@code --workers} is not specified
     * and the caller-supplied {@link SocketConfig} does not pin a
     * value. Matches the {@code barback.default_workers = "1C"}
     * default from the daemon configuration spec ("one per core"). The
     * Rust CLI will resolve the expression {@code "1C"} / {@code "0.75C"}
     * / etc. before spawning the daemon; this value is the fallback
     * for the bare {@code java -jar barback-uber.jar} entry point that
     * exercises the daemon without going through the CLI.
     */
    static final int DEFAULT_WORKERS = Math.max(1, Runtime.getRuntime().availableProcessors());

    /**
     * Default idle-shutdown window in seconds when {@code --idle-shutdown}
     * is not specified and the caller-supplied {@link SocketConfig}
     * does not pin a value. Matches the {@code barback.idle_shutdown_seconds
     * = 1800} default from the daemon configuration spec (PRD §11.2.3
     * — "after {@code idle_shutdown_seconds} of no requests, the
     * daemon self-terminates").
     */
    static final int DEFAULT_IDLE_SHUTDOWN_SECONDS = 1800;

    /**
     * POSIX exit code chosen for the {@code --crash-after} debug-flag
     * self-immolation path. {@code 128 + SIGKILL(9) = 137} matches the
     * status the kernel would have reported if an external
     * {@code kill -9} had terminated the JVM, so logs and tests can
     * pin to the canonical value regardless of how the crash was
     * induced. See the {@code crashAfter} field of {@link SocketConfig}
     * and the {@link #handleConnection} dispatch counter for the
     * trigger path.
     */
    static final int CRASH_EXIT_CODE = 137;

    /**
     * Strategy seam between the connection-handling layer and whatever
     * actually executes a mojo. The production implementation is
     * {@link EmbeddedMavenActionDispatcher}, which drives the
     * daemon-resident Maven core; tests substitute a fake that returns
     * a canned {@link ActionResult} without spinning up Maven.
     */
    @FunctionalInterface
    public interface ActionDispatcher {
        /**
         * Run one action and return the terminal {@link ActionResult}.
         * Implementations may block; the caller invokes this on a
         * worker thread.
         */
        ActionResult dispatch(ActionRequest request);
    }

    /**
     * Sentinel dispatcher used by {@link #startWith(SocketConfig, WorkerPool)}
     * — the four-arg compatibility entry point retained for the M4.2
     * conformance and worker-pool integration tests, which pin to the
     * historical {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} stub response
     * shape. Production callers go through
     * {@link #startWithEmbeddedMaven(SocketConfig)} or pass an
     * explicit dispatcher to {@link #startWith(SocketConfig, WorkerPool,
     * ActionDispatcher)}; this constant is the M4.2-compat fallback
     * only.
     */
    public static final ActionDispatcher NOT_YET_IMPLEMENTED_DISPATCHER = request -> {
        Error err = Error.newBuilder()
                .setCode(NOT_YET_IMPLEMENTED_CODE)
                .setMessage("embedded Maven core is wired in M4.2 T3; "
                        + "this daemon build cannot yet execute mojos")
                .setActionId(request.getActionId())
                .build();
        return ActionResult.newBuilder()
                .setActionId(request.getActionId())
                .setStatus(ActionResult.Status.FAILURE)
                .setExitCode(1)
                .setError(err)
                .setFailureMessage(err.getMessage())
                .build();
    };

    private final Path socketPath;
    private final ServerSocketChannel serverChannel;
    private final WorkerPool workerPool;
    private final IdleTimer idleTimer;
    private final Thread acceptThread;
    private final AtomicBoolean shutdownRequested = new AtomicBoolean(false);
    private final CountDownLatch terminated = new CountDownLatch(1);
    private final AtomicLong nextConnectionId = new AtomicLong(0);
    /**
     * Count of action-dispatch envelopes routed so far. Wired only by
     * the {@code --crash-after} debug path: every {@link Envelope.BodyCase#ACTION}
     * that reaches {@link #dispatch} increments this counter, and once
     * the value matches {@code crashAfter} the JVM self-terminates via
     * {@link Runtime#halt(int)} mid-write. The increment site is
     * <em>after</em> the action reply has been queued onto the socket
     * but <em>before</em> the SocketChannel write has drained, which
     * mirrors the realistic crash window the CLI must defend against —
     * the daemon may have produced bytes the kernel never flushes.
     */
    private final AtomicLong actionsDispatched = new AtomicLong(0);
    /** Trigger threshold for the {@code --crash-after} debug path; 0 disables. */
    private final int crashAfter;
    /**
     * Per-server action dispatcher. The production path
     * ({@link #startWithEmbeddedMaven}) installs an
     * {@link EmbeddedMavenActionDispatcher} that drives the resident
     * Maven core; tests and the M4.2 {@link #startWith(SocketConfig,
     * WorkerPool)} compatibility entry point install
     * {@link #NOT_YET_IMPLEMENTED_DISPATCHER} which preserves the
     * historical "BAR-DAEMON-NOT-YET-IMPLEMENTED" response — the
     * stub-shaped digest contract the M4.2 conformance suites
     * exercise stays byte-identical on that path.
     */
    private final ActionDispatcher actionDispatcher;
    /**
     * Optional resource handle that lives as long as this server and
     * gets closed during {@link #shutdown()}. The production entry
     * point parks the daemon-owned {@link EmbeddedMaven} here so the
     * core's class-world + resident invoker are torn down cleanly on
     * idle-shutdown / SIGTERM. {@code null} on the test paths where
     * no embedded core was built.
     */
    private final AutoCloseable ownedResource;

    private Server(Path socketPath,
                   ServerSocketChannel serverChannel,
                   WorkerPool workerPool,
                   Duration idleTimeout,
                   int crashAfter,
                   ActionDispatcher actionDispatcher,
                   AutoCloseable ownedResource) {
        this.socketPath = socketPath;
        this.serverChannel = serverChannel;
        this.workerPool = workerPool;
        this.crashAfter = crashAfter;
        this.actionDispatcher = Objects.requireNonNull(actionDispatcher, "actionDispatcher");
        this.ownedResource = ownedResource;
        // The idle-timer callback is `this::shutdownDueToIdle` — a
        // method reference that flips the shutdown flag and closes
        // the listener, which is idempotent and safe to invoke from
        // the timer's daemon thread. The accept loop sees the
        // resulting `ClosedChannelException`, drains workers, and
        // counts down `terminated`. Captured here (in the
        // constructor) so the timer field can stay final.
        this.idleTimer = new IdleTimer(
                idleTimeout, this::shutdownDueToIdle, Clock.systemUTC());
        this.acceptThread = new Thread(this::runAcceptLoop, "barback-accept");
        this.acceptThread.setDaemon(false);
    }

    /**
     * Configuration for a single {@link Server} instance.
     *
     * @param socketPath absolute path the server should bind. The
     *     parent directory is created with {@code 0700} permissions if
     *     it does not already exist. Any existing inode at this path
     *     is unlinked before {@code bind()}, so the start path is
     *     idempotent across restarts.
     * @param workers concurrency budget for the per-connection
     *     {@link WorkerPool}. Realises the {@code barback.workers}
     *     setting from the daemon configuration spec; the Rust CLI
     *     resolves {@code default_workers} expressions like
     *     {@code "1C"} or {@code "0.75C"} to a concrete integer before
     *     spawning the daemon. Must be &ge; 1.
     * @param idleShutdownSeconds idle window (in seconds) after which
     *     the daemon self-terminates with no in-flight or recently-
     *     completed work. Realises the {@code barback.idle_shutdown_seconds}
     *     setting from the daemon configuration spec (PRD §11.2.3).
     *     Must be &ge; 1. Defaults to {@link #DEFAULT_IDLE_SHUTDOWN_SECONDS}
     *     (30 minutes) when the no-arg overload is used. The Rust CLI
     *     will resolve {@code barback.toml}'s {@code idle_shutdown_seconds}
     *     value before spawning the daemon; that wire-through is the
     *     companion follow-up to T2's worker-count plumbing.
     */
    public record SocketConfig(Path socketPath, int workers, int idleShutdownSeconds, int crashAfter) {

        public SocketConfig {
            Objects.requireNonNull(socketPath, "socketPath");
            if (workers <= 0) {
                throw new IllegalArgumentException(
                        "workers must be >= 1; got " + workers);
            }
            if (idleShutdownSeconds <= 0) {
                throw new IllegalArgumentException(
                        "idleShutdownSeconds must be >= 1; got " + idleShutdownSeconds);
            }
            if (crashAfter < 0) {
                throw new IllegalArgumentException(
                        "crashAfter must be >= 0 (0 disables); got " + crashAfter);
            }
        }

        /**
         * Three-arg compatibility constructor: picks {@code crashAfter
         * = 0} (disabled) so production call sites that never need the
         * debug self-immolation path keep compiling. The
         * {@code --crash-after} flag exists exclusively to drive the
         * M4.2 T6 failure-model integration tests against a real
         * daemon — see {@link #CRASH_EXIT_CODE} for the trigger
         * semantics.
         */
        public SocketConfig(Path socketPath, int workers, int idleShutdownSeconds) {
            this(socketPath, workers, idleShutdownSeconds, 0);
        }

        /**
         * Two-arg compatibility constructor: picks the default idle
         * window + {@code crashAfter = 0} so existing call sites that
         * only specify a worker count keep compiling.
         */
        public SocketConfig(Path socketPath, int workers) {
            this(socketPath, workers, DEFAULT_IDLE_SHUTDOWN_SECONDS, 0);
        }

        /**
         * Convenience constructor that picks the per-host
         * {@link #DEFAULT_WORKERS} value and the
         * {@link #DEFAULT_IDLE_SHUTDOWN_SECONDS} idle window. Retained
         * so existing call sites that did not specify either keep
         * compiling.
         */
        public SocketConfig(Path socketPath) {
            this(socketPath, DEFAULT_WORKERS, DEFAULT_IDLE_SHUTDOWN_SECONDS, 0);
        }

        /**
         * Return a copy of this config with the given idle-shutdown
         * window. Helper for callers (especially tests) that already
         * have a {@code SocketConfig} from {@link #defaultPath()} or
         * one of the other constructors and want to override a single
         * field without restating the others.
         */
        public SocketConfig withIdleShutdownSeconds(int seconds) {
            return new SocketConfig(socketPath, workers, seconds, crashAfter);
        }

        /**
         * Return a copy of this config with the given {@code crashAfter}
         * threshold. {@code 0} disables. Used by the failure-model
         * integration tests in {@link com.bluminal.barista.barback.integration.CrashFailureModelIT}
         * to launch a daemon that self-terminates with {@link #CRASH_EXIT_CODE}
         * after dispatching {@code n} action envelopes — the fixture
         * for the cross-language M4.2 T6 acceptance criterion.
         */
        public SocketConfig withCrashAfter(int crashAfter) {
            return new SocketConfig(socketPath, workers, idleShutdownSeconds, crashAfter);
        }

        /**
         * Build a config that binds at the freedesktop-default location
         * for the current user, falling back to
         * {@code $HOME/.barista/run/barback.sock} when
         * {@code XDG_RUNTIME_DIR} is unset. Worker count defaults to
         * {@link #DEFAULT_WORKERS}; idle window defaults to
         * {@link #DEFAULT_IDLE_SHUTDOWN_SECONDS}.
         */
        public static SocketConfig defaultPath() {
            String xdg = System.getenv("XDG_RUNTIME_DIR");
            Path base;
            if (xdg != null && !xdg.isEmpty()) {
                base = Path.of(xdg, "barista");
            } else {
                String home = System.getProperty("user.home");
                if (home == null || home.isEmpty()) {
                    throw new IllegalStateException(
                            "neither XDG_RUNTIME_DIR nor user.home is set; "
                                    + "supply an explicit socket path");
                }
                base = Path.of(home, ".barista", "run");
            }
            return new SocketConfig(
                    base.resolve("barback.sock"),
                    DEFAULT_WORKERS,
                    DEFAULT_IDLE_SHUTDOWN_SECONDS,
                    0);
        }
    }

    /**
     * Bind the socket described by {@code config} and start the
     * accept loop on a dedicated thread. Returns once the listener is
     * ready to accept connections; the accept loop continues running
     * until {@link #shutdown()} (or a {@link Shutdown} envelope) is
     * received.
     *
     * @throws UnsupportedOperationException on Windows, where the
     *     production named-pipe binding is deferred (see class
     *     javadoc).
     * @throws IOException if the socket bind, the {@code chmod 0600},
     *     or the parent-directory creation fails.
     */
    public static Server start(SocketConfig config) throws IOException {
        Objects.requireNonNull(config, "config");
        if (isWindows()) {
            // TODO(m4.2-windows-follow-up): The Rust side already binds
            // the DACL'd pipe in `crates/barista-ipc::transport::win`
            // for the M4.1 conformance harness, but in that harness the
            // Java side is the *client* — the production server-side
            // bind from Java is what is missing. The follow-up scope
            // is: (1) a small JNI shim (or a Project-Panama
            // `Linker.downcallHandle()` call into
            // `CreateNamedPipeW` + `SetSecurityDescriptorDacl`) that
            // mirrors the Rust `bind_secure` builder, (2) a
            // ServerSocketChannel-equivalent surface that lets the
            // accept loop below stay unchanged on Windows, and (3) the
            // Windows leg of the integration tests in ServerTest.
            throw new UnsupportedOperationException(
                    "barback server on Windows is deferred to a follow-up task; "
                            + "the Windows CLI can talk to a remote daemon over the "
                            + "conformance-validated wire format pending the production "
                            + "server binding");
        }
        // Default production wiring: install the embedded Maven core.
        // Callers that want a stub dispatcher (the M4.2 conformance
        // suites) go through {@link #startWith(SocketConfig, WorkerPool)}.
        return startWithEmbeddedMaven(config);
    }

    /**
     * Bind the socket and start the accept loop with the production
     * action dispatcher installed: an {@link EmbeddedMavenActionDispatcher}
     * wrapping an {@link EmbeddedMaven} core built from the host's
     * Maven&nbsp;4 distribution (see {@link EmbeddedMavenFactory#discover()}
     * for the resolution rules). The embedded core's lifetime tracks
     * this server: {@link #shutdown()} closes it.
     *
     * <p>This is the M4.3 T2 entry point that closes the T1 caveat —
     * before T2, every ACTION envelope reached the
     * {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} stub even though the
     * embedded core was already wired (M4.2 T3). After T2, the
     * production daemon path is end-to-end: connection &rarr; dispatch
     * &rarr; embedded Maven &rarr; {@link ActionResult}.
     *
     * @throws IOException if the socket bind fails or the embedded
     *     core's distribution discovery fails
     *     ({@link EmbeddedMavenFactory#resolveMavenHome()} surfaces
     *     the canonical error message).
     */
    public static Server startWithEmbeddedMaven(SocketConfig config) throws IOException {
        Objects.requireNonNull(config, "config");
        if (isWindows()) {
            throw new UnsupportedOperationException(
                    "barback server on Windows is deferred to a follow-up task; "
                            + "see Server.start(SocketConfig)");
        }
        WorkerPool pool = WorkerPool.create(config.workers());
        EmbeddedMaven embedded;
        try {
            embedded = EmbeddedMavenFactory.discover();
        } catch (IOException | RuntimeException e) {
            // Discovery failed (no maven.home, malformed distribution,
            // etc.). The worker pool we just built is dead weight at
            // this point — close it before we propagate so the JVM
            // doesn't leak the executor threads.
            try {
                pool.close();
            } catch (Exception ignored) {
                // Best-effort.
            }
            if (e instanceof IOException io) {
                throw io;
            }
            throw new IOException("failed to discover embedded Maven distribution: "
                    + e.getMessage(), e);
        }
        EmbeddedMavenActionDispatcher dispatcher = new EmbeddedMavenActionDispatcher(embedded);
        // The concrete dispatcher's method signature is structurally
        // identical to {@link ActionDispatcher#dispatch}, but Java's
        // structural-typing-via-functional-interface doesn't pick up
        // the relationship from class declaration alone. Convert via a
        // method reference. The dispatcher also owns the cleanup of
        // any per-action temp resources; the embedded core is closed
        // as the {@code ownedResource} below.
        ActionDispatcher seam = dispatcher::dispatch;
        return startWith(config, pool, seam, embedded);
    }

    /**
     * Start a server with a caller-supplied {@link WorkerPool}. The
     * runtime branch in {@link WorkerPool#create(int)} is bypassed and
     * the pool is wrapped as given. Lifetime of {@code pool} is
     * transferred to the returned {@link Server} &mdash; closing the
     * server closes the pool.
     *
     * <p>This is the injection seam the test suite uses to drive the
     * platform-thread fallback path under a JDK 21 runtime (and vice
     * versa) without rebooting the JVM. The CI matrix also runs the
     * full integration suite under both JDK 17 and JDK 21 cells so the
     * runtime-branch selection in {@link WorkerPool#create(int)} is
     * exercised end-to-end on a real JDK 17 in CI.
     *
     * @throws UnsupportedOperationException on Windows (see {@link #start(SocketConfig)}).
     * @throws IOException if the socket bind fails.
     */
    public static Server startWith(SocketConfig config, WorkerPool pool) throws IOException {
        return startWith(config, pool, NOT_YET_IMPLEMENTED_DISPATCHER, null);
    }

    /**
     * Start a server with a caller-supplied {@link WorkerPool} and a
     * caller-supplied {@link ActionDispatcher}. The dispatcher receives
     * every ACTION envelope after the worker-pool dispatch + idle-timer
     * activity reset; the production path supplies an
     * {@link EmbeddedMavenActionDispatcher}, tests supply a fake.
     */
    public static Server startWith(SocketConfig config, WorkerPool pool, ActionDispatcher dispatcher)
            throws IOException {
        return startWith(config, pool, dispatcher, null);
    }

    /**
     * Internal startWith that additionally accepts an owned resource
     * (typically the {@link EmbeddedMaven} wrapped by the production
     * dispatcher) so the server can close it during
     * {@link #shutdown()}. Package-private so test fixtures that own
     * their own resource lifetimes don't accidentally hand them off.
     */
    static Server startWith(SocketConfig config,
                            WorkerPool pool,
                            ActionDispatcher dispatcher,
                            AutoCloseable ownedResource) throws IOException {
        Objects.requireNonNull(config, "config");
        Objects.requireNonNull(pool, "pool");
        Objects.requireNonNull(dispatcher, "dispatcher");
        if (isWindows()) {
            throw new UnsupportedOperationException(
                    "barback server on Windows is deferred to a follow-up task; "
                            + "the Windows CLI can talk to a remote daemon over the "
                            + "conformance-validated wire format pending the production "
                            + "server binding");
        }
        ServerSocketChannel channel = bindUnix(config.socketPath());
        Server server = new Server(
                config.socketPath(),
                channel,
                pool,
                Duration.ofSeconds(config.idleShutdownSeconds()),
                config.crashAfter(),
                dispatcher,
                ownedResource);
        server.acceptThread.start();
        server.idleTimer.start();
        if (config.crashAfter() > 0) {
            // Surface the debug self-immolation arming at WARNING so a
            // production deployment that accidentally enables it shows
            // up loudly in the log stream. Tests under
            // `integration/CrashFailureModelIT` opt in deliberately.
            LOG.log(Level.WARNING,
                    () -> "barback armed for self-termination after "
                            + config.crashAfter()
                            + " actions (debug failure-model path; exit "
                            + CRASH_EXIT_CODE + ")");
        }
        LOG.log(Level.INFO,
                () -> "barback listening on " + config.socketPath()
                        + " (workers=" + pool.workers()
                        + ", backend=" + pool.backend()
                        + ", idleShutdownSeconds=" + config.idleShutdownSeconds() + ")");
        return server;
    }

    /**
     * Request a clean shutdown. The accept loop stops accepting new
     * connections, in-flight handlers are allowed to finish, and the
     * socket inode is removed. Safe to call from any thread, including
     * from inside a connection handler (the {@link Shutdown} envelope
     * dispatch invokes this directly).
     *
     * <p>This call is non-blocking; pair with {@link #awaitShutdown()}
     * to block until the listener has fully terminated.
     */
    public void shutdown() {
        if (shutdownRequested.compareAndSet(false, true)) {
            LOG.log(Level.INFO, "shutdown requested");
            // Stop the idle timer up-front so it cannot race the
            // listener close + worker drain. Safe to call from any
            // path — idleTimer.stop() is idempotent and never blocks.
            idleTimer.stop();
            // Closing the server channel unblocks an in-flight
            // accept() call by raising AsynchronousCloseException; the
            // accept loop checks `shutdownRequested` on the catch path
            // and exits cleanly.
            try {
                serverChannel.close();
            } catch (IOException e) {
                LOG.log(Level.FINE, "ignored exception closing listener during shutdown", e);
            }
        }
    }

    /**
     * Callback target for the {@link IdleTimer}. Logs the idle-shutdown
     * reason at {@code INFO} (so operators see why the daemon exited)
     * and then delegates to {@link #shutdown()}. Package-private so
     * the timer wiring in the constructor can reference it via
     * {@code this::shutdownDueToIdle}; not part of the public surface.
     */
    void shutdownDueToIdle() {
        LOG.log(Level.INFO, "idle window elapsed; daemon shutting down");
        shutdown();
    }

    /**
     * Block the calling thread until the accept loop has exited and
     * the per-connection executor has fully drained. Intended for use
     * from {@link #main(String[])} so the JVM stays alive while the
     * daemon serves traffic.
     */
    public void awaitShutdown() throws InterruptedException {
        terminated.await();
    }

    /**
     * Cooperatively shut down and wait for termination. Equivalent to
     * {@code shutdown(); awaitShutdown();} with the JVM-friendly try-
     * with-resources surface.
     */
    @Override
    public void close() throws IOException {
        shutdown();
        try {
            awaitShutdown();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new IOException("interrupted while awaiting shutdown", e);
        }
    }

    /** The path the server bound at. Useful for tests and diagnostics. */
    public Path socketPath() {
        return socketPath;
    }

    /**
     * CLI entry point. Three flags are recognised:
     *
     * <ul>
     *   <li>{@code --socket <path>}: override the default socket
     *       location;</li>
     *   <li>{@code --workers <n>}: concurrency budget for the worker
     *       pool. Must be &ge; 1. Defaults to
     *       {@code Runtime.availableProcessors()} ("1C") when omitted.
     *       The Rust CLI resolves {@code default_workers} expressions
     *       like {@code "1C"} / {@code "0.75C"} from
     *       {@code barback.toml} to a concrete integer before spawning
     *       this entry point.</li>
     *   <li>{@code --idle-shutdown <seconds>}: idle window in seconds
     *       after which the daemon self-terminates. Must be &ge; 1.
     *       Defaults to {@link #DEFAULT_IDLE_SHUTDOWN_SECONDS} (30
     *       minutes). The Rust CLI resolves
     *       {@code barback.idle_shutdown_seconds} from
     *       {@code barback.toml} before spawning this entry point.</li>
     * </ul>
     *
     * <p>Unknown flags cause the process to exit with status 2 and a
     * one-line usage summary on stderr. The server blocks on
     * {@link #awaitShutdown()} until a {@link Shutdown} envelope is
     * received from a client, or until the idle window elapses without
     * any inbound activity (whichever fires first).
     */
    public static void main(String[] args) throws Exception {
        Path socketPath = null;
        Integer workers = null;
        Integer idleShutdownSeconds = null;
        Integer crashAfter = null;
        for (int i = 0; i < args.length; i++) {
            String arg = args[i];
            switch (arg) {
                case "--socket" -> {
                    if (i + 1 >= args.length) {
                        System.err.println("--socket requires a path argument");
                        System.exit(2);
                        return;
                    }
                    socketPath = Path.of(args[++i]);
                }
                case "--workers" -> {
                    if (i + 1 >= args.length) {
                        System.err.println("--workers requires an integer argument");
                        System.exit(2);
                        return;
                    }
                    int parsed;
                    try {
                        parsed = Integer.parseInt(args[++i]);
                    } catch (NumberFormatException e) {
                        System.err.println("--workers requires an integer argument; got '"
                                + args[i] + "'");
                        System.exit(2);
                        return;
                    }
                    if (parsed <= 0) {
                        System.err.println("--workers must be >= 1; got " + parsed);
                        System.exit(2);
                        return;
                    }
                    workers = parsed;
                }
                case "--idle-shutdown" -> {
                    if (i + 1 >= args.length) {
                        System.err.println("--idle-shutdown requires an integer argument (seconds)");
                        System.exit(2);
                        return;
                    }
                    int parsed;
                    try {
                        parsed = Integer.parseInt(args[++i]);
                    } catch (NumberFormatException e) {
                        System.err.println("--idle-shutdown requires an integer argument; got '"
                                + args[i] + "'");
                        System.exit(2);
                        return;
                    }
                    if (parsed <= 0) {
                        System.err.println("--idle-shutdown must be >= 1; got " + parsed);
                        System.exit(2);
                        return;
                    }
                    idleShutdownSeconds = parsed;
                }
                case "--crash-after" -> {
                    // Debug-only failure-model fixture for M4.2 T6.
                    // Hidden from the production usage string below;
                    // documented in the class javadoc's "Robustness"
                    // section and in CrashFailureModelIT. Arming it
                    // makes the daemon call `Runtime.halt(137)` after
                    // exactly N action envelopes — the canonical
                    // "daemon kill -9 mid-action" reproducer.
                    if (i + 1 >= args.length) {
                        System.err.println("--crash-after requires an integer argument (action count)");
                        System.exit(2);
                        return;
                    }
                    int parsed;
                    try {
                        parsed = Integer.parseInt(args[++i]);
                    } catch (NumberFormatException e) {
                        System.err.println("--crash-after requires an integer argument; got '"
                                + args[i] + "'");
                        System.exit(2);
                        return;
                    }
                    if (parsed < 1) {
                        System.err.println("--crash-after must be >= 1; got " + parsed);
                        System.exit(2);
                        return;
                    }
                    crashAfter = parsed;
                }
                default -> {
                    System.err.println(
                            "barback: unknown flag '" + arg + "'. "
                                    + "Usage: barback [--socket <path>] [--workers <n>] "
                                    + "[--idle-shutdown <seconds>]");
                    System.exit(2);
                    return;
                }
            }
        }
        Path effectiveSocketPath = socketPath == null
                ? SocketConfig.defaultPath().socketPath()
                : socketPath;
        int effectiveWorkers = workers == null ? DEFAULT_WORKERS : workers;
        int effectiveIdle = idleShutdownSeconds == null
                ? DEFAULT_IDLE_SHUTDOWN_SECONDS
                : idleShutdownSeconds;
        int effectiveCrashAfter = crashAfter == null ? 0 : crashAfter;
        SocketConfig config = new SocketConfig(
                effectiveSocketPath, effectiveWorkers, effectiveIdle, effectiveCrashAfter);
        Server server = Server.start(config);
        Runtime.getRuntime().addShutdownHook(new Thread(server::shutdown, "barback-sigterm"));
        server.awaitShutdown();
    }

    // ---------------------------------------------------------------
    // Implementation details below this line
    // ---------------------------------------------------------------

    private static boolean isWindows() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        return os.startsWith("windows");
    }

    private static ServerSocketChannel bindUnix(Path socketPath) throws IOException {
        Path parent = socketPath.getParent();
        if (parent != null) {
            if (!Files.exists(parent)) {
                Files.createDirectories(parent);
                trySetPermissions(parent, DIR_PERMS_0700);
            }
        }
        // Unlink any stale inode so bind() does not fail with EADDRINUSE
        // after an unclean shutdown.
        Files.deleteIfExists(socketPath);
        UnixDomainSocketAddress addr = UnixDomainSocketAddress.of(socketPath);
        ServerSocketChannel ch = ServerSocketChannel.open(StandardProtocolFamily.UNIX);
        boolean opened = false;
        try {
            ch.bind(addr);
            // Narrow the inode to 0600 immediately. Any client with
            // visibility on the socket inode is implicitly trusted on
            // the wire, so this `chmod` is the IPC layer's only access
            // control.
            Files.setPosixFilePermissions(socketPath, SOCKET_PERMS_0600);
            opened = true;
            return ch;
        } finally {
            if (!opened) {
                try {
                    ch.close();
                } catch (IOException ignored) {
                    // The caller is already raising; preserve that.
                }
            }
        }
    }

    private static void trySetPermissions(Path path, Set<PosixFilePermission> perms) throws IOException {
        try {
            Files.setPosixFilePermissions(path, perms);
        } catch (UnsupportedOperationException e) {
            LOG.log(Level.FINE,
                    () -> "POSIX permission control not supported on " + path
                            + "; relying on filesystem defaults");
        }
    }

    private void runAcceptLoop() {
        try {
            while (!shutdownRequested.get()) {
                SocketChannel client;
                try {
                    client = serverChannel.accept();
                } catch (ClosedChannelException e) {
                    // Listener was closed by shutdown(); exit cleanly.
                    break;
                } catch (IOException e) {
                    if (shutdownRequested.get()) {
                        break;
                    }
                    LOG.log(Level.WARNING, "accept() failed; continuing", e);
                    continue;
                }
                if (client == null) {
                    continue;
                }
                long id = nextConnectionId.incrementAndGet();
                LOG.log(Level.FINE, () -> "accepted connection #" + id);
                try {
                    workerPool.execute(() -> handleConnection(id, client));
                } catch (RuntimeException e) {
                    // The pool refused the task (e.g. mid-shutdown
                    // race, including RejectedExecutionException from
                    // an already-closed backing executor). Log + close
                    // the connection so we never leak a SocketChannel.
                    LOG.log(Level.WARNING,
                            "worker pool rejected connection #" + id + "; closing", e);
                    closeQuietly(client);
                }
            }
        } finally {
            // Stop the idle timer first so it cannot race the listener
            // teardown (e.g. if the accept loop exited for a reason
            // other than the shutdown() path that already stopped the
            // timer). Idempotent — a second stop() after shutdown()
            // is a no-op.
            idleTimer.stop();
            // WorkerPool#close drains in-flight handlers within
            // WorkerPool.SHUTDOWN_GRACE and forces shutdownNow() if
            // any remain.
            workerPool.close();
            // Close the dispatcher-owned resource (the embedded Maven
            // core on the production path). The accept loop has already
            // drained, so no further dispatches can race this close.
            // Best-effort: a teardown failure here is logged but does
            // not block the rest of the shutdown sequence.
            if (ownedResource != null) {
                try {
                    ownedResource.close();
                } catch (Exception e) {
                    LOG.log(Level.WARNING,
                            "ignored failure closing server-owned resource", e);
                }
            }
            try {
                Files.deleteIfExists(socketPath);
            } catch (IOException e) {
                LOG.log(Level.FINE, "ignored failure removing socket inode", e);
            }
            terminated.countDown();
            LOG.log(Level.INFO, "barback listener terminated");
        }
    }

    private void handleConnection(long connectionId, SocketChannel client) {
        try (SocketChannel c = client) {
            ByteBuffer lenBuf = ByteBuffer.allocate(LENGTH_FIELD_BYTES).order(ByteOrder.BIG_ENDIAN);
            while (!shutdownRequested.get()) {
                lenBuf.clear();
                if (!readFully(c, lenBuf)) {
                    if (lenBuf.position() == 0) {
                        // Clean EOF at a frame boundary — peer
                        // disconnected between frames. Nothing to
                        // log at WARNING; this is the happy-path
                        // connection-close.
                        LOG.log(Level.FINE,
                                () -> "connection #" + connectionId + " closed cleanly");
                    } else {
                        LOG.log(Level.WARNING,
                                "connection #" + connectionId
                                        + " hung up mid-length-prefix after "
                                        + lenBuf.position() + "/" + LENGTH_FIELD_BYTES
                                        + " bytes");
                    }
                    return;
                }
                lenBuf.flip();
                int announced = lenBuf.getInt();
                if (announced < 0 || announced > MAX_FRAME_BYTES) {
                    LOG.log(Level.WARNING,
                            () -> "connection #" + connectionId
                                    + " announced oversized/invalid frame ("
                                    + announced + " bytes); closing");
                    return;
                }
                ByteBuffer payload = ByteBuffer.allocate(announced);
                if (!readFully(c, payload)) {
                    LOG.log(Level.WARNING,
                            "connection #" + connectionId
                                    + " hung up mid-frame after "
                                    + payload.position() + "/" + announced + " bytes");
                    return;
                }
                payload.flip();
                byte[] payloadBytes = new byte[announced];
                payload.get(payloadBytes);
                Envelope envelope;
                try {
                    envelope = Envelope.parseFrom(payloadBytes);
                } catch (InvalidProtocolBufferException e) {
                    LOG.log(Level.WARNING,
                            "connection #" + connectionId
                                    + " sent malformed Envelope; closing", e);
                    return;
                }
                if (!dispatch(connectionId, c, envelope)) {
                    // Dispatch decided the connection is done (Shutdown
                    // routed through this connection sets the flag).
                    return;
                }
            }
        } catch (IOException e) {
            LOG.log(Level.WARNING,
                    "connection #" + connectionId + " IO error; closing", e);
        } catch (RuntimeException e) {
            // Guard the executor against a per-connection bug taking
            // out the worker thread.
            LOG.log(Level.SEVERE,
                    "connection #" + connectionId + " uncaught exception", e);
        }
    }

    /**
     * Route one envelope. Returns {@code true} if the connection
     * should keep reading, {@code false} if the handler decided to
     * close it (e.g. after acknowledging a {@link Shutdown}).
     */
    private boolean dispatch(long connectionId, SocketChannel client, Envelope envelope) throws IOException {
        Envelope.BodyCase body = envelope.getBodyCase();
        // Every envelope we route counts as activity. Reset the idle
        // window before doing any work so the timer cannot fire
        // between the read-loop pulling a frame off the wire and the
        // dispatch completing. recordActivity() is thread-safe and
        // cheap — a single CAS on AtomicReference + a schedule on the
        // timer's daemon executor.
        idleTimer.recordActivity();
        LOG.log(Level.FINE,
                () -> "connection #" + connectionId
                        + " dispatch " + body
                        + " (request_id=" + envelope.getRequestId() + ")"
                        + redactedCredentialNote(envelope));
        long requestId = envelope.getRequestId();
        return switch (body) {
            case PING -> {
                Envelope reply = pongReply(requestId);
                writeEnvelope(client, reply);
                yield true;
            }
            case ACTION -> {
                long dispatched = actionsDispatched.incrementAndGet();
                if (crashAfter > 0 && dispatched >= crashAfter) {
                    // Debug self-immolation fixture for M4.2 T6. Halt
                    // *before* writing the reply so the in-flight
                    // action observes the canonical "daemon kill -9
                    // mid-action" path: the connection's TCP/UDS
                    // read returns EOF / ConnectionReset, and the
                    // CLI-side mux maps that to BAR-DAEMON-CRASHED.
                    // We log at SEVERE so the test harness's stderr
                    // capture surfaces the trigger.
                    //
                    // `Runtime.halt(137)` (not `System.exit`) skips
                    // shutdown hooks: the production accept loop's
                    // socket-cleanup + worker-pool drain never runs,
                    // which is exactly the failure shape we want to
                    // exercise on the CLI side. 137 == 128 + SIGKILL
                    // so logs/exit-status pin to the canonical
                    // external-kill-9 value.
                    LOG.log(Level.SEVERE,
                            () -> "DEBUG-CRASH: action #" + dispatched
                                    + " triggered --crash-after; calling Runtime.halt("
                                    + CRASH_EXIT_CODE + ")");
                    Runtime.getRuntime().halt(CRASH_EXIT_CODE);
                }
                // M4.3 T2 — execute the action against the installed
                // dispatcher. The production path
                // ({@link #startWithEmbeddedMaven}) installs an
                // {@link EmbeddedMavenActionDispatcher} that drives the
                // resident Maven core; the legacy M4.2 conformance
                // path ({@link #startWith(SocketConfig, WorkerPool)})
                // installs {@link #NOT_YET_IMPLEMENTED_DISPATCHER}
                // which preserves the historical stub response shape
                // so byte-identical replays under that fixture stay
                // green. Any RuntimeException thrown by the dispatcher
                // is caught and surfaced as a FAILURE result so a
                // per-action bug cannot take out the connection
                // handler thread.
                ActionRequest req = envelope.getAction();
                ActionResult result;
                try {
                    result = actionDispatcher.dispatch(req);
                } catch (RuntimeException re) {
                    LOG.log(Level.WARNING,
                            "action " + req.getActionId() + " threw from dispatcher", re);
                    Error err = Error.newBuilder()
                            .setCode("BAR-DAEMON-DISPATCH-INTERNAL")
                            .setMessage("dispatcher threw "
                                    + re.getClass().getSimpleName() + ": "
                                    + (re.getMessage() != null ? re.getMessage() : "(no message)"))
                            .setActionId(req.getActionId())
                            .build();
                    result = ActionResult.newBuilder()
                            .setActionId(req.getActionId())
                            .setStatus(ActionResult.Status.FAILURE)
                            .setExitCode(1)
                            .setError(err)
                            .setFailureMessage(err.getMessage())
                            .build();
                }
                Envelope reply = Envelope.newBuilder()
                        .setVersion(PROTOCOL_VERSION)
                        .setRequestId(requestId)
                        .setResult(result)
                        .build();
                writeEnvelope(client, reply);
                yield true;
            }
            case SHUTDOWN -> {
                // Acknowledge with the same not-yet-implemented marker
                // (the structured drain handshake is owned by Task 5)
                // and exit the connection loop. The server-level
                // shutdown flag is flipped so the accept loop also
                // tears down.
                Envelope ack = Envelope.newBuilder()
                        .setVersion(PROTOCOL_VERSION)
                        .setRequestId(requestId)
                        .setError(Error.newBuilder()
                                .setCode(NOT_YET_IMPLEMENTED_CODE)
                                .setMessage("daemon shutdown handshake is wired in M4.2 T5; "
                                        + "the daemon is exiting now")
                                .build())
                        .build();
                writeEnvelope(client, ack);
                shutdown();
                yield false;
            }
            default -> {
                Envelope reply = Envelope.newBuilder()
                        .setVersion(PROTOCOL_VERSION)
                        .setRequestId(requestId)
                        .setError(Error.newBuilder()
                                .setCode(NOT_YET_IMPLEMENTED_CODE)
                                .setMessage("envelope variant " + body
                                        + " is not yet handled by the daemon")
                                .build())
                        .build();
                writeEnvelope(client, reply);
                yield true;
            }
        };
    }

    private static Envelope pongReply(long requestId) {
        Runtime.Version v = Runtime.version();
        long nowMicros = System.currentTimeMillis() * 1_000L;
        return Envelope.newBuilder()
                .setVersion(PROTOCOL_VERSION)
                .setRequestId(requestId)
                .setPong(Pong.newBuilder()
                        .setDaemon("barback 0.1.0-alpha.0")
                        .setJdkId(System.getProperty("java.vendor", "unknown")
                                .toLowerCase(Locale.ROOT)
                                .replace(' ', '-')
                                + "-" + v.feature())
                        .setJdkVersion(v.toString())
                        .setServerUnixMicros(nowMicros)
                        // client_unix_micros is the echo of the Ping's
                        // sent_at_unix_micros; we don't have that on
                        // this code path because we only carry the
                        // request_id. T1 leaves the echo at 0 and the
                        // CLI tolerates that; M4.2 T5 will plumb the
                        // ping payload through.
                        .setClientUnixMicros(0L)
                        .build())
                .build();
    }

    /**
     * If the envelope carries an {@link ActionRequest} with a
     * credentials envelope, render it through
     * {@link RedactedCredential} for log inclusion. Returns the empty
     * string for envelopes that do not carry credentials so log lines
     * stay clean.
     */
    private static String redactedCredentialNote(Envelope envelope) {
        if (envelope.getBodyCase() != Envelope.BodyCase.ACTION) {
            return "";
        }
        ActionRequest req = envelope.getAction();
        if (!req.hasCredentials() || req.getCredentials().getEntriesCount() == 0) {
            return "";
        }
        StringBuilder sb = new StringBuilder(" credentials=[");
        boolean first = true;
        for (int i = 0; i < req.getCredentials().getEntriesCount(); i++) {
            if (!first) {
                sb.append(", ");
            }
            sb.append(RedactedCredential.redactedToString(
                    req.getCredentials().getEntries(i)));
            first = false;
        }
        return sb.append(']').toString();
    }

    private static void writeEnvelope(SocketChannel client, Envelope envelope) throws IOException {
        byte[] payload = envelope.toByteArray();
        ByteBuffer lenBuf = ByteBuffer.allocate(LENGTH_FIELD_BYTES).order(ByteOrder.BIG_ENDIAN);
        lenBuf.putInt(payload.length);
        lenBuf.flip();
        writeFully(client, lenBuf);
        writeFully(client, ByteBuffer.wrap(payload));
    }

    /**
     * Read until {@code buf} is full or EOF.
     *
     * @return {@code true} if the buffer was filled completely;
     *     {@code false} on EOF. The caller distinguishes "clean close
     *     at a frame boundary" from "EOF mid-frame" by checking
     *     {@link ByteBuffer#position()} on a {@code false} return:
     *     position zero ⇒ clean close, position non-zero ⇒ peer hung
     *     up mid-frame.
     */
    private static boolean readFully(SocketChannel ch, ByteBuffer buf) throws IOException {
        while (buf.hasRemaining()) {
            int n = ch.read(buf);
            if (n < 0) {
                return false;
            }
        }
        return true;
    }

    private static void writeFully(SocketChannel ch, ByteBuffer buf) throws IOException {
        while (buf.hasRemaining()) {
            int n = ch.write(buf);
            if (n < 0) {
                throw new IOException("write returned -1 on a connected SocketChannel");
            }
        }
    }

    private static void closeQuietly(SocketChannel ch) {
        try {
            ch.close();
        } catch (IOException ignored) {
            // No actionable recovery — the caller has already logged
            // the reason the channel is being closed.
        }
    }
}
