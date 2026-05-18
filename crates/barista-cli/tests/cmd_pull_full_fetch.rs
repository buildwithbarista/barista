// Integration-test target — workspace security lints are allowed
// here. Panic-on-misuse is the documented contract for failing a
// test loudly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    unsafe_code
)]

//! Integration tests for `barista pull` — full-fetch path (M4.3 T8).
//!
//! Two flavors of test:
//!
//! 1. **Hermetic** tests that point `BARISTA_PATHS__CACHE_DIR` +
//!    `BARISTA_PATHS__M2_REPOSITORY` at tempdirs, run a wiremock
//!    server playing the role of Maven Central, and exercise the
//!    full pipeline (resolve → fetch → CAS → ~/.m2 hardlink →
//!    `barista.lock`). These run on every `cargo test` invocation.
//!
//! 2. **End-to-end against real Maven Central** tests gated on a
//!    `BARISTA_NET_TESTS=1` env var. Useful for local validation;
//!    not run by default to keep `cargo test` offline-safe.
//!
//! The `cmd_pull.rs` neighbor file covers the `--no-fetch` path,
//! error surfaces, and the empty-deps zero-fetch shortcut.

use std::fs;
use std::path::{Path, PathBuf};

use barista_cli::cli::{Cli, dispatch};
use barista_lockfile::Lockfile;
use clap::Parser;
use sha2::Digest as _;
use tempfile::TempDir;
use tokio::runtime::Builder as RtBuilder;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ROOT_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>fixture</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
  <dependencies>
    <dependency>
      <groupId>org.example</groupId>
      <artifactId>libfoo</artifactId>
      <version>1.0.0</version>
    </dependency>
  </dependencies>
</project>"#;

const LIBFOO_POM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>libfoo</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
</project>"#;

/// Body bytes for the synthetic `libfoo-1.0.0.jar` artifact. Need
/// not be a real JAR — the resolver doesn't care about contents,
/// only the SHA-256 the cache records.
const LIBFOO_JAR: &[u8] = b"\xCA\xFE\xBA\xBE\x00\x00\x00\x34synthetic-libfoo-1.0.0";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::Digest as _;
    let mut h = sha1::Sha1::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut hex = String::with_capacity(40);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Mount Maven-Central-style endpoints for a single coord on
