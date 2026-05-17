/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.bench;

import java.io.IOException;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

import com.bluminal.barista.barback.bench.util.WorkloadManifest;
import com.bluminal.barista.barback.bench.util.WorkloadManifest.ResolvedPlugin;
import com.bluminal.barista.barback.classloader.PluginCache;
import com.bluminal.barista.barback.classloader.PluginKey;

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
 * JMH bench: {@link PluginCache} miss path on the M4.0-spike 5-plugin
 * manifest &mdash; the "every action pays the realm-build cost" arm.
 *
 * <p>Pairs with {@link PluginCacheHitBench}: same workload, but the
 * {@link PluginCache} is constructed with the GA set of all 5 plugins
 * on its override list, so every {@link PluginCache#loadOrBuild}
 * invocation bypasses the cache entirely and rebuilds the
 * {@link URLClassLoader} fresh. The value-side workload mirrors what
 * Maven core does on a real plugin realm bootstrap: scan
 * {@code META-INF/maven/plugin.xml}, enumerate the {@code *Mojo}
 * classes, and {@link ClassLoader#loadClass(String) loadClass} each
 * one to force linkage.
 *
 * <p>Reusing the override-list seam (rather than calling
 * {@link PluginCache#invalidateAll()} inside the timed section) keeps
 * the bench faithful to a real-world miss: the cache is in normal
 * operating mode, the {@link PluginCache#loadOrBuild} entrypoint is
 * what's called, and the only difference from
 * {@link PluginCacheHitBench} is which branch the cache takes
 * internally. {@code invalidateAll} would have measured the wrong
 * thing &mdash; it includes a {@link URLClassLoader#close()} round-trip
 * per cached entry that an override-list bypass never pays.
 *
 * <h2>What this measures</h2>
 *
 * <p>Wall-clock time to:
 * <ol>
 *   <li>compute the override-list bypass branch (constant);</li>
 *   <li>build a fresh {@link URLClassLoader} over the plugin's main
 *       JAR (single {@code Path}-to-URL conversion);</li>
 *   <li>{@link ClassLoader#loadClass(String) loadClass} every Mojo
 *       class in the plugin (the {@code defineClass} cost; the
 *       expensive part).</li>
 * </ol>
 *
 * <p>This is a faithful approximation of the cost Maven core would
 * pay on a cold action: the plugin-realm scan and class linkage are
 * the dominant work, dwarfing the URL bookkeeping.
 *
 * <h2>JDK 17 vs JDK 21</h2>
 *
 * <p>The miss path is bytecode-loading-heavy: {@code defineClass}
 * cost is sensitive to verifier changes, JIT differences in the
 * classloader's locking strategy, and ClassData improvements that
 * landed in later JDK 17 patch releases. Recording both gives the
 * dashboard the signal a "fallback path is slower" claim would need.
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.MICROSECONDS)
@Fork(1)
@Warmup(iterations = 3, time = 1)
@Measurement(iterations = 5, time = 2)
public class PluginCacheMissBench {

    private PluginCache cache;
    private List<PluginKey> keys;
    private Map<PluginKey, ResolvedPlugin> byKey;
    private Map<PluginKey, List<String>> mojoClassNames;
    private PluginCache.LoaderBuilder realmBootstrap;

    @Setup(Level.Trial)
    public void setUp() throws IOException {
        List<ResolvedPlugin> plugins = WorkloadManifest.resolve();
        if (plugins.size() != WorkloadManifest.PLUGIN_COUNT) {
            throw new IllegalStateException(
                    "PluginCacheMissBench requires all "
                            + WorkloadManifest.PLUGIN_COUNT
                            + " plugin JARs under ~/.m2/repository; resolved "
                            + plugins.size() + ". Populate the local repository "
                            + "(e.g. run barback/spike/m40-t1/run.sh) to enable "
                            + "this bench.");
        }
        // Override-list every plugin: forces loadOrBuild() to take the
        // bypass branch and rebuild a fresh URLClassLoader every call.
        // The cache itself never grows entries in this configuration.
        this.cache = new PluginCache(WorkloadManifest.gaSet());
        this.keys = new ArrayList<>(plugins.size());
        this.byKey = new HashMap<>(plugins.size());
        this.mojoClassNames = new HashMap<>(plugins.size());

        // Pre-scan JARs for *Mojo class names so the timed section
        // doesn't include the JAR-entry enumeration cost. Maven core's
        // plugin descriptor parsing happens once per session, well
        // outside the per-action hot path — pinning the scan to setup
        // keeps the bench focused on what cache MISSES actually pay.
        for (ResolvedPlugin p : plugins) {
            keys.add(p.key());
            byKey.put(p.key(), p);
            mojoClassNames.put(p.key(), discoverMojoClassNames(p.jar()));
        }

        // The miss-builder mirrors the PluginCacheSpeedupIT methodology:
        // build a URLClassLoader, force-link every Mojo class. Each call
        // is a fresh loader the JVM will GC once the @Benchmark return
        // drops the reference — the bench measures STEADY-STATE miss
        // cost, not the cost of cache-entry churn (override-list
        // bypasses don't go in the entries map at all).
        this.realmBootstrap = k -> {
            ResolvedPlugin p = byKey.get(k);
            URLClassLoader cl = PluginCache.buildUrlClassLoader(
                    p.spec().gav(), List.of(p.jar()));
            for (String cn : mojoClassNames.get(k)) {
                try {
                    cl.loadClass(cn);
                } catch (Throwable ignored) {
                    // Mojo linkage can fail without maven-plugin-api on
                    // the parent chain — but the defineClass cost (the
                    // expensive part) has already been paid by the
                    // class-load attempt. See PluginCacheSpeedupIT for
                    // the same swallow-and-continue rationale.
                }
            }
            return cl;
        };
    }

    @TearDown(Level.Trial)
    public void tearDown() throws IOException {
        cache.close();
    }

    /**
     * Sweep all 5 keys through the cache; every call bypasses the
     * cache and rebuilds. One {@code @Benchmark} call = 5 full
     * URLClassLoader builds + Mojo-linkage passes. Divide the reported
     * time by 5 for per-plugin miss latency.
     */
    @Benchmark
    public void cacheMiss(Blackhole bh) {
        for (PluginKey k : keys) {
            ClassLoader cl = cache.loadOrBuild(k, realmBootstrap);
            bh.consume(cl);
        }
    }

    /**
     * Enumerate the fully-qualified names of every {@code *Mojo}
     * class in the plugin's main JAR. Lifted from
     * {@code PluginCacheSpeedupIT} so the two harnesses produce
     * comparable numbers on the same workload.
     */
    private static List<String> discoverMojoClassNames(java.nio.file.Path jar) throws IOException {
        List<String> out = new ArrayList<>();
        try (JarFile jf = new JarFile(jar.toFile())) {
            if (jf.getEntry("META-INF/maven/plugin.xml") == null) {
                return out;
            }
            Enumeration<JarEntry> entries = jf.entries();
            while (entries.hasMoreElements()) {
                JarEntry e = entries.nextElement();
                String name = e.getName();
                if (!name.endsWith("Mojo.class")) continue;
                if (name.startsWith("META-INF/")) continue;
                out.add(name.substring(0, name.length() - ".class".length())
                        .replace('/', '.'));
            }
        }
        return out;
    }
}
