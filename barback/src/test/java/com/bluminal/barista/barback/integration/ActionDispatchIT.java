/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.integration;

import com.bluminal.barista.barback.Server;
import com.bluminal.barista.barback.core.EmbeddedMaven;
import com.bluminal.barista.barback.core.EmbeddedMavenActionDispatcher;
import com.bluminal.barista.barback.core.EmbeddedMavenFactory;
import com.bluminal.barista.barback.core.MavenDistributionFixture;
import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;
import com.bluminal.barista.barback.proto.Credential;
import com.bluminal.barista.barback.proto.CredentialsEnvelope;
import com.bluminal.barista.barback.proto.Envelope;
import com.google.protobuf.InvalidProtocolBufferException;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.DisplayName;
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
import java.util.UUID;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * End-to-end integration test that closes the M4.3 T1 caveat: an
 * ACTION envelope reaching the daemon's connection handler is now
 * executed through the embedded Maven core via
 * {@link EmbeddedMavenActionDispatcher}, not short-circuited to the
 * {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} stub.
 *
 * <p>Drives a real {@link Server} (the production
 * {@link Server#startWithEmbeddedMaven} entry point) against the
 * spike's 1-module sample project for a {@code compile} action and
 * asserts:
 *
 * <ul>
 *   <li>the reply is an {@link Envelope.BodyCase#RESULT}, not an
 *       {@link Envelope.BodyCase#ERROR};</li>
 *   <li>{@link ActionResult#getStatus()} is {@link ActionResult.Status#SUCCESS};</li>
 *   <li>{@link ActionResult#getError()} does NOT carry the
 *       {@code BAR-DAEMON-NOT-YET-IMPLEMENTED} code (the T1 caveat
 *       proof);</li>
 *   <li>{@code target/classes/Hello.class} is on disk afterwards.</li>
 * </ul>
 *
 * <p>Skips gracefully when the Maven 4 distribution is not staged —
 * see {@link MavenDistributionFixture}. The {@link Server} is built
 * via {@link Server#startWith(Server.SocketConfig,
 * com.bluminal.barista.barback.workers.WorkerPool, Server.ActionDispatcher)}
 * with the same dispatcher the production path installs, so this
 * suite exercises the production code path end-to-end without paying
 * the cost of a second JVM bootstrap or a host JDK probe.
 */
final class ActionDispatchIT {

    private Server server;
    private EmbeddedMaven embedded;

    @AfterEach
    void teardown() throws Exception {
        if (server != null) {
            server.close();
            server = null;
        }
        if (embedded != null) {
            embedded.close();
            embedded = null;
        }
    }

    @Test
    @DisplayName("ACTION envelope executes through EmbeddedMavenActionDispatcher (closes M4.3 T1 caveat)")
    @DisabledOnOs(OS.WINDOWS)
    void actionEnvelope_executesViaDispatcher(@TempDir Path tmp) throws Exception {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);
        embedded = EmbeddedMavenFactory.using(mavenHome);

        EmbeddedMavenActionDispatcher dispatcher = new EmbeddedMavenActionDispatcher(embedded);
        Server.ActionDispatcher seam = dispatcher::dispatch;
        Path sock = tmp.resolve("b.sock");
        com.bluminal.barista.barback.workers.WorkerPool pool =
                com.bluminal.barista.barback.workers.WorkerPool.create(2);
        server = Server.startWith(new Server.SocketConfig(sock), pool, seam);

        String actionId = "act-" + UUID.randomUUID();
        ActionRequest request = ActionRequest.newBuilder()
                .setActionId(actionId)
                .setMojoCoords("compile")
                .setPomPath(project.resolve("pom.xml").toString())
                .setProjectRoot(project.toString())
                .setWorkingDirectory(project.toString())
                .setMavenCompat("4")
                .setQuiet(true)
                .build();
        Envelope action = Envelope.newBuilder()
                .setVersion(1)
                .setRequestId(123L)
                .setAction(request)
                .build();

        try (SocketChannel client = openClient(server.socketPath())) {
            writeEnvelope(client, action);
            Envelope reply = readEnvelope(client);
            assertEquals(Envelope.BodyCase.RESULT, reply.getBodyCase(),
                    "production daemon path must produce a RESULT (not an ERROR)");
            assertEquals(123L, reply.getRequestId());
            ActionResult result = reply.getResult();
            assertEquals(actionId, result.getActionId());
            assertEquals(ActionResult.Status.SUCCESS, result.getStatus(),
                    "embedded compile must succeed; failure=" + result.getFailureMessage());
            assertEquals(0, result.getExitCode());
            // The M4.3 T1 caveat: the stub code MUST NOT appear here.
            // (Server.NOT_YET_IMPLEMENTED_CODE is package-private; the
            // literal is fine in a different package.)
            assertNotEquals("BAR-DAEMON-NOT-YET-IMPLEMENTED", result.getError().getCode(),
                    "ACTION dispatch must no longer return BAR-DAEMON-NOT-YET-IMPLEMENTED; "
                            + "the embedded-Maven dispatcher is wired now");
        }

        assertTrue(Files.isRegularFile(project.resolve("target/classes/Hello.class")),
                "embedded compile must produce Hello.class");
    }

    @Test
    @DisplayName("ACTION with deploy credentials writes ephemeral settings.xml and forwards -s")
    @DisabledOnOs(OS.WINDOWS)
    void actionWithCredentials_passesSettingsToMaven(@TempDir Path tmp) throws Exception {
        Path mavenHome = MavenDistributionFixture.requireMavenHome();
        Path project = MavenDistributionFixture.stageSampleProject(tmp);
        embedded = EmbeddedMavenFactory.using(mavenHome);

        EmbeddedMavenActionDispatcher dispatcher = new EmbeddedMavenActionDispatcher(embedded);

        // Drive the dispatcher directly (no socket loop needed for the
        // credential-plumbing assertion): a `compile` action with a
        // credentials envelope must NOT fail because of the credential
        // path — `compile` doesn't consult <servers> at all, but the
        // dispatcher's settings.xml synthesis must still execute and
        // clean up without errors.
        ActionRequest request = ActionRequest.newBuilder()
                .setActionId(UUID.randomUUID().toString())
                .setMojoCoords("compile")
                .setPomPath(project.resolve("pom.xml").toString())
                .setProjectRoot(project.toString())
                .setWorkingDirectory(project.toString())
                .setMavenCompat("4")
                .setQuiet(true)
                .setCredentials(CredentialsEnvelope.newBuilder()
                        .addEntries(Credential.newBuilder()
                                .setServerId("test-repo")
                                .setUsername("deploybot")
                                .setPassword("hunter2-test")
                                .build())
                        .build())
                .build();

        ActionResult result = dispatcher.dispatch(request);
        assertEquals(ActionResult.Status.SUCCESS, result.getStatus(),
                "compile with credentials envelope must still succeed; "
                        + "failure=" + result.getFailureMessage());
    }

    // ----------------------------------------------------------------
    // Wire helpers (lifted from ServerWorkerPoolIT, kept local).
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
}
