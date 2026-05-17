/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.List;
import java.util.Locale;
import java.util.Objects;
import java.util.Set;
import java.util.logging.Level;
import java.util.logging.Logger;
import java.util.stream.Stream;

import com.bluminal.barista.barback.classloader.BaristaPluginRealmCache;
import com.bluminal.barista.barback.classloader.PluginCache;
import org.codehaus.plexus.classworlds.ClassWorld;
import org.codehaus.plexus.classworlds.realm.ClassRealm;

/**
 * Builds the daemon-wide {@link EmbeddedMaven} instance once at startup
 * and hands it back as a reusable singleton.
 *
 * <p>The factory owns two things that must never be reconstructed
 * inside the hot path:
 *
 * <ol>
 *   <li>The {@link ClassWorld} backing the embedded Maven 4 core. It is
 *       seeded from a Maven 4 distribution's {@code boot/} +
 *       {@code conf/logging/} + {@code lib/} JARs in the same order
 *       {@code classworlds.conf} uses for {@code bin/mvn}, so the
 *       in-process bootstrap behaves identically to the out-of-process
 *       bootstrap.</li>
 *   <li>The single {@link EmbeddedMaven} that wraps a single
 *       {@code ResidentMavenInvoker} backed by that {@code ClassWorld}.
 *       The resident invoker caches one {@code MavenContext} per
 *       request signature, which is exactly the warm-path cache the
 *       daemon relies on for &ge;10&times; over {@code mvn 3.9.x}.</li>
 * </ol>
 *
 * <h2>Maven distribution discovery</h2>
 *
 * <p>The factory needs a Maven 4 distribution directory containing
 * {@code lib/}, {@code boot/}, and {@code conf/logging/}. For v0.1 the
 * discovery model is intentionally simple &mdash; we pick exactly one
 * of these, in order, and fail fast if none resolves to a usable
 * directory:
 *
 * <ol>
 *   <li>{@code -Dbarista.maven.home=&lt;path&gt;} JVM system property
 *       (highest precedence; lets tests pin a specific distribution
 *       without touching the process environment);</li>
 *   <li>{@code BARISTA_MAVEN_HOME} environment variable (the daemon
 *       launcher sets this when {@code barista} ships a bundled
 *       distribution);</li>
 *   <li>explicit constructor argument passed to {@link #using(Path)}
 *       (used by callers that already know the distribution path).</li>
 * </ol>
 *
 * <p>Bundling a Maven 4 distribution inside the {@code barback} jar
 * (e.g. unpacking to a per-user cache on first run) is a follow-up,
 * not a v0.1 requirement &mdash; the spike harness and the M4.2 tests
 * already stage a distribution alongside the build artifacts. The env
 * var / system property surface keeps the daemon launchable in both
 * setups without baking a 14&nbsp;MiB tarball into the jar prematurely.
 *
 * <h2>Lifecycle</h2>
 *
 * <p>The factory is a singleton scoped to one daemon JVM. The
 * {@link EmbeddedMaven} it returns is {@link AutoCloseable}; closing
 * it tears down the resident invoker (and the classworlds container
 * with it). The factory itself holds no state beyond the cached
 * {@link EmbeddedMaven} reference, so closing the {@link EmbeddedMaven}
 * is sufficient to release every native resource the embedded core
 * acquired.
 */
public final class EmbeddedMavenFactory {

    private static final Logger LOG = Logger.getLogger(EmbeddedMavenFactory.class.getName());

    /** System property checked first for the Maven 4 distribution path. */
    public static final String MAVEN_HOME_PROPERTY = "barista.maven.home";

    /** Environment variable checked after the system property. */
    public static final String MAVEN_HOME_ENV = "BARISTA_MAVEN_HOME";

    /**
     * The fixed ClassRealm identifier the embedded core looks up. Must
     * match the identifier {@code ClingSupport.CORE_CLASS_REALM_ID}
     * uses internally; {@code plexus.core} is the canonical value
     * baked into {@code classworlds.conf} and into Maven's bootstrap
     * code. Changing this name would silently break the embed.
     */
    static final String CORE_REALM_ID = "plexus.core";

    private EmbeddedMavenFactory() {
        // utility class — instantiate via the static factory methods.
    }

