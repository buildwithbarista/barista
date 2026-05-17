/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;
import com.bluminal.barista.barback.proto.Error;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;
import java.util.logging.Level;
import java.util.logging.Logger;

import org.apache.maven.api.cli.Invoker;
import org.apache.maven.api.cli.InvokerException;
import org.apache.maven.api.cli.InvokerRequest;
import org.apache.maven.api.cli.Parser;
import org.apache.maven.api.cli.ParserRequest;
import org.apache.maven.api.services.MessageBuilderFactory;
import org.apache.maven.cling.invoker.ProtoLookup;
import org.apache.maven.cling.invoker.mvn.MavenParser;
import org.apache.maven.cling.invoker.mvn.resident.ResidentMavenInvoker;
import org.apache.maven.jline.JLineMessageBuilderFactory;
import org.codehaus.plexus.classworlds.ClassWorld;

/**
 * Embedded Maven&nbsp;4 core. Executes {@link ActionRequest} payloads
 * in-process against a long-lived classloader container instead of
 * forking {@code bin/mvn} per action.
 *
 * <h2>Embed-with-care contract</h2>
 *
 * <p>This class binds to two Maven&nbsp;4 surfaces that the project
 * itself does not yet stabilise:
 *
 * <ul>
 *   <li><b>{@code org.apache.maven.cling.invoker.mvn.resident.ResidentMavenInvoker}</b>
 *       &mdash; ships no stability annotation. Maven exercises it on
 *       every {@code mvn} invocation so it is implicitly load-bearing
 *       for the project, but the contract is internal-by-convention.
 *       The {@code embedded.maven.version} pin in {@code barback/pom.xml}
 *       compensates by gating every bump on a re-run of the integration
 *       suite (and the corpus baseline, once it lands) under the new
 *       version.</li>
 *   <li><b>{@code org.apache.maven.api.cli.Invoker} /
 *       {@code Parser} / {@code InvokerRequest} /
 *       {@code ParserRequest}</b> &mdash; every member is
 *       {@code @Experimental}, Maven's own annotation explicitly
 *       signalling minor-version compatibility is not guaranteed.</li>
 * </ul>
 *
 * <p>The embed-with-care discipline that follows from those facts:
 * the daemon only ever touches these surfaces through the methods on
 * this class. Any future refactor that broadens the surface area
 * (e.g. reaching into {@code LookupContext} or
 * {@code PlexusContainerCapsule}) inflates the API churn surface and
 * must be reviewed against the version-pin policy.
 *
 * <h2>Cold path vs warm path</h2>
 *
 * <p>The first call to {@link #execute(ActionRequest)} pays the full
 * Maven&nbsp;4 bootstrap cost: building the Plexus container, wiring
 * the resolver, instantiating the model builder, discovering plugins.
 * On the spike harness this is &asymp;1.0&nbsp;s. Subsequent calls in
 * the same JVM go through the same {@link ResidentMavenInvoker}
 * instance whose internal {@code residentContext} cache returns the
 * already-built container in &asymp;160&nbsp;ms (a &gt;6&times;
 * speed-up; see the M4.0 spike measurements).
 *
 * <p><b>Important:</b> closing the resident invoker tears down every
 * cached {@code MavenContext}, which destroys the warm-path
 * advantage. Callers must <em>not</em> dispose this instance between
 * actions; only at daemon shutdown.
 *
 * <h2>Plugin classloader cache hook</h2>
 *
 * <p>The plugin classloader cache (a separate task tracked under the
 * core/ module's M4.2 T4 placeholder) will extend
 * {@link ResidentMavenInvoker} rather than reimplement the cache
 * keying logic. This class deliberately exposes {@link #invoker()} as
 * a package-private hook so the cache implementation can subclass or
 * decorate the held invoker without {@code EmbeddedMaven} growing a
 * second responsibility.
 *
 * <h2>Concurrency</h2>
 *
 * <p>{@link ResidentMavenInvoker} is documented (via its
 * {@code ConcurrentHashMap}-backed cache) to be safe across threads
 * for distinct request signatures, but two simultaneous invocations
 * sharing a single {@code MavenContext} would race on container
 * state. To preserve the safe-across-threads invariant the daemon
 * worker pool will hold a per-instance fairness {@link ReentrantLock}
 * around {@link #execute(ActionRequest)}. Action throughput is
 * latency-dominated, not concurrency-dominated, so the
 * single-mutator-at-a-time policy is acceptable for v0.1; the
 * worker-pool concurrency contract (the M4.2 T2 task surface) can
 * relax the lock if profiling later shows the wait time matters.
 */
