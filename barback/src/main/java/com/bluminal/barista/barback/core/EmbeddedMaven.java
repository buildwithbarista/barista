// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import com.bluminal.barista.barback.classloader.BaristaPluginRealmCache;
import com.bluminal.barista.barback.classloader.PluginCache;
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
import java.util.concurrent.atomic.AtomicInteger;
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
 * <h2>Plugin classloader cache</h2>
 *
 * <p>{@link com.bluminal.barista.barback.classloader.PluginCache}
 * caches realised plugin classloaders by
 * {@code (GAV, jar-sha256)} so subsequent actions skip the JAR-scan +
 * {@code defineClass} dance for plugins they have already loaded. The
 * cache lives strictly under this instance's lifetime: it is cleared
 * on every {@link ResidentMavenInvoker} rebuild (so cached entries
 * never reference a realm hierarchy the eviction policy has dropped)
 * and on {@link #close()}.
 *
 * <p>The cache also carries an <em>override list</em> &mdash; the OPEN-8
 * escape hatch from PRD &sect;11.6 &mdash; for plugins that misbehave
 * under classloader caching. Override-listed plugins bypass the cache
 * and are loaded fresh on every action. The factory entrypoint
 * {@code EmbeddedMavenFactory.with(mavenHome, overrideList)} accepts
 * an explicit set; the bootstrap path reads it from the
 * {@code barista.daemon.classloader_cache.override} JVM property.
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
 *
 * <h2>Periodic invoker eviction (Maven&nbsp;4 rc-3 mitigation)</h2>
 *
 * <p>{@link ResidentMavenInvoker} in Maven&nbsp;4.0.0-rc-3 keys its
 * internal session cache on the literal string {@code "resident"} and
 * its {@code copyIfDifferent} path allocates a fresh
 * {@code MavenContext} per call. The two together produce
 * &asymp;0.57&nbsp;MiB of unreclaimable old-gen growth per
 * {@code invoke()} that the resident-cache contract does not bound.
 * Measured baseline against this codebase's 1-module sample-project:
 * 100 sequential compiles grew the old-gen by &asymp;57&nbsp;MiB.
 *
 * <p>This is an upstream bug. An issue against {@code apache/maven}
 * is drafted but the public tracking link is still
 * {@code TBD-MAVEN-RESIDENT-INVOKER-LEAK} pending submission and
 * triage. Until a fix lands and
 * {@code embedded.maven.version} is bumped past it, the daemon caps
 * the lifetime of a single {@link ResidentMavenInvoker} at
 * {@link #MAX_ACTIONS_PER_INVOKER} calls and rebuilds the invoker on
 * the boundary. Math:
 *
 * <ul>
 *   <li>baseline growth rate: &asymp;0.57&nbsp;MiB / action (measured
 *       against the M4.0 spike's 1-module sample-project on Maven
 *       4.0.0-rc-3; full-suite runs see a slightly higher rate because
 *       prior tests warm up additional caches);</li>
 *   <li>{@code N = 12} actions per invoker &rArr; worst-case peak
 *       inside a cycle is {@code 11 * 0.57 = ~6.3 MiB} above the
 *       just-evicted baseline (sampled at the end of the cycle, before
 *       the eviction releases the references);</li>
 *   <li>6.3&nbsp;MiB sits comfortably under the M4.2 acceptance
 *       criterion's "&plusmn;10&nbsp;MiB" envelope, with margin left
 *       over for heap-sampling jitter and any small growth rate
 *       increases as the daemon evolves (the leak IT samples every
 *       10 actions, so the recorded peak inside a cycle may be lower
 *       than the analytical worst case, never higher).</li>
 * </ul>
 *
 * <p>The {@link ClassWorld} is retained across evictions &mdash; only
 * the held invoker is closed and rebuilt. The held invoker's
 * {@code close()} releases the cached {@code MavenContext}s and lets
 * the next major GC reclaim the accumulated descriptor state. Cold
 * cost of one eviction-boundary call is on the order of the original
 * cold-start (&asymp;1&nbsp;s) because a fresh
 * {@code ResidentMavenInvoker} has an empty session cache; non-boundary
 * calls still hit the warm path (&asymp;120&nbsp;ms), preserving the
 * 9.4&times; cold/warm ratio that
 * {@code ResidentInvokerWarmPathTest} guards.
 *
 * <p><b>Removal condition.</b> Delete this policy &mdash; revert to a
 * single, never-evicted invoker &mdash; once the upstream session-cache
 * shape is corrected and a Maven&nbsp;4 release containing that fix is
 * pinned via {@code embedded.maven.version}. Re-run the leak IT under
 * the new pin; the assertion should pass without an eviction policy
 * if the upstream fix is sufficient.
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

    /**
     * Default number of actions one {@link ResidentMavenInvoker}
     * services before it is closed and rebuilt. See the
     * "Periodic invoker eviction" javadoc on this class for the math
     * behind the choice. Package-private and mutable so the eviction
     * unit test can drive the boundary at a low value without
     * standing up 16+ Maven invocations per assertion.
     */
    static volatile int MAX_ACTIONS_PER_INVOKER = 12;

    private final ClassWorld classWorld;
    private final Path mavenHome;
    private final ProtoLookup protoLookup;
    private final Parser parser;
    private final MessageBuilderFactory messageBuilderFactory;

    /**
     * Plugin classloader cache. Lives strictly under this instance's
     * lifetime; cleared on every invoker rebuild (so cached entries
     * never outlive the realm hierarchy they point into) and on
     * {@link #close()}. See the "Plugin classloader cache" javadoc
     * section on this class for the integration contract.
     */
    private final PluginCache pluginCache;

    /**
     * Held resident invoker. Mutable (replaced on eviction) so the
     * daemon can recycle the cached session state every
     * {@link #MAX_ACTIONS_PER_INVOKER} actions without tearing down
     * the surrounding {@link ClassWorld}. Guarded by
     * {@link #executionLock}; never observed outside the lock.
     */
    private ResidentMavenInvoker invoker;

    /**
     * Number of actions the currently-held {@link #invoker} has
     * serviced. Reset to {@code 0} each time the invoker is rebuilt.
     * Guarded by {@link #executionLock}; the {@link AtomicInteger}
     * type is for cheap volatile reads from
     * {@link #invocationCountInCurrentCycle()} (a test instrument).
     */
    private final AtomicInteger actionsInCurrentCycle = new AtomicInteger(0);

    /**
     * Total number of times the held invoker has been rebuilt. Bumps
     * on each eviction (not on the initial construction). Surfaced to
     * tests via {@link #invokerRebuildCount()}.
     */
    private final AtomicInteger invokerRebuildCount = new AtomicInteger(0);

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
     *
     * @param pluginCache the plugin classloader cache scoped to this
     *     instance; never {@code null}. The factory builds it from the
     *     daemon's override-list configuration.
     */
    EmbeddedMaven(ClassWorld classWorld, Path mavenHome, PluginCache pluginCache) {
        this.classWorld = Objects.requireNonNull(classWorld, "classWorld");
        this.mavenHome = Objects.requireNonNull(mavenHome, "mavenHome");
        this.pluginCache = Objects.requireNonNull(pluginCache, "pluginCache");
        this.messageBuilderFactory = new JLineMessageBuilderFactory();
        // ProtoLookup is the bootstrap-lookup Maven uses to publish a
        // small set of pre-Plexus objects (ClassWorld is the canonical
        // example) into the eventual session lookup. Reproducing the
        // pattern is required: ResidentMavenInvoker's createContext
        // pulls ClassWorld out of the lookup to construct the realm
        // for the request. We retain the lookup as a field so the
        // eviction path can rebuild the invoker without rebuilding
        // the ClassWorld.
        this.protoLookup = ProtoLookup.builder()
                .addMapping(ClassWorld.class, classWorld)
                .build();
        this.invoker = new ResidentMavenInvoker(this.protoLookup);
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
            // Pre-execute eviction: if the held invoker has already
            // serviced MAX_ACTIONS_PER_INVOKER actions, drop it and
            // build a fresh one before driving this action through.
            // We pre-evict (rather than post-evict at the boundary
            // call) so the boundary call itself runs against a clean
            // invoker — every executed action sees a well-defined
            // resident cache, and the cold-cost of the rebuild is
            // bundled into this action's recorded duration rather
            // than appearing as a phantom latency on the *previous*
            // call's exit path. The first call after construction
            // skips eviction because actionsInCurrentCycle starts at
            // zero; the first eviction fires on call N+1.
            maybeEvictInvoker();
            ResidentMavenInvoker current = this.invoker;
            ActionResult result = doExecute(action, current, seq, cold, startNanos);
            actionsInCurrentCycle.incrementAndGet();
            return result;
        } finally {
            executionLock.unlock();
        }
    }

    /**
     * If the held invoker has already serviced
     * {@link #MAX_ACTIONS_PER_INVOKER} actions, close it and replace
     * it with a freshly built {@link ResidentMavenInvoker} backed by
     * the same {@link ProtoLookup}. Called from inside
     * {@link #executionLock} so no concurrent {@link #execute(ActionRequest)}
     * can observe a half-rebuilt invoker.
     *
     * <p>The held {@link ClassWorld} is retained &mdash; it is the
     * expensive piece of the bootstrap (&asymp;600&nbsp;ms of disk +
     * classloader work) and its content does not grow under load. Only
     * the invoker, whose session cache is the source of the rc-3
     * growth, gets rebuilt. The rebuild itself takes &asymp;1&nbsp;ms;
     * the cold-start cost an eviction-boundary call pays comes from
     * the new invoker having an empty {@code MavenContext} cache,
     * which is the standard cold-path penalty we already accept on
     * the first call.
     */
    private void maybeEvictInvoker() {
        if (actionsInCurrentCycle.get() < MAX_ACTIONS_PER_INVOKER) {
            return;
        }
        ResidentMavenInvoker stale = this.invoker;
        try {
            stale.close();
        } catch (InvokerException e) {
            // Closing the resident invoker is a best-effort tear-down:
            // it releases the cached MavenContext entries so the next
            // GC can reclaim them, but a failure to close cleanly
            // does not block the daemon from continuing. The new
            // invoker we install below will not share state with the
            // old one regardless of whether close() completed.
            LOG.log(Level.WARNING,
                    "ignored failure closing resident Maven invoker during periodic eviction", e);
        }
        // M4.3 T3: previously we called pluginCache.invalidateAll()
        // here so cached URLClassLoaders did not outlive the invoker
        // they were built under. With the Sisu-wired
        // BaristaPluginRealmCache landing the host cache no longer
        // mirrors plugin realms (the realm-cache contract is owned by
        // Maven via PluginRealmCache, and that surface deliberately
        // survives the invoker rebuild because the parent realm
        // chain — plexus.core under the retained ClassWorld — is
        // stable). The local PluginCache entries that remain are
        // diagnostic / test-only loaders whose lifetime is tied to
        // the host instance, not to the resident invoker. Holding
        // them across the rebuild preserves the SM-3.2 warm-shot
        // budget; clearing here would invalidate work the next
        // action would have to redo on a hot path.
        //
        // The eviction-interaction test (PluginCacheEvictionInteractionTest)
        // pins this contract explicitly: PluginCache entries SURVIVE
        // invoker rebuild and are only invalidated at
        // EmbeddedMaven#close() (i.e. when the ClassWorld itself is
        // going away).
        //
        // The BaristaPluginRealmCache (Maven's realm-cache hook) is
        // per-Plexus-container (Sisu @Singleton); its entries drop
        // automatically when the container disposes below. No
        // explicit clearing call is needed here — the Sisu lifecycle
        // owns it.
        this.invoker = new ResidentMavenInvoker(protoLookup);
        actionsInCurrentCycle.set(0);
        int rebuilds = invokerRebuildCount.incrementAndGet();
        LOG.log(Level.FINE,
                () -> "evicted resident Maven invoker after "
                        + MAX_ACTIONS_PER_INVOKER + " actions (rebuild #" + rebuilds + ")");
    }

    private ActionResult doExecute(ActionRequest action,
                                   ResidentMavenInvoker invoker,
                                   long seq,
                                   boolean cold,
                                   long startNanos) {
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

        // Plugin classloader cache hook site (M4.2 T4): the held
        // PluginCache is consulted by callers that know the plugin
        // coord at dispatch time. In the v0.1 EmbeddedMaven path the
        // Maven core itself owns plugin resolution (it reads
        // plugin.xml off the realized realm); the cache is exercised
        // by the integration tests directly and will be wired into
        // the action-dispatch path proper in M4.3 once the dispatcher
        // learns to surface PluginKey for each Mojo. The hit/miss
        // metrics are surfaced via pluginCache().hitCount() etc. for
        // the status RPC.

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
     *
     * <p><b>Reproducibility env propagation (M4.3 T6).</b> Both
     * {@link ActionRequest#getSystemPropertiesMap() system_properties}
     * and {@link ActionRequest#getEnvironmentMap() environment} are
     * translated to {@code -D} flags here. The two maps serve distinct
     * purposes:
     * <ul>
     *   <li><b>system_properties</b> → {@code -D<key>=<value>}
     *       verbatim. Carries the reproducible-builds
     *       {@code project.build.outputTimestamp} property the CLI
     *       injects under {@code --ci} so {@code maven-archiver} stamps
     *       deterministic timestamps into JARs.</li>
     *   <li><b>environment</b> → {@code -Denv.<key>=<value>}. Maven
     *       references environment variables in POM expressions as
     *       {@code ${env.X}}; the cling parser already wires
     *       {@code System.getenv()} into that namespace, but the
     *       daemon's JVM environment is not the CLI's — we can't
     *       mutate JVM env post-startup. Translating to
     *       {@code -Denv.X=...} restores the convention so a plugin
     *       reading {@code ${env.SOURCE_DATE_EPOCH}} in a pom
     *       expression sees the value the CLI intended.</li>
     * </ul>
     * Iteration order for both maps is sorted by key so the resulting
     * argv is byte-stable across daemon restarts (some plugins consult
     * the full system-property table at startup and any incidental
     * ordering would otherwise propagate into their output).
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
        // forwards what the dispatcher chose to allow. Sorted by key
        // for argv byte-stability — see method-level docs.
        if (!action.getSystemPropertiesMap().isEmpty()) {
            java.util.List<Map.Entry<String, String>> sysEntries =
                    new ArrayList<>(action.getSystemPropertiesMap().entrySet());
            sysEntries.sort(Map.Entry.comparingByKey());
            for (Map.Entry<String, String> entry : sysEntries) {
                args.add("-D" + entry.getKey() + "=" + entry.getValue());
            }
        }
        // Environment-variable propagation (M4.3 T6). Maps each
        // wire-level env key to `-Denv.<key>=<value>` so Maven's
        // `${env.X}` POM expressions resolve consistently across the
        // daemon and `--no-daemon` paths.
        if (!action.getEnvironmentMap().isEmpty()) {
            java.util.List<Map.Entry<String, String>> envEntries =
                    new ArrayList<>(action.getEnvironmentMap().entrySet());
            envEntries.sort(Map.Entry.comparingByKey());
            for (Map.Entry<String, String> entry : envEntries) {
                args.add("-Denv." + entry.getKey() + "=" + entry.getValue());
            }
        }
        // Extra Maven CLI args forwarded by the daemon-side dispatcher.
        // Used (today) for `-s <ephemeral-settings.xml>` when a deploy
        // action carried credentials — the dispatcher writes the
        // settings file and threads the flag through here so the
        // embedded core sees it identically to a CLI `mvn -s ...` run.
        // Position: after -D flags, before the trailing mojo coord, so
        // a settings-XML pointer takes effect before goal resolution.
        for (int i = 0; i < action.getExtraMvnArgsCount(); i++) {
            args.add(action.getExtraMvnArgs(i));
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
     * The currently held {@link ResidentMavenInvoker}. Exposed
     * package-private so a plugin classloader cache implementation
     * (the M4.2 follow-up) can subclass / decorate the invoker without
     * {@link EmbeddedMaven} sprouting a second responsibility, and so
     * the eviction unit test can observe invoker identity across
     * boundary calls. Do not call from production code outside the
     * {@code com.bluminal.barista.barback.core} or
     * {@code com.bluminal.barista.barback.classloader} packages.
     *
     * <p>Because the eviction policy replaces this reference every
     * {@link #MAX_ACTIONS_PER_INVOKER} actions, callers must not hang
     * onto the returned value across a call to
     * {@link #execute(ActionRequest)}.
     */
    ResidentMavenInvoker invoker() {
        executionLock.lock();
        try {
            return invoker;
        } finally {
            executionLock.unlock();
        }
    }

    /**
     * The plugin classloader cache scoped to this instance. Cleared on
     * every invoker rebuild and on {@link #close()} so cached entries
     * never reference a realm hierarchy that the eviction policy has
     * dropped. Surfaced for the dispatcher (which materialises the
     * cache key per mojo lookup) and for diagnostics
     * ({@link PluginCache#hitCount()} feeds the daemon's status RPC).
     */
    public PluginCache pluginCache() {
        return pluginCache;
    }

    /**
     * Number of times the held {@link ResidentMavenInvoker} has been
     * closed and rebuilt by the eviction policy. Zero immediately
     * after construction; bumps on the call that crosses the
     * {@link #MAX_ACTIONS_PER_INVOKER} threshold. Test instrument.
     */
    int invokerRebuildCount() {
        return invokerRebuildCount.get();
    }

    /**
     * Number of actions the current invoker has serviced since the
     * most recent rebuild (or since construction, for the first
     * cycle). Test instrument.
     */
    int invocationCountInCurrentCycle() {
        return actionsInCurrentCycle.get();
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
        executionLock.lock();
        try {
            // Close the plugin classloader cache first: its entries
            // hold URLClassLoader references whose parent realm chains
            // go up into the invoker we are about to dispose. Closing
            // out-of-order would leak the loaders' file handles for
            // the duration of the JVM, which we observed in the
            // EmbeddedMavenLeakIT shake-down runs.
            try {
                pluginCache.close();
            } catch (RuntimeException e) {
                LOG.log(Level.FINE, () -> "ignored failure closing plugin cache: " + e);
            }
            // Drop the BaristaPluginRealmCache's static entry map. The
            // realm objects it referenced are about to be invalid
            // because the ClassWorld is being closed below; holding on
            // would prevent their classloaders from being reclaimed
            // for the rest of the JVM's lifetime. This is the one
            // place we clear the realm cache — not on the invoker-
            // rebuild boundary — because that boundary leaves the
            // ClassWorld intact and the cached realms still usable
            // (the M4.3 T3 "cache survives invoker rebuild" contract).
            try {
                BaristaPluginRealmCache.clearAll();
            } catch (RuntimeException e) {
                LOG.log(Level.FINE,
                        () -> "ignored failure clearing plugin realm cache: " + e);
            }
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
        } finally {
            executionLock.unlock();
        }
    }

    @Override
    public String toString() {
        return "EmbeddedMaven{mavenHome=" + mavenHome
                + ", invocations=" + sequence.get()
                + ", cold=" + isColdStartPending() + "}";
    }
}
