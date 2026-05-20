// SPDX-License-Identifier: MIT OR Apache-2.0

//! Container round-trip integration test: [`RoasteryClient`] against a
//! **real ephemeral roastery Docker container**, over a real TCP
//! socket.
//!
//! # What this proves
//!
//! The in-process suite (`round_trip.rs`) drives the client against a
//! roastery `axum::Router` mounted inside the test process. That proves
//! the wire contract, but it shares an address space, a TLS provider,
//! and a build with the client. This test closes the remaining gap: it
//! exercises the client against the **actual shipped server binary**,
//! built from `roastery/Dockerfile` and running in its own container,
//! reached only over a published TCP port. If the client and the
//! distroless release image disagree about the protocol, this is where
//! it surfaces.
//!
//! The round trip covers the full CAS surface plus the always-public
//! probes: `health()` → `capabilities()` → `PUT` a blob → `GET` it back
//! byte-equal → `HEAD` it (present) → `missing()` reports it present.
//!
//! # How to run it
//!
//! This test is `#[ignore]`d by default **and** gated on the
//! `BARISTA_ROASTERY_CONTAINER_TEST` environment variable, so a plain
//! `cargo test` / `cargo test --workspace` on a machine without Docker
//! stays green — the test is skipped, not failed. To run it explicitly
//! on a Docker-equipped host:
//!
//! ```text
//! BARISTA_ROASTERY_CONTAINER_TEST=1 \
//!   cargo test -p barista-roastery-client \
//!   --test container_roundtrip -- --ignored --nocapture
//! ```
//!
//! ## Image contract
//!
//! By default the test builds the image itself via
//! `roastery/scripts/build-image.sh` (tagging it `roastery:test`),
//! mirroring how `roastery/tests/e2e/kind.sh` builds before it
//! deploys. Set `SKIP_BUILD=1` to skip the build and assume a prebuilt
//! `roastery:test` (or whatever `ROASTERY_TEST_IMAGE` names) already
//! exists locally — useful for fast re-runs and for a CI job that
//! builds the image in a dedicated earlier step. The env knobs:
//!
//! | Variable                          | Default          | Meaning                                   |
//! |-----------------------------------|------------------|-------------------------------------------|
//! | `BARISTA_ROASTERY_CONTAINER_TEST` | unset            | Must be set (any value) to run.           |
//! | `ROASTERY_TEST_IMAGE`             | `roastery:test`  | Image tag to run (and build, unless skipped). |
//! | `SKIP_BUILD`                      | unset            | Set (any value) to skip the image build.  |
//!
//! # What this does NOT prove
//!
//! Nothing here speaks to the milestone-level "cold cache + warm
//! roastery beats cold cache + Central direct by ≥5× on the 100-project
//! corpus median" target. That is a corpus-and-hardware benchmark owned
//! by the benchmark workstream. The speedup *mechanism* is demonstrated
//! separately in `roastery_speedup.rs` under simulated WAN latency. This
//! file proves only client↔real-container functional correctness.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions
)]

use std::io::Cursor;
use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use barista_roastery_client::{ClientConfig, Digest, RoasteryClient, TlsConfig};
use tokio::io::AsyncReadExt;
use url::Url;

/// Env var that opts the test in. Mirrors how `kind.sh` requires both
/// Docker and an explicit invocation; without this set, the test is
/// skipped even when run with `--ignored`.
const OPT_IN_ENV: &str = "BARISTA_ROASTERY_CONTAINER_TEST";

/// Default image tag the test builds + runs.
const DEFAULT_IMAGE: &str = "roastery:test";

/// Container-internal port the roastery binds (matches the Dockerfile's
/// `EXPOSE 7878` and `ROASTERY_BIND` default port).
const CONTAINER_PORT: u16 = 7878;

