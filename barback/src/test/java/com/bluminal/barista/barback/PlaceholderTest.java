// SPDX-License-Identifier: MIT OR Apache-2.0

package com.bluminal.barista.barback;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class PlaceholderTest {

    @Test
    void versionStringIsStable() {
        assertEquals("barback 0.1.0-alpha.0 (scaffold)", Placeholder.version());
    }
}
