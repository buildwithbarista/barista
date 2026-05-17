/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import java.io.IOException;
import java.lang.ref.WeakReference;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.List;
import java.util.UUID;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.stream.Stream;

import com.bluminal.barista.barback.classloader.PluginCache;
import com.bluminal.barista.barback.classloader.PluginKey;
import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Tag;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Proves the {@link PluginCache} <em>survives</em> M4.2 T3's periodic
 * invoker eviction. The cache is invalidated only when the host's
 * {@link ClassWorld} is disposed (i.e. on {@link EmbeddedMaven#close()}).
 *
 * <h2>Why this contract changed (M4.3 T3)</h2>
 *
 * <p>Originally the host cleared {@link PluginCache#invalidateAll()}
 * on every invoker rebuild so cached {@link URLClassLoader}s never
 * outlived the resident invoker they were built under. That contract
 * predated the {@link com.bluminal.barista.barback.classloader.BaristaPluginRealmCache}
 * wiring (M4.3 T3), where the plugin-realm storage is a process-wide
 * static map that intentionally outlives the resident invoker so the
 * warm-shot SM-3.2 path never rebuilds the same realm twice.
 *
 * <p>With the realm-cache contract owning the survival semantics, the
 * companion {@link PluginCache} (PluginKey → URLClassLoader, the
 * diagnostic / OPEN-8 override surface) follows the same lifetime:
 * it is invalidated only when {@code EmbeddedMaven#close()} disposes
 * the ClassWorld. Holding entries across the invoker rebuild is safe
 * because the parent realm hierarchy (the {@code plexus.core} realm
 * in the retained ClassWorld) is identical before and after the
 * rebuild.
 *
 * <h2>What this test pins</h2>
 *
 * <ol>
 *   <li>A pre-populated {@link PluginCache} entry is still present
 *       after the host's resident invoker has been rebuilt at least
 *       once.</li>
 *   <li>The entry is dropped (and its {@link URLClassLoader} becomes
 *       GC-eligible) only after {@link EmbeddedMaven#close()} fires.</li>
 * </ol>
 *
 * <p>Tagged {@code integration} because the host EmbeddedMaven still
 * needs a real Maven distribution to bring up its ClassWorld + boot
 * its first ResidentMavenInvoker.
 */
@Tag("integration")
final class PluginCacheEvictionInteractionTest {

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
    @DisplayName("PluginCache survives invoker rebuild; entries drop only on EmbeddedMaven.close()")
    void cacheSurvivesInvokerRebuild(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);
        Path fakeJar = writeFakeJar(tmp.resolve("fake-plugin.jar"));

        // Drive T3's eviction at N=3 so the test only needs 4 Maven
        // actions to cross the boundary once.
        savedThreshold = EmbeddedMaven.MAX_ACTIONS_PER_INVOKER;
        EmbeddedMaven.MAX_ACTIONS_PER_INVOKER = 3;

        embedded = EmbeddedMavenFactory.using(mavenHome);
        PluginCache cache = embedded.pluginCache();

        // Pre-populate the cache with a fake entry. We hold only a
        // weak reference to the realised loader so we can prove the
        // cache lets it go on close.
        PluginKey fakeKey = new PluginKey("com.example", "fake-plugin", "1.0",
                PluginCache.sha256(fakeJar));
        WeakReference<URLClassLoader> weakLoader = primeCacheEntry(cache, fakeKey, fakeJar);

        assertEquals(1, cache.size(), "cache should hold the seeded entry");
        assertEquals(0, embedded.invokerRebuildCount(),
                "fresh EmbeddedMaven should not have rebuilt yet");

        // Three Maven actions fill the cycle without crossing the
        // boundary. The cache entry should survive.
        runOne(project); // #1
        runOne(project); // #2
        runOne(project); // #3 — fills the cycle but doesn't yet evict
        assertEquals(1, cache.size(),
                "cache should still hold the seeded entry mid-cycle");
        assertEquals(0, embedded.invokerRebuildCount(),
                "no rebuild should have fired yet");

        // The fourth action crosses the threshold: T3 evicts the
        // invoker. With the M4.3 T3 contract the PluginCache entry
        // SURVIVES the rebuild — only the resident invoker is
        // recycled; the ClassWorld (and the cached loader's parent
        // realm chain) is retained.
        runOne(project); // #4 — boundary call → rebuild #1
        assertEquals(1, embedded.invokerRebuildCount(),
                "T3 must have rebuilt the invoker on the boundary call");
        assertEquals(1, cache.size(),
                "PluginCache MUST survive invoker rebuild (M4.3 T3 contract); "
                        + "entries are only dropped on EmbeddedMaven#close() when "
                        + "the ClassWorld goes away");

        // Closing the host disposes the ClassWorld; the cache must
        // drop every entry at that point so cached loaders become
        // GC-eligible (no realm chain into a live ClassWorld holds
        // them alive any longer).
        embedded.close();
        embedded = null;

        assertTrue(waitForCollection(weakLoader),
                "cached URLClassLoader must be reclaimable after EmbeddedMaven#close(); "
                        + "if this fails, PluginCache or EmbeddedMaven is still "
                        + "holding a strong reference into a disposed realm hierarchy");
    }

    /**
     * M4.3 T3 wiring smoke test. Drives a small handful of Maven
     * compile actions and asserts the
     * {@link com.bluminal.barista.barback.classloader.BaristaPluginRealmCache}
     * Sisu hook is the one Maven consulted &mdash; not the default
     * {@code DefaultPluginRealmCache}. Visible via the realm-cache
     * hit / miss counters on the host's {@link PluginCache}: hits
     * only flow into those counters when our impl is the one Sisu
     * bound. If a future Maven version reshuffles the Sisu hint
     * ordering and our {@code @Priority(100)} stops winning, this
     * test will fail with zero counters — making the breakage
     * visible at the bring-up surface rather than as a silent
     * regression in the warm-shot path.
     */
    @Test
    @DisplayName("BaristaPluginRealmCache is the Sisu-bound PluginRealmCache for the embedded core")
    void baristaPluginRealmCacheWiringActive(@TempDir Path tmp) throws IOException {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);

        savedThreshold = EmbeddedMaven.MAX_ACTIONS_PER_INVOKER;
        EmbeddedMaven.MAX_ACTIONS_PER_INVOKER = 12;

        embedded = EmbeddedMavenFactory.using(mavenHome);
        PluginCache cache = embedded.pluginCache();

        // Two actions within a single invoker cycle: the first
        // populates the cache for every plugin it loads, the
        // second's lookups hit. Our hook records each on the
        // companion's realm-cache counters.
        runOne(project);
        runOne(project);

        long hits = cache.realmCacheHitCount();
        long misses = cache.realmCacheMissCount();

        // Misses fire whenever our hook is consulted for a fresh
        // plugin-key lookup. A 1-module compile engages
        // resources / compiler / jar (and friends), so the first
        // call should record at least one miss against our hook.
        //
        // Note: subsequent calls within the same invoker cycle don't
        // necessarily hit our hook — Maven core's
        // {@code PluginDescriptorCache} caches the descriptor with
        // its ClassRealm attached, so once the descriptor is warm
        // {@code setupPluginRealm} short-circuits and our
        // {@link PluginRealmCache#get(Key)} is never called for
        // that plugin a second time. That layering is why the
        // SM-3.2 measurement is dominated by what runs INSIDE
        // {@code ResidentMavenInvoker.invoke()} after plugin
        // resolution settles, not by anything our hook controls.
        // For the wiring check it is enough that we observe at
        // least one miss flowing through our hook.
        assertTrue(misses > 0L,
                "BaristaPluginRealmCache hook never observed a lookup — "
                        + "Sisu likely bound the default PluginRealmCache. "
                        + "Check the @Priority(100) annotation and the "
                        + "META-INF/sisu/javax.inject.Named index in the "
                        + "uber-jar. hits=" + hits + " misses=" + misses);
    }

    /**
     * Drop a fake entry into the cache and return a weak reference to
     * its realised loader. The loader has no strong reference outside
     * the cache after this method returns, so a subsequent
     * {@code invalidateAll()} releases it for GC.
     */
    private WeakReference<URLClassLoader> primeCacheEntry(PluginCache cache,
                                                          PluginKey key,
                                                          Path jar) {
        URLClassLoader[] holder = new URLClassLoader[1];
        cache.loadOrBuild(key, k -> {
            URLClassLoader cl = PluginCache.buildUrlClassLoader(k.gav(), List.of(jar));
            holder[0] = cl;
            return cl;
        });
        WeakReference<URLClassLoader> weak = new WeakReference<>(holder[0]);
        holder[0] = null; // drop our only local strong reference
        return weak;
    }

    /**
     * Spin a small GC-pressure loop and check whether the referent
     * has been reclaimed. We don't trust a single {@code System.gc()}
     * call — the JVM is free to ignore it — so we apply gentle heap
     * pressure across several iterations and observe whether the
     * reference clears.
     */
    private static boolean waitForCollection(WeakReference<?> ref) {
        for (int i = 0; i < 20; i++) {
            if (ref.get() == null) {
                return true;
            }
            // Coax the collector: a hint plus a small allocation burst
            // that doesn't blow the IT's heap. The JVM may decline gc()
            // but allocation pressure rarely fails to schedule one
            // within the 20 iterations.
            System.gc();
            allocate(1_000_000);
            try {
                Thread.sleep(20);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
        }
        // One last check after the loop.
        return ref.get() == null;
    }

    /** Allocation pressure to encourage the collector to run. */
    private static void allocate(int n) {
        Object[] junk = new Object[n];
        for (int i = 0; i < n; i++) {
            junk[i] = new byte[16];
        }
        // Consume the array so the JIT doesn't eliminate the
        // allocations as dead code. A side-effecting hashCode read
        // is enough; the reference falls out of scope on return.
        if (junk.hashCode() == Integer.MIN_VALUE) {
            // Astronomically unlikely; this branch exists only to
            // make 'junk' live across the loop in the JIT's eyes.
            System.out.println("(allocation pressure marker)");
        }
    }

    /**
     * Build a minimal valid JAR so the URLClassLoader has something
     * real to point at. We don't put any Mojo classes in it; the test
     * only needs a loader instance whose GC behaviour we can observe.
     */
    private static Path writeFakeJar(Path out) throws IOException {
        Manifest mf = new Manifest();
        mf.getMainAttributes().putValue("Manifest-Version", "1.0");
        try (var fos = Files.newOutputStream(out);
             var jos = new JarOutputStream(fos, mf)) {
            jos.putNextEntry(new JarEntry("placeholder.txt"));
            jos.write("plugin\n".getBytes(java.nio.charset.StandardCharsets.UTF_8));
            jos.closeEntry();
        }
        return out;
    }

    private void runOne(Path project) throws IOException {
        cleanTarget(project);
        ActionRequest req = ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(project.resolve("pom.xml").toString())
                .setProjectRoot(project.toString())
                .setWorkingDirectory(project.toString())
                .setQuiet(true)
                .build();
        ActionResult result = embedded.execute(req);
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
                    // Best-effort; @TempDir mops up the rest.
                }
            });
        }
    }

}
