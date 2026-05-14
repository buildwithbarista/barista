//! Round-trip property tests for the lockfile schema.
//!
//! For each hand-crafted fixture we run:
//!
//! ```text
//! toml1 = lf.to_toml()
//! lf2   = Lockfile::from_toml(&toml1)
//! toml2 = lf2.to_toml()
//! assert toml1 == toml2   (byte-stable: serializer is a fixed point on its own output)
//! assert lf   == lf2      (semantic preservation: deserializer recovers every field)
//! ```
//!
//! The byte-stability check catches accidental nondeterminism in the
//! serializer (e.g. unordered maps), and the structural equality check
//! catches lossy serialization (a field that survives the first round
//! but mutates / disappears on the second).
//!
//! ### Why no timestamp normalization is needed
//!
//! `Lockfile::new` calls `now()` exactly once at construction. Each
//! fixture is built once; the round-trip never re-constructs, so the
//! `generated_at` string is carried through both serializations
//! unchanged.
//!
//! ### Fixture count
//!
//! Fifty named fixtures are assembled in [`build_fixtures`], covering:
//!
//! - empty / single / multi reactor shapes
//! - one fixture per Maven scope
//! - classifier-distinct and type-distinct entries
//! - optional dependencies
//! - SHA-1 present / absent
//! - `from_path` depths 1 / 3 / 10
//! - populated `exclusions`
//! - `snapshot_resolution` (timestamped SNAPSHOT versions)
//! - `SettingsSnapshot` with mirrors and repositories
//! - 100+ entry stress shape
//! - assorted edge cases

use barista_lockfile::schema::{
    Exclusion, Lockfile, LockfileEntry, MirrorRef, ReactorEntry, RepositoryRef, SettingsSnapshot,
};

// ----- builders --------------------------------------------------------------

fn empty_lockfile() -> Lockfile {
    Lockfile::new("a".repeat(64), "b".repeat(64))
}

/// Construct a minimal, valid `LockfileEntry` for `group:artifact` and version.
/// All optional fields default to "absent" so callers can opt-in field by field.
fn entry(coords: &str, version: &str) -> LockfileEntry {
    LockfileEntry {
        coords: coords.to_string(),
        version: version.to_string(),
        scope: "compile".to_string(),
        optional: false,
        sha256: "0".repeat(64),
        sha1: None,
        size_bytes: 1024,
        source_url: format!(
            "https://repo.maven.apache.org/maven2/{}/{}-{}.jar",
            coords.replace([':', '.'], "/"),
            coords.split(':').next_back().unwrap_or(""),
            version,
        ),
        etag: None,
        last_modified: None,
        classifier: None,
        type_: "jar".to_string(),
        from_path: Vec::new(),
        depth: 0,
        snapshot_resolution: None,
        exclusions: Vec::new(),
    }
}

fn reactor(coords: &str, version: &str, relative_path: &str) -> ReactorEntry {
    ReactorEntry {
        coords: coords.to_string(),
        version: version.to_string(),
        relative_path: relative_path.to_string(),
    }
}

// ----- fixture factories -----------------------------------------------------

fn fx_empty() -> Lockfile {
    empty_lockfile()
}

fn fx_single_reactor() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.reactor
        .push(reactor("com.example:single", "1.0.0", "pom.xml"));
    lf
}

fn fx_multi_reactor_3() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.reactor
        .push(reactor("com.example:parent", "1.0.0", "pom.xml"));
    lf.reactor
        .push(reactor("com.example:child-a", "1.0.0", "child-a/pom.xml"));
    lf.reactor
        .push(reactor("com.example:child-b", "1.0.0", "child-b/pom.xml"));
    lf
}

fn fx_one_entry_compile() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.entries.push(entry("org.slf4j:slf4j-api", "2.0.16"));
    lf
}

fn fx_scope(scope: &str) -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:thing", "1.0.0");
    e.scope = scope.to_string();
    lf.entries.push(e);
    lf
}

fn fx_one_per_scope() -> Lockfile {
    let mut lf = empty_lockfile();
    for (i, s) in ["compile", "runtime", "test", "provided", "system"]
        .iter()
        .enumerate()
    {
        let mut e = entry(&format!("org.example:scope{i}"), "1.0.0");
        e.scope = (*s).to_string();
        lf.entries.push(e);
    }
    lf
}