public final class EmbeddedMaven implements AutoCloseable {

    private static final Logger LOG = Logger.getLogger(EmbeddedMaven.class.getName());

    /**
     * Canonical machine-readable error code returned in
     * {@link ActionResult#getError()} when the daemon-side wiring (not
     * the user's build) is responsible for a failure. Mirrors the
     * format used by {@code Server.NOT_YET_IMPLEMENTED_CODE}.
     */
    public static final String CORE_ERROR_CODE = "BAR-DAEMON-CORE-FAILURE";

    private final ClassWorld classWorld;
    private final Path mavenHome;
    private final ResidentMavenInvoker invoker;
    private final Parser parser;
    private final MessageBuilderFactory messageBuilderFactory;

    /**
     * Serialises {@link #execute(ActionRequest)} calls. See the
     * "Concurrency" javadoc on this class for the rationale.
     */
    private final ReentrantLock executionLock = new ReentrantLock(true);

    /** Tracks the cold-vs-warm distinction in diagnostics. */
    private final AtomicBoolean firstCallSeen = new AtomicBoolean(false);

    /** Monotonically increasing per-action sequence for log correlation. */
    private final AtomicLong sequence = new AtomicLong(0);

    /**
     * Package-private; instances are constructed via
     * {@link EmbeddedMavenFactory}, which owns the heavy
     * {@link ClassWorld} bring-up.
     */
    EmbeddedMaven(ClassWorld classWorld, Path mavenHome) {
        this.classWorld = Objects.requireNonNull(classWorld, "classWorld");
        this.mavenHome = Objects.requireNonNull(mavenHome, "mavenHome");
        this.messageBuilderFactory = new JLineMessageBuilderFactory();
        // ProtoLookup is the bootstrap-lookup Maven uses to publish a
        // small set of pre-Plexus objects (ClassWorld is the canonical
        // example) into the eventual session lookup. Reproducing the
        // pattern is required: ResidentMavenInvoker's createContext
        // pulls ClassWorld out of the lookup to construct the realm
        // for the request.
        ProtoLookup lookup = ProtoLookup.builder()
                .addMapping(ClassWorld.class, classWorld)
                .build();
        this.invoker = new ResidentMavenInvoker(lookup);
        this.parser = new MavenParser();
    }

    /**
     * Execute one action against the embedded core and return the
     * terminal {@link ActionResult}.
     *
     * <p>The current implementation supports the
     * {@code maven-compat:4} subset the corpus baseline targets:
     * passing {@code mojo_coords} plus the {@code pom_path} through to
     * Maven via the standard CLI syntax. Full mojo configuration
     * (system properties, environment, classpath overrides, the CBOR
     * effective POM blob from {@link ActionRequest#getEffectivePomBlob()})
     * is wired up in subsequent action-execution tasks. The
     * single-mojo CLI-equivalent path is what the M4.0 spike validated
     * and what the M4.2 leak / warm-path tests exercise.
     *
     * @param action the action to run; never {@code null}
     * @return the terminal action result; {@code status == SUCCESS} on
     *     exit code 0, otherwise {@code FAILURE} with a populated
     *     {@code failure_message} carrying the captured stderr (so the
     *     CLI can render it verbatim).
     */
    public ActionResult execute(ActionRequest action) {
        Objects.requireNonNull(action, "action");
        long seq = sequence.incrementAndGet();
        boolean cold = firstCallSeen.compareAndSet(false, true);
        long startNanos = System.nanoTime();

        executionLock.lock();
        try {
            return doExecute(action, seq, cold, startNanos);
        } finally {
            executionLock.unlock();
        }
    }

