// SPDX-License-Identifier: MIT OR Apache-2.0

package com.bluminal.barista.barback;

/**
 * Placeholder type for the barback worker daemon.
 *
 * <p>This class exists so the Maven build has at least one compilable
 * source. Real implementation lands in a subsequent release.
 */
public final class Placeholder {

    private Placeholder() {
        // no instances
    }

    /**
     * Returns a human-readable identifier for the current build.
     *
     * @return the string {@code "barback 0.1.0-alpha.0 (scaffold)"}.
     */
    public static String version() {
        return "barback 0.1.0-alpha.0 (scaffold)";
    }
}
