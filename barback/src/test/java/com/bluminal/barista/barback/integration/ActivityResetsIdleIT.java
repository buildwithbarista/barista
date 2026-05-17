/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.integration;

import com.bluminal.barista.barback.Server;
import com.bluminal.barista.barback.proto.Envelope;
import com.bluminal.barista.barback.proto.Ping;
import com.bluminal.barista.barback.workers.WorkerPool;
import com.google.protobuf.InvalidProtocolBufferException;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Tag;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.DisabledOnOs;
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
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Integration test proving that inbound activity resets the
 * idle-shutdown window end-to-end (timer + Server dispatch wiring).
 *
 * <p>The test starts a Server with {@code idleShutdownSeconds = 2},
 * waits about 1.5s (which is past the halfway mark of the window but
 * not the full window), sends a {@code Ping} envelope, then waits
 * another 1.5s. If the timer reset path is wired correctly, the
 * cumulative 3s wall-clock time should NOT trigger an idle shutdown
 * (no quiet stretch was &ge; 2s). The Server must still be reachable
 * at the end of the 3s window.
 *
 * <p>Tagged {@code integration} because the test consumes 3-4 seconds
 * of wall-clock time. The companion {@link IdleShutdownIT} is left
 * untagged so the headline AC fires on every {@code mvn test}.
 */
@Tag("integration")
class ActivityResetsIdleIT {

    private static final int IDLE_SECONDS = 2;

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

    @Test
    @DisplayName("inbound Ping mid-window resets the idle timer end-to-end")
    @DisabledOnOs(OS.WINDOWS)
    void midWindowActivity_resetsIdleTimer() throws Exception {
        WorkerPool pool = WorkerPool.createWith(
                WorkerPool.newPlatformThreadPool(2), 2);
        Path sock = tempDir.resolve("activity.sock");
        Server.SocketConfig config = new Server.SocketConfig(
                sock, /* workers */ 2, /* idleShutdownSeconds */ IDLE_SECONDS);
        server = Server.startWith(config, pool);
        assertTrue(Files.exists(sock));

        // 1.5s in: past 75% of the 2s idle window, well before the
        // 2s deadline. The Server must still be up and accepting
        // connections — if not, the idle timer fired prematurely.
        Thread.sleep(1500);
        assertTrue(Files.exists(sock),
                "socket must still be live 1.5s into a 2s idle window");

        // Send a Ping. This routes through dispatch() which is the
        // call site that invokes idleTimer.recordActivity(); the
        // round-trip Pong reply proves the dispatch path actually ran.
        try (SocketChannel client = openClient(sock)) {
            Envelope ping = Envelope.newBuilder()
                    .setVersion(1)
                    .setRequestId(42L)
                    .setPing(Ping.newBuilder().setSentAtUnixMicros(0L).build())
                    .build();
            writeEnvelope(client, ping);
            Envelope reply = readEnvelope(client);
            assertEquals(Envelope.BodyCase.PONG, reply.getBodyCase(),
                    "server must reply with a Pong");
            assertEquals(42L, reply.getRequestId(),
                    "server must echo the Ping's request_id");
        }

        // Another 1.5s. Cumulative 3.0s wall time since start — past
        // the literal 2s idle window — but only ~1.5s since the
        // dispatch, so the timer reset should keep us alive.
        Thread.sleep(1500);
        assertTrue(Files.exists(sock),
                "socket must still be live 1.5s after the mid-window Ping; "
                        + "if it isn't, recordActivity() did not reset the timer");

        // Final correctness probe: send another Ping and confirm we
        // still get a Pong back.
        try (SocketChannel client = openClient(sock)) {
            Envelope ping = Envelope.newBuilder()
                    .setVersion(1)
                    .setRequestId(43L)
                    .setPing(Ping.newBuilder().setSentAtUnixMicros(0L).build())
                    .build();
            writeEnvelope(client, ping);
            Envelope reply = readEnvelope(client);
            assertEquals(Envelope.BodyCase.PONG, reply.getBodyCase(),
                    "server must still answer Pings after the activity-reset window");
        }

        // Now go quiet. The idle timer should fire within
        // (IDLE_SECONDS + 5)s of the last activity. Use the same
        // deadline budget as IdleShutdownIT.
        long deadlineMs = TimeUnit.SECONDS.toMillis(IDLE_SECONDS + 5L);
        long start = System.nanoTime();
        boolean terminated = awaitShutdown(server, deadlineMs);
        long elapsedMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - start);
        assertTrue(terminated,
                "after final quiet period, daemon must self-terminate within "
                        + deadlineMs + "ms; actually waited " + elapsedMs + "ms");
        assertFalse(Files.exists(sock),
                "socket inode must be removed by the shutdown path");
    }

    private static boolean awaitShutdown(Server s, long millis)
            throws InterruptedException {
        Thread waiter = new Thread(() -> {
            try {
                s.awaitShutdown();
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }, "activity-reset-it-waiter");
        waiter.setDaemon(true);
        waiter.start();
        waiter.join(millis);
        return !waiter.isAlive();
    }

    // ----------------------------------------------------------------
    // Wire helpers — mirror ServerWorkerPoolIT's framing.
    // ----------------------------------------------------------------

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
            throw new IOException("server closed before sending a length prefix");
        }
        header.flip();
        int len = header.getInt();
        if (len < 0 || len > 16 * 1024 * 1024) {
            throw new IOException("server announced absurd reply length: " + len);
        }
        ByteBuffer body = ByteBuffer.allocate(len);
        if (!readFully(ch, body)) {
            throw new IOException("server closed mid-reply");
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
}