fn fx_classifier_distinct() -> Lockfile {
    // Same coord, different classifiers — both must survive.
    let mut lf = empty_lockfile();
    let mut a = entry("io.netty:netty-tcnative-boringssl-static", "2.0.62.Final");
    a.classifier = Some("linux-x86_64".to_string());
    let mut b = entry("io.netty:netty-tcnative-boringssl-static", "2.0.62.Final");
    b.classifier = Some("osx-aarch_64".to_string());
    lf.entries.push(a);
    lf.entries.push(b);
    lf
}

fn fx_type_pom() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("com.example:bom", "1.0.0");
    e.type_ = "pom".to_string();
    lf.entries.push(e);
    lf
}

fn fx_type_war() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("com.example:webapp", "1.0.0");
    e.type_ = "war".to_string();
    lf.entries.push(e);
    lf
}

fn fx_type_jar_explicit() -> Lockfile {
    let mut lf = empty_lockfile();
    let e = entry("com.example:lib", "1.0.0");
    // type_ is already "jar".
    lf.entries.push(e);
    lf
}

fn fx_optional_dep() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:optional-lib", "1.0.0");
    e.optional = true;
    lf.entries.push(e);
    lf
}

fn fx_with_sha1() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:has-sha1", "1.0.0");
    e.sha1 = Some("a".repeat(40));
    lf.entries.push(e);
    lf
}

fn fx_without_sha1() -> Lockfile {
    // Same as compile entry but explicit: sha1 is None.
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:no-sha1", "1.0.0");
    e.sha1 = None;
    lf.entries.push(e);
    lf
}

fn fx_with_etag() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:etagged", "1.0.0");
    e.etag = Some("\"abc123\"".to_string());
    lf.entries.push(e);
    lf
}

fn fx_with_last_modified() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:lm", "1.0.0");
    e.last_modified = Some("Wed, 01 May 2026 00:00:00 GMT".to_string());
    lf.entries.push(e);
    lf
}

fn fx_with_etag_and_last_modified() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:both", "1.0.0");
    e.etag = Some("W/\"weak-tag\"".to_string());
    e.last_modified = Some("Mon, 13 May 2026 12:34:56 GMT".to_string());
    lf.entries.push(e);
    lf
}

fn fx_from_path_depth_1() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:d1", "1.0.0");
    e.from_path = vec!["root:app".to_string()];
    e.depth = 1;
    lf.entries.push(e);
    lf
}

fn fx_from_path_depth_3() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:d3", "1.0.0");
    e.from_path = vec![
        "root:app".to_string(),
        "mid:framework".to_string(),
        "leaf:helper".to_string(),
    ];
    e.depth = 3;
    lf.entries.push(e);
    lf
}

fn fx_from_path_depth_10() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:d10", "1.0.0");
    e.from_path = (0..10).map(|i| format!("g{i}:a{i}")).collect();
    e.depth = 10;
    lf.entries.push(e);
    lf
}

fn fx_exclusions_one() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:has-excl", "1.0.0");
    e.exclusions.push(Exclusion {
        group: "commons-logging".to_string(),
        artifact: "commons-logging".to_string(),
    });
    lf.entries.push(e);
    lf
}

fn fx_exclusions_many() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:has-many-excl", "1.0.0");
    for i in 0..5 {
        e.exclusions.push(Exclusion {
            group: format!("g{i}"),
            artifact: format!("a{i}"),
        });
    }
    lf.entries.push(e);
    lf
}

fn fx_snapshot_resolution() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("com.example:libsnap", "1.2.3-SNAPSHOT");
    e.snapshot_resolution = Some("1.2.3-20260513.123456-7".to_string());
    lf.entries.push(e);
    lf
}

fn fx_settings_mirrors_only() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.settings_snapshot = Some(SettingsSnapshot {
        mirrors: vec![MirrorRef {
            id: "central-mirror".to_string(),
            url: "https://mirror.example.com/maven2".to_string(),
            mirror_of: "central".to_string(),
        }],
        repositories: Vec::new(),
    });
    lf
}

