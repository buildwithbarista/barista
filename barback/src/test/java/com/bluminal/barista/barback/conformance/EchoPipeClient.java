/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.conformance;

import com.bluminal.barista.barback.proto.Envelope;
import com.google.protobuf.InvalidProtocolBufferException;

import java.io.EOFException;
import java.io.IOException;
import java.io.RandomAccessFile;

/**
 * Echo <em>client</em> for the cross-language Rust&harr;Java named-pipe
 * conformance harness on Windows.
 *
 * <p>The role inversion vs the UDS {@link EchoServer} is deliberate.
 * On Win32 the security-bearing call is
 * {@code CreateNamedPipeW(...SECURITY_ATTRIBUTES{ DACL })}, which the
 * Rust side already implements via
 * {@code barista_ipc::transport::pipe::NamedPipeTransport::bind_secure}.
 * Inverting the roles (Rust = server, Java = client) means the Rust
 * side enforces the DACL and Java just connects via the Win32 pipe
 * namespace surfaced as a filesystem path. No JNI required.
 *
 * <h2>Pipe access via {@link RandomAccessFile}</h2>
 *
 * <p>The Win32 pipe namespace exposes pipes as paths like
 * {@code \\.\pipe\<name>}. Opening such a path with
 * {@code new RandomAccessFile(path, "rw")} performs a {@code CreateFileW}
 * under the hood with both read and write access, which is exactly
 * what the echo loop needs. We wrap the file's input / output streams
 * in {@link DataInputStream} / {@link DataOutputStream} to get
 * big-endian 4-byte length framing for free
 * ({@link DataInputStream#readInt()} is BE by spec).
 *
 * <h2>Why not {@code AsynchronousFileChannel}?</h2>
 *
 * <p>{@code AsynchronousFileChannel} would let us issue overlapping IO
 * against the pipe, but the conformance harness is single-action-at-a-
 * time by construction (one envelope sent, one received, repeat). The
 * mux-layer parallelism story is covered on UDS by
 * {@code tests/conformance.rs::concurrent_32_inflight_preserves_order}
 * and the Rust-only {@code tests/mux_concurrent.rs} — replicating it on
 * named pipes would force a multi-instance pipe pool we don't yet need
 * for v0.1. {@code RandomAccessFile} keeps the spawn-from-Rust ceremony
 * trivial and the failure modes obvious.
 *
 * <h2>Wire-format contract (identical to {@link EchoServer})</h2>
 *
 * <pre>
 *   &lt;4-byte BE length&gt; &lt;length bytes of Envelope payload&gt;
 * </pre>
 *
 * <p>Each iteration of the echo loop:
 *
 * <ol>
 *   <li>read 4-byte big-endian length prefix via
 *       {@link RandomAccessFile#readInt()} ({@link java.io.DataInput}
 *       contract guarantees BE),</li>
 *   <li>read exactly {@code length} bytes of payload via
 *       {@link RandomAccessFile#readFully(byte[])},</li>
 *   <li>parse as an {@link Envelope} via {@link Envelope#parseFrom(byte[])},</li>
 *   <li>re-serialize via {@link Envelope#toByteArray()},</li>
 *   <li>write the 4-byte BE length via {@link RandomAccessFile#writeInt(int)},
 *       then the payload bytes.</li>
 * </ol>
 *
 * <p>The decode/re-encode round-trip is the security-bearing step: a
 * passthrough copy would not catch schema skew between the Rust
 * {@code prost}-generated code and the Java {@code protobuf-java}-
 * generated code. Forcing both sides through {@code parseFrom} then
 * {@code toByteArray} pins every field tag, wire type, and default.
 *
 * <h2>Frame-size policy</h2>
 *
 * <p>Matches {@link EchoServer#MAX_FRAME_BYTES} (16 MiB). Frames
 * larger than this trigger a clean close on the read path &mdash; we
 * never allocate a 4 GiB buffer because a peer announced a 4 GiB
 * length.
 *
 * <h2>Concurrency</h2>
 *
 * <p>Single-threaded by design. The harness spawns one JVM per test
 * path.
 *
 * <h2>Thread safety</h2>
 *
 * <p>None: this class is constructed and {@link #serve()}-d on a single
 * thread. The conformance harness spawns one JVM per test path.
 */
