package com.example.moda;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertTrue;

class GreeterTest {
    @Test void greets() {
        assertTrue(Greeter.greet("spike").contains("mod-a"));
    }
}
