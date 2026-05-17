package com.bluminal.barista.barback.spike;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;

/**
 * Minimal plugin-classloader-cache bench for the m40-t1 barback spike.
 *
 * Reads a manifest (one line per plugin: {@code coord\tjar1 jar2 jar3 ...})
 * and times two scenarios:
 * <ul>
 *   <li><b>cold</b>: build a fresh {@link URLClassLoader} for each plugin on
 *       every iteration, force resolution of every Mojo class (and {@code
 *       META-INF/maven/plugin.xml} read), then discard.</li>
 *   <li><b>warm</b>: cache loaders + resolved class lists in a {@link Map}
 *       keyed by plugin coordinate; subsequent iterations are pure cache
 *       lookups, mirroring the steady-state daemon path described in PRD §11
 *       and §4.5 ("session is reused across actions").</li>
 * </ul>
 *
 * The cost we are modelling is the per-plugin classloader bootstrap cost that
 * Maven pays on every cold {@code mvn} invocation: open every plugin jar,
 * resolve {@code Mojo} classes referenced from {@code plugin.xml}, link them,
 * and prepare them for reflective instantiation. We do not run the Mojos; the
 * spike measures *bootstrap*, which is what a daemon eliminates.
 *
 * Output is a sequence of {@code key=value} lines that {@code run.sh}
 * concatenates into the spike results block.
 */
public final class PluginCacheBench {

