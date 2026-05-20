// SPDX-License-Identifier: MIT OR Apache-2.0

/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.core;

import com.bluminal.barista.barback.proto.ActionRequest;
import com.bluminal.barista.barback.proto.ActionResult;
import com.bluminal.barista.barback.proto.Credential;
import com.bluminal.barista.barback.proto.Error;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.logging.Level;
import java.util.logging.Logger;
import java.util.stream.Stream;

/**
 * Adapter that turns an {@link ActionRequest} into a call against the
 * resident {@link EmbeddedMaven} core. Plumbs repository-deploy
 * credentials through the Maven CLI surface by writing an ephemeral
 * {@code settings.xml} per action and splicing {@code -s <path>} into
 * the embedded invocation.
 *
 * <p><b>Why an adapter, not a method on {@link EmbeddedMaven}?</b>
 * {@code EmbeddedMaven} owns the resident invoker, the
 * eviction/cache plumbing, and the {@link Path}/argv construction for
 * a single mojo run — adding wire-shape concerns (credentials decode,
 * temp-file lifecycle, BAR-DEPLOY-AUTH error classification) directly
 * inside that class would couple the embedded-core lifetime to the
 * IPC schema. This adapter is the seam where wire semantics meet
 * core execution: it is owned by the daemon's connection-handling
 * path and built once at {@link com.bluminal.barista.barback.Server}
 * startup.
 *
 * <p><b>Settings.xml lifecycle.</b> When {@link ActionRequest#getCredentials()}
 * carries one or more {@link Credential} entries, the adapter writes
 * a single-action {@code settings.xml} into a fresh temp directory,
 * appends {@code -s <path>} to the action's
 * {@link ActionRequest#getExtraMvnArgsList() extra_mvn_args}, and
 * deletes the directory after the embedded invocation returns. The
 * file is mode-0600 by best effort (see {@link #writeEphemeralSettings})
 * so other users on the host cannot inspect the materialized
 * passwords; the CLI-side {@code CredentialsEnvelope} zero-on-drop
 * guarantee is enforced by the IPC layer, so the lifetime of any
 * decrypted password inside the daemon is bounded by this method's
 * stack frame.
 *
 * <p><b>BAR-DEPLOY-AUTH error classification.</b> When the embedded
 * Maven invocation fails for a {@code deploy}-shaped action, the
 * adapter inspects the failure message for the canonical "401"/"403"
 * markers maven-deploy-plugin emits when Nexus / Artifactory rejects
 * the request, and rewrites the {@link ActionResult#getError()} code
 * to {@code BAR-DEPLOY-AUTH-INVALID}. A deploy action with no
 * credentials whose failure also matches the 401 pattern is rewritten
 * as {@code BAR-DEPLOY-AUTH-MISSING}. Every other failure shape
 * passes through unmodified.
 */
public final class EmbeddedMavenActionDispatcher implements AutoCloseable {

    private static final Logger LOG =
            Logger.getLogger(EmbeddedMavenActionDispatcher.class.getName());

    /** Error code: deploy authentication failed (server rejected). */
    public static final String CODE_DEPLOY_AUTH_INVALID = "BAR-DEPLOY-AUTH-INVALID";

    /** Error code: deploy attempted with no credentials in the envelope. */
    public static final String CODE_DEPLOY_AUTH_MISSING = "BAR-DEPLOY-AUTH-MISSING";

    /**
     * Filename inside the per-action temp directory. The Maven CLI
     * doesn't care about the basename — {@code -s} takes any path —
     * but a stable name is friendlier in failure logs.
     */
    private static final String EPHEMERAL_SETTINGS_NAME = "barback-action-settings.xml";

    private final EmbeddedMaven embedded;

    /**
     * Wrap the given {@link EmbeddedMaven} as an action dispatcher.
     * Lifetime of {@code embedded} is borrowed, not transferred:
     * closing this dispatcher does NOT close the underlying core.
     * (The daemon's {@code Server} owns the core directly.)
     */
    public EmbeddedMavenActionDispatcher(EmbeddedMaven embedded) {
        this.embedded = embedded;
    }

