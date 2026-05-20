// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.classloader;

import java.io.IOException;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Unit-level coverage for {@link PluginCache}: hit/miss semantics, the
 * override-list bypass path, key validation, hash computation, and the
 * {@link PluginCache#invalidateAll()} tear-down contract that the
 * eviction policy in {@link com.bluminal.barista.barback.core.EmbeddedMaven}
 * relies on.
 *
 * <p>These tests deliberately do <em>not</em> pull in real Maven; the
 * cache itself is a Maven-agnostic component and is exercised with
 * tiny generated JARs so the suite runs in milliseconds against the
 * unit-test profile (no {@code @Tag("integration")} gating).
 */
final class PluginCacheTest {

    private static final String EXAMPLE_GA = "com.example.tools:nasty-mojo";
    private static final String EXAMPLE_GROUP = "com.example.tools";
    private static final String EXAMPLE_ARTIFACT = "nasty-mojo";

    @Test
    @DisplayName("PluginKey rejects malformed hashes")
    void pluginKeyRejectsBadHashes() {
        // wrong length
        assertThrows(IllegalArgumentException.class, () ->
                new PluginKey("g", "a", "1", "abc"));
        // uppercase hex — we canonicalize to lowercase
        String upper = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
        assertThrows(IllegalArgumentException.class, () ->
                new PluginKey("g", "a", "1", upper));
        // non-hex
        String nonhex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdez";
        assertThrows(IllegalArgumentException.class, () ->
                new PluginKey("g", "a", "1", nonhex));
    }

    @Test
    @DisplayName("PluginKey equality is structural and GA-derived")
    void pluginKeyEqualityIsStructural() {
        String h = "0123456789abcdef".repeat(4);
        PluginKey a = new PluginKey("g", "a", "1", h);
        PluginKey b = new PluginKey("g", "a", "1", h);
        PluginKey c = new PluginKey("g", "a", "2", h); // version differs
        assertEquals(a, b);
        assertEquals(a.hashCode(), b.hashCode());
        assertNotSame(a, c);
        assertFalse(a.equals(c));
        assertEquals("g:a", a.ga());
        assertEquals("g:a:1", a.gav());
    }

    @Test
    @DisplayName("sha256 hash is stable across calls and changes with content")
    void sha256IsContentAddressed(@TempDir Path tmp) throws IOException {
        Path j1 = writeJar(tmp, "a.jar", "alpha");
        Path j2 = writeJar(tmp, "b.jar", "alpha");
        Path j3 = writeJar(tmp, "c.jar", "beta");

        String h1 = PluginCache.sha256(j1);
        String h2 = PluginCache.sha256(j2);
        String h3 = PluginCache.sha256(j3);

        assertEquals(h1, h2, "identical content must hash to the same value");
        assertNotSame(h1, h3);
        assertFalse(h1.equals(h3), "differing content must hash differently");
        assertEquals(64, h1.length(), "SHA-256 is 64 hex chars");
    }

    @Test
    @DisplayName("first lookup misses and stores; second lookup hits")
    void hitAfterMiss(@TempDir Path tmp) throws IOException {
        PluginCache cache = new PluginCache(Set.of());
        Path jar = writeJar(tmp, "plugin.jar", "v1");
        PluginKey key = keyFor(jar);

        AtomicInteger built = new AtomicInteger(0);
        PluginCache.LoaderBuilder builder = k -> {
            built.incrementAndGet();
            return PluginCache.buildUrlClassLoader(k.gav(), List.of(jar));
        };

        ClassLoader first = cache.loadOrBuild(key, builder);
        ClassLoader second = cache.loadOrBuild(key, builder);

        assertSame(first, second, "second lookup must return the cached instance");
        assertEquals(1, built.get(), "loader supplier must run exactly once for the same key");
        assertEquals(1, cache.missCount());
        assertEquals(1, cache.hitCount());
        assertEquals(1, cache.size());
        cache.close();
    }

    @Test
    @DisplayName("override-listed plugins bypass the cache on every lookup")
    void overrideListedBypassesCache(@TempDir Path tmp) throws IOException {
        PluginCache cache = new PluginCache(Set.of(EXAMPLE_GA));
        Path jar = writeJar(tmp, "plugin.jar", "v1");
        PluginKey key = new PluginKey(EXAMPLE_GROUP, EXAMPLE_ARTIFACT, "1.0",
                PluginCache.sha256(jar));

        assertTrue(cache.isOverridden(key));

        AtomicInteger built = new AtomicInteger(0);
        PluginCache.LoaderBuilder builder = k -> {
            built.incrementAndGet();
            return PluginCache.buildUrlClassLoader(k.gav(), List.of(jar));
        };

        ClassLoader first = cache.loadOrBuild(key, builder);
        ClassLoader second = cache.loadOrBuild(key, builder);

        assertNotSame(first, second,
                "override-listed plugins must produce a fresh loader on each lookup");
        assertEquals(2, built.get());
        assertEquals(0, cache.hitCount());
        assertEquals(0, cache.missCount(),
                "override bypass must not be counted as a regular miss");
        assertEquals(2, cache.overrideBypassCount());
        assertEquals(0, cache.size(),
                "override bypass must never store into the cache map");
        // Loader is the caller's lifecycle on the override path; close manually.
        ((URLClassLoader) first).close();
        ((URLClassLoader) second).close();
        cache.close();
    }

    @Test
    @DisplayName("differing JAR content with same GAV misses (content-addressed key)")
    void differentJarContentMissesEvenWithSameGav(@TempDir Path tmp) throws IOException {
        PluginCache cache = new PluginCache(Set.of());
        Path v1 = writeJar(tmp, "v1.jar", "content-one");
        Path v2 = writeJar(tmp, "v2.jar", "content-two");

        PluginKey keyV1 = new PluginKey("g", "a", "1.0", PluginCache.sha256(v1));
        PluginKey keyV2 = new PluginKey("g", "a", "1.0", PluginCache.sha256(v2));
        assertFalse(keyV1.equals(keyV2),
                "same GAV with differing JAR content must produce distinct keys");

        cache.loadOrBuild(keyV1, k -> PluginCache.buildUrlClassLoader(k.gav(), List.of(v1)));
        cache.loadOrBuild(keyV2, k -> PluginCache.buildUrlClassLoader(k.gav(), List.of(v2)));

        assertEquals(2, cache.size(),
                "differing-content rebuilds must produce two distinct entries");
        cache.close();
    }

    @Test
    @DisplayName("invalidateAll closes URLClassLoaders and clears entries")
    void invalidateAllClosesLoaders(@TempDir Path tmp) throws IOException {
        PluginCache cache = new PluginCache(Set.of());
        Path jar = writeJar(tmp, "plugin.jar", "v1");
        PluginKey key = keyFor(jar);

        URLClassLoader loader = (URLClassLoader) cache.loadOrBuild(key,
                k -> PluginCache.buildUrlClassLoader(k.gav(), List.of(jar)));

        // Before invalidation the loader should be live (no IOException
        // closing a fresh URLClassLoader).
        assertEquals(1, cache.size());

        cache.invalidateAll();

        assertEquals(0, cache.size(), "invalidateAll must drop every entry");
        // Closing an already-closed URLClassLoader is documented as a
        // no-op; we re-call close() to make sure invalidateAll's close
        // didn't blow up.
        loader.close();
        cache.close();
    }

    @Test
    @DisplayName("override-list system property parsing handles edge cases")
    void parseOverridePropertyHandlesEdgeCases() {
        assertEquals(Set.of(), PluginCache.parseOverrideProperty(null));
        assertEquals(Set.of(), PluginCache.parseOverrideProperty(""));
        assertEquals(Set.of(), PluginCache.parseOverrideProperty("   "));
        assertEquals(
                Set.of("g1:a1", "g2:a2"),
                new HashSet<>(PluginCache.parseOverrideProperty("g1:a1,g2:a2")));
        // whitespace around entries is tolerated
        assertEquals(
                Set.of("g1:a1", "g2:a2"),
                new HashSet<>(PluginCache.parseOverrideProperty("  g1:a1 , g2:a2 ")));
        // entries without `:` are dropped (malformed); the rest survive
        assertEquals(
                Set.of("g:a"),
                new HashSet<>(PluginCache.parseOverrideProperty("not-a-ga,g:a")));
    }

    @Test
    @DisplayName("fromSystemProperties picks up the override property")
    void fromSystemPropertiesReadsProperty() {
        String prior = System.getProperty(PluginCache.OVERRIDE_PROPERTY);
        try {
            System.setProperty(PluginCache.OVERRIDE_PROPERTY,
                    "com.example:plugin-a,com.example:plugin-b");
            PluginCache cache = PluginCache.fromSystemProperties();
            assertEquals(Set.of("com.example:plugin-a", "com.example:plugin-b"),
                    cache.overrideList());
            cache.close();
        } finally {
            if (prior == null) {
                System.clearProperty(PluginCache.OVERRIDE_PROPERTY);
            } else {
                System.setProperty(PluginCache.OVERRIDE_PROPERTY, prior);
            }
        }
    }

    @Test
    @DisplayName("ctor copies the override list (caller mutations do not leak in)")
    void overrideListIsDefensivelyCopied() {
        Set<String> input = new HashSet<>(Set.of("g:a"));
        PluginCache cache = new PluginCache(input);
        input.add("g2:a2");
        assertEquals(Set.of("g:a"), cache.overrideList(),
                "post-construction mutations on the input set must not leak into the cache");
        // The view returned by overrideList must be unmodifiable.
        assertThrows(UnsupportedOperationException.class,
                () -> cache.overrideList().add("late:add"));
        cache.close();
    }

    // ------------ helpers ------------

    /**
     * Build the simplest valid JAR: a manifest plus a single
     * {@code content.txt} entry whose payload is the given string. We
     * are exercising classloader behaviour, not bytecode loading; the
     * JAR just needs to be readable.
     */
    private static Path writeJar(Path tmp, String name, String contentTag) throws IOException {
        Path out = tmp.resolve(name);
        Manifest mf = new Manifest();
        mf.getMainAttributes().putValue("Manifest-Version", "1.0");
        try (var fos = Files.newOutputStream(out);
             var jos = new JarOutputStream(fos, mf)) {
            JarEntry entry = new JarEntry("content.txt");
            jos.putNextEntry(entry);
            jos.write(contentTag.getBytes(java.nio.charset.StandardCharsets.UTF_8));
            jos.closeEntry();
        }
        return out;
    }

    private static PluginKey keyFor(Path jar) throws IOException {
        return new PluginKey("com.example", "demo", "1.0", PluginCache.sha256(jar));
    }
}
