/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.proto;

import static org.junit.jupiter.api.Assertions.assertAll;
import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.google.protobuf.ByteString;
import com.google.protobuf.InvalidProtocolBufferException;
import org.junit.jupiter.api.DisplayName;
import org.junit.jupiter.api.Nested;
import org.junit.jupiter.api.Test;

/**
 * Smoke tests for the Java bindings of {@code proto/barista/v1/worker.proto}.
 *
 * <p>Each test constructs a message via the generated builder, round-trips
 * it through {@code toByteArray} / {@code parseFrom}, and asserts the
 * deserialized message is equal to the original. This proves three things
 * at once:
 *
 * <ol>
 *   <li>the codegen step ran and produced a usable Java class for the
 *       message in question;</li>
 *   <li>every field that the test sets survives a wire round-trip
 *       (catches a forgotten {@code reserved}, a misnumbered tag, or a
 *       missing field type);</li>
 *   <li>the protobuf-java runtime version is ABI-compatible with the
 *       protoc that produced the bindings.</li>
 * </ol>
 *
 * <p>The {@code Envelope#body} oneof has 11 variants — Ping, Pong,
 * ActionRequest, ActionStream, ActionResult, ProgressEvent,
 * CancelRequest, Shutdown, StatusRequest, StatusResponse, Error.
 * One {@code Envelope}-level test per variant verifies the oneof
 * discriminator is correctly set and round-trips.
 *
 * <p>The {@code Credential#secret} oneof + {@code toString} redaction
 * contract is exercised in {@link CredentialRedactionTests}.
 */
class WorkerProtoSmokeTest {

    /**
     * The wire protocol version barback speaks. Mirrors the constant the
     * CLI uses to populate {@code Envelope.version}.
     */
    private static final int PROTOCOL_VERSION = 1;

    // ----- Top-level message round-trips --------------------------------------

    @Test
    @DisplayName("Ping round-trips through toByteArray/parseFrom")
    void ping_roundtrip() throws InvalidProtocolBufferException {
        Ping original = Ping.newBuilder()
                .setClient("barista 0.1.0")
                .setSentAtUnixMicros(1_700_000_000_000_000L)
                .build();

        Ping decoded = Ping.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
    }

    @Test
    @DisplayName("Pong round-trips with toolchain fields populated")
    void pong_roundtrip() throws InvalidProtocolBufferException {
        Pong original = Pong.newBuilder()
                .setDaemon("barback 0.1.0")
                .setJdkId("temurin-21")
                .setJdkVersion("21.0.4")
                .setServerUnixMicros(1_700_000_000_500_000L)
                .setClientUnixMicros(1_700_000_000_000_000L)
                .build();

        Pong decoded = Pong.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
    }

    @Test
    @DisplayName("ActionRequest round-trips with classpath, env, and credentials")
    void actionRequest_roundtrip() throws InvalidProtocolBufferException {
        ActionRequest original = ActionRequest.newBuilder()
                .setActionId("a-7f6c8b1e-1234")
                .setMojoCoords("org.apache.maven.plugins:maven-compiler-plugin:3.13.0:compile")
                .setProjectRoot("/home/dev/work/example")
                .setPomPath("/home/dev/work/example/pom.xml")
                .setEffectivePomBlob(ByteString.copyFrom(new byte[]{0x01, 0x02, 0x03}))
                .addClasspath("/cache/cas/aa/bb/junit-5.10.2.jar")
                .addClasspath("/cache/cas/cc/dd/hamcrest-2.2.jar")
                .addPluginClasspath("/cache/cas/ee/ff/compiler-plugin.jar")
                .putSystemProperties("maven.compiler.release", "17")
                .putEnvironment("PATH", "/usr/bin")
                .setWorkingDirectory("/home/dev/work/example")
                .setStdoutStreamId(1)
                .setStderrStreamId(2)
                .setQuiet(false)
                .setMavenCompat("3")
                .addJvmArgs("-Xmx2g")
                .setCredentials(CredentialsEnvelope.newBuilder()
                        .addEntries(Credential.newBuilder()
                                .setServerId("central")
                                .setUsername("deploy-bot")
                                .setToken("redact-me")
                                .build())
                        .build())
                .build();

        ActionRequest decoded = ActionRequest.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals(2, decoded.getClasspathCount());
        assertEquals("17", decoded.getSystemPropertiesMap().get("maven.compiler.release"));
        assertEquals(Credential.SecretCase.TOKEN,
                decoded.getCredentials().getEntries(0).getSecretCase());
    }

