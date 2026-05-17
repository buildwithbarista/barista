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
 * Proves the {@link PluginCache} survives M4.2 T3's periodic invoker
 * eviction without leaking entries that point at realms attached to
 * the dropped invoker hierarchy.
 *
 * <p>The contract under test (see the "Eviction" section of
 * {@link PluginCache}'s javadoc):
 *
 * <ol>
 *   <li>When {@code EmbeddedMaven} rebuilds its
 *       {@code ResidentMavenInvoker} on the
 *       {@code MAX_ACTIONS_PER_INVOKER} boundary, the cache's
 *       {@link PluginCache#invalidateAll()} is called inside the same
 *       lock that swaps the invoker reference.</li>
 *   <li>Every cached {@link URLClassLoader} is therefore closed and
 *       the entry map is cleared.</li>
 *   <li>No live reference into the dropped invoker's realm hierarchy
 *       remains in {@code PluginCache} after the eviction.</li>
 * </ol>
 *
 * <p>We verify (1) and (2) by inspecting cache statistics directly,
 * and we verify (3) by holding a {@link WeakReference} to a loader
 * instance that was cached pre-eviction and asserting that it becomes
 * garbage-collectable after the eviction fires.
 *
 * <p>The test drives an {@link EmbeddedMaven} through enough actions
 * to force at least one T3 eviction (with {@code MAX_ACTIONS_PER_INVOKER}
 * pinned to 3 for cheap reproduction). It does <em>not</em> exercise
 * Maven's actual plugin loading: we install a fake cache entry via
 * the public {@link EmbeddedMaven#pluginCache()} accessor between
 * Maven action calls, then watch what happens to it on the boundary
 * call.
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
    @DisplayName("PluginCache is invalidated on invoker rebuild; cached loaders become GC-eligible")
    void cacheClearsOnInvokerRebuild(@TempDir Path tmp) throws IOException {
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
        // cache lets it go.
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
        // invoker AND drops every cached PluginCache entry in the
        // same locked region.
        runOne(project); // #4 — boundary call → rebuild #1 → cache invalidate
        assertEquals(1, embedded.invokerRebuildCount(),
                "T3 must have rebuilt the invoker on the boundary call");
        assertEquals(0, cache.size(),
                "PluginCache must drop every entry when T3 rebuilds the invoker");

        // Now prove (3): no strong reference into the dropped realm
        // chain remains. We dropped our local strong reference inside
        // primeCacheEntry; after the invalidate cleared the cache's
        // own reference, the loader must be GC-collectable.
        assertTrue(waitForCollection(weakLoader),
                "cached URLClassLoader must be reclaimable after invalidateAll; "
                        + "if this fails, PluginCache or EmbeddedMaven is still "
                        + "holding a strong reference into the dropped invoker hierarchy");
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