fn fx_settings_repos_only() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.settings_snapshot = Some(SettingsSnapshot {
        mirrors: Vec::new(),
        repositories: vec![RepositoryRef {
            id: "central".to_string(),
            url: "https://repo.maven.apache.org/maven2".to_string(),
        }],
    });
    lf
}

fn fx_settings_full() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.settings_snapshot = Some(SettingsSnapshot {
        mirrors: vec![
            MirrorRef {
                id: "central-mirror".to_string(),
                url: "https://mirror.example.com/maven2".to_string(),
                mirror_of: "central".to_string(),
            },
            MirrorRef {
                id: "all-mirror".to_string(),
                url: "https://mirror.example.com/all".to_string(),
                mirror_of: "*".to_string(),
            },
        ],
        repositories: vec![
            RepositoryRef {
                id: "central".to_string(),
                url: "https://repo.maven.apache.org/maven2".to_string(),
            },
            RepositoryRef {
                id: "internal".to_string(),
                url: "https://nexus.example.com/repository/maven-public".to_string(),
            },
        ],
    });
    lf
}

fn fx_everything_on_one_entry() -> Lockfile {
    // A single entry that exercises every optional field at once.
    let mut lf = empty_lockfile();
    let mut e = entry("org.springframework:spring-core", "6.1.6");
    e.scope = "runtime".to_string();
    e.optional = true;
    e.sha1 = Some("a".repeat(40));
    e.etag = Some("\"abc\"".to_string());
    e.last_modified = Some("Wed, 01 May 2026 00:00:00 GMT".to_string());
    e.classifier = Some("sources".to_string());
    e.type_ = "jar".to_string();
    e.from_path = vec!["root".to_string(), "mid".to_string(), "leaf".to_string()];
    e.depth = 3;
    e.snapshot_resolution = Some("6.1.6-20260513.000000-1".to_string());
    e.exclusions.push(Exclusion {
        group: "commons-logging".to_string(),
        artifact: "commons-logging".to_string(),
    });
    lf.entries.push(e);
    lf
}

fn fx_reactor_plus_entries() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.reactor
        .push(reactor("com.example:app", "1.0.0", "pom.xml"));
    lf.entries.push(entry("org.slf4j:slf4j-api", "2.0.16"));
    lf.entries.push(entry("org.slf4j:slf4j-simple", "2.0.16"));
    lf
}

fn fx_reactor_entries_and_settings() -> Lockfile {
    let mut lf = fx_reactor_plus_entries();
    lf.settings_snapshot = Some(SettingsSnapshot {
        mirrors: vec![MirrorRef {
            id: "m1".to_string(),
            url: "https://mirror.example.com".to_string(),
            mirror_of: "*".to_string(),
        }],
        repositories: vec![RepositoryRef {
            id: "central".to_string(),
            url: "https://repo.maven.apache.org/maven2".to_string(),
        }],
    });
    lf
}

fn fx_stress_100_entries() -> Lockfile {
    let mut lf = empty_lockfile();
    for i in 0..120 {
        let mut e = entry(
            &format!("com.example.group{}:artifact{}", i % 5, i),
            &format!("1.{}.{}", i / 10, i % 10),
        );
        e.depth = (i % 7) as u32;
        e.scope = match i % 5 {
            0 => "compile",
            1 => "runtime",
            2 => "test",
            3 => "provided",
            _ => "system",
        }
        .to_string();
        if i % 3 == 0 {
            e.sha1 = Some("c".repeat(40));
        }
        if i % 4 == 0 {
            e.classifier = Some("sources".to_string());
        }
        if i % 6 == 0 {
            e.optional = true;
        }
        if e.depth > 0 {
            e.from_path = (0..e.depth).map(|d| format!("g{d}:a{d}")).collect();
        }
        lf.entries.push(e);
    }
    lf
}

fn fx_unicode_in_strings() -> Lockfile {
    // Maven coords are ASCII, but `etag`, `last_modified`, signatures
    // are arbitrary strings — make sure non-ASCII survives.
    let mut lf = Lockfile::new("プロジェクト".to_string(), "設定".to_string());
    let mut e = entry("org.example:unicode", "1.0.0");
    e.etag = Some("\"étag-✓\"".to_string());
    lf.entries.push(e);
    lf
}

