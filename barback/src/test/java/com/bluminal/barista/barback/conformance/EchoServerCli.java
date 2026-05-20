// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.conformance;

import java.io.IOException;
import java.nio.file.Path;
import java.nio.file.Paths;

/**
 * Command-line entry point for the {@link EchoServer} used by the
 * cross-language Rust&harr;Java conformance harness.
 *
 * <h2>Usage</h2>
 *
 * <pre>
 *   java -cp &lt;classpath&gt; com.bluminal.barista.barback.conformance.EchoServerCli
 *       --socket /path/to/conformance.sock
 * </pre>
 *
 * <h2>Lifecycle contract</h2>
 *
 * <p>The Rust test harness spawns this process, waits for the
 * {@code READY &lt;path&gt;} line on stdout, then dials the socket as a
 * client and drives the test. The process exits when:
 *
 * <ul>
 *   <li>the echo loop returns (client closed the connection or sent an
 *       oversized frame), OR</li>
 *   <li>{@code stdin} closes (the parent process terminated &mdash; we tear
 *       down so we don't leak a JVM on test-failure aborts).</li>
 * </ul>
 *
 * <p>Stdout is reserved for the {@code READY} handshake plus structured
 * status lines; stderr carries diagnostics. The Rust harness streams
 * stderr to a log file alongside the test artifacts so flakes have a
 * paper trail.
 *
 * <h2>Why not annotations / a test framework?</h2>
 *
 * <p>This is deliberately a plain {@code main} class, not a JUnit test:
 * the conformance harness needs to invoke it as a subprocess from
 * Rust, and Maven Surefire&apos;s test runner is the wrong fit for that
 * (it would discover the class, run its lifecycle, and exit). A naked
 * {@code main} keeps the spawn-from-Rust ceremony trivial.
 */
public final class EchoServerCli {

    private EchoServerCli() {
        // utility class
    }

    public static void main(String[] args) throws IOException {
        Path socketPath = parseSocketArg(args);

        // Background watchdog: if the parent process (Rust test harness)
        // dies, our stdin closes (EOF on read). Tear down the JVM in
        // that case so we never leak after a test crash.
        Thread watchdog = new Thread(EchoServerCli::watchStdinForParentDeath,
                "echo-server-stdin-watchdog");
        watchdog.setDaemon(true);
        watchdog.start();

        try (EchoServer server = new EchoServer(socketPath)) {
            // Signal readiness on stdout. The Rust harness reads stdout
            // line-by-line and waits for this exact prefix; flush
            // explicitly so the line is not buffered across the
            // subprocess pipe.
            System.out.println("READY " + socketPath.toAbsolutePath());
            System.out.flush();

            try {
                server.serve();
            } catch (IOException e) {
                // Log to stderr for the Rust harness's debug log; exit
                // with non-zero so the test surfaces the failure if
                // the harness isn't expecting it.
                System.err.println("[echo-server] serve loop failed: " + e);
                e.printStackTrace(System.err);
                System.exit(2);
            }
        }
    }

    private static Path parseSocketArg(String[] args) {
        for (int i = 0; i < args.length; i++) {
            if ("--socket".equals(args[i])) {
                if (i + 1 >= args.length) {
                    System.err.println("--socket requires a path argument");
                    System.exit(64); // EX_USAGE
                }
                return Paths.get(args[i + 1]);
            }
        }
        System.err.println("usage: EchoServerCli --socket <path>");
        System.exit(64); // EX_USAGE
        throw new IllegalStateException("unreachable");
    }

    /**
     * Watch stdin for EOF. The Rust harness keeps stdin open for the
     * lifetime of the test; when the parent dies (or closes stdin
     * deliberately), {@link System#in#read()} returns -1 and we
     * terminate the JVM. This keeps zombie echo-server processes from
     * piling up on the test runner.
     */
    @SuppressWarnings("PMD.DoNotCallSystemExit") // intentional &mdash; subprocess cleanup
    private static void watchStdinForParentDeath() {
        try {
            int b;
            // Drain stdin; we don't actually consume commands &mdash; the
            // only purpose is to observe EOF.
            //noinspection StatementWithEmptyBody
            while ((b = System.in.read()) != -1) {
                // discard
            }
        } catch (IOException e) {
            // Parent's pipe broke. Same outcome as EOF.
        }
        // Use halt() rather than exit() to bypass shutdown hooks &mdash;
        // we've already detected the parent is gone, no need to run
        // finalizers that might block.
        Runtime.getRuntime().halt(0);
    }
}