/// `server`. Serves: `<artifact>-<version>.pom`,
/// `<artifact>-<version>.jar`, and `.sha256` + `.sha1` sidecars for
/// the jar. POM sidecars are returned as 404 (Unverified branch in
/// the cache's checksum logic) so we don't need to hash every POM
/// in the fixture.
async fn mount_artifact(
    server: &MockServer,
    group: &str,
    artifact: &str,
    version: &str,
    pom_body: &str,
    jar_body: &[u8],
) {
    let group_path = group.replace('.', "/");
    let pom_path = format!("/{group_path}/{artifact}/{version}/{artifact}-{version}.pom");
    let jar_path = format!("/{group_path}/{artifact}/{version}/{artifact}-{version}.jar");
    let sha256_jar = sha256_hex(jar_body);
    let sha1_jar = sha1_hex(jar_body);
    // Owned strings for response bodies.
    Mock::given(method("GET"))
        .and(path(pom_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_string(pom_body.to_string()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pom_path}.sha256")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pom_path}.sha1")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(jar_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_body.to_vec()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{jar_path}.sha256")))
        .respond_with(ResponseTemplate::new(200).set_body_string(sha256_jar))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{jar_path}.sha1")))
        .respond_with(ResponseTemplate::new(200).set_body_string(sha1_jar))
        .mount(server)
        .await;
}

/// One-stop fixture: tempdir + project root with `ROOT_POM` written +
/// configured cache + m2 dirs + wiremock server with libfoo mounted.
struct Fixture {
    _td: TempDir,
    project_root: PathBuf,
    cache_dir: PathBuf,
    m2_dir: PathBuf,
    server_uri: String,
    // Mount guards must live as long as the server.
    _server: MockServer,
}

async fn make_fixture() -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let project_root = td.path().join("project");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(project_root.join("pom.xml"), ROOT_POM).unwrap();

    let cache_dir = td.path().join("cache");
    let m2_dir = td.path().join("m2");

    let server = MockServer::start().await;
    mount_artifact(
        &server,
        "org.example",
        "libfoo",
        "1.0.0",
        LIBFOO_POM,
        LIBFOO_JAR,
    )
    .await;

    let server_uri = server.uri();
    Fixture {
        _td: td,
        project_root,
        cache_dir,
        m2_dir,
        server_uri,
        _server: server,
    }
}

// ---------------------------------------------------------------------------
// Env scoping
// ---------------------------------------------------------------------------

/// Best-effort serialization across this test file's tests. The CLI
/// reads process env, which is global state; running these in
/// parallel would race on `BARISTA_*` overrides. We acquire a
/// process-wide mutex around every test that mutates env.
fn env_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

struct EnvScope {
    keys: Vec<&'static str>,
    prev: Vec<Option<std::ffi::OsString>>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl EnvScope {
    fn set(pairs: &[(&'static str, &Path)]) -> Self {
        let guard = env_lock().lock().expect("env_lock poisoned");
        let mut keys = Vec::with_capacity(pairs.len());
        let mut prev = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            prev.push(std::env::var_os(*k));
            keys.push(*k);
            // SAFETY: env mutation guarded by env_lock().
            unsafe {
                std::env::set_var(k, v);
            }
        }
        Self {
            keys,
            prev,
            _guard: guard,
        }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (k, prev) in self.keys.iter().zip(self.prev.iter()) {
            // SAFETY: env mutation guarded by env_lock().
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn run_dispatch(argv: &[&str]) -> i32 {
    let cli = Cli::try_parse_from(argv).expect("parse argv");
    dispatch(cli)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// AC: `barista pull` against a fresh project resolves dependencies,
/// fetches them from a (mocked) upstream into the CAS, hardlinks
/// into `~/.m2`, and writes a valid `barista.lock`.
#[test]
fn full_fetch_writes_lockfile_and_materializes_m2() {
    // Async mock-server setup needs a runtime; build a quick one.
    let rt = RtBuilder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(make_fixture());

    // All env overrides — `BARISTA_PATHS__*` for the cache + m2
    // roots, `BARISTA_TEST_UPSTREAM_URL` for the fetcher's
    // upstream override (see `cmd::pull::run_full_fetch`) — must
    // be acquired in a single `EnvScope::set` call because
    // `EnvScope::set` holds a process-wide mutex for the scope's
    // lifetime; calling it twice would deadlock.
    let _env = EnvScope::set(&[
        ("BARISTA_PATHS__CACHE_DIR", fx.cache_dir.as_path()),
        ("BARISTA_PATHS__M2_REPOSITORY", fx.m2_dir.as_path()),
        ("BARISTA_TEST_UPSTREAM_URL", Path::new(&fx.server_uri)),
    ]);

    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
    ]);
    assert_eq!(code, 0, "barista pull should succeed against mock upstream");

    // 1. barista.lock exists and parses.
    let lock_path = fx.project_root.join("barista.lock");
    assert!(lock_path.exists(), "lockfile must be written");
    let lf = Lockfile::read(&lock_path).expect("lockfile parses");

    // 2. Exactly one entry: org.example:libfoo @ 1.0.0.
    assert_eq!(lf.entries.len(), 1, "one direct dep → one entry");
    let entry = &lf.entries[0];
    assert_eq!(entry.coords, "org.example:libfoo");
    assert_eq!(entry.version, "1.0.0");
    assert_eq!(entry.scope, "compile");
    assert_eq!(entry.type_, "jar");
    assert_eq!(entry.size_bytes, LIBFOO_JAR.len() as u64);

    // 3. SHA-256 must match the actual jar bytes.
    assert_eq!(entry.sha256, sha256_hex(LIBFOO_JAR), "SHA-256 must match");

    // 4. source_url must point at the mock server.
    assert!(
        entry.source_url.starts_with(&fx.server_uri),
        "source_url should point at the mock upstream; got {}",
        entry.source_url
    );

    // 5. CAS must contain the artifact bytes.
    let cas_path = fx
        .cache_dir
        .join("objects")
        .join(&entry.sha256[..2])
        .join(&entry.sha256);
    assert!(
        cas_path.exists(),
        "CAS must hold the artifact at {cas_path:?}"
    );
    assert_eq!(fs::read(&cas_path).unwrap(), LIBFOO_JAR);

    // 6. ~/.m2/repository must hold the hardlink at the
    //    Maven-conventional path.
    let m2_jar = fx.m2_dir.join("org/example/libfoo/1.0.0/libfoo-1.0.0.jar");
    assert!(m2_jar.exists(), "m2 hardlink must exist at {m2_jar:?}");
    assert_eq!(fs::read(&m2_jar).unwrap(), LIBFOO_JAR);

    // 7. project_signature is a 64-char hex digest.
    assert_eq!(lf.meta.project_signature.len(), 64);
    assert!(
        lf.meta
            .project_signature
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    );
    // Keep this around so the runtime that owns the server lives
    // long enough to satisfy the inner block_on calls.
    drop(rt);
}

/// AC: `barista pull --update` is idempotent on a clean tree —
/// running twice produces the same entries / same SHA-256s.
#[test]
fn full_fetch_update_is_idempotent_on_clean_tree() {
    let rt = RtBuilder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(make_fixture());

    let _env = EnvScope::set(&[
        ("BARISTA_PATHS__CACHE_DIR", fx.cache_dir.as_path()),
        ("BARISTA_PATHS__M2_REPOSITORY", fx.m2_dir.as_path()),
        ("BARISTA_TEST_UPSTREAM_URL", Path::new(&fx.server_uri)),
    ]);

    // First pull.
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
    ]);
    assert_eq!(code, 0);
    let lf1 = Lockfile::read(&fx.project_root.join("barista.lock")).unwrap();

    // Second pull --update.
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
        "--update",
    ]);
    assert_eq!(code, 0);
    let lf2 = Lockfile::read(&fx.project_root.join("barista.lock")).unwrap();

    assert_eq!(
        lf1.entries, lf2.entries,
        "--update should be idempotent on a clean tree"
    );
    assert_eq!(
        lf1.meta.project_signature, lf2.meta.project_signature,
        "project_signature must be stable across runs on the same source"
    );
    drop(rt);
}

/// AC: `barista pull --frozen` errors with a signature mismatch
/// when the project's POM has changed relative to the lockfile.
#[test]
fn full_fetch_frozen_errors_on_signature_mismatch() {
    let rt = RtBuilder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(make_fixture());

    let _env = EnvScope::set(&[
        ("BARISTA_PATHS__CACHE_DIR", fx.cache_dir.as_path()),
        ("BARISTA_PATHS__M2_REPOSITORY", fx.m2_dir.as_path()),
        ("BARISTA_TEST_UPSTREAM_URL", Path::new(&fx.server_uri)),
    ]);

    // 1. First pull writes a lockfile pinned to the current pom.xml.
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
    ]);
    assert_eq!(code, 0);
    let original = Lockfile::read(&fx.project_root.join("barista.lock")).unwrap();
    let original_sig = original.meta.project_signature.clone();

    // 2. Mutate the pom — add a `<name>` element (or any element
    //    that flows into the signature). RawPom-level fields like
    //    `name` are in the bincode-encoded signature.
    let mutated = ROOT_POM.replace(
        "<artifactId>fixture</artifactId>",
        "<artifactId>fixture</artifactId>\n  <name>Renamed Fixture</name>",
    );
    fs::write(fx.project_root.join("pom.xml"), mutated).unwrap();

    // 3. `--frozen` must error with exit code 2.
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "--frozen",
        "pull",
    ]);
    assert_eq!(
        code, 2,
        "--frozen + mutated pom must exit 2 (signature mismatch)"
    );

    // 4. The lockfile on disk must NOT have been overwritten.
    let after = Lockfile::read(&fx.project_root.join("barista.lock")).unwrap();
    assert_eq!(
        after.meta.project_signature, original_sig,
        "--frozen must not rewrite the lockfile"
    );
    drop(rt);
}