fn fx_long_signatures() -> Lockfile {
    // Stress the meta fields with realistic full-length hex signatures.
    let mut lf = Lockfile::new(
        "f".repeat(64),
        "0123456789abcdef".repeat(4), // 64 chars
    );
    lf.entries.push(entry("org.example:lib", "1.0.0"));
    lf
}

fn fx_large_size_bytes() -> Lockfile {
    // TOML integers are i64, so the practical upper bound for
    // `size_bytes` is `i64::MAX`. That's 9.2 EB, more than enough
    // headroom for any real artifact.
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:huge", "1.0.0");
    e.size_bytes = i64::MAX as u64;
    lf.entries.push(e);
    lf
}

fn fx_zero_size_bytes() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:empty-jar", "1.0.0");
    e.size_bytes = 0;
    lf.entries.push(e);
    lf
}

fn fx_long_classifier() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:c", "1.0.0");
    e.classifier = Some("a-very-long-classifier-name-that-might-stress-toml-quoting".to_string());
    lf.entries.push(e);
    lf
}

fn fx_url_with_query_and_fragment() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:url", "1.0.0");
    e.source_url = "https://example.com/repo/x.jar?token=abc#frag".to_string();
    lf.entries.push(e);
    lf
}

fn fx_versions_with_qualifiers() -> Lockfile {
    let mut lf = empty_lockfile();
    for v in [
        "1.0.0",
        "1.0.0-SNAPSHOT",
        "1.0.0.RELEASE",
        "1.0.0-rc.1",
        "1.0.0-beta-3",
        "1.0",
        "1",
    ] {
        let mut e = entry(&format!("org.example:vq-{}", v.replace(['.', '-'], "_")), v);
        e.version = v.to_string();
        lf.entries.push(e);
    }
    lf
}

fn fx_deeply_nested_groups() -> Lockfile {
    let mut lf = empty_lockfile();
    for g in [
        "a:x",
        "a.b:x",
        "a.b.c:x",
        "a.b.c.d:x",
        "a.b.c.d.e.f.g.h.i.j:x",
    ] {
        lf.entries.push(entry(g, "1.0.0"));
    }
    lf
}

fn fx_etag_with_quotes() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:eq", "1.0.0");
    // ETags are quoted by HTTP spec; we must round-trip the quotes.
    e.etag = Some("\"strong-etag\"".to_string());
    lf.entries.push(e);
    lf
}

fn fx_weak_etag() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:weak", "1.0.0");
    e.etag = Some("W/\"weak-etag\"".to_string());
    lf.entries.push(e);
    lf
}

fn fx_many_reactor_modules() -> Lockfile {
    let mut lf = empty_lockfile();
    for i in 0..15 {
        lf.reactor.push(reactor(
            &format!("com.example:module-{i}"),
            "1.0.0-SNAPSHOT",
            &format!("module-{i}/pom.xml"),
        ));
    }
    lf
}

fn fx_mixed_types() -> Lockfile {
    let mut lf = empty_lockfile();
    for t in ["jar", "pom", "war", "ear", "zip", "test-jar"] {
        let mut e = entry(&format!("org.example:type-{t}"), "1.0.0");
        e.type_ = t.to_string();
        lf.entries.push(e);
    }
    lf
}

fn fx_mixed_classifiers() -> Lockfile {
    let mut lf = empty_lockfile();
    for c in [
        "sources",
        "javadoc",
        "tests",
        "linux-x86_64",
        "osx-aarch_64",
        "windows-x86_64",
    ] {
        let mut e = entry(&format!("org.example:cls-{c}"), "1.0.0");
        e.classifier = Some(c.to_string());
        lf.entries.push(e);
    }
    lf
}

fn fx_minimum_meta() -> Lockfile {
    // Empty signatures (edge case: caller passed "" — schema doesn't
    // enforce length, but round-trip should still work).
    let mut lf = Lockfile::new(String::new(), String::new());
    lf.entries.push(entry("g:a", "1.0.0"));
    lf
}

fn fx_settings_only_no_entries() -> Lockfile {
    // Lockfile with no entries but populated settings — captures the
    // "we resolved nothing but pinned the environment" shape.
    let mut lf = empty_lockfile();
    lf.settings_snapshot = Some(SettingsSnapshot {
        mirrors: vec![MirrorRef {
            id: "m".to_string(),
            url: "https://m.example.com".to_string(),
            mirror_of: "*".to_string(),
        }],
        repositories: Vec::new(),
    });
    lf
}

