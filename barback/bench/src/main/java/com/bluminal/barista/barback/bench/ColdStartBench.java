// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.bench;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.Comparator;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.TimeUnit;
import java.util.stream.Stream;

import com.bluminal.barista.barback.bench.util.MavenHome;
import com.bluminal.barista.barback.core.EmbeddedMaven;
import com.bluminal.barista.barback.core.EmbeddedMavenFactory;
import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;

import org.openjdk.jmh.annotations.Benchmark;
import org.openjdk.jmh.annotations.BenchmarkMode;
import org.openjdk.jmh.annotations.Fork;
import org.openjdk.jmh.annotations.Level;
import org.openjdk.jmh.annotations.Measurement;
import org.openjdk.jmh.annotations.Mode;
import org.openjdk.jmh.annotations.OutputTimeUnit;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.Setup;
import org.openjdk.jmh.annotations.State;
import org.openjdk.jmh.annotations.TearDown;
import org.openjdk.jmh.annotations.Warmup;
import org.openjdk.jmh.infra.Blackhole;

/**
 * JMH bench: barback cold-start cost &mdash; the wall-clock from
 * "no embedded core" to "first action terminated" on a fresh
 * {@link EmbeddedMaven} instance.
 *
 * <h2>What "cold start" means here</h2>
 *
 * <p>One {@code @Benchmark} call performs the full daemon-startup
 * critical path:
 *
 * <ol>
 *   <li>{@link EmbeddedMavenFactory#discover(Set)} with an empty
 *       override set &mdash; resolves the Maven&nbsp;4 distribution,
 *       constructs the {@link org.codehaus.plexus.classworlds.ClassWorld},
 *       boots a {@code ResidentMavenInvoker};</li>
 *   <li>{@link EmbeddedMaven#execute(ActionRequest)} on a
 *       {@code maven-compiler-plugin:compile} action against a
 *       1-module sample project &mdash; the first execute pays the
 *       full warm-up tax (Plexus container, Sisu wiring, plugin realm
 *       bootstrap, mojo descriptor resolution).</li>
 * </ol>
 *
 * <p>The M4.2 T3 completion record measured this at
 * &asymp;1133&nbsp;ms cold vs &asymp;120&nbsp;ms warm on the same
 * fixture (9.48&times; cold/warm ratio); this bench gives the
 * dashboard a continuous, JMH-stable view of that number.
 *
 * <h2>Why {@code @Setup(Level.Invocation)}</h2>
 *
 * <p>Cold-start is by definition a per-invocation property: a single
 * {@link EmbeddedMaven} services many actions and is no longer cold
 * after the first one. Constructing a fresh factory + invoker per
 * iteration is the only way to keep every measurement honest. The
 * cost is amortised against {@link Fork @Fork(1)}, warmup=0, and
 * iterations=5 &mdash; total bench wall-clock &asymp; 6&times; cold
 * cost &asymp; 7&nbsp;s on M-series hardware.
 *
 * <h2>Sample fixture</h2>
 *
 * <p>The same 1-module {@code maven-compiler-plugin:compile} fixture
 * the M4.0 T2 embedding spike (and the M4.2 T3 leak / warm-path
 * tests) staged under {@code barback/spike/m40-t2/sample-project/}.
 * The fixture is copied into a JVM-private temp directory at
 * {@code @Setup(Level.Trial)} so {@code target/} cleanup between
 * iterations doesn't race the spike's own contents on disk.
 *
 * <h2>JDK 17 vs JDK 21</h2>
 *
 * <p>Cold-start is dominated by classloader work; JDK 21's class-data
 * sharing improvements and Sisu's reflection-heavy bootstrap path
 * have shown 5&ndash;15% deltas in prior measurements. Recording both
 * JDKs surfaces that delta on the dashboard.
 *
 * <h2>Pre-requisites</h2>
 *
 * <p>The bench requires a staged Maven&nbsp;4 distribution
 * ({@code -Dbarista.maven.home=&lt;path&gt;}, {@code $BARISTA_MAVEN_HOME},
 * or {@code barback/spike/m40-t2/} populated via the spike's
 * {@code run.sh}). If no distribution resolves, {@code @Setup} throws
 * with a clear remediation message; JMH surfaces it as a benchmark
 * setup failure.
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.SingleShotTime)
@OutputTimeUnit(TimeUnit.MILLISECONDS)
@Fork(value = 1, warmups = 0)
@Warmup(iterations = 0)
@Measurement(iterations = 5)
public class ColdStartBench {

    private Path mavenHome;
    private Path stagedProject;
    private Path pomPath;
    private Path projectRoot;

    /** Per-invocation state. New every {@code @Benchmark} call. */
    private EmbeddedMaven embedded;

    @Setup(Level.Trial)
    public void setUpTrial() throws IOException {
        this.mavenHome = MavenHome.require();
        // Re-stage the spike's sample project into a JMH-private temp
        // directory so iteration teardown can wipe target/ without
        // touching the spike fixture on disk.
        Path tmpRoot = Files.createTempDirectory("barback-coldstart-bench-");
        // Best-effort cleanup on JVM exit; JMH does not guarantee
        // calling @TearDown(Level.Trial) on a fork-crash, so a hook
        // keeps the tmp footprint bounded.
        Runtime.getRuntime().addShutdownHook(new Thread(() -> deleteRecursive(tmpRoot)));
        this.stagedProject = stageSampleProject(tmpRoot);
        this.pomPath = stagedProject.resolve("pom.xml");
        this.projectRoot = stagedProject;
        // Pin maven.home into the system properties so the factory's
        // discover() picks it up without the developer needing to set
        // it externally. This is the same mechanism the IT fixture
        // uses; see EmbeddedMavenFactory for the resolution order.
        System.setProperty(MavenHome.PROPERTY, mavenHome.toString());
    }

    @Setup(Level.Invocation)
    public void setUpInvocation() throws IOException {
        // Each invocation needs a pristine project: target/ from a
        // prior compile would short-circuit the work the bench is
        // trying to measure. Delete it (best-effort) before the
        // factory + execute call.
        deleteRecursive(stagedProject.resolve("target"));
        // Fresh factory + invoker per invocation. discover(Set.of())
        // matches the task spec's "emptySet" entrypoint.
        this.embedded = EmbeddedMavenFactory.discover(Set.of());
    }

    @TearDown(Level.Invocation)
    public void tearDownInvocation() throws IOException {
        if (embedded != null) {
            embedded.close();
            embedded = null;
        }
    }

    /**
     * Single-shot: build the embedded core and execute the first
     * action. The action result is consumed via {@link Blackhole}.
     */
    @Benchmark
    public ActionResult coldStart(Blackhole bh) {
        ActionRequest action = ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(pomPath.toString())
                .setProjectRoot(projectRoot.toString())
                .setWorkingDirectory(projectRoot.toString())
                .setQuiet(true)
                .build();
        ActionResult result = embedded.execute(action);
        bh.consume(result);
        return result;
    }

    // ----- fixture staging -----

    /**
     * Copy {@code barback/spike/m40-t2/sample-project/} into
     * {@code destination}, returning the copy's root. Mirrors
     * {@code MavenDistributionFixture.stageSampleProject} from the
     * IT suite &mdash; replicated here because the IT helper lives in
     * {@code barback/src/test/java} and is unreachable from this
     * bench module.
     */
    private static Path stageSampleProject(Path destination) throws IOException {
        Path source = findSampleProjectSource();
        Path dest = destination.resolve("sample-project");
        copyDirectory(source, dest);
        // Maven 4 needs a multi-module-root marker; the spike's pom
        // predates that requirement, so a .mvn/ stub next to pom.xml
        // is the standard recipe. Matches MavenDistributionFixture.
        Files.createDirectories(dest.resolve(".mvn"));
        return dest;
    }

    private static Path findSampleProjectSource() {
        Path cwd = Path.of("").toAbsolutePath().normalize();
        Path candidate = cwd;
        for (int hops = 0; hops < 5 && candidate != null; hops++, candidate = candidate.getParent()) {
            Path tryPath = candidate.resolve("spike").resolve("m40-t2").resolve("sample-project");
            if (Files.isDirectory(tryPath)) {
                return tryPath;
            }
            tryPath = candidate.resolve("barback").resolve("spike").resolve("m40-t2").resolve("sample-project");
            if (Files.isDirectory(tryPath)) {
                return tryPath;
            }
        }
        throw new IllegalStateException(
                "spike sample-project not found relative to cwd " + cwd
                        + "; run barback/spike/m40-t2/run.sh first or invoke the "
                        + "bench from the barback/ directory");
    }

    private static void copyDirectory(Path source, Path dest) throws IOException {
        Files.createDirectories(dest);
        try (Stream<Path> walk = Files.walk(source)) {
            walk.forEach(src -> {
                try {
                    Path rel = source.relativize(src);
                    Path target = dest.resolve(rel.toString());
                    if (Files.isDirectory(src)) {
                        Files.createDirectories(target);
                    } else {
                        Files.createDirectories(target.getParent());
                        Files.copy(src, target, StandardCopyOption.REPLACE_EXISTING);
                    }
                } catch (IOException e) {
                    throw new RuntimeException("failed to copy " + src + " under " + source, e);
                }
            });
        }
    }

    private static void deleteRecursive(Path root) {
        if (!Files.exists(root)) {
            return;
        }
        try (Stream<Path> walk = Files.walk(root)) {
            walk.sorted(Comparator.reverseOrder()).forEach(p -> {
                try {
                    Files.deleteIfExists(p);
                } catch (IOException ignored) {
                    // Best-effort cleanup; the JMH JVM is about to
                    // exit anyway and the OS will reap the tmp tree.
                }
            });
        } catch (IOException ignored) {
            // Same rationale — best-effort.
        }
    }
}
