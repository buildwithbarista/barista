// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.conformance;

import java.io.IOException;

/**
 * Command-line entry point for {@link EchoPipeClient} used by the
 * cross-language Rust&harr;Java named-pipe conformance harness on
 * Windows.
 *
 * <h2>Usage</h2>
 *
 * <pre>
 *   java -cp &lt;classpath&gt; com.bluminal.barista.barback.conformance.EchoPipeClientCli
 *       --pipe \\.\pipe\barista-ipc-test-foo-123-456
 * </pre>
 *
 * <h2>Lifecycle contract</h2>
 *
 * <p>The Rust test harness:
 *
 * <ol>
 *   <li>binds the named pipe (via
 *       {@code NamedPipeTransport::bind_secure} or plain
 *       {@code ServerOptions::create}),</li>
 *   <li>spawns this process with {@code --pipe &lt;name&gt;},</li>
 *   <li>awaits {@code NamedPipeServer::connect()} (which resolves
 *       when our {@code RandomAccessFile} open lands on the kernel
 *       side),</li>
 *   <li>reads {@code READY &lt;pipe&gt;} from our stdout to confirm
 *       we successfully opened the pipe,</li>
 *   <li>drives the test &mdash; sends one or more envelopes and
 *       expects each to come back via the echo loop.</li>
 * </ol>
 *
 * <p>The process exits when:
 *
 * <ul>
 *   <li>the echo loop returns (server closed the connection or sent
 *       an oversized frame), OR</li>
 *   <li>{@code stdin} closes (the parent process terminated &mdash;
 *       we tear down so we don't leak a JVM on test-failure aborts).</li>
 * </ul>
 *
 * <p>Stdout is reserved for the {@code READY} handshake; stderr
 * carries diagnostics. The Rust harness streams stderr to a log file
 * alongside the test artifacts so flakes have a paper trail.
 *
 * <h2>Why not annotations / a test framework?</h2>
 *
 * <p>This is deliberately a plain {@code main} class, not a JUnit
 * test: the conformance harness needs to invoke it as a subprocess
 * from Rust, and Maven Surefire's test runner is the wrong fit for
 * that (it would discover the class, run its lifecycle, and exit). A
 * naked {@code main} keeps the spawn-from-Rust ceremony trivial. Same
 * pattern as {@link EchoServerCli}.
 */
public final class EchoPipeClientCli {

    private EchoPipeClientCli() {
        // utility class
    }

    public static void main(String[] args) throws IOException {
        String pipePath = parsePipeArg(args);

        // Background watchdog: if the parent process (Rust test
        // harness) dies, our stdin closes (EOF on read). Tear down
        // the JVM in that case so we never leak after a test crash.
        Thread watchdog = new Thread(EchoPipeClientCli::watchStdinForParentDeath,
                "echo-pipe-client-stdin-watchdog");
        watchdog.setDaemon(true);
        watchdog.start();

        // Open the pipe BEFORE printing READY so the parent only sees
        // READY once we have a live handle. If the open fails, we
        // exit non-zero and the parent's `wait_for_ready` panic
        // surfaces the JVM stderr (which carries the IOException
        // stack trace).
        try (EchoPipeClient client = new EchoPipeClient(pipePath)) {
            // Signal readiness on stdout. The Rust harness reads
            // stdout line-by-line and waits for this exact prefix;
            // flush explicitly so the line is not buffered across
            // the subprocess pipe.
            System.out.println("READY " + pipePath);
            System.out.flush();

            try {
                client.serve();
            } catch (IOException e) {
                // Log to stderr for the Rust harness's debug log;
                // exit with non-zero so the test surfaces the
                // failure if the harness isn't expecting it.
                System.err.println("[echo-pipe-client] serve loop failed: " + e);
                e.printStackTrace(System.err);
                System.exit(2);
            }
        } catch (IOException e) {
            System.err.println("[echo-pipe-client] failed to open pipe " + pipePath + ": " + e);
            e.printStackTrace(System.err);
            System.exit(3);
        }
    }

    private static String parsePipeArg(String[] args) {
        for (int i = 0; i < args.length; i++) {
            if ("--pipe".equals(args[i])) {
                if (i + 1 >= args.length) {
                    System.err.println("--pipe requires a path argument");
                    System.exit(64); // EX_USAGE
                }
                return args[i + 1];
            }
        }
        System.err.println("usage: EchoPipeClientCli --pipe <\\\\.\\pipe\\name>");
        System.exit(64); // EX_USAGE
        throw new IllegalStateException("unreachable");
    }

    /**
     * Watch stdin for EOF. The Rust harness keeps stdin open for the
     * lifetime of the test; when the parent dies (or closes stdin
     * deliberately), {@link System#in#read()} returns -1 and we
     * terminate the JVM. This keeps zombie echo-pipe-client
     * processes from piling up on the test runner.
     */
    @SuppressWarnings("PMD.DoNotCallSystemExit") // intentional &mdash; subprocess cleanup
    private static void watchStdinForParentDeath() {
        try {
            int b;
            // Drain stdin; we don't actually consume commands &mdash;
            // the only purpose is to observe EOF.
            //noinspection StatementWithEmptyBody
            while ((b = System.in.read()) != -1) {
                // discard
            }
        } catch (IOException e) {
            // Parent's pipe broke. Same outcome as EOF.
        }
        // Use halt() rather than exit() to bypass shutdown hooks
        // &mdash; we've already detected the parent is gone, no
        // need to run finalizers that might block.
        Runtime.getRuntime().halt(0);
    }
}