    /**
     * Run one action against the resident embedded Maven core.
     * Synchronous; the caller (the daemon worker pool) provides
     * concurrency by invoking this from multiple threads. The
     * embedded core's own {@code executionLock} serialises actions
     * inside the JVM, so calls from different worker threads queue
     * naturally.
     */
    public ActionResult dispatch(ActionRequest action) {
        if (!action.hasCredentials() || action.getCredentials().getEntriesCount() == 0) {
            ActionResult result = embedded.execute(action);
            return maybeRewriteAuthError(action, result, /* hadCredentials= */ false);
        }

        Path tempDir = null;
        try {
            tempDir = Files.createTempDirectory("barback-settings-");
            Path settings = writeEphemeralSettings(tempDir, action);
            ActionRequest augmented = action.toBuilder()
                    .addExtraMvnArgs("-s")
                    .addExtraMvnArgs(settings.toAbsolutePath().toString())
                    .build();
            ActionResult result = embedded.execute(augmented);
            return maybeRewriteAuthError(action, result, /* hadCredentials= */ true);
        } catch (IOException e) {
            // Producing the ephemeral settings.xml failed — surface a
            // clean error to the CLI rather than passing the
            // un-credentialed action to Maven (which would race and
            // silently fail auth).
            return failed(action, CODE_DEPLOY_AUTH_INVALID,
                    "failed to materialise ephemeral settings.xml for credentials: "
                            + e.getClass().getSimpleName() + ": " + e.getMessage());
        } finally {
            if (tempDir != null) {
                deleteRecursive(tempDir);
            }
        }
    }

    /**
     * Inspect a terminal {@link ActionResult} for the deploy-auth
     * failure patterns. The classifier runs only when the action's
     * {@code mojo_coords} starts with {@code "deploy"} (the lifecycle
     * phase name the CLI submits) — the {@code maven-deploy-plugin}
     * is the only mojo that produces 401/403 transport errors in
     * the v0.1 surface, and other phases that authenticate (e.g.
     * authenticated mirror fetches during {@code compile}) leave
     * Maven's resolver-side messaging untouched.
     */
    private ActionResult maybeRewriteAuthError(
            ActionRequest action, ActionResult result, boolean hadCredentials) {
        if (result.getStatus() != ActionResult.Status.FAILURE) {
            return result;
        }
        String coords = action.getMojoCoords();
        if (!coords.startsWith("deploy")
                && !coords.contains(":deploy")
                && !coords.contains("maven-deploy-plugin")) {
            return result;
        }
        String message = result.getFailureMessage();
        if (!looksLikeAuthFailure(message)) {
            return result;
        }
        String newCode = hadCredentials ? CODE_DEPLOY_AUTH_INVALID : CODE_DEPLOY_AUTH_MISSING;
        Error.Builder err = result.getError().toBuilder().setCode(newCode);
        return result.toBuilder()
                .setError(err.build())
                .setFailureMessage(prefixDeployHint(newCode, message))
                .build();
    }

    /**
     * Match the canonical Maven-side strings the deploy plugin emits
     * when the remote repository rejects credentials. Maven's HTTP
     * transport surfaces the upstream status code in the failure
     * message verbatim ("Returned: 401 Unauthorized", "status code:
     * 403"); both forms are checked. We don't try to be exhaustive
     * here — a false negative falls through to the generic
     * {@code BAR-MAVEN-CORE} error code, which is recoverable; a
     * false positive would mislabel a non-auth failure as auth,
     * which is worse.
     */
    private static boolean looksLikeAuthFailure(String message) {
        if (message == null || message.isEmpty()) {
            return false;
        }
        return message.contains(" 401 ")
                || message.contains(": 401")
                || message.contains("401 Unauthorized")
                || message.contains(" 403 ")
                || message.contains(": 403")
                || message.contains("403 Forbidden")
                || message.contains("authentication failed")
                || message.contains("Authentication failed");
    }

    private static String prefixDeployHint(String code, String original) {
        String hint;
        if (CODE_DEPLOY_AUTH_MISSING.equals(code)) {
            hint = "deploy failed: no credentials were sent for the target repository. "
                    + "Configure <server> credentials in your settings.xml for the "
                    + "matching <distributionManagement><repository><id>.";
        } else {
            hint = "deploy failed: the remote repository rejected the credentials. "
                    + "Check the <server> entry in your settings.xml for the matching id.";
        }
        return code + ": " + hint + System.lineSeparator() + original;
    }

