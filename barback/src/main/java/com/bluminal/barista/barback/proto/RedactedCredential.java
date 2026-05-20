// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.proto;

import java.util.Objects;

/**
 * Safe-by-default view over a generated {@link Credential}.
 *
 * <p>The protobuf-java compiler emits a {@code toString()} on every message
 * that prints the full field set, including the bytes of {@code password},
 * {@code token}, and {@code ssh_key.private_key_pem}. That output is
 * dangerous: a single {@code log.debug(envelope)} on the daemon side
 * exfiltrates a decrypted credential into the log stream, in violation of
 * the {@code CredentialsEnvelope} contract documented in
 * {@code proto/barista/v1/worker.proto}.
 *
 * <p>This adapter wraps a {@link Credential} and overrides {@code toString}
 * to render {@code server_id}, {@code username}, and the
 * <em>shape</em> of the secret (which variant of the {@code secret} oneof
 * is set) without ever printing the secret material itself. Application
 * code that needs to log or display a credential routes it through this
 * adapter; code that needs to actually authenticate against a server
 * extracts the underlying {@link Credential} via {@link #unwrap()} and
 * MUST NOT call {@code toString()} on it.
 *
 * <p>This adapter is <strong>not</strong> a zeroize-on-drop wrapper. The
 * lifetime + zero-on-drop story for credential bytes is owned by Task 5
 * of milestone 4.1 (IPC transport); that task extends the redaction
 * story to include receive-buffer zeroization in the framing layer.
 * What this class provides today is just the toString-safety contract.
 *
 * <p>The adapter follows protobuf's redaction conventions:
 * <ul>
 *   <li>identifying fields ({@code server_id}, {@code username})
 *       remain visible — they are the keys diagnostics use to refer
 *       to a credential entry;</li>
 *   <li>secret variants render as a literal {@code "[REDACTED:&lt;kind&gt;]"}
 *       marker so the kind-of-secret (password / token / ssh_key) is
 *       discoverable for debugging without leaking the secret value;</li>
 *   <li>the empty-secret case renders as {@code "[NO_SECRET]"}
 *       to distinguish "username-only" entries from corrupted ones.</li>
 * </ul>
 */
public final class RedactedCredential {

    private final Credential delegate;

    private RedactedCredential(Credential delegate) {
        this.delegate = Objects.requireNonNull(delegate, "delegate");
    }

    /**
     * Wrap a {@link Credential} for safe rendering. The original message
     * is held by reference; mutations on the underlying message (none are
     * possible — generated protobuf messages are immutable) would be
     * reflected here.
     */
    public static RedactedCredential of(Credential delegate) {
        return new RedactedCredential(delegate);
    }

    /**
     * The underlying generated {@link Credential}. Callers extracting the
     * secret to authenticate against a server use this; callers logging
     * or diagnostic-rendering the credential MUST NOT touch the
     * underlying message and instead rely on {@link #toString()} on this
     * adapter.
     */
    public Credential unwrap() {
        return delegate;
    }

    /**
     * Render the credential without leaking secret material. The output
     * shape is stable and documented as part of the diagnostic contract;
     * it is suitable for inclusion in error messages, logs, and exception
     * strings.
     *
     * <p>Example outputs:
     * <pre>
     *   Credential{server_id="central", username="alice", secret=[REDACTED:PASSWORD]}
     *   Credential{server_id="central", username="alice", secret=[REDACTED:TOKEN]}
     *   Credential{server_id="central", username="alice", secret=[REDACTED:SSH_KEY]}
     *   Credential{server_id="central", username="alice", secret=[NO_SECRET]}
     * </pre>
     */
    @Override
    public String toString() {
        return "Credential{"
                + "server_id=" + quote(delegate.getServerId())
                + ", username=" + quote(delegate.getUsername())
                + ", secret=" + redactedSecret(delegate)
                + "}";
    }

    @Override
    public boolean equals(Object o) {
        if (this == o) {
            return true;
        }
        if (!(o instanceof RedactedCredential other)) {
            return false;
        }
        return delegate.equals(other.delegate);
    }

    @Override
    public int hashCode() {
        return delegate.hashCode();
    }

    /**
     * Static helper for the common case where a caller has a generated
     * {@link Credential} in hand and just wants a safe string for a log
     * line, without holding a wrapper reference. Equivalent to
     * {@code RedactedCredential.of(credential).toString()}.
     */
    public static String redactedToString(Credential credential) {
        return of(credential).toString();
    }

    private static String quote(String s) {
        return s == null ? "null" : "\"" + s + "\"";
    }

    private static String redactedSecret(Credential c) {
        return switch (c.getSecretCase()) {
            case PASSWORD -> "[REDACTED:PASSWORD]";
            case TOKEN -> "[REDACTED:TOKEN]";
            case SSH_KEY -> "[REDACTED:SSH_KEY]";
            case SECRET_NOT_SET -> "[NO_SECRET]";
        };
    }
}