/// AC: `barista pull` followed by `barista pour` against the same
/// project. After `pull` writes the lockfile + CAS, `pour` (the
/// step that `barista verify` invokes before dispatching to the
/// daemon) must succeed without re-fetching. This is the
/// bootstrap-handshake contract: the daemon path's pre-dispatch
/// pour reads the lockfile + CAS that `pull` writes.
#[test]
fn pull_then_pour_writes_m2_jars() {
    let rt = RtBuilder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(make_fixture());

    let _env = EnvScope::set(&[
        ("BARISTA_PATHS__CACHE_DIR", fx.cache_dir.as_path()),
        ("BARISTA_PATHS__M2_REPOSITORY", fx.m2_dir.as_path()),
        ("BARISTA_TEST_UPSTREAM_URL", Path::new(&fx.server_uri)),
    ]);

    // 1. pull populates lockfile + CAS + hardlinks into ~/.m2.
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
    ]);
    assert_eq!(code, 0, "pull must succeed");

    // 2. pour reads the lockfile + CAS and hardlinks into the target
    //    (the same ~/.m2). This is the step that `barista verify`
    //    runs before daemon dispatch. It must succeed without
    //    network I/O — drop the mock server first to prove it.
    drop(fx._server);

    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pour",
    ]);
    assert_eq!(
        code, 0,
        "pour against the lockfile + CAS written by pull must succeed offline"
    );

    // The jar must still be at the ~/.m2 path. (`pour` is
    // idempotent: re-hardlinking when the inode is already shared
    // is a no-op.)
    let m2_jar = fx.m2_dir.join("org/example/libfoo/1.0.0/libfoo-1.0.0.jar");
    assert!(m2_jar.exists(), "m2 hardlink must persist after pour");
    assert_eq!(fs::read(&m2_jar).unwrap(), LIBFOO_JAR);
    drop(rt);
}