    @Test
    @DisplayName("ActionStream round-trips with raw payload bytes")
    void actionStream_roundtrip() throws InvalidProtocolBufferException {
        ActionStream original = ActionStream.newBuilder()
                .setStreamId(1)
                .setPayload(ByteString.copyFromUtf8("[INFO] Building example 0.1.0\n"))
                .setEnd(false)
                .setActionId("a-7f6c8b1e-1234")
                .build();

        ActionStream decoded = ActionStream.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertFalse(decoded.getEnd());
    }

    @Test
    @DisplayName("ActionResult round-trips with artifacts, attributes, and embedded Error")
    void actionResult_roundtrip() throws InvalidProtocolBufferException {
        ActionResult original = ActionResult.newBuilder()
                .setActionId("a-7f6c8b1e-1234")
                .setStatus(ActionResult.Status.FAILURE)
                .setExitCode(1)
                .setDurationMicros(2_500_000L)
                .addArtifacts(ProducedArtifact.newBuilder()
                        .setPath("/home/dev/work/example/target/example-0.1.0.jar")
                        .setSizeBytes(48_127)
                        .setSha256("a".repeat(64))
                        .build())
                .setFailureMessage("Compilation failed: 3 errors")
                .setFailureStack("at o.a.m.compiler.CompilerMojo.execute(CompilerMojo.java:42)")
                .putAttributes("compile.errors", "3")
                .setError(Error.newBuilder()
                        .setCode("BAR-COMPILER-001")
                        .setMessage("javac reported errors")
                        .putDetails("file", "Foo.java")
                        .setActionId("a-7f6c8b1e-1234")
                        .build())
                .build();

        ActionResult decoded = ActionResult.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals(ActionResult.Status.FAILURE, decoded.getStatus());
        assertEquals(1, decoded.getArtifactsCount());
    }

    @Test
    @DisplayName("ProgressEvent round-trips with Mojo + details map")
    void progressEvent_roundtrip() throws InvalidProtocolBufferException {
        ProgressEvent original = ProgressEvent.newBuilder()
                .setKind(ProgressEvent.Kind.FETCHING)
                .setActionId("a-7f6c8b1e-1234")
                .setTimestamp("2026-05-14T12:34:56.789Z")
                .setCoord("org.apache.maven.plugins:maven-compiler-plugin:3.13.0")
                .setPhase("fetch")
                .setProgress(42.5)
                .setMojo(Mojo.newBuilder()
                        .setGroupId("org.apache.maven.plugins")
                        .setArtifactId("maven-compiler-plugin")
                        .setVersion("3.13.0")
                        .setGoal("compile")
                        .setExecutionId("default-compile")
                        .build())
                .putDetails("bytes_so_far", "1024")
                .build();

        ProgressEvent decoded = ProgressEvent.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals(ProgressEvent.Kind.FETCHING, decoded.getKind());
    }

    @Test
    @DisplayName("CancelRequest round-trips with grace period")
    void cancelRequest_roundtrip() throws InvalidProtocolBufferException {
        CancelRequest original = CancelRequest.newBuilder()
                .setActionId("a-7f6c8b1e-1234")
                .setGracePeriodMs(5000)
                .build();

        CancelRequest decoded = CancelRequest.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals(5000, decoded.getGracePeriodMs());
    }

    @Test
    @DisplayName("Shutdown round-trips")
    void shutdown_roundtrip() throws InvalidProtocolBufferException {
        Shutdown original = Shutdown.newBuilder().setDrainSeconds(30).build();
        Shutdown decoded = Shutdown.parseFrom(original.toByteArray());
        assertEquals(original, decoded);
    }

