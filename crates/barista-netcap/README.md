# barista-netcap

Traffic-capture harness for Barista's resource-efficiency program.

`barista-netcap` records the HTTP/HTTPS conversation between a JVM build
tool (`mvn`, `mvnd`, `barista` itself) and the upstream Maven
repositories it talks to during a benchmark run. The recording is
written to a `.har` ([HTTP Archive][har-spec]) file that
`barista-netanalyze` then mines for redundant fetches, missed
compression, connection churn, and similar efficiency anti-patterns.

## Why mitmproxy, not pcap

Maven traffic is TLS-encrypted: Sonatype OSSRH, Maven Central CDN, and
every commercial Artifactory / Nexus deployment terminate HTTPS at the
edge. A passive `pcap` capture would tell you which bytes went where but
nothing about the HTTP semantics — no headers, no `If-Modified-Since`,
no `Accept-Encoding`, no `Cache-Control`. The whole efficiency program
revolves around HTTP semantics, so the harness needs an active TLS-MITM
proxy with HAR output. mitmproxy is the canonical mature implementation
of exactly that.

## Install mitmproxy locally

```sh
brew install mitmproxy        # macOS / Linuxbrew
pipx install mitmproxy        # cross-platform via Python
```

Other install routes live at <https://mitmproxy.org/#install>.

## Set up the CA

The first time you run mitmproxy it writes a self-signed CA bundle to
`~/.mitmproxy/`. For the JDK to accept mitmproxy-signed certs you must
import that CA into the active JDK's truststore:

```sh
# 1. Generate the CA (only needed once; mitmdump exits immediately if
#    started with --listen-port 0).
mitmdump --listen-port 0 &
sleep 1 && kill %1

# 2. Import it.
sudo keytool -importcert -trustcacerts \
  -keystore "$JAVA_HOME/lib/security/cacerts" \
  -storepass changeit \
  -alias mitmproxy-barista \
  -file ~/.mitmproxy/mitmproxy-ca-cert.pem

# 3. Verify.
keytool -list -keystore "$JAVA_HOME/lib/security/cacerts" \
  -storepass changeit -alias mitmproxy-barista
```

`barista-netcap` will *not* perform this import for you. It is a
deliberately consent-gated operation: importing a root CA into the JDK
truststore expands what TLS certificates the JVM will accept, and that
warrants an explicit user decision.

To check the current state of the host:

```rust
use barista_netcap::CaSetup;
println!("{:#?}", CaSetup::ensure_installed()?);
```

## Capture a Maven build (manual)

```sh
# Terminal A: start the proxy.
mitmdump --listen-port 8080 \
  --set hardump=./out.har \
  --ssl-insecure

# Terminal B: route Maven through it.
mvn -Dhttps.proxyHost=127.0.0.1 -Dhttps.proxyPort=8080 \
    -Dhttp.proxyHost=127.0.0.1  -Dhttp.proxyPort=8080  \
    -DskipTests clean verify

# Stop with Ctrl-C in Terminal A. `out.har` is your capture.
```

For programmatic use, drive `CaptureSession::start` / `::stop` from
Rust:

```rust
use barista_netcap::{CaptureConfig, CaptureSession};

let cfg = CaptureConfig::for_har("out.har");
let session = CaptureSession::start(cfg).await?;
let port = session.listen_port();
// ... spawn `mvn` with HTTPS_PROXY=127.0.0.1:{port} ...
let summary = session.stop().await?;
println!("captured {} requests", summary.har.entry_count);
```

## Tests

```sh
# Default: stub-subprocess lifecycle + HAR validator + CA reporter.
# Runs cleanly even with mitmproxy uninstalled.
cargo test -p barista-netcap

# Optional: real-mitmdump round-trip. Requires mitmdump on $PATH.
cargo test -p barista-netcap -- --ignored
```

## Stability

Pre-1.0. The crate is built around a single near-term consumer
(`barista-bench` for benchmark captures); the public surface may evolve
as `barista-netanalyze` and the matrix-capture CLI take shape.

[har-spec]: http://www.softwareishard.com/blog/har-12-spec/