    private ActionResult doExecute(ActionRequest action, long seq, boolean cold, long startNanos) {
        String actionId = action.getActionId();
        List<String> args = buildMavenArgs(action);
        Path cwd = resolveCwd(action);

        // Capture stdout/stderr at the InvokerRequest level instead of
        // redirecting java.lang.System streams. Two reasons:
        //   1. The daemon serves multiple connections; replacing
        //      System.out would cross-contaminate concurrent actions.
        //      Even with the executionLock above, a future relaxation
        //      to a per-context concurrency model would silently
        //      reintroduce the cross-talk if we relied on System.out.
        //   2. The Maven cling wiring already accepts std{In,Out,Err}
        //      on ParserRequest, which is the correct injection seam.
        ByteArrayOutputStream stdoutSink = new ByteArrayOutputStream();
        ByteArrayOutputStream stderrSink = new ByteArrayOutputStream();
        InputStream stdin = InputStream.nullInputStream();

        ParserRequest parserRequest = ParserRequest
                .mvn(args, messageBuilderFactory)
                .cwd(cwd)
                .mavenHome(mavenHome)
                .stdIn(stdin)
                .stdOut(stdoutSink)
                .stdErr(stderrSink)
                .embedded(true)
                .build();

        int exit;
        Throwable failure = null;
        try {
            InvokerRequest invokerRequest = parser.parseInvocation(parserRequest);
            exit = invoker.invoke(invokerRequest);
        } catch (InvokerException.ExitException e) {
            // Maven raises ExitException when the parser short-circuits
            // (e.g. --version): the exit code lives on the exception.
            // Treat it as a normal result; the upstream `cling`
            // dispatcher does the same.
            exit = e.getExitCode();
        } catch (InvokerException e) {
            failure = e;
            exit = 1;
        } catch (RuntimeException e) {
            // Catching RuntimeException is a deliberate choice: the
            // embedded core surfaces NullPointerException etc. on
            // malformed inputs (pre-execution), and the daemon must
            // not let a per-action bug take out the worker thread.
            failure = e;
            exit = 1;
        }

        long durationMicros = (System.nanoTime() - startNanos) / 1_000L;
        LOG.log(Level.FINE,
                () -> "action #" + seq + " (cold=" + cold + ") exit=" + 0
                        + " duration_ms=" + (durationMicros / 1_000L));

        // TODO(plugin classloader cache): when the cache hooks in via a
        // ResidentMavenInvoker subclass (see #invoker() and the class-
        // level "Plugin classloader cache hook" javadoc), it should be
        // able to observe cache-hit / cache-miss for this action by
        // inspecting the held invoker's residentContext map size before
        // and after the call. No additional seam needed here.

        ActionResult.Builder result = ActionResult.newBuilder()
                .setActionId(actionId)
                .setExitCode(exit)
                .setDurationMicros(durationMicros);

        String stderrText = stderrSink.toString(StandardCharsets.UTF_8);

        String stdoutText = stdoutSink.toString(StandardCharsets.UTF_8);
        if (exit == 0 && failure == null) {
            result.setStatus(ActionResult.Status.SUCCESS);
        } else {
            result.setStatus(ActionResult.Status.FAILURE);
            String message = failure != null
                    ? failure.getClass().getSimpleName() + ": "
                            + (failure.getMessage() != null ? failure.getMessage() : "(no message)")
                    : "embedded Maven exited with status " + exit;
            if (!stderrText.isBlank()) {
                message = message + System.lineSeparator() + stderrText.stripTrailing();
            }
            if (!stdoutText.isBlank()) {
                // Maven 4 routes parser/early-bootstrap errors to
                // stdout in `-q` mode; include the trailing chunk so
                // the diagnostic is recoverable from the result.
                message = message + System.lineSeparator() + stdoutText.stripTrailing();
            }
            result.setFailureMessage(message);
            Error.Builder err = Error.newBuilder()
                    .setCode(CORE_ERROR_CODE)
                    .setMessage(message)
                    .setActionId(actionId);
            if (failure != null) {
                err.putDetails("exception", failure.getClass().getName());
                result.setFailureStack(stackTrace(failure));
            }
            result.setError(err.build());
        }

        return result.build();
    }