fn fx_reactor_with_snapshot_versions() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.reactor
        .push(reactor("com.example:r", "1.0.0-SNAPSHOT", "pom.xml"));
    lf.reactor.push(reactor(
        "com.example:r-child",
        "1.0.0-SNAPSHOT",
        "child/pom.xml",
    ));
    lf
}

fn fx_entries_all_depth_zero() -> Lockfile {
    let mut lf = empty_lockfile();
    for i in 0..10 {
        let e = entry(&format!("org.example:direct{i}"), "1.0.0");
        // depth stays 0 — exercises the omit-when-zero serializer.
        lf.entries.push(e);
    }
    lf
}

fn fx_entries_increasing_depth() -> Lockfile {
    let mut lf = empty_lockfile();
    for d in 0..8u32 {
        let mut e = entry(&format!("org.example:d{d}"), "1.0.0");
        e.depth = d;
        if d > 0 {
            e.from_path = (0..d).map(|i| format!("g{i}:a{i}")).collect();
        }
        lf.entries.push(e);
    }
    lf
}

fn fx_all_optional_set() -> Lockfile {
    // Every entry has optional = true.
    let mut lf = empty_lockfile();
    for i in 0..6 {
        let mut e = entry(&format!("org.example:opt{i}"), "1.0.0");
        e.optional = true;
        lf.entries.push(e);
    }
    lf
}

fn fx_repeated_coords_distinct_versions() -> Lockfile {
    let mut lf = empty_lockfile();
    for v in ["1.0.0", "1.1.0", "2.0.0", "3.0.0"] {
        lf.entries.push(entry("org.example:multi-version", v));
    }
    lf
}

fn fx_single_module_one_dep() -> Lockfile {
    // The smallest "real" project shape.
    let mut lf = empty_lockfile();
    lf.reactor
        .push(reactor("com.example:app", "1.0.0", "pom.xml"));
    lf.entries.push(entry("org.slf4j:slf4j-api", "2.0.16"));
    lf
}

fn fx_no_reactor_with_entries() -> Lockfile {
    // Pre-resolved transitive lockfile with no reactor (unusual but
    // schema-legal).
    let mut lf = empty_lockfile();
    for i in 0..4 {
        lf.entries.push(entry(&format!("g{i}:a{i}"), "1.0.0"));
    }
    lf
}

fn fx_snapshot_resolution_multiple() -> Lockfile {
    let mut lf = empty_lockfile();
    for i in 0..3 {
        let mut e = entry(&format!("com.example:snap{i}"), "1.2.3-SNAPSHOT");
        e.snapshot_resolution = Some(format!("1.2.3-20260513.12345{i}-{i}"));
        lf.entries.push(e);
    }
    lf
}

fn fx_mirror_of_wildcards() -> Lockfile {
    let mut lf = empty_lockfile();
    lf.settings_snapshot = Some(SettingsSnapshot {
        mirrors: vec![
            MirrorRef {
                id: "all".to_string(),
                url: "https://m/all".to_string(),
                mirror_of: "*".to_string(),
            },
            MirrorRef {
                id: "external".to_string(),
                url: "https://m/ext".to_string(),
                mirror_of: "external:*".to_string(),
            },
            MirrorRef {
                id: "not-central".to_string(),
                url: "https://m/nc".to_string(),
                mirror_of: "*,!central".to_string(),
            },
        ],
        repositories: Vec::new(),
    });
    lf
}

fn fx_special_chars_in_etag() -> Lockfile {
    let mut lf = empty_lockfile();
    let mut e = entry("org.example:special", "1.0.0");
    e.etag = Some("\"a/b\\c:d=e\"".to_string());
    lf.entries.push(e);
    lf
}

