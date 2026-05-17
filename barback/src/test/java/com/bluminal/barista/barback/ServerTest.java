/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;
import com.bluminal.barista.barback.proto.Envelope;
import com.bluminal.barista.barback.proto.Ping;
import com.google.protobuf.InvalidProtocolBufferException;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.DisabledOnOs;
import org.junit.jupiter.api.condition.EnabledOnOs;
import org.junit.jupiter.api.condition.OS;
import org.junit.jupiter.api.io.TempDir;

import java.io.IOException;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermission;
import java.time.Duration;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Integration tests for {@link Server}.
 *
 * <p>Each test boots a real {@link Server} on a temporary socket path,
 * speaks the length-prefixed protobuf wire format against it from a
 * thin in-process client, and tears the server down via
 * {@link Server#close()} / {@link Server#shutdown()} on exit. The
 * tests run on Unix only — the Windows server-side path is deferred
 * (see the {@code UnsupportedOperationException} surfaced by
 * {@link Server#start}).
 *
 * <h2>What this suite pins</h2>
 *
 * <ul>
 *   <li>Lifecycle + Ping/Pong round-trip on a single connection.</li>
 *   <li>The {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} {@link ActionResult}
 *       placeholder returned by the action-dispatch stub — downstream
 *       CLI tests can pin to this stable wire shape now and the
 *       placeholder will be replaced when the embedded Maven core
 *       lands in milestone 4.2 Task 3.</li>
 *   <li>Bad-connection tolerance: garbage bytes on one connection
 *       close that connection but do not crash the server; sibling
 *       connections still succeed.</li>
 *   <li>Concurrent connections: 16 clients each Ping the server,
 *       each receives a Pong — validates the virtual-threads default
 *       under modest fan-out.</li>
 *   <li>Socket inode permissions: the file mode is exactly
 *       {@code 0600} immediately after the bind completes.</li>
 *   <li>Windows guard: the {@link UnsupportedOperationException}
 *       fires with the canonical message documented in the class
 *       javadoc.</li>
 * </ul>
 */
class ServerTest {

    /**
     * Generous default for any "wait for the server to make progress"
     * synchronisation. Tests should complete well under this; the
     * timeout exists so a stuck server fails fast rather than hanging
     * the CI run.
     */
    private static final Duration DEFAULT_WAIT = Duration.ofSeconds(10);

    @TempDir
    Path tempDir;

    private Server server;

    @AfterEach
    void tearDown() throws Exception {
        if (server != null) {
            server.close();
            server = null;
        }
    }

    // ---------------------------------------------------------------
    // Lifecycle + Ping/Pong
    // ---------------------------------------------------------------

    @Test
    @DisplayName("Ping receives Pong and the server shuts down cleanly")
    @DisabledOnOs(OS.WINDOWS)
    void ping_roundtrip_thenShutdown() throws Exception {
        server = startOnTempPath();

        Envelope ping = Envelope.newBuilder()
                .setVersion(1)
                .setRequestId(11L)
                .setPing(Ping.newBuilder()
                        .setClient("barista-test")
                        .setSentAtUnixMicros(1L)
                        .build())
                .build();

        try (SocketChannel client = openClient(server.socketPath())) {
            writeEnvelope(client, ping);
            Envelope reply = readEnvelope(client);
            assertEquals(Envelope.BodyCase.PONG, reply.getBodyCase());
            assertEquals(11L, reply.getRequestId(),
                    "request_id must be echoed on the Pong reply");
            assertEquals(1, reply.getVersion(),
                    "Pong must carry the protocol version");
            assertFalse(reply.getPong().getDaemon().isBlank(),
                    "Pong daemon identifier must be populated");
            assertFalse(reply.getPong().getJdkVersion().isBlank(),
                    "Pong jdk_version must be populated from the running JVM");
        }

        server.shutdown();
        // awaitShutdown blocks on the latch the accept loop releases on
        // exit; failing this means the listener thread is wedged.
        assertTrue(awaitWithTimeout(server, DEFAULT_WAIT),
                "server.awaitShutdown() did not return within "
                        + DEFAULT_WAIT.toSeconds() + "s");
        assertFalse(Files.exists(server.socketPath()),
                "socket inode should be cleaned up after shutdown");
    }

    // ---------------------------------------------------------------
    // ActionRequest stub: BAR-DAEMON-NOT-YET-IMPLEMENTED placeholder
    // ---------------------------------------------------------------

    @Test
    @DisplayName("ActionRequest produces an ActionResult carrying BAR-DAEMON-NOT-YET-IMPLEMENTED")
    @DisabledOnOs(OS.WINDOWS)
    void actionRequest_dispatchStub_returnsNotYetImplemented() throws Exception {
        server = startOnTempPath();

        String actionId = "a-deadbeef";
        Envelope action = Envelope.newBuilder()
                .setVersion(1)
                .setRequestId(42L)
                .setAction(ActionRequest.newBuilder()
                        .setActionId(actionId)
                        .setMojoCoords("org.apache.maven.plugins:maven-compiler-plugin:3.13.0:compile")
                        .setProjectRoot("/tmp/example")
                        .setPomPath("/tmp/example/pom.xml")
                        .setMavenCompat("3")
                        .build())
                .build();

        try (SocketChannel client = openClient(server.socketPath())) {
            writeEnvelope(client, action);
            Envelope reply = readEnvelope(client);

            assertEquals(Envelope.BodyCase.RESULT, reply.getBodyCase());
            assertEquals(42L, reply.getRequestId());
            ActionResult result = reply.getResult();
            assertEquals(actionId, result.getActionId(),
                    "ActionResult.action_id must echo the request");
            assertEquals(ActionResult.Status.FAILURE, result.getStatus(),
                    "stub returns FAILURE so the CLI does not interpret the "
                            + "placeholder as a successful build");
            assertEquals(1, result.getExitCode(),
                    "non-zero exit code is the placeholder's signal that the "
                            + "dispatch was rejected, not that a mojo ran");
            assertEquals(Server.NOT_YET_IMPLEMENTED_CODE, result.getError().getCode(),
                    "embedded error code is the placeholder contract downstream "
                            + "tests pin to");
            assertEquals(actionId, result.getError().getActionId(),
                    "Error.action_id is populated so connection-level "
                            + "diagnostics survive without re-deriving from the result");
        }
    }

    // ---------------------------------------------------------------
    // Bad-connection tolerance
    // ---------------------------------------------------------------

    @Test
    @DisplayName("Garbage bytes on one connection do not crash the server; siblings still succeed")
    @DisabledOnOs(OS.WINDOWS)
    void badConnection_doesNotCrashServer() throws Exception {
        server = startOnTempPath();

        // First connection: send a deliberately corrupt length prefix
        // followed by random bytes. The server should close this
        // connection and continue accepting.
        try (SocketChannel bad = openClient(server.socketPath())) {
            ByteBuffer junk = ByteBuffer.allocate(64).order(ByteOrder.BIG_ENDIAN);
            // Frame-length prefix of 8 bytes (well below the cap so the
            // server will try to parse what follows as a real protobuf).
            junk.putInt(8);
            for (int i = 0; i < 60; i++) {
                junk.put((byte) (0xAA ^ i));
            }
            junk.flip();
            while (junk.hasRemaining()) {
                bad.write(junk);
            }
            // The server should close the socket after it fails to
            // parse the 8 bytes as an Envelope. We read until EOF or a
            // short delay so the test does not depend on race-y timing.
            ByteBuffer drain = ByteBuffer.allocate(16);
            long deadlineMillis = System.currentTimeMillis()
                    + DEFAULT_WAIT.toMillis();
            while (System.currentTimeMillis() < deadlineMillis) {
                int n = bad.read(drain);
                if (n < 0) {
                    break;
                }
                drain.clear();
            }
        }

        // Sibling connection: should still succeed after the bad one.
        Envelope ping = Envelope.newBuilder()
                .setVersion(1)
                .setRequestId(7L)
                .setPing(Ping.newBuilder().setClient("after-garbage").build())
                .build();
        try (SocketChannel good = openClient(server.socketPath())) {
            writeEnvelope(good, ping);
            Envelope reply = readEnvelope(good);
            assertEquals(Envelope.BodyCase.PONG, reply.getBodyCase(),
                    "sibling connection must still receive a Pong after a "
                            + "neighbouring connection sent garbage");
            assertEquals(7L, reply.getRequestId());
        }
    }

    // ---------------------------------------------------------------
    // Concurrent connections (virtual-threads default)
    // ---------------------------------------------------------------

    @Test
    @DisplayName("16 concurrent connections each Ping/Pong successfully")
    @DisabledOnOs(OS.WINDOWS)
    void concurrentConnections_allRoundTrip() throws Exception {
        server = startOnTempPath();
        int n = 16;

        ExecutorService clients = Executors.newFixedThreadPool(n);
        CountDownLatch ready = new CountDownLatch(n);
        CountDownLatch fire = new CountDownLatch(1);
        List<Future<Long>> futures = new ArrayList<>(n);
        try {
            for (int i = 0; i < n; i++) {
                final long requestId = 1000L + i;
                futures.add(clients.submit(() -> {
                    ready.countDown();
                    // Wait until every client thread is staged so the
                    // server sees a real fan-out rather than a serial
                    // trickle.
                    fire.await();
                    try (SocketChannel c = openClient(server.socketPath())) {
                        writeEnvelope(c, Envelope.newBuilder()
                                .setVersion(1)
                                .setRequestId(requestId)
                                .setPing(Ping.newBuilder()
                                        .setClient("concurrent-" + requestId)
                                        .build())
                                .build());
                        Envelope reply = readEnvelope(c);
                        if (reply.getBodyCase() != Envelope.BodyCase.PONG) {
                            throw new AssertionError(
                                    "expected PONG, got " + reply.getBodyCase());
                        }
                        return reply.getRequestId();
                    }
                }));
            }
            assertTrue(ready.await(DEFAULT_WAIT.toSeconds(), TimeUnit.SECONDS),
                    "client threads did not stage in time");
            fire.countDown();

            for (int i = 0; i < n; i++) {
                long got = futures.get(i).get(DEFAULT_WAIT.toSeconds(), TimeUnit.SECONDS);
                assertEquals(1000L + i, got,
                        "request_id must be echoed correctly under concurrency");
            }
        } finally {
            clients.shutdownNow();
            assertTrue(clients.awaitTermination(5, TimeUnit.SECONDS));
        }

        // Server must still be healthy after the burst — open one more
        // connection and confirm Ping/Pong still works.
        try (SocketChannel c = openClient(server.socketPath())) {
            writeEnvelope(c, Envelope.newBuilder()
                    .setVersion(1).setRequestId(9999L)
                    .setPing(Ping.newBuilder().build())
                    .build());
            Envelope reply = readEnvelope(c);
            assertEquals(Envelope.BodyCase.PONG, reply.getBodyCase());
            assertEquals(9999L, reply.getRequestId());
        }
    }

    // ---------------------------------------------------------------
    // 0600 socket permissions
    // ---------------------------------------------------------------

    @Test
    @DisplayName("Socket inode is chmod 0600 immediately after bind")
    @DisabledOnOs(OS.WINDOWS)
    void socketPerms_are0600AfterBind() throws Exception {
        server = startOnTempPath();
        Set<PosixFilePermission> got = Files.getPosixFilePermissions(server.socketPath());
        Set<PosixFilePermission> want = EnumSet.of(
                PosixFilePermission.OWNER_READ,
                PosixFilePermission.OWNER_WRITE);
        assertEquals(want, got,
                "socket inode must be 0600; got " + PosixFilePermissionsToString(got));
    }

    private static String PosixFilePermissionsToString(Set<PosixFilePermission> perms) {
        StringBuilder sb = new StringBuilder();
        sb.append(perms.contains(PosixFilePermission.OWNER_READ) ? 'r' : '-');
        sb.append(perms.contains(PosixFilePermission.OWNER_WRITE) ? 'w' : '-');
        sb.append(perms.contains(PosixFilePermission.OWNER_EXECUTE) ? 'x' : '-');
        sb.append(perms.contains(PosixFilePermission.GROUP_READ) ? 'r' : '-');
        sb.append(perms.contains(PosixFilePermission.GROUP_WRITE) ? 'w' : '-');
        sb.append(perms.contains(PosixFilePermission.GROUP_EXECUTE) ? 'x' : '-');
        sb.append(perms.contains(PosixFilePermission.OTHERS_READ) ? 'r' : '-');
        sb.append(perms.contains(PosixFilePermission.OTHERS_WRITE) ? 'w' : '-');
        sb.append(perms.contains(PosixFilePermission.OTHERS_EXECUTE) ? 'x' : '-');
        return sb.toString();
    }

    // ---------------------------------------------------------------
    // Windows guard
    // ---------------------------------------------------------------

    @Test
    @DisplayName("Server.start throws UnsupportedOperationException on Windows")
    @EnabledOnOs(OS.WINDOWS)
    void windowsStart_throwsUnsupported() {
        UnsupportedOperationException e = assertThrows(
                UnsupportedOperationException.class,
                () -> Server.start(new Server.SocketConfig(tempDir.resolve("barback.sock"))));
        assertTrue(e.getMessage().contains("Windows"),
                "exception message must mention Windows: " + e.getMessage());
        assertTrue(e.getMessage().contains("deferred"),
                "exception message must mention the deferral: " + e.getMessage());
    }

    // ---------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------

    private Server startOnTempPath() throws IOException {
        // Short suffix so the absolute path does not exceed the
        // platform's UDS sun_path limit (~104 chars on macOS,
        // ~108 on Linux) when the @TempDir prefix is long.
        //
        // We go through startWith(SocketConfig, WorkerPool) — the
        // M4.2-compat entry point that installs the
        // NOT_YET_IMPLEMENTED_DISPATCHER — rather than the production
        // {@link Server#start} (which now bootstraps an
        // {@link com.bluminal.barista.barback.core.EmbeddedMaven} per
        // M4.3 T2). ServerTest covers the protocol-layer fundamentals
        // (Ping/Pong, frame tolerance, 0600 perms, concurrent
        // dispatch) — those are independent of which dispatcher is
        // installed, and we don't want to require a Maven distribution
        // on the host for this suite to pass. End-to-end action
        // execution against the embedded core is covered by
        // {@code core/EmbeddedMavenTest} and
        // {@code integration/ActionDispatchIT}.
        Path sock = tempDir.resolve("b.sock");
        com.bluminal.barista.barback.workers.WorkerPool pool =
                com.bluminal.barista.barback.workers.WorkerPool.create(
                        Server.DEFAULT_WORKERS);
        return Server.startWith(new Server.SocketConfig(sock), pool);
    }

    private static SocketChannel openClient(Path socketPath) throws IOException {
        UnixDomainSocketAddress addr = UnixDomainSocketAddress.of(socketPath);
        SocketChannel ch = SocketChannel.open(StandardProtocolFamily.UNIX);
        ch.connect(addr);
        return ch;
    }

    private static void writeEnvelope(SocketChannel ch, Envelope env) throws IOException {
        byte[] payload = env.toByteArray();
        ByteBuffer header = ByteBuffer.allocate(4).order(ByteOrder.BIG_ENDIAN);
        header.putInt(payload.length);
        header.flip();
        while (header.hasRemaining()) {
            ch.write(header);
        }
        ByteBuffer body = ByteBuffer.wrap(payload);
        while (body.hasRemaining()) {
            ch.write(body);
        }
    }

    private static Envelope readEnvelope(SocketChannel ch) throws IOException {
        ByteBuffer header = ByteBuffer.allocate(4).order(ByteOrder.BIG_ENDIAN);
        if (!readFully(ch, header)) {
            throw new IOException("server closed the connection before sending a length prefix");
        }
        header.flip();
        int len = header.getInt();
        assertTrue(len >= 0 && len <= 16 * 1024 * 1024,
                "server announced absurd reply length: " + len);
        ByteBuffer body = ByteBuffer.allocate(len);
        if (!readFully(ch, body)) {
            throw new IOException("server closed mid-reply after "
                    + body.position() + "/" + len + " bytes");
        }
        body.flip();
        byte[] bytes = new byte[len];
        body.get(bytes);
        try {
            return Envelope.parseFrom(bytes);
        } catch (InvalidProtocolBufferException e) {
            throw new IOException("server returned malformed Envelope", e);
        }
    }

    private static boolean readFully(SocketChannel ch, ByteBuffer buf) throws IOException {
        while (buf.hasRemaining()) {
            int n = ch.read(buf);
            if (n < 0) {
                return false;
            }
        }
        return true;
    }

    /**
     * {@link Server#awaitShutdown()} on a fresh thread so the test can
     * apply a wall-clock timeout — a wedged accept loop fails fast
     * instead of hanging the CI run.
     */
    private static boolean awaitWithTimeout(Server s, Duration timeout) throws InterruptedException {
        CountDownLatch done = new CountDownLatch(1);
        Thread waiter = new Thread(() -> {
            try {
                s.awaitShutdown();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            } finally {
                done.countDown();
            }
        }, "server-await-waiter");
        waiter.setDaemon(true);
        waiter.start();
        boolean ok = done.await(timeout.toMillis(), TimeUnit.MILLISECONDS);
        if (!ok) {
            waiter.interrupt();
        }
        return ok;
    }
}