    @Test
    @DisplayName("StatusRequest round-trips (empty message)")
    void statusRequest_roundtrip() throws InvalidProtocolBufferException {
        StatusRequest original = StatusRequest.newBuilder().build();
        StatusRequest decoded = StatusRequest.parseFrom(original.toByteArray());
        assertEquals(original, decoded);
    }

    @Test
    @DisplayName("StatusResponse round-trips with the full counter set")
    void statusResponse_roundtrip() throws InvalidProtocolBufferException {
        StatusResponse original = StatusResponse.newBuilder()
                .setUptimeSeconds(3600)
                .setWorkersTotal(8)
                .setWorkersBusy(3)
                .setActionsExecuted(142)
                .setActionsFailed(2)
                .setCachedClassloaders(17)
                .setHeapUsedBytes(512 * 1024 * 1024L)
                .setHeapMaxBytes(2L * 1024 * 1024 * 1024)
                .setJitState("warm")
                .build();

        StatusResponse decoded = StatusResponse.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
    }

    @Test
    @DisplayName("Error round-trips with structured details")
    void error_roundtrip() throws InvalidProtocolBufferException {
        Error original = Error.newBuilder()
                .setCode("BAR-PROTO-001")
                .setMessage("Protocol version mismatch")
                .putDetails("peer_version", "2")
                .putDetails("server_version", "1")
                .setActionId("")
                .build();

        Error decoded = Error.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals("BAR-PROTO-001", decoded.getCode());
    }

    @Test
    @DisplayName("Mojo round-trips with optional version absent")
    void mojo_roundtrip_versionless() throws InvalidProtocolBufferException {
        Mojo original = Mojo.newBuilder()
                .setGroupId("org.apache.maven.plugins")
                .setArtifactId("maven-surefire-plugin")
                .setGoal("test")
                .setExecutionId("default-test")
                .build();

        Mojo decoded = Mojo.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals("", decoded.getVersion());
    }

    @Test
    @DisplayName("ProducedArtifact round-trips")
    void producedArtifact_roundtrip() throws InvalidProtocolBufferException {
        ProducedArtifact original = ProducedArtifact.newBuilder()
                .setPath("/cache/build/target/foo-0.1.0.jar")
                .setSizeBytes(12345)
                .setSha256("e".repeat(64))
                .build();

        ProducedArtifact decoded = ProducedArtifact.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
    }

    @Test
    @DisplayName("CredentialsEnvelope round-trips with one entry of each secret kind")
    void credentialsEnvelope_roundtrip() throws InvalidProtocolBufferException {
        CredentialsEnvelope original = CredentialsEnvelope.newBuilder()
                .addEntries(Credential.newBuilder()
                        .setServerId("server-password")
                        .setUsername("alice")
                        .setPassword("p4ssw0rd!")
                        .build())
                .addEntries(Credential.newBuilder()
                        .setServerId("server-token")
                        .setUsername("bot")
                        .setToken("ghp_token123")
                        .build())
                .addEntries(Credential.newBuilder()
                        .setServerId("server-ssh")
                        .setUsername("git")
                        .setSshKey(SshKey.newBuilder()
                                .setPrivateKeyPem(ByteString.copyFromUtf8(
                                        "-----BEGIN PRIVATE KEY-----\nMIIBV\n-----END PRIVATE KEY-----\n"))
                                .setPassphrase("phrase")
                                .build())
                        .build())
                .build();

        CredentialsEnvelope decoded = CredentialsEnvelope.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals(3, decoded.getEntriesCount());
        assertEquals(Credential.SecretCase.PASSWORD, decoded.getEntries(0).getSecretCase());
        assertEquals(Credential.SecretCase.TOKEN, decoded.getEntries(1).getSecretCase());
        assertEquals(Credential.SecretCase.SSH_KEY, decoded.getEntries(2).getSecretCase());
    }

