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

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * End-to-end test for {@link EmbeddedMaven} against the spike's
 * 1-module sample project.
 *
 * <p>Asserts the milestone&nbsp;4.2 task&nbsp;3 acceptance criteria
 * scoped to this task: the embedded core compiles a project
 * end-to-end (exit code&nbsp;0 + classes in {@code target/}) and the
 * {@link ActionResult} envelope is populated correctly.
 *
 * <p>Skips gracefully when the Maven&nbsp;4 distribution is not
 * staged; see {@link MavenDistributionFixture}.
 */
final class EmbeddedMavenTest {

    private EmbeddedMaven embedded;

    @AfterEach
    void closeEmbedded() throws IOException {
        if (embedded != null) {
            embedded.close();
            embedded = null;
        }
    }

    @Test
    @DisplayName("factory rejects directories that are not a Maven 4 distribution")
    void rejectsNonDistributionPath(@TempDir Path tmp) {
        Path bogus = tmp.resolve("not-a-maven-dist");
        try {
            Files.createDirectories(bogus);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
        IllegalArgumentException ex = assertThrows(IllegalArgumentException.class,
                () -> EmbeddedMavenFactory.using(bogus));
        assertTrue(ex.getMessage().contains("Maven 4 distribution"),
                "exception message should explain the failure: " + ex.getMessage());
    }

    @Test
    @DisplayName("compiles the spike sample-project via the embedded core")
    void compilesSampleProject(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);

        embedded = EmbeddedMavenFactory.using(mavenHome);
        ActionRequest request = compileRequest(project);

        ActionResult result = embedded.execute(request);

        assertEquals(ActionResult.Status.SUCCESS, result.getStatus(),
                "embedded compile should succeed; failure=" + result.getFailureMessage());
        assertEquals(0, result.getExitCode());
        assertEquals(request.getActionId(), result.getActionId());
        assertTrue(result.getDurationMicros() > 0,
                "duration_micros should be populated: " + result.getDurationMicros());

        Path targetClasses = project.resolve("target").resolve("classes");
        assertTrue(Files.isDirectory(targetClasses),
                "expected target/classes/ after compile: " + targetClasses);
        Path helloClass = targetClasses.resolve("Hello.class");
        assertTrue(Files.isRegularFile(helloClass),
                "expected Hello.class after compile: " + helloClass);
    }

    @Test
    @DisplayName("execute() failure surfaces via ActionResult, not a thrown exception")
    void failureReportedAsActionResult(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        embedded = EmbeddedMavenFactory.using(mavenHome);

        Path missingPom = tmp.resolve("nonexistent").resolve("pom.xml");
        ActionRequest request = ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(missingPom.toString())
                .setWorkingDirectory(tmp.toString())
                .setQuiet(true)
                .build();

        ActionResult result = embedded.execute(request);

        assertEquals(ActionResult.Status.FAILURE, result.getStatus());
        assertEquals(request.getActionId(), result.getActionId());
        // A non-zero exit code carries the build failure (parser or
        // Maven core); we don't pin a specific code because the
        // upstream value can shift between rc-3 and GA.
        assertTrue(result.getExitCode() != 0, "expected non-zero exit code on failure");
        assertNotNull(result.getError());
        assertEquals(EmbeddedMaven.CORE_ERROR_CODE, result.getError().getCode());
    }

    @Test
    @DisplayName("invocationCount() and isColdStartPending() track lifecycle")
    void tracksInvocationLifecycle(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);
        embedded = EmbeddedMavenFactory.using(mavenHome);

        assertTrue(embedded.isColdStartPending());
        assertEquals(0L, embedded.invocationCount());

        ActionResult first = embedded.execute(compileRequest(project));
        assertEquals(ActionResult.Status.SUCCESS, first.getStatus());
        assertEquals(1L, embedded.invocationCount());
        assertTrue(!embedded.isColdStartPending(), "cold-start flag should flip after first invoke");

        // Run a second action so the resident invoker's cache is
        // exercised; the invocationCount has to keep monotonically
        // increasing.
        cleanTarget(project);
        ActionResult second = embedded.execute(compileRequest(project));
        assertEquals(ActionResult.Status.SUCCESS, second.getStatus());
        assertEquals(2L, embedded.invocationCount());
    }

    private static ActionRequest compileRequest(Path project) {
        return ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(project.resolve("pom.xml").toString())
                .setProjectRoot(project.toString())
                .setWorkingDirectory(project.toString())
                .setQuiet(true)
                .build();
    }

    /**
     * Delete {@code target/} so a subsequent compile actually does
     * work. Used by tests that want the second action to exercise the
     * full mojo pipeline.
     */
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
                    // Best-effort; the @TempDir guarantees final cleanup.
                }
            });
        }
    }
}
