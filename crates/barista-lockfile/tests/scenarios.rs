//! Spike scenarios for the semantic lockfile diff renderer.
//!
//! Each scenario exercises a "large change shape" that occurs in
//! real-world projects, and asserts the rendered output via `insta`
//! snapshots. Snapshots double as reviewable design artifacts: if
//! they ever change unintentionally, the human-readable format has
//! drifted and the change requires explicit review.

use barista_lockfile::diff::{LockEntry, diff, render};

fn mk(g: &str, a: &str, v: &str) -> LockEntry {
    LockEntry {
        group_id: g.to_string(),
        artifact_id: a.to_string(),
        version: v.to_string(),
        scope: "compile".to_string(),
        classifier: None,
        type_: "jar".to_string(),
    }
}

fn mks(g: &str, a: &str, v: &str, scope: &str) -> LockEntry {
    let mut x = mk(g, a, v);
    x.scope = scope.to_string();
    x
}

/// Scenario 1: a Spring Boot 3.2.x -> 3.3.x bump. ~30 transitives
/// upgrade in lockstep, one new transitive arrives, one disappears.
#[test]
fn scenario_spring_boot_bump() {
    let old_version = "6.1.4";
    let new_version = "6.1.6";
    let springs = [
        "spring-aop",
        "spring-beans",
        "spring-context",
        "spring-context-support",
        "spring-core",
        "spring-expression",
        "spring-jcl",
        "spring-jdbc",
        "spring-messaging",
        "spring-orm",
        "spring-oxm",
        "spring-tx",
        "spring-web",
        "spring-webflux",
        "spring-webmvc",
    ];
    let boots = [
        "spring-boot",
        "spring-boot-autoconfigure",
        "spring-boot-starter",
        "spring-boot-starter-actuator",
        "spring-boot-starter-json",
        "spring-boot-starter-logging",
        "spring-boot-starter-tomcat",
        "spring-boot-starter-web",
    ];
    let micrometer = [
        "micrometer-commons",
        "micrometer-core",
        "micrometer-jakarta9",
        "micrometer-observation",
        "micrometer-registry-prometheus",
    ];

    let mut left: Vec<LockEntry> = Vec::new();
    let mut right: Vec<LockEntry> = Vec::new();

    for a in springs {
        left.push(mk("org.springframework", a, old_version));
        right.push(mk("org.springframework", a, new_version));
    }
    for a in boots {
        left.push(mk("org.springframework.boot", a, "3.2.5"));
        right.push(mk("org.springframework.boot", a, "3.3.0"));
    }
    for a in micrometer {
        left.push(mk("io.micrometer", a, "1.12.5"));
        right.push(mk("io.micrometer", a, "1.13.0"));
    }

    // Stable, unchanged transitive.
    left.push(mk("org.slf4j", "slf4j-api", "2.0.13"));
    right.push(mk("org.slf4j", "slf4j-api", "2.0.13"));

    // New transitive: Tomcat 10.1 bumped to a version that pulls a
    // new helper jar.
    right.push(mk("org.apache.tomcat.embed", "tomcat-embed-el", "10.1.24"));

    // Removed transitive: deprecated in 3.3.
    left.push(mk("jakarta.annotation", "jakarta.annotation-api", "2.1.1"));

    let d = diff(&left, &right);
    insta::assert_snapshot!("spring_boot_bump", render(&d));
}

/// Scenario 2: Jackson minor bump plus adding an observability
/// stack. Mixes upgraded entries with added entries.
#[test]
fn scenario_jackson_plus_observability() {
    let jackson_old = "2.16.1";
    let jackson_new = "2.17.0";
    let jackson_artifacts = [
        ("com.fasterxml.jackson.core", "jackson-annotations"),
        ("com.fasterxml.jackson.core", "jackson-core"),
        ("com.fasterxml.jackson.core", "jackson-databind"),
        ("com.fasterxml.jackson.datatype", "jackson-datatype-jdk8"),
        ("com.fasterxml.jackson.datatype", "jackson-datatype-jsr310"),
        ("com.fasterxml.jackson.module", "jackson-module-parameter-names"),
    ];

    let mut left = Vec::new();
    let mut right = Vec::new();
    for (g, a) in jackson_artifacts {
        left.push(mk(g, a, jackson_old));
        right.push(mk(g, a, jackson_new));
    }

    // Existing unchanged baseline.
    for (g, a, v) in [
        ("org.slf4j", "slf4j-api", "2.0.13"),
        ("ch.qos.logback", "logback-classic", "1.5.6"),
        ("ch.qos.logback", "logback-core", "1.5.6"),
    ] {
        left.push(mk(g, a, v));
        right.push(mk(g, a, v));
    }

    // Newly added: micrometer + opentelemetry observability stack.
    for (g, a, v) in [
        ("io.micrometer", "micrometer-core", "1.12.5"),
        ("io.micrometer", "micrometer-observation", "1.12.5"),
        ("io.micrometer", "micrometer-tracing", "1.2.5"),
        ("io.micrometer", "micrometer-tracing-bridge-otel", "1.2.5"),
        ("io.opentelemetry", "opentelemetry-api", "1.36.0"),
        ("io.opentelemetry", "opentelemetry-context", "1.36.0"),
    ] {
        right.push(mk(g, a, v));
    }

    let d = diff(&left, &right);
    insta::assert_snapshot!("jackson_plus_observability", render(&d));
}

/// Scenario 3: switching test framework from JUnit 4 to JUnit 5.
/// Pure removals + pure additions, no overlap on coords.
#[test]
fn scenario_junit_4_to_5() {
    let left = vec![
        mks("junit", "junit", "4.13.2", "test"),
        mks("org.hamcrest", "hamcrest-core", "1.3", "test"),
        mks("org.hamcrest", "hamcrest-library", "1.3", "test"),
        mks("org.mockito", "mockito-core", "4.11.0", "test"),
        mks("org.mockito", "mockito-junit-jupiter", "4.11.0", "test"),
    ];
    let right = vec![
        mks("org.junit.jupiter", "junit-jupiter", "5.10.2", "test"),
        mks("org.junit.jupiter", "junit-jupiter-api", "5.10.2", "test"),
        mks("org.junit.jupiter", "junit-jupiter-engine", "5.10.2", "test"),
        mks("org.junit.jupiter", "junit-jupiter-params", "5.10.2", "test"),
        mks("org.junit.platform", "junit-platform-commons", "1.10.2", "test"),
        mks("org.junit.platform", "junit-platform-engine", "1.10.2", "test"),
        mks("org.opentest4j", "opentest4j", "1.3.0", "test"),
        mks("org.apiguardian", "apiguardian-api", "1.1.2", "test"),
    ];

    let d = diff(&left, &right);
    insta::assert_snapshot!("junit_4_to_5", render(&d));
}
