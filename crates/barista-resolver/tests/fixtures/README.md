# Resolver test fixtures

Pre-fetched POM + `maven-metadata.xml` snapshots from real Maven
Central artifacts. Loaded by `FixtureMetadataSource` in
`tests/common/fixture_source.rs` to give resolver tests an offline
`MetadataSource` that doesn't depend on the on-disk cache.

## Layout

```
<groupId>/<artifactId>/
  maven-metadata.xml         # group:artifact level (optional)
  <version>/
    pom.xml                  # group:artifact:version level
```

`groupId` directories use the **flat dotted form** (e.g.
`org.apache.commons/`), not the Maven on-disk repository layout
(`org/apache/commons/`). This keeps the fixtures human-greppable
and matches how POMs declare coordinates.

## Adding a fixture

1. Fetch the POM from Maven Central:

   ```bash
   GROUP_SLASHED=org/apache/commons
   ARTIFACT=commons-lang3
   VERSION=3.14.0
   curl -fsSL -o pom.xml \
     "https://repo.maven.apache.org/maven2/${GROUP_SLASHED}/${ARTIFACT}/${VERSION}/${ARTIFACT}-${VERSION}.pom"
   ```

2. Place it under `<groupId>/<artifactId>/<version>/pom.xml` (dotted
   groupId, not slashed).

3. (Optional but recommended) Fetch the artifact-level metadata:

   ```bash
   curl -fsSL -o maven-metadata.xml \
     "https://repo.maven.apache.org/maven2/${GROUP_SLASHED}/${ARTIFACT}/maven-metadata.xml"
   ```

4. Run `cargo test -p barista-resolver` to confirm
   `FixtureMetadataSource::load_default()` parses everything cleanly.

## Hand-written vs upstream POMs

The fixtures committed here are **lightly trimmed copies** of real
upstream POMs — enough fields to exercise the resolver, with
non-resolver-relevant metadata (developers, scm, mailingLists, ci,
distributionManagement) removed for readability. The POMs are still
schema-valid `<modelVersion>4.0.0</modelVersion>` documents.

## Regen automation (placeholder)

A regen script lives at
`crates/barista-test-fixtures/scripts/snapshot-corpus-metadata.sh`.
It currently prints the documented manual workflow above; full
automation is deferred past v0.1 (the fixture set is tiny and rarely
churns).
