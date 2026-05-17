/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.Comparator;
import java.util.Optional;
import java.util.stream.Stream;

import org.junit.jupiter.api.Assumptions;

/**
 * Locates the Maven&nbsp;4 distribution and the spike's sample-project
 * fixture for tests that exercise the embedded core.
 *
 * <p>The M4.0 embedding spike already curates an extracted Maven&nbsp;4
 * distribution and a 1-module {@code sample-project} under
 * {@code barback/spike/m40-t2/}. Re-using that staging avoids
 * downloading the 14&nbsp;MiB distribution in CI and keeps every
 * embedded-Maven test pinned to the exact version the
 * {@code embedded.maven.version} property declares.
 *
 * <p>Resolution order for the distribution:
 *
 * <ol>
 *   <li>{@code -Dbarista.test.maven.home=&lt;path&gt;} JVM property
 *       (CI's preferred override);</li>
 *   <li>{@code barback/spike/m40-t2/apache-maven-4.0.0-rc-3/} relative
 *       to the working directory.</li>
 * </ol>
 *
 * <p>If neither resolves, every helper that requires a real Maven
 * distribution calls {@link Assumptions#assumeTrue(boolean, String)}
 * to skip the test. That makes the unit-test job green on machines
 * that have not staged the distribution, while CI (which always
 * stages it) executes the suite for real.
 */
public final class MavenDistributionFixture {

    /**
     * JVM property a CI runner can set to point at a pre-staged
     * distribution.
     */
    public static final String TEST_MAVEN_HOME_PROPERTY = "barista.test.maven.home";

    private MavenDistributionFixture() {
        // utility — instantiate via the static methods.
    }

    /**
     * Resolve a usable Maven&nbsp;4 distribution path or return
     * {@link Optional#empty()} when none is staged. Use
     * {@link #requireMavenHome()} from inside a test method to skip
     * the test cleanly via {@link Assumptions} when staging is
     * missing.
     */
    public static Optional<Path> findMavenHome() {
        String prop = System.getProperty(TEST_MAVEN_HOME_PROPERTY);
        if (prop != null && !prop.isEmpty()) {
            Path explicit = Path.of(prop).toAbsolutePath().normalize();
            if (looksLikeDistribution(explicit)) {
                return Optional.of(explicit);
            }
            return Optional.empty();
        }
        // Walk up from the test JVM's cwd to find barback/spike/m40-t2.
        // Tests typically run with cwd at the module root, but a multi-
        // module IDE run may launch with the repository root as cwd.
        Path cwd = Path.of("").toAbsolutePath().normalize();
        Path candidate = cwd;
        for (int hops = 0; hops < 4 && candidate != null; hops++, candidate = candidate.getParent()) {
            Path tryPath = candidate.resolve("spike").resolve("m40-t2").resolve("apache-maven-4.0.0-rc-3");
            if (looksLikeDistribution(tryPath)) {
                return Optional.of(tryPath);
            }
            tryPath = candidate.resolve("barback").resolve("spike").resolve("m40-t2").resolve("apache-maven-4.0.0-rc-3");
            if (looksLikeDistribution(tryPath)) {
                return Optional.of(tryPath);
            }
        }
        return Optional.empty();
    }

    /**
     * As {@link #findMavenHome()} but skips the calling test via
     * {@link Assumptions} when no distribution is staged. Returns the
     * distribution path on success.
     */
    public static Path requireMavenHome() {
        Optional<Path> resolved = findMavenHome();
        Assumptions.assumeTrue(resolved.isPresent(),
                "Maven 4 distribution not staged. Run barback/spike/m40-t2/run.sh "
                        + "or set -D" + TEST_MAVEN_HOME_PROPERTY + "=<path>");
        return resolved.get();
    }

    /**
     * Copy the spike's {@code sample-project} (a 1-module fixture
     * with {@code maven-compiler-plugin} and one Java source) into
     * {@code destination}. Returns the copy's root path. The copy is
     * destructive against {@code destination}'s existing contents; use
     * a JUnit {@code @TempDir} as the destination.
     */
    public static Path stageSampleProject(Path destination) throws IOException {
        Path source = findSampleProjectSource();
        Path dest = destination.resolve("sample-project");
        copyDirectory(source, dest);
        // Maven 4 requires either a .mvn/ directory or a
        // root="true" attribute on the project's model to identify
        // the multi-module root. The spike's sample-project pom does
        // not declare root="true" (it predates the M4.2 requirement);
        // dropping an empty .mvn/ directory next to pom.xml is the
        // standard recipe and matches how a Maven 4-ready project
        // would be laid out on disk.
        Files.createDirectories(dest.resolve(".mvn"));
        return dest;
    }

    private static Path findSampleProjectSource() {
        Path cwd = Path.of("").toAbsolutePath().normalize();
        Path candidate = cwd;
        for (int hops = 0; hops < 4 && candidate != null; hops++, candidate = candidate.getParent()) {
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
                        + "; cannot stage the test fixture");
    }

    private static boolean looksLikeDistribution(Path candidate) {
        return Files.isDirectory(candidate)
                && Files.isDirectory(candidate.resolve("lib"))
                && Files.isDirectory(candidate.resolve("boot"));
    }

    private static void copyDirectory(Path source, Path dest) throws IOException {
        if (Files.exists(dest)) {
            try (Stream<Path> walk = Files.walk(dest)) {
                walk.sorted(Comparator.reverseOrder()).forEach(p -> {
                    try {
                        Files.deleteIfExists(p);
                    } catch (IOException ignored) {
                        // Best-effort cleanup; @TempDir will mop up.
                    }
                });
            }
        }
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
}
