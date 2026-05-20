// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.bench;

import java.io.IOException;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.concurrent.TimeUnit;

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
import org.openjdk.jmh.annotations.Param;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.Setup;
import org.openjdk.jmh.annotations.State;
import org.openjdk.jmh.annotations.TearDown;
import org.openjdk.jmh.annotations.Warmup;
import org.openjdk.jmh.infra.Blackhole;

/**
 * JMH bench: {@link PluginCache} steady-state hit path on the M4.0-spike
 * 5-plugin manifest.
 *
 * <p>This is the daemon's warm-path price for a plugin lookup: a
 * pre-warmed {@link PluginCache} fields the same {@link PluginKey}
 * lookups the dispatcher makes on every action. The
 * {@link PluginCacheMissBench} bench reuses the same workload with the
 * override list inflated to force a miss on every call so the cache's
 * value-build cost (URLClassLoader + Mojo-class linkage) is visible
 * head-to-head with this bench's HashMap-probe cost. The expected
 * ratio is hundreds-to-thousands of times (the M4.2 T4 IT measured
 * 705&ndash;817&times; on the same manifest).
 *
 * <h2>Workload</h2>
 *
 * <p>5 standard Maven plugins resolved from the user's
 * {@code ~/.m2/repository} &mdash; identical to
 * {@code PluginCacheSpeedupIT}. See {@link WorkloadManifest} for the
 * exact GAV pins.
 *
 * <h2>JDK 17 vs JDK 21</h2>
 *
 * <p>The hit path is a pure {@link java.util.LinkedHashMap} probe; we
 * expect &lt;5% variation between JDK 17 and JDK 21 because nothing on
 * the path benefits from virtual threads, ZGC generational mode, or
 * the JDK&nbsp;21 pattern-matching switch lowering. Recording it under
 * both JDKs anyway lets us spot a regression should one appear (e.g.
 * the {@link PluginKey} record's {@code hashCode} micro-cost shifting
 * between releases).
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.NANOSECONDS)
@Fork(1)
@Warmup(iterations = 3, time = 1)
@Measurement(iterations = 5, time = 2)
public class PluginCacheHitBench {

    /**
     * Iteration multiplier. The cache hit on a single key is a
     * sub-microsecond operation; sweeping all 5 keys per
     * {@code @Benchmark} call makes the per-op cost large enough to
     * stay above JMH's measurement noise floor without distorting the
     * shape of what's being measured.
     */
    @Param({"5"})
    public int sweep;

    private PluginCache cache;
    private List<PluginKey> keys;
    private List<URLClassLoader> retainedLoaders;
    private PluginCache.LoaderBuilder noopBuilder;

    @Setup(Level.Trial)
    public void setUp() throws IOException {
        List<ResolvedPlugin> plugins = WorkloadManifest.resolve();
        if (plugins.size() != WorkloadManifest.PLUGIN_COUNT) {
            throw new IllegalStateException(
                    "PluginCacheHitBench requires all "
                            + WorkloadManifest.PLUGIN_COUNT
                            + " plugin JARs under ~/.m2/repository; resolved "
                            + plugins.size() + ". Populate the local repository "
                            + "(e.g. run barback/spike/m40-t1/run.sh) to enable "
                            + "this bench.");
        }
        this.cache = new PluginCache(Set.of());
        this.keys = new ArrayList<>(plugins.size());
        this.retainedLoaders = new ArrayList<>(plugins.size());

        // Warm the cache once. Subsequent loadOrBuild calls must hit
        // for every key — the LoaderBuilder we install below would
        // throw if invoked. That throw is the bench's correctness
        // gate: if any iteration accidentally misses, JMH surfaces the
        // exception and the bench fails loud rather than reporting a
        // bogus number.
        PluginCache.LoaderBuilder warmBuilder = k -> {
            ResolvedPlugin p = byKey(plugins, k);
            URLClassLoader cl = PluginCache.buildUrlClassLoader(
                    p.spec().gav(), List.of(p.jar()));
            retainedLoaders.add(cl);
            return cl;
        };
        for (ResolvedPlugin p : plugins) {
            cache.loadOrBuild(p.key(), warmBuilder);
            keys.add(p.key());
        }

        this.noopBuilder = k -> {
            throw new IllegalStateException(
                    "PluginCacheHitBench observed a miss on key " + k
                            + " — the cache should be fully warmed at setup time");
        };
    }

    @TearDown(Level.Trial)
    public void tearDown() throws IOException {
        // Close the cache first (it tries to close every cached
        // URLClassLoader); the explicit retain-list pass below is a
        // belt-and-braces tear-down for any loader the cache might
        // have skipped (e.g. an injected adapter type — not the case
        // here but cheap insurance).
        cache.close();
        for (URLClassLoader cl : retainedLoaders) {
            try {
                cl.close();
            } catch (IOException ignored) {
                // bench teardown best-effort; the JVM is about to exit
            }
        }
    }

    /**
     * Sweep all 5 keys through the cache and consume the resulting
     * {@link ClassLoader} into the {@link Blackhole} so JMH can't
     * dead-code-eliminate the lookup. One {@code @Benchmark} call =
     * 5 cache probes; divide the reported time by {@link #sweep} to
     * get per-probe latency.
     */
    @Benchmark
    public void cacheHit(Blackhole bh) {
        for (int i = 0; i < sweep; i++) {
            ClassLoader cl = cache.loadOrBuild(keys.get(i), noopBuilder);
            bh.consume(cl);
        }
    }

    private static ResolvedPlugin byKey(List<ResolvedPlugin> plugins, PluginKey key) {
        for (ResolvedPlugin p : plugins) {
            if (p.key().equals(key)) {
                return p;
            }
        }
        throw new IllegalStateException("no resolved plugin for key " + key);
    }
}
