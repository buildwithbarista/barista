package example;

/// Single class for the Windows smoke-build fixture; the body is
/// intentionally trivial so a smoke-build failure unambiguously
/// points at the CLI / Maven invocation, not the compiler.
public final class Hello {
    private Hello() {}

    public static String greet() {
        return "hello, barista";
    }

    public static void main(String[] args) {
        System.out.println(greet());
    }
}