fn fx_combined_kitchen_sink() -> Lockfile {
    // Everything at once: reactor + entries + settings + every optional
    // field somewhere. Mirrors the realistic "fully populated" shape.
    let mut lf = fx_reactor_entries_and_settings();
    let mut e = entry("io.netty:netty-tcnative", "2.0.62.Final");
    e.scope = "runtime".to_string();
    e.optional = true;
    e.sha1 = Some("a".repeat(40));
    e.etag = Some("\"netty-tag\"".to_string());
    e.last_modified = Some("Mon, 13 May 2026 12:34:56 GMT".to_string());
    e.classifier = Some("linux-x86_64".to_string());
    e.type_ = "jar".to_string();
    e.from_path = vec!["com.example:app".to_string()];
    e.depth = 1;
    e.snapshot_resolution = None; // released version
    e.exclusions.push(Exclusion {
        group: "log4j".to_string(),
        artifact: "log4j".to_string(),
    });
    lf.entries.push(e);
    lf
}

// ----- fixture registry ------------------------------------------------------

/// Returns the full set of (name, lockfile) fixtures used by the
/// round-trip property test. The registry is the source of truth for
/// the milestone-level "50 hand-crafted fixtures" acceptance criterion.
fn build_fixtures() -> Vec<(&'static str, Lockfile)> {
    vec![
        // empty / reactor shapes (5)
        ("empty", fx_empty()),
        ("single_reactor", fx_single_reactor()),
        ("multi_reactor_3", fx_multi_reactor_3()),
        ("many_reactor_modules", fx_many_reactor_modules()),
        ("reactor_with_snapshot_versions", fx_reactor_with_snapshot_versions()),
        // single-entry per scope (6)
        ("one_entry_compile", fx_one_entry_compile()),
        ("scope_compile", fx_scope("compile")),
        ("scope_runtime", fx_scope("runtime")),
        ("scope_test", fx_scope("test")),
        ("scope_provided", fx_scope("provided")),
        ("scope_system", fx_scope("system")),
        // breadth of scopes (1)
        ("one_per_scope", fx_one_per_scope()),
        // classifier / type variants (4)
        ("classifier_distinct", fx_classifier_distinct()),
        ("type_pom", fx_type_pom()),
        ("type_war", fx_type_war()),
        ("type_jar_explicit", fx_type_jar_explicit()),
        // optional / checksums (4)
        ("optional_dep", fx_optional_dep()),
        ("with_sha1", fx_with_sha1()),
        ("without_sha1", fx_without_sha1()),
        ("with_etag", fx_with_etag()),
        // http revalidation hints (2)
        ("with_last_modified", fx_with_last_modified()),
        ("with_etag_and_last_modified", fx_with_etag_and_last_modified()),
        // from_path / depth (3)
        ("from_path_depth_1", fx_from_path_depth_1()),
        ("from_path_depth_3", fx_from_path_depth_3()),
        ("from_path_depth_10", fx_from_path_depth_10()),
        // exclusions (2)
        ("exclusions_one", fx_exclusions_one()),
        ("exclusions_many", fx_exclusions_many()),
        // snapshots (2)
        ("snapshot_resolution", fx_snapshot_resolution()),
        ("snapshot_resolution_multiple", fx_snapshot_resolution_multiple()),
        // settings (4)
        ("settings_mirrors_only", fx_settings_mirrors_only()),
        ("settings_repos_only", fx_settings_repos_only()),
        ("settings_full", fx_settings_full()),
        ("settings_only_no_entries", fx_settings_only_no_entries()),
        // mirror-of wildcards (1)
        ("mirror_of_wildcards", fx_mirror_of_wildcards()),
        // composite (2)
        ("everything_on_one_entry", fx_everything_on_one_entry()),
        ("combined_kitchen_sink", fx_combined_kitchen_sink()),
        // reactor + entries (2)
        ("reactor_plus_entries", fx_reactor_plus_entries()),
        ("reactor_entries_and_settings", fx_reactor_entries_and_settings()),
        // stress shape (1)
        ("stress_100_entries", fx_stress_100_entries()),
        // edge cases (12)
        ("unicode_in_strings", fx_unicode_in_strings()),
        ("long_signatures", fx_long_signatures()),
        ("large_size_bytes", fx_large_size_bytes()),
        ("zero_size_bytes", fx_zero_size_bytes()),
        ("long_classifier", fx_long_classifier()),
        ("url_with_query_and_fragment", fx_url_with_query_and_fragment()),
        ("versions_with_qualifiers", fx_versions_with_qualifiers()),
        ("deeply_nested_groups", fx_deeply_nested_groups()),
        ("etag_with_quotes", fx_etag_with_quotes()),
        ("weak_etag", fx_weak_etag()),
        ("special_chars_in_etag", fx_special_chars_in_etag()),
        ("mixed_types", fx_mixed_types()),
        // more (5)
        ("mixed_classifiers", fx_mixed_classifiers()),
        ("minimum_meta", fx_minimum_meta()),
        ("entries_all_depth_zero", fx_entries_all_depth_zero()),
        ("entries_increasing_depth", fx_entries_increasing_depth()),
        ("all_optional_set", fx_all_optional_set()),
        // more (3)
        ("repeated_coords_distinct_versions", fx_repeated_coords_distinct_versions()),
        ("single_module_one_dep", fx_single_module_one_dep()),
        ("no_reactor_with_entries", fx_no_reactor_with_entries()),
        ("single_entry_compile_minimal", fx_one_entry_compile()),
    ]
}