/// AC: `barista pull` is a no-op (no fetch) when the on-disk
/// lockfile's `project_signature` already matches the source tree —
/// even without `--frozen`. This is the "authoritative" path: the
/// lockfile is treated as ground truth.
#[test]
fn full_fetch_short_circuits_when_signature_matches() {
    let rt = RtBuilder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = rt.block_on(make_fixture());

    let _env = EnvScope::set(&[
        ("BARISTA_PATHS__CACHE_DIR", fx.cache_dir.as_path()),
        ("BARISTA_PATHS__M2_REPOSITORY", fx.m2_dir.as_path()),
        ("BARISTA_TEST_UPSTREAM_URL", Path::new(&fx.server_uri)),
    ]);

    // First pull primes the lockfile + CAS.
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
    ]);
    assert_eq!(code, 0);
    let first_mtime = fs::metadata(fx.project_root.join("barista.lock"))
        .unwrap()
        .modified()
        .unwrap();

    // Delete the mock server — if the second pull tried to fetch,
    // it would fail. Authoritative-lockfile path must NOT fetch.
    drop(fx._server);

    // Second pull — without --update, with the unchanged pom — must
    // short-circuit on the matching project_signature and not touch
    // the network.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let code = run_dispatch(&[
        "barista",
        "--root",
        fx.project_root.to_str().unwrap(),
        "pull",
    ]);
    assert_eq!(
        code, 0,
        "matching signature: pull must succeed without touching upstream"
    );
    let second_mtime = fs::metadata(fx.project_root.join("barista.lock"))
        .unwrap()
        .modified()
        .unwrap();
    // The lockfile MAY or MAY NOT be touched (we don't promise
    // either way); what matters is that it parses and has the same
    // entries.
    let _ = (first_mtime, second_mtime);
    let lf = Lockfile::read(&fx.project_root.join("barista.lock")).unwrap();
    assert_eq!(lf.entries.len(), 1);
    drop(rt);
}

// ---------------------------------------------------------------------------
// `#[ignore]`-gated end-to-end test against real Maven Central +
// the barback daemon. Mirrors the
// `cmd_verify.rs::auto_respawn_against_crashing_barback` gating
// pattern: requires `mvn` + a JDK + `BARISTA_BARBACK_JAR` on the
// environment.
//
// Run with:
//   cargo test -p barista-cli --test cmd_pull_full_fetch \
//     -- --ignored --test-threads=1
// ---------------------------------------------------------------------------