/// A running roastery container. Drops to `docker rm -f`, so a panic
/// anywhere in the test still tears the container down.
struct Container {
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        // Best-effort teardown. `docker rm -f` both stops and removes;
        // ignore errors so a double-drop or already-gone container
        // doesn't mask the test's real result.
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

/// The workspace root — `git rev-parse --show-toplevel`, matching the
/// convention `build-image.sh` / `kind.sh` use to resolve the build
/// context.
fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("run git rev-parse");
    assert!(
        out.status.success(),
        "git rev-parse --show-toplevel failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).expect("utf8 toplevel");
    PathBuf::from(s.trim())
}

/// Pick a free TCP port on loopback by binding `:0` and reading the
/// assigned port back, then dropping the listener. There's an inherent
/// (small) race between drop and `docker run -p`, but Docker binds
/// promptly and the window is negligible for a single-host test.
fn free_host_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Build the image via `roastery/scripts/build-image.sh`, tagging it
/// `image`. Honors `SKIP_BUILD`.
fn build_image(repo_root: &std::path::Path, image: &str) {
    if std::env::var_os("SKIP_BUILD").is_some() {
        eprintln!("SKIP_BUILD set — assuming {image} already exists locally");
        let inspect = Command::new("docker")
            .args(["image", "inspect", image])
            .output()
            .expect("run docker image inspect");
        assert!(
            inspect.status.success(),
            "SKIP_BUILD set but image {image} is not present locally"
        );
        return;
    }

    eprintln!("building {image} via roastery/scripts/build-image.sh …");
    let script = repo_root.join("roastery/scripts/build-image.sh");
    let status = Command::new("bash")
        .arg(&script)
        .env("TAG", image)
        .env("REPO_ROOT", repo_root)
        // Build for the host arch so a `docker run` of the image works
        // on both amd64 CI runners and arm64 laptops (the script
        // defaults PLATFORM to linux/amd64; override to the host).
        .env("PLATFORM", host_platform())
        .status()
        .expect("run build-image.sh");
    assert!(status.success(), "build-image.sh failed for {image}");
}

/// The buildx platform string for the host arch, so the built image is
/// directly runnable by the local Docker daemon without emulation.
fn host_platform() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm" => "linux/arm64",
        _ => "linux/amd64",
    }
}

/// `docker run -d` an ephemeral roastery on `host_port`, with a tmp
/// storage dir and no auth (loopback-published, so the server's
/// fail-closed BAR-AUTH-005 check is satisfied — the container binds
/// `0.0.0.0:7878` internally but we publish only to 127.0.0.1, and we
/// give it a one-line bearer-tokens file so the non-loopback internal
/// bind passes validation while the always-public probes and the CAS
/// round trip below authenticate accordingly).
///
/// We mirror `scripts/test-dockerfile.sh`: bind `0.0.0.0:7878` inside
/// the container, publish to a host port, and seed a bearer token so
/// the fail-closed validation passes. The CAS routes then require the
/// token; the client is configured with it.
fn run_container(
    repo_root: &std::path::Path,
    image: &str,
    name: &str,
    host_port: u16,
) -> (Container, String) {
    // A throwaway storage dir + tokens file on the host, mounted into
    // the container's writeable volume.
    //
    // `keep()` consumes the `TempDir` and returns the path *without*
    // scheduling deletion — the directory must outlive the container
    // (dropping the `TempDir` here would delete the tokens file out
    // from under the running container). It lives under the OS temp
    // dir and is reaped by the OS / on the next boot.
    let storage_path = tempfile::tempdir().expect("tempdir").keep();
    // The distroless image runs as uid 65532; make the mount writeable.
    let _ = Command::new("chmod")
        .args(["777", storage_path.to_str().unwrap()])
        .output();
    let tokens_path = storage_path.join("bearer-tokens.txt");
    std::fs::write(&tokens_path, "container-roundtrip-token\n").expect("write tokens");
    let _ = Command::new("chmod")
        .args(["644", tokens_path.to_str().unwrap()])
        .output();

    let publish = format!("127.0.0.1:{host_port}:{CONTAINER_PORT}");
    let volume = format!("{}:/var/lib/roastery", storage_path.display());

    let _ = repo_root; // reserved for future context-relative mounts.

    let out = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--platform",
            host_platform(),
            "--name",
            name,
            "-p",
            &publish,
            "-v",
            &volume,
            "-e",
            "ROASTERY_BIND=0.0.0.0:7878",
            "-e",
            "ROASTERY_STORAGE_DIR=/var/lib/roastery",
            "-e",
            "ROASTERY_BEARER_TOKENS_FILE=/var/lib/roastery/bearer-tokens.txt",
            image,
        ])
        .output()
        .expect("run docker run");
    assert!(
        out.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    (
        Container {
            name: name.to_string(),
        },
        "container-roundtrip-token".to_string(),
    )
}