public final class EchoPipeClient implements AutoCloseable {

    /**
     * Hard cap on the size of any single frame, matching the Rust
     * side's {@code MAX_FRAME_BYTES} constant and {@link EchoServer#MAX_FRAME_BYTES}.
     */
    public static final int MAX_FRAME_BYTES = 16 * 1024 * 1024;

    private final String pipePath;
    private final RandomAccessFile pipe;

    /**
     * Open the named pipe at {@code pipePath} for read+write IO.
     *
     * <p>{@code pipePath} must be the full Win32 pipe path including
     * the {@code \\.\pipe\} prefix (e.g.
     * {@code \\.\pipe\barista-ipc-test-foo-123-456}). The
     * {@link RandomAccessFile} constructor opens the path with
     * {@code CreateFileW(GENERIC_READ | GENERIC_WRITE)} under the
     * hood, which the kernel matches to the listening server's
     * {@code ConnectNamedPipe} call.
     *
     * @throws IOException if the open fails (no listener at the path
     *     yet, ACCESS_DENIED from the DACL, etc.). Callers should
     *     treat any {@code IOException} here as fatal &mdash; the
     *     harness expects the Rust side to have bound the pipe before
     *     spawning us.
     */
    public EchoPipeClient(String pipePath) throws IOException {
        this.pipePath = pipePath;
        // "rw" → CreateFileW with GENERIC_READ|GENERIC_WRITE. The
        // server side's CreateNamedPipeW(PIPE_ACCESS_DUPLEX) accepts
        // both directions, so the open succeeds against a barista
        // pipe.
        //
        // RandomAccessFile already implements DataInput + DataOutput:
        // readInt/writeInt are spec'd big-endian, readFully(byte[])
        // is the contract for "read or throw EOFException". No need
        // to layer DataInputStream / DataOutputStream on top — those
        // would require a FileChannel-backed Channels.newInputStream
        // adapter, and on Windows a pipe's FileChannel doesn't
        // necessarily support every method.
        this.pipe = new RandomAccessFile(pipePath, "rw");
    }

    /** The pipe path this client opened. */
    public String pipePath() {
        return pipePath;
    }

    /**
     * Run the echo loop until EOF or an oversized frame is observed.
     *
     * <p>Exits cleanly on EOF at a frame boundary (i.e. the server
     * closed after a complete frame). EOF in the middle of a frame is
     * treated as a protocol violation and surfaces as an
     * {@link EOFException}, which the caller may log but is otherwise
     * non-fatal &mdash; the client is shutting down anyway.
     */
    public void serve() throws IOException {
        while (true) {
            int announced;
            try {
                announced = pipe.readInt();
            } catch (EOFException eof) {
                // Clean EOF at a frame boundary: server closed after a
                // complete frame (or before sending anything). Exit
                // the loop without raising.
                return;
            }
            if (announced < 0 || announced > MAX_FRAME_BYTES) {
                // Oversized or negative-prefix frame: refuse to read
                // the body. Closing the pipe surfaces on the Rust
                // side as TransportError::Closed (clean EOF mid-frame
                // on the next recv). Don't throw — this is the
                // documented protective behaviour, not an error.
                return;
            }
            byte[] payload = new byte[announced];
            pipe.readFully(payload);

            // Decode + re-encode to prove the bytes round-trip through
            // the generated Java types (which is the contract this
            // test enforces; a passthrough copy would not catch a
            // schema skew between the Rust and Java generated code).
            Envelope env;
            try {
                env = Envelope.parseFrom(payload);
            } catch (InvalidProtocolBufferException e) {
                throw new IOException("malformed Envelope payload", e);
            }
            byte[] outBytes = env.toByteArray();

            pipe.writeInt(outBytes.length);
            pipe.write(outBytes);
        }
    }

    @Override
    public void close() throws IOException {
        pipe.close();
    }
}
