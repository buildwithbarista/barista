// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.UUID;
import java.util.stream.Stream;

import org.apache.maven.cling.invoker.mvn.resident.ResidentMavenInvoker;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Tag;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Exercises the periodic-eviction policy on {@link EmbeddedMaven}.
 *
 * <p>The policy itself is documented under the "Periodic invoker
 * eviction" section of {@link EmbeddedMaven}'s class javadoc. Its
 * v0.1 motivation is the Maven&nbsp;4.0.0-rc-3 session-cache leak
 * (&asymp;0.57&nbsp;MiB/action of unreclaimable state). To exercise
 * the boundary cheaply, this test drives the policy at a deliberately
 * small {@link EmbeddedMaven#MAX_ACTIONS_PER_INVOKER} value rather
 * than running 16+ Maven invocations per assertion.
 *
 * <p>Tagged {@code integration} because it still needs a real Maven
 * distribution staged (the eviction code rebuilds a
 * {@link ResidentMavenInvoker}, which expects a working
 * {@code ClassWorld} for the realm bootstrap).
 */
@Tag("integration")
final class EmbeddedMavenEvictionTest {

    private EmbeddedMaven embedded;
    private int savedThreshold;

    @AfterEach
    void restoreAndClose() throws IOException {
        EmbeddedMaven.MAX_ACTIONS_PER_INVOKER = savedThreshold;
        if (embedded != null) {
            embedded.close();
            embedded = null;
        }
    }

    @Test
    @DisplayName("invoker is rebuilt every MAX_ACTIONS_PER_INVOKER calls")
    void rebuildsInvokerOnCadence(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);

        // Drive the policy at N=3 so the assertion only needs seven
        // Maven invocations to exercise: call #1 cold; #2-#3 warm;
        // boundary at #4 → rebuild #1; #5-#6 warm against rebuild #1;
        // boundary at #7 → rebuild #2.
        savedThreshold = EmbeddedMaven.MAX_ACTIONS_PER_INVOKER;
        EmbeddedMaven.MAX_ACTIONS_PER_INVOKER = 3;

        embedded = EmbeddedMavenFactory.using(mavenHome);
        ResidentMavenInvoker initial = embedded.invoker();
        assertEquals(0, embedded.invokerRebuildCount(),
                "fresh EmbeddedMaven should have zero rebuilds");

        runOne(project);                              // #1
        runOne(project);                              // #2
        runOne(project);                              // #3 fills the cycle
        assertSame(initial, embedded.invoker(),
                "invoker must not be rebuilt before the boundary call");
        assertEquals(0, embedded.invokerRebuildCount());
        assertEquals(3, embedded.invocationCountInCurrentCycle());

        runOne(project);                              // #4 boundary → rebuild #1
        ResidentMavenInvoker afterFirstRebuild = embedded.invoker();
        assertNotSame(initial, afterFirstRebuild,
                "invoker reference identity must change on the boundary call");
        assertEquals(1, embedded.invokerRebuildCount());
        // The boundary call itself counted toward the new cycle.
        assertEquals(1, embedded.invocationCountInCurrentCycle());

        runOne(project);                              // #5
        runOne(project);                              // #6 fills the second cycle
        assertSame(afterFirstRebuild, embedded.invoker(),
                "invoker must not rebuild mid-cycle");
        assertEquals(1, embedded.invokerRebuildCount());
        assertEquals(3, embedded.invocationCountInCurrentCycle());

        runOne(project);                              // #7 boundary → rebuild #2
        ResidentMavenInvoker afterSecondRebuild = embedded.invoker();
        assertNotSame(afterFirstRebuild, afterSecondRebuild,
                "invoker reference identity must change again on the next boundary");
        assertEquals(2, embedded.invokerRebuildCount());

        // The sequence counter is monotonic across rebuilds.
        assertEquals(7L, embedded.invocationCount(),
                "invocation counter must keep increasing across rebuilds");
    }

    @Test
    @DisplayName("no eviction occurs when action count stays under the threshold")
    void noRebuildWithinSingleCycle(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);

        // N=10 so two compiles cannot trigger a rebuild.
        savedThreshold = EmbeddedMaven.MAX_ACTIONS_PER_INVOKER;
        EmbeddedMaven.MAX_ACTIONS_PER_INVOKER = 10;

        embedded = EmbeddedMavenFactory.using(mavenHome);
        ResidentMavenInvoker initial = embedded.invoker();

        runOne(project);
        runOne(project);

        assertSame(initial, embedded.invoker(),
                "invoker must be the same reference when the cycle is not exhausted");
        assertEquals(0, embedded.invokerRebuildCount(),
                "no rebuild should fire when actions stay under MAX_ACTIONS_PER_INVOKER");
        assertEquals(2, embedded.invocationCountInCurrentCycle());
    }

    private void runOne(Path project) throws IOException {
        cleanTarget(project);
        ActionRequest request = ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(project.resolve("pom.xml").toString())
                .setProjectRoot(project.toString())
                .setWorkingDirectory(project.toString())
                .setQuiet(true)
                .build();
        ActionResult result = embedded.execute(request);
        assertTrue(result.getStatus() == ActionResult.Status.SUCCESS,
                "compile should succeed; failure=" + result.getFailureMessage());
    }

    private static void cleanTarget(Path project) throws IOException {
        Path target = project.resolve("target");
        if (!Files.exists(target)) {
            return;
        }
        try (Stream<Path> walk = Files.walk(target)) {
            walk.sorted(Comparator.reverseOrder()).forEach(p -> {
                try {
                    Files.deleteIfExists(p);
                } catch (IOException ignored) {
                    // Best-effort cleanup; @TempDir handles the rest.
                }
            });
        }
    }
}
