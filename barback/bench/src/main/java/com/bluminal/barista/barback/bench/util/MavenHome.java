/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.bench.util;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Optional;

/**
 * Locate a Maven&nbsp;4 distribution for the JMH benches.
 *
 * <p>The integration-test fixture that performs the same job
 * ({@code MavenDistributionFixture}) lives under
 * {@code barback/src/test/java/}, which is unreachable from this bench
 * module (test scope of one Maven module is not visible to another).
 * We deliberately do <em>not</em> add a {@code test-jar} dependency to
 * pull it in: doing so would drag JUnit, AssertJ, and the entire
 * barback test surface onto the shaded JMH uber-JAR, inflating its
 * size by an order of magnitude and risking version drift between the
 * fixture and the production code under benchmark. Replicating the
 * minimum &mdash; the env-var + system-property lookup plus a
 * directory-shape check &mdash; is the cheaper option.
 *
 * <h2>Resolution order</h2>
 *
 * <ol>
 *   <li>{@code -Dbarista.maven.home=&lt;path&gt;} JVM system property
 *       (highest precedence; what {@code EmbeddedMavenFactory} itself
 *       consults first &mdash; benches inherit the daemon's own
 *       discovery contract);</li>
 *   <li>{@code BARISTA_MAVEN_HOME} environment variable;</li>
 *   <li>walk up from the bench JVM's cwd looking for a staged
 *       distribution at {@code barback/spike/m40-t2/apache-maven-4.0.0-rc-3/}.
 *       This matches the integration-fixture path so a developer who
 *       has already staged the spike for the IT suite gets the bench
 *       for free.</li>
 * </ol>
 *
 * <p>If nothing resolves the helper returns
 * {@link Optional#empty()}; benches that require a real distribution
 * call {@link #require()} which throws a clear error so JMH surfaces
 * the missing-fixture condition as a benchmark setup failure rather
 * than a silent NPE deep inside Maven core.
 */
public final class MavenHome {

    /** Mirror of {@code EmbeddedMavenFactory.MAVEN_HOME_PROPERTY}. */
    public static final String PROPERTY = "barista.maven.home";

    /** Mirror of {@code EmbeddedMavenFactory.MAVEN_HOME_ENV}. */
    public static final String ENV = "BARISTA_MAVEN_HOME";

    /**
     * Spike-relative path the integration fixture stages a Maven 4
     * distribution under. Re-checked here so developer machines that
     * have already run the spike pick up the distribution without
     * touching the environment.
     */
    private static final String SPIKE_REL = "spike/m40-t2/apache-maven-4.0.0-rc-3";

    private MavenHome() {
        // utility — instantiate via the static methods.
    }

    /**
     * Resolve a usable Maven&nbsp;4 distribution path or return
     * {@link Optional#empty()} when none is staged.
     */
    public static Optional<Path> find() {
        String prop = System.getProperty(PROPERTY);
        if (prop != null && !prop.isBlank()) {
            Path explicit = Path.of(prop).toAbsolutePath().normalize();
            if (looksLikeDistribution(explicit)) {
                return Optional.of(explicit);
            }
        }
        String env = System.getenv(ENV);
        if (env != null && !env.isBlank()) {
            Path fromEnv = Path.of(env).toAbsolutePath().normalize();
            if (looksLikeDistribution(fromEnv)) {
                return Optional.of(fromEnv);
            }
        }
        // Walk up from cwd for a staged spike. Five hops covers the
        // common launch sites: bench module root, barback root, repo
        // root, an enclosing checkout layout, and one extra hop for
        // safety.
        Path cwd = Path.of("").toAbsolutePath().normalize();
        Path candidate = cwd;
        for (int hops = 0; hops < 5 && candidate != null; hops++, candidate = candidate.getParent()) {
            Path tryPath = candidate.resolve(SPIKE_REL);
            if (looksLikeDistribution(tryPath)) {
                return Optional.of(tryPath);
            }
            tryPath = candidate.resolve("barback").resolve(SPIKE_REL);
            if (looksLikeDistribution(tryPath)) {
                return Optional.of(tryPath);
            }
        }
        return Optional.empty();
    }

    /**
     * As {@link #find()} but throws {@link IllegalStateException} with
     * a developer-readable message when no distribution is staged.
     * Suitable for use from a JMH {@code @Setup} method &mdash; the
     * thrown exception surfaces as a benchmark setup failure.
     */
    public static Path require() {
        return find().orElseThrow(() -> new IllegalStateException(
                "no Maven 4 distribution staged; set -D" + PROPERTY + "=<path>, "
                        + "export " + ENV + "=<path>, or run barback/spike/m40-t2/run.sh "
                        + "to stage a distribution under " + SPIKE_REL));
    }

    private static boolean looksLikeDistribution(Path candidate) {
        // A Maven distribution exposes `lib/` (the core JARs) and
        // `boot/` (the classworlds launcher). The bench only needs
        // `lib/` to materialise a ClassWorld but we keep both checks
        // so a malformed staging fails loudly instead of producing a
        // half-bootstrap and a confusing JMH crash.
        return Files.isDirectory(candidate)
                && Files.isDirectory(candidate.resolve("lib"))
                && Files.isDirectory(candidate.resolve("boot"));
    }
}