    @Test
    @DisplayName("SshKey round-trips with optional passphrase absent")
    void sshKey_roundtrip_noPassphrase() throws InvalidProtocolBufferException {
        SshKey original = SshKey.newBuilder()
                .setPrivateKeyPem(ByteString.copyFromUtf8("PEM"))
                .build();

        SshKey decoded = SshKey.parseFrom(original.toByteArray());

        assertEquals(original, decoded);
        assertEquals("", decoded.getPassphrase());
    }

    // ----- Envelope#body oneof — all 11 variants ------------------------------
    //
    // Each test sets exactly one body variant, round-trips the whole Envelope,
    // and asserts (a) the BodyCase discriminator survives, (b) the inner
    // message survives.

    @Nested
    @DisplayName("Envelope.body oneof — every variant must round-trip")
    class EnvelopeBodyOneofTests {

        @Test
        @DisplayName("body=ping")
        void body_ping() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setPing(Ping.newBuilder().setClient("barista 0.1.0").build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.PING, decoded.getBodyCase());
            assertEquals("barista 0.1.0", decoded.getPing().getClient());
        }

        @Test
        @DisplayName("body=pong")
        void body_pong() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setPong(Pong.newBuilder().setDaemon("barback 0.1.0").build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.PONG, decoded.getBodyCase());
            assertEquals("barback 0.1.0", decoded.getPong().getDaemon());
        }

        @Test
        @DisplayName("body=action (ActionRequest)")
        void body_action() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setAction(ActionRequest.newBuilder().setActionId("a-1").build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.ACTION, decoded.getBodyCase());
            assertEquals("a-1", decoded.getAction().getActionId());
        }

        @Test
        @DisplayName("body=stream (ActionStream)")
        void body_stream() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setStream(ActionStream.newBuilder()
                            .setStreamId(1)
                            .setActionId("a-1")
                            .setPayload(ByteString.copyFromUtf8("hello"))
                            .build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.STREAM, decoded.getBodyCase());
            assertEquals(1, decoded.getStream().getStreamId());
        }

        @Test
        @DisplayName("body=result (ActionResult)")
        void body_result() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setResult(ActionResult.newBuilder()
                            .setActionId("a-1")
                            .setStatus(ActionResult.Status.SUCCESS)
                            .build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.RESULT, decoded.getBodyCase());
            assertEquals(ActionResult.Status.SUCCESS, decoded.getResult().getStatus());
        }

        @Test
        @DisplayName("body=progress (ProgressEvent)")
        void body_progress() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setProgress(ProgressEvent.newBuilder()
                            .setKind(ProgressEvent.Kind.STARTED)
                            .setActionId("a-1")
                            .build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.PROGRESS, decoded.getBodyCase());
            assertEquals(ProgressEvent.Kind.STARTED, decoded.getProgress().getKind());
        }

        @Test
        @DisplayName("body=cancel (CancelRequest)")
        void body_cancel() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setCancel(CancelRequest.newBuilder().setActionId("a-1").build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.CANCEL, decoded.getBodyCase());
            assertEquals("a-1", decoded.getCancel().getActionId());
        }

        @Test
        @DisplayName("body=shutdown (Shutdown)")
        void body_shutdown() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setShutdown(Shutdown.newBuilder().setDrainSeconds(15).build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.SHUTDOWN, decoded.getBodyCase());
            assertEquals(15, decoded.getShutdown().getDrainSeconds());
        }

        @Test
        @DisplayName("body=status_request (StatusRequest)")
        void body_statusRequest() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setStatusRequest(StatusRequest.newBuilder().build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.STATUS_REQUEST, decoded.getBodyCase());
        }

        @Test
        @DisplayName("body=status (StatusResponse)")
        void body_status() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setStatus(StatusResponse.newBuilder()
                            .setUptimeSeconds(100)
                            .setJitState("warming")
                            .build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.STATUS, decoded.getBodyCase());
            assertEquals("warming", decoded.getStatus().getJitState());
        }

        @Test
        @DisplayName("body=error (Error)")
        void body_error() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope()
                    .setError(Error.newBuilder()
                            .setCode("BAR-PROTO-001")
                            .setMessage("Protocol version mismatch")
                            .build())
                    .build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.ERROR, decoded.getBodyCase());
            assertEquals("BAR-PROTO-001", decoded.getError().getCode());
        }

        @Test
        @DisplayName("body unset round-trips as BODY_NOT_SET (no oneof variant)")
        void body_unset() throws InvalidProtocolBufferException {
            Envelope env = baseEnvelope().build();
            Envelope decoded = roundtrip(env);
            assertEquals(Envelope.BodyCase.BODY_NOT_SET, decoded.getBodyCase());
        }

        private Envelope.Builder baseEnvelope() {
            return Envelope.newBuilder()
                    .setVersion(PROTOCOL_VERSION)
                    .setRequestId(42L);
        }

        private Envelope roundtrip(Envelope env) throws InvalidProtocolBufferException {
            Envelope decoded = Envelope.parseFrom(env.toByteArray());
            assertEquals(env, decoded);
            assertEquals(PROTOCOL_VERSION, decoded.getVersion());
            assertEquals(42L, decoded.getRequestId());
            return decoded;
        }
    }

    // ----- Credential.toString redaction --------------------------------------
    //
    // protobuf-java's default `Message#toString()` (via TextFormat) is NOT
    // safe-by-default for secret fields unless `[debug_redact = true]` is set
    // on the .proto. The worker.proto schema deliberately does not carry that
    // annotation today (T1 scoped to wire shape only), so this test set:
    //
    //   1. documents that the *raw* generated `Credential.toString()` DOES
    //      include the secret bytes in plain text, and
    //   2. verifies the hand-written `RedactedCredential` adapter renders a
    //      safe string regardless of which secret variant is set.
    //
    // The T5 implementation will extend this story to receive-buffer
    // zeroization; this test pins the toString contract callers can rely on
    // for the duration of v0.1.

    @Nested
    @DisplayName("Credential#secret oneof — redaction contract")
    class CredentialRedactionTests {

        private static final String PASSWORD_SECRET = "p4ssw0rd-NEVER-LOG";
        private static final String TOKEN_SECRET = "ghp_token-NEVER-LOG";
        private static final String PEM_SECRET = "PRIVATE-KEY-BYTES-NEVER-LOG";
        private static final String PASSPHRASE_SECRET = "passphrase-NEVER-LOG";

        @Test
        @DisplayName("RedactedCredential.toString never leaks the password")
        void password_isRedacted() {
            Credential c = Credential.newBuilder()
                    .setServerId("central")
                    .setUsername("alice")
                    .setPassword(PASSWORD_SECRET)
                    .build();

            String safe = RedactedCredential.of(c).toString();

            assertAll(
                    () -> assertTrue(safe.contains("server_id=\"central\""),
                            "server_id should be visible: " + safe),
                    () -> assertTrue(safe.contains("username=\"alice\""),
                            "username should be visible: " + safe),
                    () -> assertTrue(safe.contains("[REDACTED:PASSWORD]"),
                            "secret-kind marker should be present: " + safe),
                    () -> assertFalse(safe.contains(PASSWORD_SECRET),
                            "redacted toString must not contain the password: " + safe)
            );
        }

        @Test
        @DisplayName("RedactedCredential.toString never leaks the token")
        void token_isRedacted() {
            Credential c = Credential.newBuilder()
                    .setServerId("artifactory")
                    .setToken(TOKEN_SECRET)
                    .build();

            String safe = RedactedCredential.of(c).toString();

            assertTrue(safe.contains("[REDACTED:TOKEN]"), safe);
            assertFalse(safe.contains(TOKEN_SECRET),
                    "redacted toString must not contain the token: " + safe);
        }

        @Test
        @DisplayName("RedactedCredential.toString never leaks the SSH key or passphrase")
        void sshKey_isRedacted() {
            Credential c = Credential.newBuilder()
                    .setServerId("scm-prod")
                    .setUsername("git")
                    .setSshKey(SshKey.newBuilder()
                            .setPrivateKeyPem(ByteString.copyFromUtf8(PEM_SECRET))
                            .setPassphrase(PASSPHRASE_SECRET)
                            .build())
                    .build();

            String safe = RedactedCredential.of(c).toString();

            assertAll(
                    () -> assertTrue(safe.contains("[REDACTED:SSH_KEY]"), safe),
                    () -> assertFalse(safe.contains(PEM_SECRET),
                            "redacted toString must not contain the PEM bytes: " + safe),
                    () -> assertFalse(safe.contains(PASSPHRASE_SECRET),
                            "redacted toString must not contain the passphrase: " + safe)
            );
        }

        @Test
        @DisplayName("RedactedCredential.toString renders [NO_SECRET] when no secret variant is set")
        void noSecret_rendersExplicitSentinel() {
            Credential c = Credential.newBuilder()
                    .setServerId("anonymous-mirror")
                    .build();

            String safe = RedactedCredential.of(c).toString();

            assertTrue(safe.contains("[NO_SECRET]"), safe);
        }

        @Test
        @DisplayName("RedactedCredential.unwrap returns the same generated message")
        void unwrap_returnsDelegate() {
            Credential c = Credential.newBuilder().setServerId("x").build();
            RedactedCredential wrapped = RedactedCredential.of(c);
            assertEquals(c, wrapped.unwrap());
        }

        @Test
        @DisplayName("RedactedCredential equals/hashCode are delegated to the underlying Credential")
        void equalsHashCode_delegated() {
            Credential a = Credential.newBuilder()
                    .setServerId("x").setPassword("p").build();
            Credential b = Credential.newBuilder()
                    .setServerId("x").setPassword("p").build();
            Credential c = Credential.newBuilder()
                    .setServerId("y").setPassword("p").build();
            assertEquals(RedactedCredential.of(a), RedactedCredential.of(b));
            assertEquals(RedactedCredential.of(a).hashCode(),
                    RedactedCredential.of(b).hashCode());
            assertNotEquals(RedactedCredential.of(a), RedactedCredential.of(c));
        }

        @Test
        @DisplayName("redactedToString static helper produces the same output as the wrapper")
        void staticHelper_matchesWrapperOutput() {
            Credential c = Credential.newBuilder()
                    .setServerId("central")
                    .setUsername("alice")
                    .setPassword(PASSWORD_SECRET)
                    .build();
            assertEquals(RedactedCredential.of(c).toString(),
                    RedactedCredential.redactedToString(c));
        }

        /**
         * Pinning test. The raw generated {@code Credential.toString()} from
         * protobuf-java <strong>does</strong> include secret bytes today
         * because {@code worker.proto} does not annotate the secret fields
         * with {@code [debug_redact = true]}. Diagnostic code MUST route
         * through {@link RedactedCredential} — this test exists so that if
         * protobuf-java's default ever changes to be safe-by-default, we
         * notice and can simplify the adapter (or remove it).
         *
         * <p>This is an acceptance-criterion-bracketing test: it documents
         * the protobuf-java behavior we are protecting against rather than
         * asserting our own correctness.
         */
        @Test
        @DisplayName("Raw Credential.toString() leaks the secret (documents protobuf-java default)")
        void rawToString_leaksSecret_documentsProtobufDefault() {
            Credential c = Credential.newBuilder()
                    .setServerId("central")
                    .setPassword(PASSWORD_SECRET)
                    .build();
            String raw = c.toString();
            // If this assertion ever flips, protobuf-java has changed its
            // default; revisit `RedactedCredential` in light of the new
            // upstream behavior.
            assertTrue(raw.contains(PASSWORD_SECRET),
                    "Expected raw protobuf toString to expose the password "
                            + "(documents the unsafe default this adapter protects against). "
                            + "Output was: " + raw);
        }
    }

    // ----- Cross-cutting checks -----------------------------------------------

    @Test
    @DisplayName("Generated descriptor file populates the outer WorkerProto class")
    void outerClass_descriptorIsPresent() {
        assertNotNull(WorkerProto.getDescriptor(), "outer descriptor must be present");
        assertDoesNotThrow(() -> WorkerProto.getDescriptor().getMessageTypes(),
                "message types should be enumerable on the file descriptor");
    }
}
