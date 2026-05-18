package example;

/**
 * Trivial class for the parity-check baseline fixture.
 * Source is deliberately tiny so the meta-test is fast.
 */
public final class Hello {
    private Hello() {}

    public static String greet() {
        return "hi";
    }
}
