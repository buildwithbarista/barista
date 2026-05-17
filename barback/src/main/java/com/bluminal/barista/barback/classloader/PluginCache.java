/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.classloader;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Collections;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.atomic.AtomicLong;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * Per-daemon cache of realized plugin classloaders, keyed by plugin
 * coordinate plus a content hash of the plugin's main JAR.
 *
 * <h2>Why this exists</h2>
 *
 * <p>Maven plugins are loaded by enumerating each plugin's JARs into a
 * fresh {@link URLClassLoader} per invocation. On a cold {@code mvn}
 * the cost is unavoidable. On a long-lived daemon every action repeats
 * the same JAR-scan + {@code defineClass} dance for the same plugins,
 * which the M4.0 spike measured at hundreds of milliseconds per plugin
 * on the 5-plugin sample workload. Caching the realized
 * {@link URLClassLoader} per signature collapses subsequent invocations
 * to a single map lookup.
 *
 * <p>PRD &sect;11.6 specifies the cache shape; this is its v0.1
 * implementation.
 *
 * <h2>Cache key</h2>
 *
 * <p>{@link PluginKey} = {@code groupId:artifactId:version} + the
 * SHA-256 of the plugin's main JAR file bytes. The hash binds the cache
 * entry to the exact bytes-on-disk of the plugin JAR Maven would have
 * resolved, so a {@code SNAPSHOT} that was rebuilt between two daemon
 * actions invalidates the entry naturally without the cache having to
 * track timestamps. The dependency closure of the plugin is <em>not</em>
 * hashed: the closure's content is dictated by the GAV (Maven's
 * resolution is deterministic), so the GAV pin plus the main-JAR hash
 * uniquely identify the classloader contents. Tests pin both halves of
 * this contract.
 *
 * <h2>Override list (PRD &sect;11.6, OPEN-8)</h2>
 *
 * <p>Some plugins assume a cold JVM (static fields, thread-local state,
 * setting {@code Thread.currentThread().setContextClassLoader} in ways
 * that interact badly with a reused loader). The daemon ships an escape
 * hatch: {@link #overrideList()} is a {@link Set} of {@code groupId:
 * artifactId} (GA, no version) strings; any plugin whose key matches a
 * GA in this set bypasses the cache &mdash; every action rebuilds the
 * classloader fresh, paying the cold cost so the misbehaving plugin
 * sees the JVM state it expects.
 *
 * <p>v0.1 defaults the override list to empty. The list will be
 * populated as we encounter problems in the wild (the PRD's policy);
 * users opting in can either:
 * <ul>
 *   <li>Construct an {@link com.bluminal.barista.barback.core.EmbeddedMavenFactory}
 *       with an explicit override set via
 *       {@code EmbeddedMavenFactory.with(mavenHome, overrideList)} (the
 *       programmatic seam exercised by integration tests);</li>
 *   <li>Pass {@code -Dbarista.daemon.classloader_cache.override=ga1,ga2}
 *       as a daemon JVM property (the bootstrap surface used by the CLI
 *       until the {@code barista.toml} plumbing lands &mdash; see the
 *       v0.1 escape-hatch documentation pointer in the
 *       {@code EmbeddedMavenFactory} javadoc).</li>
 * </ul>
 *
 * <h2>Eviction</h2>
 *
 * <p>The cache lives strictly under the lifetime of the
 * {@link com.bluminal.barista.barback.core.EmbeddedMaven} instance that
 * owns it. The host {@link com.bluminal.barista.barback.core.EmbeddedMaven#close()}
 * call propagates here via {@link #invalidateAll()}, which closes every
 * cached {@link URLClassLoader} and clears the entry map.
 *
 * <p>The host also periodically rebuilds its
 * {@code ResidentMavenInvoker} to work around the Maven 4.0.0-rc-3
 * session-cache leak (see the "Periodic invoker eviction" javadoc on
 * {@code EmbeddedMaven}). That rebuild discards every realm Maven
 * itself constructed under the prior invoker, so cached entries
 * pointing at child realms of the dropped invoker hierarchy must be
 * dropped too. The host calls {@link #invalidateAll()} from inside the
 * same lock that rebuilds the invoker, ensuring no action ever observes
 * a stale loader. This is the eviction-interaction contract the
 * {@code PluginCacheEvictionInteractionTest} guards.
 *
 * <p>An LRU cap (PRD &sect;11.6, default 64) is <em>not</em>
 * implemented in v0.1: the override list plus the per-invoker
 * eviction-driven {@code invalidateAll} already bound the live entry
 * count, and the 5-plugin workload AC is orders of magnitude below the
 * cap. Wiring LRU is a follow-up tracked in the open-questions section
 * of the roadmap.
 *
 * <h2>Concurrency</h2>
 *
 * <p>The host serialises {@code execute(ActionRequest)} with a
 * {@link java.util.concurrent.locks.ReentrantLock}, so the cache is
 * only ever read or mutated by one thread at a time. The internal map
 * is a plain {@link LinkedHashMap} (insertion-ordered) under that
 * external invariant. We do not depend on {@link java.util.concurrent}
 * primitives here because adding one would imply we relax the host's
 * lock without auditing every other place the
 * {@code ResidentMavenInvoker} touches shared state.
 */
public final class PluginCache implements AutoCloseable {

    private static final Logger LOG = Logger.getLogger(PluginCache.class.getName());

    /**
     * JVM system property the daemon launcher uses to populate the
     * override list when {@link com.bluminal.barista.barback.core.EmbeddedMavenFactory#discover()}
     * boots the embedded core. Format: comma-separated {@code groupId:
     * artifactId} entries (no version). Empty / unset means no
     * overrides.
     */
    public static final String OVERRIDE_PROPERTY = "barista.daemon.classloader_cache.override";

    private final Set<String> overrideList;
    private final Map<PluginKey, CachedClassLoader> entries;
    private final AtomicLong hits = new AtomicLong(0);
    private final AtomicLong misses = new AtomicLong(0);
    private final AtomicLong overrideBypasses = new AtomicLong(0);

    /**
     * Build a cache with an explicit override list. The set is copied
     * and unmodifiable internally; callers may mutate their input
     * afterward without affecting the cache.
     *
     * @param overrideList the {@code groupId:artifactId} entries that
     *     should bypass caching; never {@code null}. Pass
     *     {@link Collections#emptySet()} for the default policy.
     */
    public PluginCache(Set<String> overrideList) {
        Objects.requireNonNull(overrideList, "overrideList");
        // defensive copy + unmodifiable view; cheap because the list is
        // small (single-digit entries even in pathological deployments)
        this.overrideList = Set.copyOf(overrideList);
        this.entries = new LinkedHashMap<>();
    }

    /**
     * Build a cache with the override list resolved from the
     * {@link #OVERRIDE_PROPERTY} system property. Convenience for the
     * factory bootstrap path.
     */
    public static PluginCache fromSystemProperties() {
        return new PluginCache(parseOverrideProperty(System.getProperty(OVERRIDE_PROPERTY)));
    }

    /**
     * Parse the comma-separated override-list property value into a
     * normalised set of {@code groupId:artifactId} entries. Visible for
     * tests.
     */
    static Set<String> parseOverrideProperty(String raw) {
        if (raw == null || raw.isBlank()) {
            return Set.of();
        }
        String[] parts = raw.split(",");
        java.util.LinkedHashSet<String> out = new java.util.LinkedHashSet<>(parts.length);
        for (String part : parts) {
            String trimmed = part.trim();
            if (trimmed.isEmpty()) {
                continue;
            }
            // Reject malformed entries early — a {@code groupId:artifactId}
            // miswrite (e.g. {@code groupId/artifactId}) would otherwise
            // silently never match anything and the operator would have
            // no signal the override was a no-op.
            if (trimmed.indexOf(':') < 0) {
                LOG.log(Level.WARNING,
                        () -> "ignoring malformed PluginCache override entry (expected groupId:artifactId): "
                                + trimmed);
                continue;
            }
            out.add(trimmed);
        }
        return Collections.unmodifiableSet(out);
    }

    /**
     * Look up the cached classloader for {@code plugin}, building and
     * caching it via {@code loader} on a miss. Returns the realized
     * {@link ClassLoader} ready for mojo lookup.
     *
     * <p>Override-listed plugins bypass the cache entirely: the loader
     * function runs every call and the returned {@link ClassLoader} is
     * a fresh instance. The caller owns the resulting loader's
     * lifecycle in that case (it is not closed by
     * {@link #invalidateAll()}).
     *
     * @param key the cache key; never {@code null}
     * @param loader supplier called only on a cache miss; receives the
     *     same {@code key}, returns the freshly-built loader; never
     *     {@code null}
     * @return the realized classloader for {@code key}; never {@code null}
     */
    public ClassLoader loadOrBuild(PluginKey key, LoaderBuilder loader) {
        Objects.requireNonNull(key, "key");
        Objects.requireNonNull(loader, "loader");
        if (isOverridden(key)) {
            overrideBypasses.incrementAndGet();
            // Build a fresh loader without consulting (or polluting) the
            // cache. The caller is responsible for closing it; the
            // common case is that the action that triggered this lookup
            // discards the loader on exit, which matches Maven's
            // current per-invocation behaviour.
            return loader.build(key);
        }
        CachedClassLoader cached = entries.get(key);
        if (cached != null) {
            hits.incrementAndGet();
            return cached.loader;
        }
        misses.incrementAndGet();
        ClassLoader fresh = loader.build(key);
        // We tolerate plain ClassLoader from the supplier so adapters
        // (e.g. Plexus ClassRealm) compose, but we can only close
        // URLClassLoaders cleanly. Other loaders are tracked but
        // skipped on invalidateAll; the JVM GCs them when the host
        // EmbeddedMaven drops its reference.
        entries.put(key, new CachedClassLoader(fresh));
        return fresh;
    }

    /**
     * Is the given plugin override-listed?
     */
    public boolean isOverridden(PluginKey key) {
        return overrideList.contains(key.ga());
    }

    /**
     * Read-only view of the override list. Useful for diagnostics and
     * status responses ({@code StatusResponse.cache_overrides_active}).
     */
    public Set<String> overrideList() {
        return overrideList;
    }

    /** Number of cache hits since construction. */
    public long hitCount() {
        return hits.get();
    }

    /** Number of cache misses (i.e. fresh loader builds) since construction. */
    public long missCount() {
        return misses.get();
    }

    /** Number of override-list bypasses since construction. */
    public long overrideBypassCount() {
        return overrideBypasses.get();
    }

    /** Number of live cache entries. */
    public int size() {
        return entries.size();
    }

    /**
     * Drop every cached entry and close any associated
     * {@link URLClassLoader}. Called by
     * {@link com.bluminal.barista.barback.core.EmbeddedMaven#close()}
     * and at the host's invoker-rebuild boundary.
     *
     * <p>Closing each cached loader breaks references the JVM would
     * otherwise hold via the loader's parent chain &mdash; the
     * heap-stability guarantee from M4.2 T3's eviction depends on
     * this. If a cached loader is not a {@link URLClassLoader} (a
     * legitimate but uncommon case for an adapter type), we drop the
     * map entry and let GC handle it; the loader's own
     * {@link AutoCloseable} contract is the caller's to honour if it
     * implements one.
     */
    public void invalidateAll() {
        for (Map.Entry<PluginKey, CachedClassLoader> e : entries.entrySet()) {
            ClassLoader cl = e.getValue().loader;
            if (cl instanceof URLClassLoader ucl) {
                try {
                    ucl.close();
                } catch (IOException ex) {
                    // Best-effort tear-down: a failure to close a
                    // loader does not block the cache from continuing.
                    // Log at FINE because this is uncommon enough that
                    // anything louder would be noise on healthy runs.
                    LOG.log(Level.FINE,
                            () -> "ignored failure closing cached plugin classloader "
                                    + e.getKey() + ": " + ex);
                }
            }
        }
        entries.clear();
    }

    @Override
    public void close() {
        invalidateAll();
    }

    /**
     * Compute the SHA-256 content hash of {@code jar} and return it as
     * a 64-char lowercase hex string. Public so callers building a
     * {@link PluginKey} can stamp the hash without re-implementing the
     * canonical recipe.
     */
    public static String sha256(Path jar) throws IOException {
        try {
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            try (var in = Files.newInputStream(jar)) {
                byte[] buf = new byte[8192];
                int n;
                while ((n = in.read(buf)) > 0) {
                    digest.update(buf, 0, n);
                }
            }
            return HexFormat.of().formatHex(digest.digest());
        } catch (NoSuchAlgorithmException e) {
            // SHA-256 is mandated by the JDK spec; absence here implies a
            // catastrophically broken JRE, which we cannot recover from.
            throw new IllegalStateException("SHA-256 unavailable in this JRE", e);
        }
    }

    /**
     * Build a {@link URLClassLoader} over {@code jars} with the
     * platform classloader as parent. The platform parent matches what
     * Maven core does for plugin realms (so plugin code only sees the
     * JDK base modules from its parent chain, never barback's own
     * classpath). The {@code name} surfaces in stack traces as the
     * loader's identity. Public so the integration test can drive the
     * cache without pulling in real Maven plugin-resolution.
     */
    public static URLClassLoader buildUrlClassLoader(String name, List<Path> jars) {
        URL[] urls = new URL[jars.size()];
        for (int i = 0; i < jars.size(); i++) {
            try {
                urls[i] = jars.get(i).toUri().toURL();
            } catch (java.net.MalformedURLException e) {
                throw new UncheckedIOException(new IOException(
                        "cannot convert plugin jar path to URL: " + jars.get(i), e));
            }
        }
        return new URLClassLoader(name, urls, ClassLoader.getPlatformClassLoader());
    }

    /**
     * Builder seam for cache misses. Equivalent to a
     * {@link java.util.function.Function} but typed for clarity at the
     * call site and to make stack traces blame the right component.
     */
    @FunctionalInterface
    public interface LoaderBuilder {
        /**
         * Build a fresh classloader for {@code key}. The supplier is
         * called only when the cache decides a fresh build is needed
         * (a miss, or an override-list bypass).
         */
        ClassLoader build(PluginKey key);
    }

    /**
     * Wrap-record so the entry map can grow auxiliary fields (e.g. a
     * last-access timestamp for the future LRU cap) without churning
     * the value type. v0.1 holds only the loader.
     */
    private static final class CachedClassLoader {
        final ClassLoader loader;
        CachedClassLoader(ClassLoader loader) {
            this.loader = loader;
        }
    }
}
