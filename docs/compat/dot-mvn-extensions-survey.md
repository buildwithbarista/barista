# `.mvn/extensions.xml` corpus-impact survey

Barista does not yet apply Maven build extensions (`.mvn/extensions.xml`). Full extensions support is out of scope for v0.1 — but visibility into how common extensions are in real-world Maven projects keeps the v0.2 scoping decision data-driven.

This document is the running survey, regenerated as the test corpus grows.

## Method

1. Materialize the corpus: `bash scripts/materialize-corpus.sh`.
2. For each project, check for `.mvn/extensions.xml`. If present, parse the extensions list and tally per-extension counts (keyed by `groupId:artifactId`).
3. Regenerate this document via `cargo run -p barista-config --example survey-extensions --release > docs/compat/dot-mvn-extensions-survey.md`.

## Current findings (regenerated 2026-05-13)

- **Total projects surveyed:** 5
- **Projects using `.mvn/extensions.xml`:** 0 / 5
- **Extensions seen:** none.

## Interpretation

None of the currently materialized corpus projects ship a `.mvn/extensions.xml`. The corpus is small (5 projects, growing toward ~100) and skews toward Apache Commons / FasterXML / SLF4J / AssertJ — libraries that prefer to pin tooling in `pom.xml` rather than via build extensions. As the corpus grows to include projects with richer build environments (Spring, Quarkus, Hibernate, gRPC-Java, large internal-style monorepos), this baseline number will shift; the survey will surface that change.

For v0.2 scoping: extensions are not blocking *this* corpus, but two extension families warrant pre-emptive planning because they appear at the moment a project does adopt extensions:

- **`os-maven-plugin`** (`kr.motd.maven:os-maven-plugin`) — sets `os.detected.*` properties used by protobuf and other native-bridge plugins. Without it, dependent plugins fail at execution time.
- **`maven-build-cache-extension`** (`com.gradle:maven-build-cache-extension`, formerly `org.apache.maven.extensions:maven-build-cache-extension`) — overlaps with Barista's content-addressed cache and may conflict if both are active.

## Open questions

- Should Barista support the `maven-build-cache-extension` natively (its goals overlap with Barista's content-addressed cache)?
- Which extensions change effective-POM output? Those are the ones that affect the resolver and need top priority.
- For extensions that *don't* affect resolution (e.g. reporting-only), is warn-and-skip an acceptable long-term policy?
