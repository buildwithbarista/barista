// Maven 4 embedding spike for M4.0 T2 (Q13 / D18).
//
// This single-file spike answers two questions:
//   1. Can a JVM library embed Maven 4.0.x in-process and drive a real
//      lifecycle phase (compile) end-to-end?
//   2. How does that compare in wall-clock time to forking the bundled
//      mvn binary against the same fixture?
//
// We exercise three modes against the same fixture:
//   * EMBED-COLD  — first invocation in a fresh JVM via MavenCling.main(...)
//                   (the same path bin/mvn drives, but in-process).
//   * EMBED-WARM  — second invocation in the same JVM, classworlds/Plexus
//                   container already loaded. Approximates the steady-state
//                   barback daemon path.
//   * SUBPROC     — Runtime.exec("mvn compile") against the same fixture,
//                   using the extracted Maven 4 distribution (NOT the system
//                   mvn from asdf). Each run is a fresh JVM, matching the
//                   "fork per build" model of any subprocess-fallback path.
//
// Why MavenCling and not org.apache.maven.api.cli.Invoker directly?
//   Booting Invoker by hand requires reproducing the classworlds dance done
//   by ClingSupport.run(): wiring the core ClassRealm from plexus-core,
//   discovering CoreExtensionEntry, building the Lookup/MessageBuilderFactory
//   pair, etc. Maven exposes that wiring through MavenCling.main(args,
//   ClassWorld) which is exactly what bin/mvn calls. We use the same
//   entrypoint and explicitly construct the ClassWorld so the spike is
//   reproducible without reading classworlds.conf. ADR-008 records the
//   tradeoff.
//
// Compile: javac --release 17 -classpath "<maven-home>/boot/*:<maven-home>/lib/*" EmbedSpike.java
// Run:     java -classpath "<maven-home>/boot/*:<maven-home>/lib/*:." \
//              -Dmaven.home=<maven-home> \
//              -Dmaven.multiModuleProjectDirectory=<fixture-dir> \
//              -Dmaven.mainClass=org.apache.maven.cling.MavenCling \
//              --enable-native-access=ALL-UNNAMED \
//              EmbedSpike <maven-home> <fixture-dir> [iterations]
//
// Output: machine-parseable lines on stdout of the form
//   RESULT mode=<EMBED-COLD|EMBED-WARM|SUBPROC> exit=<int> wall_ms=<long>
// followed by a SUMMARY block and an OVERALL exit code (0 = all green).

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.OutputStream;
import java.io.PrintStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.stream.Stream;

import org.codehaus.plexus.classworlds.ClassWorld;
import org.codehaus.plexus.classworlds.realm.ClassRealm;

public final class EmbedSpike {

