package build.barista.corpus.p01;

import com.fasterxml.jackson.core.JsonFactory;
import org.apache.commons.lang3.StringUtils;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Minimal entry point for the P01 floor-case corpus project.
 *
 * <p>This class exists purely so the Java compiler has something to
 * compile during the build. Every direct dependency is referenced at
 * least once so {@code mvn package} does not warn about unused
 * declarations and so a build that silently dropped a dependency would
 * fail compilation rather than succeed quietly.
 */
public final class HelloWorld {

    private static final Logger LOG = LoggerFactory.getLogger(HelloWorld.class);

    private HelloWorld() {
        // utility class
    }

    public static void main(String[] args) {
        // Touch each dependency so the compile step exercises them.
        JsonFactory factory = new JsonFactory();
        String banner = StringUtils.repeat('=', 20);
        LOG.info("{} hello, world ({}) {}", banner, factory.getClass().getSimpleName(), banner);
    }
}