/// Poll `GET /healthz` (the ops liveness probe — always public) until it
/// returns 200, or panic after `timeout`. Dumps `docker logs` on
/// timeout to aid diagnosis.
async fn wait_for_healthz(host_port: u16, name: &str, timeout: Duration) {
    let url = format!("http://127.0.0.1:{host_port}/healthz");
    let http = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    let mut last: Option<String> = None;
    while Instant::now() < deadline {
        match http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => last = Some(format!("status {}", resp.status())),
            Err(e) => last = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let logs = Command::new("docker")
        .args(["logs", name])
        .output()
        .map(|o| {
            format!(
                "stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        })
        .unwrap_or_else(|e| format!("(could not collect docker logs: {e})"));
    panic!(
        "/healthz did not return 200 within {timeout:?}; last={last:?}\n--- docker logs ---\n{logs}"
    );
}

/// Build a bearer-auth, plain-HTTP client at the container's published
/// loopback URL.
fn client(host_port: u16, token: &str) -> RoasteryClient {
    let base = format!("http://127.0.0.1:{host_port}");
    let url: Url = base.parse().expect("parse base url");
    let cfg = ClientConfig::builder(url)
        .tls(TlsConfig::PlainHttp)
        .auth(barista_roastery_client::AuthConfig::Bearer {
            token: token.to_string(),
        })
        .timeout(Duration::from_secs(30))
        .build();
    RoasteryClient::new(cfg).expect("client")
}

/// Drain a `BlobStream` into a `Vec<u8>`.
async fn drain(mut blob: barista_roastery_client::BlobStream) -> Vec<u8> {
    let mut buf = Vec::with_capacity(blob.stat.size as usize);
    blob.body.read_to_end(&mut buf).await.expect("read_to_end");
    buf
}

// -------------------------------------------------------------------
// The container round-trip. `#[ignore]` + opt-in env-gated.
// -------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker; opt in with BARISTA_ROASTERY_CONTAINER_TEST=1 and run with --ignored"]
async fn client_round_trips_against_real_roastery_container() {
    if std::env::var_os(OPT_IN_ENV).is_none() {
        eprintln!(
            "{OPT_IN_ENV} not set — skipping container round-trip. \
             Set {OPT_IN_ENV}=1 (Docker required) to run."
        );
        return;
    }

    let image = std::env::var("ROASTERY_TEST_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string());
    let root = repo_root();

    // 1. Build (or assume) the image.
    build_image(&root, &image);

    // 2. Run an ephemeral container on a free host port.
    let host_port = free_host_port();
    let name = format!("roastery-roundtrip-{}-{host_port}", std::process::id());
    let (_guard, token) = run_container(&root, &image, &name, host_port);

    // 3. Wait for liveness.
    wait_for_healthz(host_port, &name, Duration::from_secs(30)).await;

    // 4. Drive the full client surface against the real container.
    let c = client(host_port, &token);

    // --- always-public probes (anonymous on the wire).
    let health = c.health().await.expect("health()");
    assert_eq!(health.status, "ok");
    assert_eq!(health.protocol, "barista");
    assert_eq!(health.version, "v1");

    let caps = c.capabilities().await.expect("capabilities()");
    assert_eq!(caps.protocol, "barista");
    assert_eq!(caps.version, "v1");
    assert_eq!(caps.cas.hashes, vec!["sha256".to_string()]);
    assert!(caps.cas.max_batch_missing >= 1);

    // --- PUT → GET byte-equal.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let digest = Digest::of_bytes(&payload);
    c.put_blob(digest, Cursor::new(payload.clone()), payload.len() as u64)
        .await
        .expect("put_blob");

    let got = c.get_blob(digest).await.expect("get_blob");
    assert_eq!(got.stat.digest, digest);
    assert_eq!(got.stat.size, payload.len() as u64);
    let bytes = drain(got).await;
    assert_eq!(bytes, payload, "GET body must be byte-equal to PUT body");

    // --- HEAD reports present.
    let stat = c.stat_blob(digest).await.expect("stat_blob");
    let stat = stat.expect("blob should be present after PUT");
    assert_eq!(stat.digest, digest);
    assert_eq!(stat.size, payload.len() as u64);

    // --- missing() reports the PUT blob present (absent from the
    // returned set) and an unwritten blob absent (present in the set).
    let unwritten = Digest::of_bytes(b"never written to this container");
    let missing = c.missing(&[digest, unwritten]).await.expect("missing()");
    assert!(
        !missing.contains(&digest),
        "PUT blob must NOT be reported missing"
    );
    assert!(
        missing.contains(&unwritten),
        "unwritten blob MUST be reported missing"
    );

    eprintln!("container round-trip OK against {image} on host port {host_port}");
    // `_guard` drops here → `docker rm -f` tears the container down,
    // even if any assertion above panicked.
}