    private static final String FIXTURE_GOAL = "compile";

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: EmbedSpike <maven-home> <fixture-dir> [warm-iters]");
            System.exit(2);
        }
        Path mavenHome = Paths.get(args[0]).toAbsolutePath().normalize();
        Path fixture = Paths.get(args[1]).toAbsolutePath().normalize();
        int warmIters = args.length >= 3 ? Integer.parseInt(args[2]) : 1;

        require(Files.isDirectory(mavenHome), "maven-home does not exist: " + mavenHome);
        require(Files.isDirectory(fixture), "fixture does not exist: " + fixture);
        require(Files.isRegularFile(fixture.resolve("pom.xml")), "fixture has no pom.xml: " + fixture);

        // Clean target/ from the fixture so each invocation does real work.
        clean(fixture.resolve("target"));

        List<Result> results = new ArrayList<>();

        // --- EMBED-COLD: fresh in-process Maven invocation.
        Result embedCold = embedInvoke("EMBED-COLD", mavenHome, fixture);
        results.add(embedCold);
        require(embedCold.exit == 0, "EMBED-COLD did not exit 0 (got " + embedCold.exit + ")");

        // --- EMBED-WARM: subsequent invocations in the same JVM. We need
        // a clean target/ between runs so compile actually runs.
        for (int i = 0; i < warmIters; i++) {
            clean(fixture.resolve("target"));
            Result embedWarm = embedInvoke("EMBED-WARM", mavenHome, fixture);
            results.add(embedWarm);
            require(embedWarm.exit == 0, "EMBED-WARM iter " + i + " did not exit 0 (got " + embedWarm.exit + ")");
        }

        // --- SUBPROC: forked mvn against the same fixture, fresh JVM each run.
        clean(fixture.resolve("target"));
        Result subproc = subprocessInvoke("SUBPROC", mavenHome, fixture);
        results.add(subproc);
        require(subproc.exit == 0, "SUBPROC did not exit 0 (got " + subproc.exit + ")");

        for (Result r : results) {
            System.out.printf("RESULT mode=%s exit=%d wall_ms=%d%n", r.mode, r.exit, r.wallMs);
        }

        // Summary stats — embedded-warm avg vs subprocess avg.
        long embedColdMs = results.stream()
            .filter(r -> r.mode.equals("EMBED-COLD"))
            .mapToLong(r -> r.wallMs)
            .findFirst()
            .orElse(-1);
        long embedWarmMs = (long) results.stream()
            .filter(r -> r.mode.equals("EMBED-WARM"))
            .mapToLong(r -> r.wallMs)
            .average()
            .orElse(-1);
        long subprocMs = results.stream()
            .filter(r -> r.mode.equals("SUBPROC"))
            .mapToLong(r -> r.wallMs)
            .findFirst()
            .orElse(-1);

        System.out.println("---");
        System.out.printf("SUMMARY embed_cold_ms=%d embed_warm_avg_ms=%d subproc_ms=%d%n",
            embedColdMs, embedWarmMs, subprocMs);
        if (embedWarmMs > 0 && subprocMs > 0) {
            double ratio = (double) subprocMs / (double) embedWarmMs;
            System.out.printf("SUMMARY subproc_over_embed_warm=%.2fx%n", ratio);
        }
        System.out.println("OVERALL OK");
    }

    /**
     * Invokes Maven 4 in-process via MavenCling, the same entrypoint bin/mvn
     * uses. Builds a fresh ClassWorld containing plexus.core seeded with the
     * full lib/ and boot/ classpaths. Captures stdout/stderr so we can observe
     * the build result without it dominating spike output.
     */
    private static Result embedInvoke(String mode, Path mavenHome, Path fixture) throws Exception {
        // Maven 4 expects these two properties globally for path resolution.
        // ClingSupport reads maven.home eagerly via System.getProperty.
        System.setProperty("maven.home", mavenHome.toString());
        System.setProperty("maven.multiModuleProjectDirectory", fixture.toString());

        ClassWorld classWorld = buildClassWorld(mavenHome);

        // Capture child stdout/stderr so its noise doesn't bury our RESULT
        // lines, but write it to a per-run log for debugging.
        Path logDir = fixture.getParent().resolve("logs");
        Files.createDirectories(logDir);
        Path stdoutLog = logDir.resolve(mode.toLowerCase() + ".stdout.log");
        Path stderrLog = logDir.resolve(mode.toLowerCase() + ".stderr.log");

        String[] mvnArgs = new String[] { "-f", fixture.resolve("pom.xml").toString(), FIXTURE_GOAL, "-q" };

        int exit;
        long start = System.nanoTime();
        try (OutputStream outSink = Files.newOutputStream(stdoutLog);
             OutputStream errSink = Files.newOutputStream(stderrLog);
             PrintStream outPs = new PrintStream(outSink, true);
             PrintStream errPs = new PrintStream(errSink, true)) {

            // Reflectively call MavenCling.main(String[], ClassWorld,
            // InputStream, OutputStream, OutputStream) so the spike has no
            // compile-time dependency on the cli jar (it's only on the
            // classpath at runtime). This also makes the entrypoint
            // contract visible at the call site.
            Class<?> clingClass = classWorld.getClassRealm("plexus.core")
                .loadClass("org.apache.maven.cling.MavenCling");
            java.lang.reflect.Method mainMethod = clingClass.getMethod(
                "main",
                String[].class,
                ClassWorld.class,
                java.io.InputStream.class,
                OutputStream.class,
                OutputStream.class);
            Object result = mainMethod.invoke(null, mvnArgs, classWorld, System.in, outPs, errPs);
            exit = ((Integer) result).intValue();
        }
        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        return new Result(mode, exit, wallMs);
    }

    /**
     * Forks "<maven-home>/bin/mvn compile" against the fixture. Mirrors the
     * subprocess-fallback path that the daemon would use if the embedded API
     * proves unstable. Fresh JVM every call by definition.
     */
    private static Result subprocessInvoke(String mode, Path mavenHome, Path fixture) throws IOException, InterruptedException {
        Path mvnBin = mavenHome.resolve("bin/mvn");
        require(Files.isExecutable(mvnBin), "mvn is not executable: " + mvnBin);

        ProcessBuilder pb = new ProcessBuilder(
            mvnBin.toString(),
            "-f", fixture.resolve("pom.xml").toString(),
            FIXTURE_GOAL,
            "-q");
        pb.redirectErrorStream(true);

        // Redirect subprocess output to a log file so its noise doesn't bury
        // our RESULT lines.
        Path logDir = fixture.getParent().resolve("logs");
        Files.createDirectories(logDir);
        Path log = logDir.resolve(mode.toLowerCase() + ".log");
        pb.redirectOutput(log.toFile());

        // Ensure the subprocess uses the SAME JDK we're already on, not whatever
        // JAVA_HOME the shell happened to inherit. asdf-managed JDK selection
        // is handled at script level; this is just a belt-and-suspenders.
        pb.environment().put("JAVA_HOME", System.getProperty("java.home"));

        long start = System.nanoTime();
        Process p = pb.start();
        int exit = p.waitFor();
        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        return new Result(mode, exit, wallMs);
    }

    /**
     * Builds the same ClassWorld layout that classworlds.conf builds for the
     * bin/mvn launcher: a single `plexus.core` realm seeded with every JAR
     * under boot/ and lib/ (recursively). We skip the optional ext/* loads
     * since the fixture doesn't use them; ADR-008 captures that as a follow-up
     * if barback needs core extensions.
     */
    private static ClassWorld buildClassWorld(Path mavenHome) throws Exception {
        ClassWorld world = new ClassWorld("plexus.core", Thread.currentThread().getContextClassLoader());
        ClassRealm core = world.getClassRealm("plexus.core");

        // logging/ must be loaded before lib/*.jar so SLF4J bindings resolve.
        addJarsFromDir(core, mavenHome.resolve("conf/logging"));
        addJarsFromDir(core, mavenHome.resolve("lib"));
        addJarsFromDir(core, mavenHome.resolve("boot"));

        return core.getWorld();
    }

    private static void addJarsFromDir(ClassRealm realm, Path dir) throws IOException {
        if (!Files.isDirectory(dir)) return;
        try (Stream<Path> stream = Files.list(dir)) {
            List<Path> jars = stream
                .filter(p -> p.toString().endsWith(".jar"))
                .sorted(Comparator.naturalOrder())
                .toList();
            for (Path jar : jars) {
                realm.addURL(jar.toUri().toURL());
            }
        }
    }

    private static void clean(Path dir) throws IOException {
        if (!Files.exists(dir)) return;
        try (Stream<Path> stream = Files.walk(dir)) {
            stream.sorted(Comparator.reverseOrder()).forEach(p -> {
                try { Files.deleteIfExists(p); } catch (IOException ignored) {}
            });
        }
    }

    private static void require(boolean cond, String msg) {
        if (!cond) {
            System.err.println("FATAL: " + msg);
            System.exit(2);
        }
    }

    private record Result(String mode, int exit, long wallMs) {}
}