    /**
     * Resolve the Maven 4 distribution path from
     * {@code -Dbarista.maven.home} or {@code $BARISTA_MAVEN_HOME} and
     * build an {@link EmbeddedMaven} backed by it. Equivalent to
     * {@code using(resolveMavenHome())}.
     *
     * @throws IllegalStateException if neither the system property nor
     *     the environment variable identifies a directory that looks
     *     like a Maven 4 distribution.
     */
    public static EmbeddedMaven discover() throws IOException {
        return using(resolveMavenHome());
    }

    /**
     * Build an {@link EmbeddedMaven} from the discovered Maven 4
     * distribution, using {@code overrideList} as the plugin
     * classloader cache's override set (OPEN-8 escape hatch).
     *
     * <p>The override list is the v0.1 mechanism for handling plugins
     * that misbehave under classloader caching &mdash; e.g. plugins
     * that store state in static fields and assume a cold JVM. PRD
     * &sect;11.6 specifies the format as a set of
     * {@code groupId:artifactId} strings; entries matching one of
     * these bypass the cache and are loaded fresh on every action.
     * Default is empty; populate via this factory entrypoint or by
     * setting {@code -Dbarista.daemon.classloader_cache.override=ga1,ga2}
     * on the daemon JVM. The {@code barista.toml} surface that exposes
     * this to end users is implemented separately as part of the
     * config-wiring task (M4.3 dispatcher batch).
     */
    public static EmbeddedMaven discover(Set<String> overrideList) throws IOException {
        return with(resolveMavenHome(), overrideList);
    }

    /**
     * Build an {@link EmbeddedMaven} that loads the embedded core from
     * {@code mavenHome}. The directory must contain {@code lib/} and
     * {@code boot/} subdirectories matching the standard Maven 4
     * distribution layout; otherwise {@link IllegalArgumentException}
     * is thrown.
     *
     * <p>This method is heavy: it constructs the {@link ClassWorld} and
     * an associated {@code ResidentMavenInvoker}, which together do
     * several hundred milliseconds of disk + classloader work. Call it
     * once at daemon startup and reuse the returned instance.
     */
    public static EmbeddedMaven using(Path mavenHome) throws IOException {
        // Honour the override-list system property when the caller
        // hasn't passed an explicit set. The launcher-set property is
        // the common path while barista.toml plumbing is in flight;
        // explicit programmatic overrides (the with(...) overload)
        // take precedence and skip property parsing entirely.
        return withCache(mavenHome, PluginCache.fromSystemProperties());
    }

    /**
     * Build an {@link EmbeddedMaven} from an explicit Maven 4
     * distribution path and override list. See
     * {@link #discover(Set)} for the override-list semantics.
     *
     * @param mavenHome the Maven 4 distribution root (contains
     *     {@code lib/}, {@code boot/}, {@code conf/}); never
     *     {@code null}
     * @param overrideList the plugin classloader cache override set
     *     (see {@link PluginCache#overrideList()}); never {@code null}.
     *     Pass {@link Set#of()} for the default policy.
     */
    public static EmbeddedMaven with(Path mavenHome, Set<String> overrideList) throws IOException {
        Objects.requireNonNull(overrideList, "overrideList");
        return withCache(mavenHome, new PluginCache(overrideList));
    }

    /**
     * Build an {@link EmbeddedMaven} from an explicit Maven 4
     * distribution path and a pre-built {@link PluginCache}. Used by
     * tests that need to install a custom cache (e.g. one with an
     * inflated override list to disable caching for the comparison
     * arm of the speedup IT) without round-tripping through the
     * {@link PluginCache#OVERRIDE_PROPERTY} system property.
     */
    public static EmbeddedMaven withCache(Path mavenHome, PluginCache pluginCache) throws IOException {
        Objects.requireNonNull(mavenHome, "mavenHome");
        Objects.requireNonNull(pluginCache, "pluginCache");
        Path normalized = mavenHome.toAbsolutePath().normalize();
        requireDistribution(normalized);

        // Maven 4 reads `maven.home` eagerly via System.getProperty in
        // its bootstrap; ClingSupport / DefaultParser will look it up
        // when resolving conf/, the wrapper script, and the bundled
        // settings.xml. We set it process-wide here because the
        // factory's contract is "owns the embedded core for this JVM";
        // setting it on a per-call basis would race with concurrent
        // invocations on the same daemon.
        System.setProperty("maven.home", normalized.toString());
        // maven.installation.conf points at the bundled conf/ root.
        // ClingSupport falls back to <maven.home>/conf when this is
        // unset, but explicit is friendlier when a future Maven
        // version reshuffles the lookup order.
        System.setProperty("maven.installation.conf", normalized.resolve("conf").toString());

        ClassWorld world = buildClassWorld(normalized);
        // Install the host PluginCache as the companion for the Sisu-
        // discovered BaristaPluginRealmCache. The Sisu component is
        // instantiated per Plexus container by the cling stack, but its
        // entry storage is process-wide (static) so a single companion
        // wired here serves every container the daemon brings up. The
        // override list and the realm-cache hit/miss counters both flow
        // through this companion reference.
        BaristaPluginRealmCache.setCompanion(pluginCache);
        LOG.log(Level.INFO,
                () -> "embedded Maven core class-world built from " + normalized
                        + " (realm=" + CORE_REALM_ID + ", overrides="
                        + pluginCache.overrideList().size() + ")");
        return new EmbeddedMaven(world, normalized, pluginCache);
    }