// ----- round-trip helpers ----------------------------------------------------

/// Run the full toml1 / lf2 / toml2 round-trip check on a single fixture.
/// Panics on failure with the fixture name in the message.
fn assert_round_trip(name: &str, lf: &Lockfile) {
    let toml1 = lf
        .to_toml()
        .unwrap_or_else(|e| panic!("[{name}] first serialize failed: {e}"));
    let lf2 = Lockfile::from_toml(&toml1)
        .unwrap_or_else(|e| panic!("[{name}] parse failed: {e}\ntoml:\n{toml1}"));
    let toml2 = lf2
        .to_toml()
        .unwrap_or_else(|e| panic!("[{name}] second serialize failed: {e}"));

    assert_eq!(
        toml1, toml2,
        "[{name}] byte-stability: second serialization must match first"
    );
    assert_eq!(
        *lf, lf2,
        "[{name}] structural equality: deserialized value must equal original"
    );
}

// ----- the actual tests ------------------------------------------------------

#[test]
fn round_trip_all_fixtures() {
    let fixtures = build_fixtures();
    assert!(
        fixtures.len() >= 50,
        "milestone AC requires at least 50 fixtures; have {}",
        fixtures.len()
    );
    for (name, lf) in &fixtures {
        assert_round_trip(name, lf);
    }
}

#[test]
fn fixture_names_are_unique() {
    // Guard against accidental copy-paste duplicates inflating the
    // fixture count without adding real coverage.
    let fixtures = build_fixtures();
    let mut names: Vec<&str> = fixtures.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "duplicate fixture names detected — every fixture must have a unique name"
    );
}

// A handful of named tests for the most schema-critical shapes, so a
// failure shows up with a sharp name in `cargo test` output rather than
// buried inside the big loop.

#[test]
fn round_trip_empty() {
    assert_round_trip("empty", &fx_empty());
}

#[test]
fn round_trip_kitchen_sink() {
    assert_round_trip("combined_kitchen_sink", &fx_combined_kitchen_sink());
}

#[test]
fn round_trip_stress_100_entries() {
    assert_round_trip("stress_100_entries", &fx_stress_100_entries());
}

#[test]
fn round_trip_unicode() {
    assert_round_trip("unicode_in_strings", &fx_unicode_in_strings());
}

#[test]
fn round_trip_every_scope() {
    assert_round_trip("one_per_scope", &fx_one_per_scope());
}

#[test]
fn round_trip_full_settings() {
    assert_round_trip("settings_full", &fx_settings_full());
}

#[test]
fn round_trip_everything_on_one_entry() {
    assert_round_trip("everything_on_one_entry", &fx_everything_on_one_entry());
}

#[test]
fn round_trip_from_path_depths() {
    assert_round_trip("from_path_depth_1", &fx_from_path_depth_1());
    assert_round_trip("from_path_depth_3", &fx_from_path_depth_3());
    assert_round_trip("from_path_depth_10", &fx_from_path_depth_10());
}

#[test]
fn round_trip_snapshot_resolutions() {
    assert_round_trip("snapshot_resolution", &fx_snapshot_resolution());
    assert_round_trip(
        "snapshot_resolution_multiple",
        &fx_snapshot_resolution_multiple(),
    );
}

#[test]
fn round_trip_mirror_wildcards() {
    assert_round_trip("mirror_of_wildcards", &fx_mirror_of_wildcards());
}