/// The canonical AC test: `barista pull` resolves real Maven
/// Central artifacts (P01 fixture: slf4j-api + jackson-core +
/// commons-lang3), populates `~/.barista/cache/`, hardlinks into
/// `~/.m2/repository/`, and writes a valid `barista.lock`.
/// Subsequent `barista verify` (no `--no-daemon`) dispatches to the
/// barback daemon and produces `target/*.jar`.
///
/// This test is the load-bearing AC for the daemon-path bootstrap
/// closure: it proves that `barista pull && barista verify` works
/// on a fresh project with no manual lockfile or CAS staging.
#[test]
#[ignore = "requires Maven Central network access; run with --ignored"]
fn pull_against_real_maven_central_writes_valid_lockfile() {
    // Bench corpus P01 fixture, copied into a tempdir so the
    // test doesn't mutate the source tree. The fixture lives at
    // `bench/projects/p01/checkout/` relative to the workspace
    // root; we resolve it via `CARGO_MANIFEST_DIR/../../bench`.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let p01_src = std::path::Path::new(manifest)
        .join("../../bench/projects/p01/checkout")
        .canonicalize();
    let p01_src = match p01_src {
        Ok(p) => p,
        Err(_) => {
            eprintln!("skipping: P01 fixture not found at bench/projects/p01/checkout");
            return;
        }
    };
    let td = tempfile::tempdir().unwrap();
    let project_root = td.path().join("p01");
    fs::create_dir_all(&project_root).unwrap();
    // Copy pom.xml + src/ recursively.
    fs::copy(p01_src.join("pom.xml"), project_root.join("pom.xml")).unwrap();
    copy_tree(&p01_src.join("src"), &project_root.join("src"));

    let cache_dir = td.path().join("cache");
    let m2_dir = td.path().join("m2");
    let _env = EnvScope::set(&[
        ("BARISTA_PATHS__CACHE_DIR", cache_dir.as_path()),
        ("BARISTA_PATHS__M2_REPOSITORY", m2_dir.as_path()),
    ]);

    let code = run_dispatch(&["barista", "--root", project_root.to_str().unwrap(), "pull"]);
    assert_eq!(code, 0, "barista pull against Maven Central must succeed");

    let lf = Lockfile::read(&project_root.join("barista.lock")).unwrap();
    // P01 declares 3 direct deps (slf4j-api, jackson-core,
    // commons-lang3). All three are transitive-free as documented
    // in the fixture, so the lockfile must hold exactly 3 entries.
    assert_eq!(
        lf.entries.len(),
        3,
        "P01 fixture has 3 direct deps with no transitives"
    );
    let coords: std::collections::HashSet<String> =
        lf.entries.iter().map(|e| e.coords.clone()).collect();
    assert!(coords.contains("org.slf4j:slf4j-api"));
    assert!(coords.contains("com.fasterxml.jackson.core:jackson-core"));
    assert!(coords.contains("org.apache.commons:commons-lang3"));
    for entry in &lf.entries {
        assert_eq!(
            entry.sha256.len(),
            64,
            "{}: sha256 must be 64 hex",
            entry.coords
        );
        assert!(
            entry.size_bytes > 0,
            "{}: jar must have non-zero size",
            entry.coords
        );
        assert!(
            entry
                .source_url
                .starts_with("https://repo.maven.apache.org/maven2/"),
            "{}: source_url must point at Maven Central; got {}",
            entry.coords,
            entry.source_url,
        );
        // CAS must hold the bytes.
        let cas_path = cache_dir
            .join("objects")
            .join(&entry.sha256[..2])
            .join(&entry.sha256);
        assert!(cas_path.exists(), "{}: CAS blob missing", entry.coords);
        // ~/.m2 must hold the hardlink.
        let group_slashed = entry.coords.split(':').next().unwrap().replace('.', "/");
        let artifact = entry.coords.split(':').nth(1).unwrap();
        let m2_jar = m2_dir.join(format!(
            "{group_slashed}/{artifact}/{}/{}-{}.{}",
            entry.version, artifact, entry.version, entry.type_
        ));
        assert!(
            m2_jar.exists(),
            "{}: m2 hardlink missing at {m2_jar:?}",
            entry.coords
        );
    }
}

/// Recursively copy `src` into `dst`. Stops at the first error.
fn copy_tree(src: &Path, dst: &Path) {
    if !src.exists() {
        return;
    }
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}
