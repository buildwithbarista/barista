/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.bench.util;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;

import com.bluminal.barista.barback.classloader.PluginCache;
import com.bluminal.barista.barback.classloader.PluginKey;

/**
 * The 5-plugin workload manifest the {@link PluginCache} benches drive.
 *
 * <p>Mirrors {@code barback/spike/m40-t1/sample-project/pom.xml} and
 * {@code PluginCacheSpeedupIT.WORKLOAD} verbatim so the JMH numbers
 * line up with the integration-test speedup measurement on the same
 * input. Shared between {@code PluginCacheHitBench} (warm-cache lookup
 * latency) and {@code PluginCacheMissBench} (uncached classloader
 * build cost).
 *
 * <p>Plugins are resolved against the user's local
 * {@code ~/.m2/repository}; if any plugin is missing, {@link #resolve()}
 * returns a partial list and the caller throws (benches treat an
 * incomplete manifest as a setup error so JMH reports it rather than
 * silently benchmarking a subset).
 */
public final class WorkloadManifest {

    /** Expected number of plugins in the manifest. */
    public static final int PLUGIN_COUNT = 5;

    /**
     * The 5-plugin manifest. Group:Artifact:Version triples mirror
     * {@code barback/spike/m40-t1/sample-project/pom.xml}.
     */
    private static final List<PluginSpec> SPECS = List.of(
            new PluginSpec("org.apache.maven.plugins", "maven-resources-plugin", "3.3.1"),
            new PluginSpec("org.apache.maven.plugins", "maven-compiler-plugin", "3.13.0"),
            new PluginSpec("org.apache.maven.plugins", "maven-surefire-plugin", "3.5.5"),
            new PluginSpec("org.apache.maven.plugins", "maven-jar-plugin", "3.4.2"),
            new PluginSpec("org.apache.maven.plugins", "maven-install-plugin", "3.1.4"));

    private WorkloadManifest() {
        // utility
    }

    /**
     * Resolve each plugin in the manifest to its main JAR under the
     * user's local {@code ~/.m2/repository}. Returns only the plugins
     * we could find &mdash; callers compare against
     * {@link #PLUGIN_COUNT} to decide whether to fail bench setup.
     */
    public static List<ResolvedPlugin> resolve() throws IOException {
        Path m2 = Path.of(System.getProperty("user.home"), ".m2", "repository");
        List<ResolvedPlugin> out = new ArrayList<>(SPECS.size());
        for (PluginSpec spec : SPECS) {
            Path jar = m2.resolve(spec.groupId().replace('.', '/'))
                    .resolve(spec.artifactId())
                    .resolve(spec.version())
                    .resolve(spec.artifactId() + "-" + spec.version() + ".jar");
            if (!Files.isRegularFile(jar)) {
                continue;
            }
            PluginKey key = new PluginKey(spec.groupId(), spec.artifactId(),
                    spec.version(), PluginCache.sha256(jar));
            out.add(new ResolvedPlugin(spec, jar, key));
        }
        return out;
    }

    /**
     * Override-list view of the manifest: the 5
     * {@code groupId:artifactId} (GA) strings, no versions. Pass this
     * to a {@link PluginCache}'s constructor to force every plugin in
     * the manifest to bypass the cache &mdash; the miss-bench harness.
     */
    public static Set<String> gaSet() {
        return Set.of(
                "org.apache.maven.plugins:maven-resources-plugin",
                "org.apache.maven.plugins:maven-compiler-plugin",
                "org.apache.maven.plugins:maven-surefire-plugin",
                "org.apache.maven.plugins:maven-jar-plugin",
                "org.apache.maven.plugins:maven-install-plugin");
    }

    /** GAV pin for a plugin in the workload manifest. */
    public record PluginSpec(String groupId, String artifactId, String version) {
        /** {@code groupId:artifactId:version} string. */
        public String gav() {
            return groupId + ":" + artifactId + ":" + version;
        }
    }

    /** A resolved plugin: spec + on-disk JAR + content-hashed cache key. */
    public record ResolvedPlugin(PluginSpec spec, Path jar, PluginKey key) { }
}