    /**
     * Resolve a Maven 4 distribution path from the standard daemon
     * sources. Public so callers (e.g. the daemon's startup banner)
     * can surface the resolved path in diagnostics without going
     * through the full factory init.
     */
    public static Path resolveMavenHome() {
        String prop = System.getProperty(MAVEN_HOME_PROPERTY);
        if (prop != null && !prop.isEmpty()) {
            return Path.of(prop).toAbsolutePath().normalize();
        }
        String env = System.getenv(MAVEN_HOME_ENV);
        if (env != null && !env.isEmpty()) {
            return Path.of(env).toAbsolutePath().normalize();
        }
        throw new IllegalStateException(
                "no embedded Maven 4 distribution configured; set -D"
                        + MAVEN_HOME_PROPERTY + "=<path> or export "
                        + MAVEN_HOME_ENV + "=<path> pointing at a Maven 4 "
                        + "distribution directory (contains lib/, boot/, conf/)");
    }

    /**
     * Builds the same {@link ClassWorld} layout that
     * {@code classworlds.conf} builds for the {@code bin/mvn}
     * launcher: a single {@code plexus.core} realm seeded with every
     * JAR under {@code conf/logging/}, {@code lib/}, and
     * {@code boot/}, in that order.
     *
     * <p>Logging JARs go first so SLF4J bindings resolve to the
     * configured logging stack before any {@code lib/} class that
     * references SLF4J statically can pick up a fallback binding from
     * the lib classpath. This mirrors the launcher's behavior.
     *
     * <p>Optional {@code lib/ext/*.jar} core extensions are skipped
     * for v0.1; the embedding-strategy doc records this as a
     * follow-up. The barback daemon does not consume
     * {@code extensions.xml}-declared core extensions yet.
     */
    private static ClassWorld buildClassWorld(Path mavenHome) throws IOException {
        ClassWorld world = new ClassWorld(CORE_REALM_ID, Thread.currentThread().getContextClassLoader());
        ClassRealm core = world.getClassRealm(CORE_REALM_ID);
        addJarsFromDir(core, mavenHome.resolve("conf").resolve("logging"));
        addJarsFromDir(core, mavenHome.resolve("lib"));
        addJarsFromDir(core, mavenHome.resolve("boot"));
        return core.getWorld();
    }

    private static void addJarsFromDir(ClassRealm realm, Path dir) throws IOException {
        if (!Files.isDirectory(dir)) {
            return;
        }
        try (Stream<Path> stream = Files.list(dir)) {
            List<Path> jars = stream
                    .filter(p -> p.getFileName().toString().toLowerCase(Locale.ROOT).endsWith(".jar"))
                    .sorted(Comparator.naturalOrder())
                    .toList();
            for (Path jar : jars) {
                realm.addURL(jar.toUri().toURL());
            }
        }
    }

    private static void requireDistribution(Path mavenHome) {
        if (!Files.isDirectory(mavenHome)) {
            throw new IllegalArgumentException(
                    "embedded Maven 4 distribution directory does not exist: " + mavenHome);
        }
        Path lib = mavenHome.resolve("lib");
        Path boot = mavenHome.resolve("boot");
        if (!Files.isDirectory(lib) || !Files.isDirectory(boot)) {
            throw new IllegalArgumentException(
                    "path does not look like a Maven 4 distribution "
                            + "(missing lib/ or boot/): " + mavenHome);
        }
    }
}
