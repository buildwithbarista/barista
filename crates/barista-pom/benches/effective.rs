// SPDX-License-Identifier: MIT OR Apache-2.0

// Integration-test / example / benchmark target — workspace security
// lints are allowed here. Panic-on-misuse (`unwrap()`/`expect()`/`panic!`)
// is the documented contract for failing a test loudly. This allow block
// keeps the crate root's `#![allow(...)]` from being silently dropped by
// the separate compilation unit each test file forms.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

//! Criterion microbenchmarks for the effective-POM pipeline.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p barista-pom --bench effective
//! ```
//!
//! These are informative numbers used to spot egregious regressions
//! during development. The canonical regression detector lives in the
//! Tier-2 gate.

use std::hint::black_box;

use barista_pom::{ParentResolver, RawParent, RawPom, build_effective, parse_pom};
use criterion::{Criterion, criterion_group, criterion_main};

/// Resolver that errors if called. The bench POMs declare no parent,
/// so this should never fire — but it keeps the bench honest by
/// failing loudly if a parent ever sneaks in.
struct NoParentResolver;

impl ParentResolver for NoParentResolver {
    fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String> {
        Err(format!(
            "NoParentResolver invoked for {}:{}:{} — bench POM should declare no parent",
            parent.group_id, parent.artifact_id, parent.version
        ))
    }
}

/// A no-parent POM with ~5 `${...}` placeholders threaded through
/// properties, dep versions, and the build `<finalName>`. Used to
/// measure the interpolation pass during `build_effective`.
const POM_WITH_PLACEHOLDERS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
    <modelVersion>4.0.0</modelVersion>
    <groupId>com.example.widgets</groupId>
    <artifactId>widget-core</artifactId>
    <version>1.0.0</version>
    <properties>
        <jackson.version>2.17.2</jackson.version>
        <slf4j.version>2.0.13</slf4j.version>
        <flavor>vanilla</flavor>
    </properties>
    <dependencies>
        <dependency>
            <groupId>com.fasterxml.jackson.core</groupId>
            <artifactId>jackson-databind</artifactId>
            <version>${jackson.version}</version>
        </dependency>
        <dependency>
            <groupId>com.fasterxml.jackson.core</groupId>
            <artifactId>jackson-core</artifactId>
            <version>${jackson.version}</version>
        </dependency>
        <dependency>
            <groupId>org.slf4j</groupId>
            <artifactId>slf4j-api</artifactId>
            <version>${slf4j.version}</version>
        </dependency>
    </dependencies>
    <build>
        <finalName>${project.artifactId}-${project.version}-${flavor}</finalName>
    </build>
</project>
"#;

/// A no-parent POM with no `${...}` placeholders. Isolates the
/// parent-chain-walk + merge fast-path (which short-circuits to a
/// clone when there is no parent) from interpolation.
const POM_NO_PLACEHOLDERS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
    <modelVersion>4.0.0</modelVersion>
    <groupId>com.example.widgets</groupId>
    <artifactId>widget-core</artifactId>
    <version>1.0.0</version>
    <dependencies>
        <dependency>
            <groupId>com.fasterxml.jackson.core</groupId>
            <artifactId>jackson-databind</artifactId>
            <version>2.17.2</version>
        </dependency>
        <dependency>
            <groupId>org.slf4j</groupId>
            <artifactId>slf4j-api</artifactId>
            <version>2.0.13</version>
        </dependency>
        <dependency>
            <groupId>org.apache.commons</groupId>
            <artifactId>commons-lang3</artifactId>
            <version>3.14.0</version>
        </dependency>
    </dependencies>
</project>
"#;

/// Measure `build_effective` on a small no-parent POM. Exercises the
/// chain walk (which immediately terminates) plus the interpolation
/// pass (which short-circuits because there are no placeholders).
fn bench_build_effective(c: &mut Criterion) {
    let raw = parse_pom(POM_NO_PLACEHOLDERS).unwrap();
    c.bench_function(
        "Effective: build_effective on no-parent small POM",
        |bench| {
            bench.iter(|| {
                let mut resolver = NoParentResolver;
                let eff = build_effective(black_box(raw.clone()), &mut resolver).unwrap();
                black_box(eff);
            });
        },
    );
}

/// Measure `build_effective` on a small no-parent POM that contains
/// ~5 `${...}` placeholders. Isolates the interpolation pass cost
/// versus `bench_build_effective` (same shape, no placeholders).
fn bench_interpolate_simple(c: &mut Criterion) {
    let raw = parse_pom(POM_WITH_PLACEHOLDERS).unwrap();
    c.bench_function("Effective: interpolate_string (~5 placeholders)", |bench| {
        bench.iter(|| {
            let mut resolver = NoParentResolver;
            let eff = build_effective(black_box(raw.clone()), &mut resolver).unwrap();
            black_box(eff);
        });
    });
}

criterion_group!(benches, bench_build_effective, bench_interpolate_simple);
criterion_main!(benches);
