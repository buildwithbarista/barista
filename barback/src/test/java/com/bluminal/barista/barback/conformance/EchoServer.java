/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.conformance;

import com.bluminal.barista.barback.proto.Envelope;
import com.google.protobuf.InvalidProtocolBufferException;

import java.io.IOException;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Echo server for the cross-language Rust&harr;Java conformance harness.
 *
 * <p>Binds a Unix domain socket at a caller-supplied path, accepts one
 * client connection, and loops:
 *
 * <ol>
 *   <li>read 4-byte big-endian length prefix,</li>
 *   <li>read exactly {@code length} bytes of payload,</li>
 *   <li>parse as an {@link Envelope} via {@link Envelope#parseFrom(byte[])},</li>
 *   <li>re-serialize via {@link Envelope#toByteArray()},</li>
 *   <li>write the 4-byte BE length, then the payload bytes.</li>
 * </ol>
 *
 * <p>This matches the wire contract documented in
 * {@code proto/barista/v1/worker.proto} (PRD &sect;12.1): each message is a
 * 4-byte big-endian length prefix followed by exactly that many bytes
 * of Envelope payload. The encoding is canonical &mdash; calling
 * {@code parseFrom} then {@code toByteArray} on the result yields the
 * same bytes the Rust side produced via {@code prost::Message::encode_to_vec}.
 *
 * <h2>Why not {@code writeDelimitedTo} / {@code parseDelimitedFrom}?</h2>
 *
 * <p>protobuf-java's {@code writeDelimitedTo} uses a varint-encoded length
 * prefix, not the fixed 4-byte big-endian prefix our codec produces.
 * Using the delimited helpers would silently break the wire contract
 * with the Rust side. The hand-rolled 4-byte BE read/write below
 * matches what
 * {@code tokio_util::codec::LengthDelimitedCodec.length_field_length(4).big_endian()}
 * emits.
 *
 * <h2>Frame-size policy</h2>
 *
 * <p>The Rust side caps frames at {@code MAX_FRAME_BYTES = 16 MiB}. The
 * echo server enforces the same cap on the read path: if a peer
 * announces a length above {@link #MAX_FRAME_BYTES}, the server closes
 * the connection without reading the body. Conformance tests on the
 * Rust side assert that this is the observable behaviour (the read
 * future surfaces {@code TransportError::Closed} when the Java side
 * rejects an oversized frame).
 *
 * <h2>Concurrency</h2>
 *
 * <p>Single-threaded by design. The conformance suite's
 * &quot;32 envelopes back-to-back&quot; variant doubles as a stream-ordering
 * check &mdash; if the Java side preserves order on a serial loop, the
 * Rust mux layer's wire-side serialisation is the only remaining
 * variable, and the test pins that down.
 *
 * <h2>Thread safety</h2>
 *
 * <p>None: this class is constructed and {@link #serve()}-d on a single
 * thread. The conformance harness spawns one JVM per test path.
 */
public final class EchoServer implements AutoCloseable {

    /**
     * Hard cap on the size of any single frame, matching the Rust side's
     * {@code MAX_FRAME_BYTES} constant in {@code crates/barista-ipc/src/transport/mod.rs}.
     * Frames larger than this trigger a clean connection close on the read
     * path &mdash; we never allocate a 4 GiB buffer because a peer announced
     * a 4 GiB length.
     */
    public static final int MAX_FRAME_BYTES = 16 * 1024 * 1024;

    /** Width of the length prefix in bytes. Matches PRD &sect;12.1. */
    public static final int LENGTH_FIELD_BYTES = 4;

    private final Path socketPath;
    private final ServerSocketChannel serverChannel;

    /**
     * Bind a {@code SOCK_STREAM} UDS at {@code socketPath} and prepare to
     * accept one client.
     *
     * <p>Any existing inode at the path is unlinked first so the bind
     * succeeds across reruns. The caller owns cleanup of the socket
     * inode on {@link #close()}.
     */
    public EchoServer(Path socketPath) throws IOException {
        this.socketPath = socketPath;
        Files.deleteIfExists(socketPath);
        UnixDomainSocketAddress addr = UnixDomainSocketAddress.of(socketPath);
        this.serverChannel = ServerSocketChannel.open(StandardProtocolFamily.UNIX);
        this.serverChannel.bind(addr);
    }

    /** The path the server bound at. */
    public Path socketPath() {
        return socketPath;
    }

    /**
     * Accept one client and run the echo loop until the client closes the
     * socket or an oversized frame is observed.
     *
     * <p>Exits cleanly on EOF at any frame boundary (i.e. the client closed
     * after a complete frame). EOF in the middle of a frame is treated as
     * a protocol violation and surfaces as an {@link IOException}, which
     * the caller may log but is otherwise non-fatal &mdash; the server is
     * shutting down anyway.
     */
    public void serve() throws IOException {
        try (SocketChannel client = serverChannel.accept()) {
            runEchoLoop(client);
        }
    }

    private void runEchoLoop(SocketChannel client) throws IOException {
        ByteBuffer lenBuf = ByteBuffer.allocate(LENGTH_FIELD_BYTES).order(ByteOrder.BIG_ENDIAN);
        while (true) {
            lenBuf.clear();
            if (!readFully(client, lenBuf)) {
                // Clean EOF at a frame boundary: client closed after a
                // complete frame (or before sending anything). Exit the
                // loop without raising.
                return;
            }
            lenBuf.flip();
            int announced = lenBuf.getInt();
            if (announced < 0 || announced > MAX_FRAME_BYTES) {
                // Oversized or negative-prefix frame: refuse to read the
                // body. Closing the SocketChannel surfaces on the Rust
                // side as TransportError::Closed (clean EOF mid-frame
                // on the next recv). Don't throw &mdash; this is the
                // documented protective behaviour, not an error.
                return;
            }
            ByteBuffer payload = ByteBuffer.allocate(announced);
            if (!readFully(client, payload)) {
                // EOF mid-payload: peer hung up unexpectedly. Surface as
                // an exception so the test process records the
                // protocol violation, but otherwise terminate the loop.
                throw new IOException(
                        "EOF after " + payload.position() + "/" + announced
                                + " bytes of an in-flight frame");
            }
            payload.flip();
            byte[] payloadBytes = new byte[announced];
            payload.get(payloadBytes);

            // Decode + re-encode to prove the bytes round-trip through the
            // generated Java types (which is the *contract* this test
            // enforces; a passthrough copy would not catch a schema
            // skew between the Rust and Java generated code).
            Envelope env;
            try {
                env = Envelope.parseFrom(payloadBytes);
            } catch (InvalidProtocolBufferException e) {
                throw new IOException("malformed Envelope payload", e);
            }
            byte[] out = env.toByteArray();

            ByteBuffer outLen = ByteBuffer.allocate(LENGTH_FIELD_BYTES).order(ByteOrder.BIG_ENDIAN);
            outLen.putInt(out.length);
            outLen.flip();
            writeFully(client, outLen);
            writeFully(client, ByteBuffer.wrap(out));
        }
    }

    /**
     * Read until {@code buf} is full or EOF.
     *
     * @return {@code true} if the buffer was filled, {@code false} on EOF
     *     before any byte was read into the current buffer (clean
     *     close at a frame boundary). EOF after at least one byte has
     *     been read into the buffer is reported as {@code false} as
     *     well; the caller distinguishes &quot;clean close at frame
     *     boundary&quot; from &quot;EOF mid-frame&quot; by checking
     *     {@link ByteBuffer#position()} on return.
     */
    private static boolean readFully(SocketChannel ch, ByteBuffer buf) throws IOException {
        while (buf.hasRemaining()) {
            int n = ch.read(buf);
            if (n < 0) {
                return !buf.hasRemaining();
            }
        }
        return true;
    }

    private static void writeFully(SocketChannel ch, ByteBuffer buf) throws IOException {
        while (buf.hasRemaining()) {
            int n = ch.write(buf);
            if (n < 0) {
                throw new IOException("write returned -1 on a connected SocketChannel");
            }
        }
    }

    @Override
    public void close() throws IOException {
        try {
            serverChannel.close();
        } finally {
            Files.deleteIfExists(socketPath);
        }
    }
}