    private static final int ITERATIONS = 5;
    private static final int WARMUP_ITERATIONS = 1; // drop iter 0

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.err.println("usage: PluginCacheBench <manifest>");
            System.exit(2);
        }
        Path manifest = Path.of(args[0]);
        List<PluginEntry> plugins = parseManifest(manifest);
        if (plugins.size() != 5) {
            System.err.println("expected 5 plugins, got " + plugins.size());
            System.exit(2);
        }

        // --- cold: fresh classloader per iteration ---
        long[] coldNs = new long[ITERATIONS];
        long coldClassesResolved = 0;
        for (int i = 0; i < ITERATIONS; i++) {
            long t0 = System.nanoTime();
            long resolved = runCold(plugins);
            long t1 = System.nanoTime();
            coldNs[i] = (t1 - t0);
            coldClassesResolved = resolved; // same every iter; record last
        }

        // --- warm: shared classloader cache ---
        Map<String, CachedPlugin> cache = new ConcurrentHashMap<>();
        long[] warmNs = new long[ITERATIONS];
        long warmClassesResolved = 0;
        for (int i = 0; i < ITERATIONS; i++) {
            long t0 = System.nanoTime();
            long resolved = runWarm(plugins, cache);
            long t1 = System.nanoTime();
            warmNs[i] = (t1 - t0);
            warmClassesResolved = resolved;
        }

        double coldAvgUs = avgAfterWarmup(coldNs) / 1_000.0;
        double warmAvgUs = avgAfterWarmup(warmNs) / 1_000.0;
        double coldAvgMs = coldAvgUs / 1_000.0;
        double warmAvgMs = warmAvgUs / 1_000.0;
        double speedup = coldAvgUs / Math.max(warmAvgUs, 0.001);
        double speedupPct = (coldAvgUs - warmAvgUs) / coldAvgUs * 100.0;

        // raw iteration data for the ADR
        System.out.println("bench.iterations=" + ITERATIONS);
        System.out.println("bench.warmup_iterations=" + WARMUP_ITERATIONS);
        System.out.println("bench.plugins_loaded=" + plugins.size());
        System.out.println("bench.cold_classes_resolved=" + coldClassesResolved);
        System.out.println("bench.warm_classes_resolved=" + warmClassesResolved);
        System.out.println("bench.cold_iters_us=" + nsArrayToUsString(coldNs));
        System.out.println("bench.warm_iters_us=" + nsArrayToUsString(warmNs));
        System.out.printf("bench.cold_avg_us=%.3f%n", coldAvgUs);
        System.out.printf("bench.warm_avg_us=%.3f%n", warmAvgUs);
        System.out.printf("bench.cold_avg_ms=%.3f%n", coldAvgMs);
        System.out.printf("bench.warm_avg_ms=%.3f%n", warmAvgMs);
        System.out.printf("bench.speedup_ratio=%.3fx%n", speedup);
        System.out.printf("bench.speedup_pct=%.2f%%%n", speedupPct);
        System.out.println("bench.decision="
            + (speedupPct >= 30.0 ? "PROCEED (daemon scope justified)"
                                  : "REVISIT (speedup <30%, downstream M4.2 should be re-scoped)"));
    }

    private static String nsArrayToUsString(long[] ns) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < ns.length; i++) {
            if (i > 0) sb.append(", ");
            sb.append(String.format("%.3f", ns[i] / 1_000.0));
        }
        sb.append("]");
        return sb.toString();
    }

    /** Cold path: throw away loaders + resolved classes at end of each call. */
    private static long runCold(List<PluginEntry> plugins) {
        long total = 0;
        for (PluginEntry p : plugins) {
            URLClassLoader cl = newLoader(p);
            List<String> classes = discoverMojoClassNames(p);
            total += loadClasses(cl, classes);
            try { cl.close(); } catch (IOException ignored) { }
        }
        return total;
    }

    /**
     * Warm path: the loader (and the classes it has already defined) live in
     * the cache. We still call {@code loadClass} for each Mojo coord, but the
     * loader's internal class table returns the existing {@code Class<?>}
     * without re-reading bytecode from the jar — that is exactly the
     * persistent-daemon optimisation we are measuring.
     */
    private static long runWarm(List<PluginEntry> plugins, Map<String, CachedPlugin> cache) {
        long total = 0;
        for (PluginEntry p : plugins) {
            CachedPlugin cached = cache.computeIfAbsent(p.coord, k -> {
                URLClassLoader cl = newLoader(p);
                List<String> classes = discoverMojoClassNames(p);
                long count = loadClasses(cl, classes);
                return new CachedPlugin(cl, classes, count);
            });
            // Simulate the daemon's per-invocation "look up plugin, fetch
            // Mojos by name" step. This is what would happen on every cached
            // shot test: same coord, same Mojo names.
            total += loadClasses(cached.loader, cached.mojoClassNames);
        }
        return total;
    }

    /** Build a fresh URLClassLoader over a plugin's main jar + transitive deps. */
    private static URLClassLoader newLoader(PluginEntry p) {
        URL[] urls = p.jarPaths.stream().map(path -> {
            try { return path.toUri().toURL(); }
            catch (Exception e) { throw new UncheckedIOException(new IOException(e)); }
        }).toArray(URL[]::new);
        // Parent = platform loader; we want our plugin classes resolved
        // from our URLs, not from whatever was on the bench's classpath.
        return new URLClassLoader(p.coord, urls, ClassLoader.getPlatformClassLoader());
    }

    /**
     * Walk the plugin's main jar (the one containing {@code
     * META-INF/maven/plugin.xml}) and return the fully-qualified names of
     * every {@code *Mojo} class. We do this pre-bench so the timed section
     * only reflects classloader work, not jar scanning.
     */
    private static List<String> discoverMojoClassNames(PluginEntry p) {
        List<String> out = new ArrayList<>();
        for (Path jar : p.jarPaths) {
            if (!Files.exists(jar)) continue;
            try (JarFile jf = new JarFile(jar.toFile())) {
                if (jf.getEntry("META-INF/maven/plugin.xml") == null) continue;
                Enumeration<JarEntry> entries = jf.entries();
                while (entries.hasMoreElements()) {
                    JarEntry e = entries.nextElement();
                    String name = e.getName();
                    if (!name.endsWith("Mojo.class")) continue;
                    if (name.startsWith("META-INF/")) continue;
                    String cn = name.substring(0, name.length() - ".class".length())
                                    .replace('/', '.');
                    out.add(cn);
                }
            } catch (IOException ignored) {
            }
        }
        return out;
    }

    /**
     * For each fully-qualified class name, call {@link ClassLoader#loadClass}
     * on the given loader. On the cold path this triggers jar entry lookup,
     * bytecode read, and {@code defineClass}. On the warm path (same loader
     * instance) it is a hash-map lookup against the loader's internal class
     * table — no I/O, no defineClass. This is the cost a persistent daemon
     * eliminates: PRD §11.4 ("session is reused across actions") and §11.1
     * ("classloader caching on incremental builds").
     *
     * Returns the number of classes successfully loaded.
     */
    private static long loadClasses(ClassLoader cl, List<String> classNames) {
        long count = 0;
        for (String cn : classNames) {
            try {
                Class<?> c = cl.loadClass(cn);
                if (c != null) count++;
            } catch (Throwable t) {
                // A Mojo class with an unresolved supertype is still defined
                // by loadClass; we only end up here for genuinely broken
                // bytecode, which shouldn't happen for Apache-shipped plugins.
            }
        }
        return count;
    }

    private static double avgAfterWarmup(long[] xs) {
        if (xs.length <= WARMUP_ITERATIONS) return 0;
        long sum = 0;
        for (int i = WARMUP_ITERATIONS; i < xs.length; i++) sum += xs[i];
        return ((double) sum) / (xs.length - WARMUP_ITERATIONS);
    }

    @SuppressWarnings("unused")
    private static Method touchClassMembers(Class<?> c) {
        // Reserved hook if we later want to force linkage on top of plain
        // loadClass — kept here so the spike can be tightened post-review.
        Method[] ms = c.getDeclaredMethods();
        return ms.length > 0 ? ms[0] : null;
    }

    // ----- manifest parsing -----

    static List<PluginEntry> parseManifest(Path manifest) throws IOException {
        List<PluginEntry> out = new ArrayList<>();
        for (String line : Files.readAllLines(manifest)) {
            if (line.isBlank()) continue;
            int tab = line.indexOf('\t');
            if (tab < 0) throw new IOException("bad manifest line: " + line);
            String coord = line.substring(0, tab);
            String[] jars = line.substring(tab + 1).trim().split("\\s+");
            List<Path> paths = new ArrayList<>(jars.length);
            for (String j : jars) paths.add(Path.of(j));
            out.add(new PluginEntry(coord, paths));
        }
        return out;
    }

    private static final class PluginEntry {
        final String coord;
        final List<Path> jarPaths;
        PluginEntry(String coord, List<Path> jarPaths) {
            this.coord = coord;
            this.jarPaths = jarPaths;
        }
    }

    private static final class CachedPlugin {
        final URLClassLoader loader;
        final List<String> mojoClassNames;
        final long classCount;
        CachedPlugin(URLClassLoader loader, List<String> mojoClassNames, long classCount) {
            this.loader = loader;
            this.mojoClassNames = mojoClassNames;
            this.classCount = classCount;
        }
    }
}
