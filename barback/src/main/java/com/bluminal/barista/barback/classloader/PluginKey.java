// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.classloader;

import java.util.Objects;

/**
 * Identity of a plugin entry in the {@link PluginCache}.
 *
 * <p>A {@code PluginKey} is the pair
 * <code>(groupId:artifactId:version, jarSha256)</code>:
 *
 * <ul>
 *   <li>The {@code GAV} (Maven coordinate) names the plugin in the
 *       canonical Maven shape; two requests asking for the same
 *       {@code groupId:artifactId:version} must resolve to the same
 *       cached classloader entry.</li>
 *   <li>The {@code jarSha256} is the SHA-256 of the plugin's main JAR
 *       file bytes (the artifact carrying
 *       {@code META-INF/maven/plugin.xml}). Pinning to the on-disk
 *       bytes &mdash; not just the GAV &mdash; makes {@code SNAPSHOT}
 *       rebuilds safe: if a user re-installs
 *       {@code mycorp:plugin:1.0-SNAPSHOT} between two daemon actions,
 *       the new JAR hashes differently, the cache misses, and a fresh
 *       loader is built. The GAV alone would silently serve the stale
 *       loader.</li>
 * </ul>
 *
 * <p>The plugin's <em>dependency closure</em> is intentionally not part
 * of the key. The closure is a deterministic function of the GAV
 * (Maven's dependency resolution algorithm), so once GAV is pinned and
 * the main JAR's bytes are pinned, the URLs in the realized
 * {@link java.net.URLClassLoader} are already uniquely determined. We
 * verified this assumption in the M4.0 spike: across five sequential
 * resolutions of the same GAV the bag of resolved dependency JAR paths
 * was identical.
 *
 * <p>Hashing the closure too would cost an extra SHA-256 per dependency
 * (often hundreds for compiler / surefire) on every cache lookup, with
 * no behavioural benefit in v0.1. If a future Maven release ships a
 * non-deterministic resolver, the cache key gets a closure-hash field
 * and this javadoc gets updated.
 */
public final class PluginKey {

    private final String groupId;
    private final String artifactId;
    private final String version;
    private final String jarSha256;

    /**
     * Construct a plugin key. All fields are required; mismatched
     * casing on the hash is rejected to keep the recorded form
     * canonical (the {@link PluginCache#sha256(java.nio.file.Path)}
     * helper produces lowercase hex).
     */
    public PluginKey(String groupId, String artifactId, String version, String jarSha256) {
        this.groupId = requireNonEmpty(groupId, "groupId");
        this.artifactId = requireNonEmpty(artifactId, "artifactId");
        this.version = requireNonEmpty(version, "version");
        this.jarSha256 = requireSha256(jarSha256);
    }

    /** {@code groupId} component of the GAV. */
    public String groupId() {
        return groupId;
    }

    /** {@code artifactId} component of the GAV. */
    public String artifactId() {
        return artifactId;
    }

    /** {@code version} component of the GAV. */
    public String version() {
        return version;
    }

    /** Lowercase-hex SHA-256 of the plugin's main JAR. */
    public String jarSha256() {
        return jarSha256;
    }

    /**
     * {@code groupId:artifactId} (no version). Used to match against
     * the {@link PluginCache#overrideList()}, which holds version-less
     * GA entries because a misbehaving plugin is misbehaving at every
     * version.
     */
    public String ga() {
        return groupId + ":" + artifactId;
    }

    /**
     * {@code groupId:artifactId:version}. Useful in diagnostics where
     * the JAR hash would be noise.
     */
    public String gav() {
        return groupId + ":" + artifactId + ":" + version;
    }

    @Override
    public boolean equals(Object o) {
        if (this == o) return true;
        if (!(o instanceof PluginKey other)) return false;
        return groupId.equals(other.groupId)
                && artifactId.equals(other.artifactId)
                && version.equals(other.version)
                && jarSha256.equals(other.jarSha256);
    }

    @Override
    public int hashCode() {
        return Objects.hash(groupId, artifactId, version, jarSha256);
    }

    @Override
    public String toString() {
        // Truncate the hash in toString so log lines stay scannable.
        // The full hash is still recoverable via jarSha256() for
        // anyone debugging a cache-key mismatch.
        return "PluginKey{" + gav() + ", sha256=" + jarSha256.substring(0, 12) + "...}";
    }

    private static String requireNonEmpty(String value, String name) {
        Objects.requireNonNull(value, name);
        if (value.isEmpty()) {
            throw new IllegalArgumentException(name + " must not be empty");
        }
        return value;
    }

    private static String requireSha256(String hex) {
        Objects.requireNonNull(hex, "jarSha256");
        if (hex.length() != 64) {
            throw new IllegalArgumentException(
                    "jarSha256 must be 64 hex chars (SHA-256); got " + hex.length()
                            + " chars: " + hex);
        }
        for (int i = 0; i < hex.length(); i++) {
            char c = hex.charAt(i);
            boolean ok = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f');
            if (!ok) {
                throw new IllegalArgumentException(
                        "jarSha256 must be lowercase hex; offending char at index "
                                + i + ": " + hex);
            }
        }
        return hex;
    }
}
