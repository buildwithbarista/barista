# Traffic captures (`bench/captures/`)

Network-traffic captures produced by `scripts/run-baseline-captures.sh`
for the resource-efficiency program. Captures land here on the
operator's local machine and are uploaded to Cloudflare R2 for shared
analysis.

## Layout

```
bench/captures/
  <corpus-id>/
    <tool>-<version>/
      <UTC-timestamp>/
        capture.har        # HAR 1.2, emitted by mitmproxy's `hardump` addon
        metadata.toml      # corpus_id, tool, tool_version, JDK, start/end, exit_code
        build.log          # raw stdout/stderr of the build command
        mitmdump.log       # mitmproxy's own log (warnings / errors)
```

`<UTC-timestamp>` is `YYYY-MM-DDTHH-MM-SSZ` (with `-` instead of `:` so
the path is friendly on every filesystem we care about).

This matches the layout mandated by the project's product requirements
for the traffic-capture harness (one directory per capture session,
keyed by corpus + tool + timestamp).

## Local vs canonical store

The local tree is **gitignored** — HAR files can be tens to hundreds
of megabytes and contain host-specific timing data. The canonical
store is Cloudflare R2; the local directory is a scratch space for
the operator who triggered the capture.

The R2 mirror layout is identical to the local layout above, rooted
at `s3://barista-captures/`. Sync via:

```sh
# (Operator credentials only — see internal runbook for R2 keys.)
aws s3 sync bench/captures/ s3://barista-captures/ \
  --endpoint-url https://<account>.r2.cloudflarestorage.com \
  --exclude '.gitkeep' --exclude 'README.md'
```

Do **not** check captures into git. The `.gitignore` in the repo root
excludes everything under this directory except this README and the
`.gitkeep` sentinel.

## Running the capture matrix

```sh
scripts/run-baseline-captures.sh \
  --projects spring-petclinic,spring-boot-starter-web-app \
  --tools mvn,mvnd
```

See `scripts/run-baseline-captures.sh --help` for the full flag set.
The script materializes the listed corpus projects, spawns
`mitmdump` on an ephemeral port, runs each build through it with a
freshly-allocated cold local Maven repository, and writes the
`capture.har` + `metadata.toml` pair to a timestamped directory under
this tree.

## Prerequisites

1. `mitmproxy` installed (`brew install mitmproxy`).
2. mitmproxy's CA imported into the active JDK's truststore — see
   the one-shot `keytool` recipe in
   `crates/barista-netcap/README.md`. Without this step the JVM
   refuses mitmproxy's TLS cert and every fetch fails with a
   handshake error.
3. The corpus project's `corpus.lock.toml` must exist under
   `test-corpus/<corpus-id>/`.

## What we capture

- `mvn clean install -DskipTests` — captures the full
  dependency-resolution and plugin-resolution traffic without paying
  the wall-time tax for test execution. Tests inflate the HAR by 1-2
  orders of magnitude without adding any resolver-traffic value.
- Cold local Maven repository per session — `--maven.repo.local` is
  pointed at a fresh `mktemp -d` so the capture sees the actual
  upstream fetches a clean-room build would make, not a warm-cache
  no-op.