    /**
     * Materialise the {@link com.bluminal.barista.barback.proto.CredentialsEnvelope}
     * into a Maven-compatible {@code settings.xml}. Only the
     * {@code <servers>} block is populated — the daemon does not need
     * to round-trip mirrors / profiles through this file because the
     * primary settings.xml the user maintains is layered separately
     * by Maven itself (the ephemeral file is consulted ONLY for
     * server entries via the {@code -s} flag, which overrides the
     * user-level settings; we don't try to merge here, by design — the
     * CLI is the authority on which servers the action's repository
     * resolves against).
     */
    private static Path writeEphemeralSettings(Path dir, ActionRequest action) throws IOException {
        Path file = dir.resolve(EPHEMERAL_SETTINGS_NAME);
        StringBuilder sb = new StringBuilder(256);
        sb.append("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        sb.append("<settings xmlns=\"http://maven.apache.org/SETTINGS/1.2.0\"\n");
        sb.append("          xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n");
        sb.append("          xsi:schemaLocation=\"http://maven.apache.org/SETTINGS/1.2.0 "
                + "http://maven.apache.org/xsd/settings-1.2.0.xsd\">\n");
        sb.append("  <servers>\n");
        for (int i = 0; i < action.getCredentials().getEntriesCount(); i++) {
            Credential c = action.getCredentials().getEntries(i);
            sb.append("    <server>\n");
            sb.append("      <id>").append(escapeXml(c.getServerId())).append("</id>\n");
            if (!c.getUsername().isEmpty()) {
                sb.append("      <username>").append(escapeXml(c.getUsername())).append("</username>\n");
            }
            switch (c.getSecretCase()) {
                case PASSWORD -> sb.append("      <password>").append(escapeXml(c.getPassword()))
                        .append("</password>\n");
                case TOKEN -> sb.append("      <password>").append(escapeXml(c.getToken()))
                        .append("</password>\n");
                case SSH_KEY -> {
                    // The Maven schema accepts <privateKey> as a path,
                    // not inline material. We don't write the PEM
                    // contents into this file — that would require
                    // an additional temp file and pin the keypath
                    // semantics in the daemon. For v0.1 we surface a
                    // log warning and fall through to a credential-less
                    // entry; SSH-based deploy auth is gated by
                    // BAR-DEPLOY-AUTH-INVALID at the Maven level when
                    // the transport actually fails. (PRD §15 follow-up.)
                    LOG.log(Level.WARNING,
                            () -> "SSH key credentials for server '" + c.getServerId()
                                    + "' are not yet wired through the daemon settings.xml; "
                                    + "the action will run with no credentials for that server.");
                }
                case SECRET_NOT_SET -> {
                    // Username-only entry (e.g. SSH-agent flows). Maven
                    // tolerates a <server> with no <password>; leave
                    // it as-is.
                }
            }
            sb.append("    </server>\n");
        }
        sb.append("  </servers>\n");
        sb.append("</settings>\n");
        Files.writeString(file, sb.toString(), StandardCharsets.UTF_8);
        // Best-effort 0600 — the daemon's parent dir is already user-
        // private (~/.barista/run on the canonical config), and the
        // temp dir is mode-0700 by default on the JDK's tempdir creator;
        // pinning the file mode is a belt-and-suspenders step. On
        // Windows the POSIX permission set isn't available; the
        // user-SID-DACL on the daemon's IPC path is the analogue.
        try {
            Files.setPosixFilePermissions(file,
                    java.util.Set.of(java.nio.file.attribute.PosixFilePermission.OWNER_READ,
                            java.nio.file.attribute.PosixFilePermission.OWNER_WRITE));
        } catch (UnsupportedOperationException | IOException ignored) {
            // Best effort only.
        }
        return file;
    }

    private static String escapeXml(String s) {
        if (s == null || s.isEmpty()) {
            return "";
        }
        StringBuilder b = new StringBuilder(s.length() + 8);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '<' -> b.append("&lt;");
                case '>' -> b.append("&gt;");
                case '&' -> b.append("&amp;");
                case '"' -> b.append("&quot;");
                case '\'' -> b.append("&apos;");
                default -> b.append(c);
            }
        }
        return b.toString();
    }

    private static void deleteRecursive(Path root) {
        if (!Files.exists(root)) {
            return;
        }
        try (Stream<Path> walk = Files.walk(root)) {
            walk.sorted(Comparator.reverseOrder()).forEach(p -> {
                try {
                    Files.deleteIfExists(p);
                } catch (IOException ignored) {
                    // Best-effort cleanup; the host's tmp reaper will
                    // sweep stale dirs on a long-enough horizon.
                }
            });
        } catch (IOException ignored) {
            // Same rationale; cannot block the daemon on tmp cleanup.
        }
    }

    private static ActionResult failed(ActionRequest action, String code, String message) {
        Error err = Error.newBuilder()
                .setCode(code)
                .setMessage(message)
                .setActionId(action.getActionId())
                .build();
        return ActionResult.newBuilder()
                .setActionId(action.getActionId())
                .setStatus(ActionResult.Status.FAILURE)
                .setExitCode(1)
                .setError(err)
                .setFailureMessage(message)
                .build();
    }

    @Override
    public void close() {
        // The embedded core's lifetime is owned by Server; nothing to
        // release here. The AutoCloseable contract is kept so callers
        // can use try-with-resources symmetrically with future
        // dispatcher implementations that own auxiliary state.
    }
}