    /**
     * Maven CLI argument vector for one action. v0.1 supports
     * {@code mojo_coords} (passed as the trailing positional arg, the
     * same shape {@code mvn} accepts), {@code -f <pom-path>}, and the
     * {@code -q}/quiet flag. Additional surface (system properties,
     * profiles, offline mode, the CBOR effective-POM blob) is grafted
     * onto this method in later tasks without disturbing the cling
     * entrypoint signatures.
     */
    private static List<String> buildMavenArgs(ActionRequest action) {
        List<String> args = new ArrayList<>(8);
        if (!action.getPomPath().isEmpty()) {
            args.add("-f");
            args.add(action.getPomPath());
        }
        if (action.getQuiet()) {
            args.add("-q");
        }
        // System properties from the action context. Daemon-level
        // policy filtering happens in the dispatcher; this method
        // forwards what the dispatcher chose to allow.
        if (!action.getSystemPropertiesMap().isEmpty()) {
            for (Map.Entry<String, String> entry : action.getSystemPropertiesMap().entrySet()) {
                args.add("-D" + entry.getKey() + "=" + entry.getValue());
            }
        }
        // mojo_coords is the trailing arg, matching `mvn compile` /
        // `mvn org.apache.maven.plugins:maven-compiler-plugin:compile`.
        String coords = action.getMojoCoords();
        if (!coords.isEmpty()) {
            args.add(coords);
        }
        return args;
    }

    private static Path resolveCwd(ActionRequest action) {
        String wd = action.getWorkingDirectory();
        if (!wd.isEmpty()) {
            return Path.of(wd).toAbsolutePath().normalize();
        }
        // Fall back to the directory containing pom.xml, then to the
        // process cwd. Mirrors how the `mvn` script behaves when -f is
        // passed without an explicit chdir.
        String pom = action.getPomPath();
        if (!pom.isEmpty()) {
            Path pomPath = Path.of(pom).toAbsolutePath().normalize();
            Path parent = pomPath.getParent();
            if (parent != null && Files.isDirectory(parent)) {
                return parent;
            }
        }
        return Path.of(".").toAbsolutePath().normalize();
    }

    private static String stackTrace(Throwable t) {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        try (PrintStream ps = new PrintStream(buf, true, StandardCharsets.UTF_8)) {
            t.printStackTrace(ps);
        }
        return buf.toString(StandardCharsets.UTF_8);
    }

    /**
     * Resolved Maven distribution directory backing this instance.
     * Surfaced for diagnostics; do not use as a stable contract.
     */
    public Path mavenHome() {
        return mavenHome;
    }

    /**
     * The class-world the embedded Maven core boots against. Exposed
     * package-private so a future plugin classloader cache can wire
     * itself into the same realm hierarchy.
     */
    ClassWorld classWorld() {
        return classWorld;
    }

    /**
     * The held {@link ResidentMavenInvoker}. Exposed package-private so
     * a plugin classloader cache implementation (the M4.2 follow-up)
     * can subclass / decorate the invoker without {@link EmbeddedMaven}
     * sprouting a second responsibility. Do not call from production
     * code outside the {@code com.bluminal.barista.barback.core} or
     * {@code com.bluminal.barista.barback.classloader} packages.
     */
    ResidentMavenInvoker invoker() {
        return invoker;
    }

    /**
     * Number of actions executed since this instance was created.
     * Used by leak-test instrumentation and surfaced through
     * {@code StatusResponse.actions_executed}.
     */
    public long invocationCount() {
        return sequence.get();
    }

    /**
     * Whether the next {@link #execute(ActionRequest)} call will be
     * the cold-path call (i.e. nothing has invoked the embedded core
     * yet on this instance). Primarily a test instrument.
     */
    public boolean isColdStartPending() {
        return !firstCallSeen.get();
    }

    /**
     * Tear down the embedded core. Closes the held resident invoker
     * (which evicts every cached {@code MavenContext}) and disposes
     * the class-world. After {@code close()} this instance is unusable.
     */
    @Override
    public void close() throws IOException {
        try {
            invoker.close();
        } catch (InvokerException e) {
            LOG.log(Level.WARNING, "ignored failure closing resident Maven invoker", e);
        }
        try {
            classWorld.close();
        } catch (RuntimeException e) {
            LOG.log(Level.FINE, () -> "ignored failure closing class-world: " + e);
        }
    }

    @Override
    public String toString() {
        return "EmbeddedMaven{mavenHome=" + mavenHome
                + ", invocations=" + sequence.get()
                + ", cold=" + isColdStartPending() + "}";
    }
}
