#!/usr/bin/env bash
#
# Run baseline network-traffic captures for the resource-efficiency
# program.
#
# For each (project, tool) cell — driven by --projects / --tools — this
# script:
#
#   1. Materializes the project via `scripts/materialize-corpus.sh`.
#   2. Allocates an isolated, *cold* local Maven repository so the
#      capture sees the full cold-fetch dependency-resolution traffic
#      rather than a warm-cache no-op.
#   3. Spawns `mitmdump` on an ephemeral port, configured to dump a HAR
#      to a timestamped output directory under `bench/captures/`.
#   4. Runs the project's build command against the proxy.
#   5. SIGTERMs `mitmdump` and writes a `metadata.toml` sidecar
#      describing the run (tool, version, exit code, wall-time, etc.).
#
# Per PRD §18.8 the output layout is:
#
#   bench/captures/<corpus-id>/<tool>/<timestamp>/
#       capture.har
#       metadata.toml
#
# Local outputs are gitignored; the canonical store is Cloudflare R2
# (see `bench/captures/README.md`).
#
# Usage:
#   scripts/run-baseline-captures.sh \
#       --projects spring-petclinic,spring-boot-starter-web-app \
#       --tools mvn,mvnd \
#       [--mvn-bin /path/to/mvn] \
#       [--mvnd-bin /path/to/mvnd] \
#       [--output-root bench/captures] \
#       [--timeout-seconds 600] \
#       [--mitmdump-bin /path/to/mitmdump] \
#       [--help]
#
# Prerequisites:
#   * `mitmdump` on $PATH (or via --mitmdump-bin) with its CA imported
#     into the active JDK's truststore — see
#     `crates/barista-netcap/README.md` for the one-shot keytool recipe.
#   * `mvn` on $PATH (or --mvn-bin) and, if `mvnd` is in --tools,
#     `mvnd` on $PATH or via --mvnd-bin.
#   * The corpus project's `corpus.lock.toml` must exist under
#     `test-corpus/<id>/`.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PROJECTS=""
TOOLS=""
MVN_BIN="${MVN_BIN:-mvn}"
MVND_BIN="${MVND_BIN:-mvnd}"
MITMDUMP_BIN="${MITMDUMP_BIN:-mitmdump}"
OUTPUT_ROOT="$REPO_ROOT/bench/captures"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-600}"

print_help() {
  awk '
    NR == 1 { next }
    /^#/    { sub(/^# ?/, ""); print; next }
    /^$/    { print ""; next }
    { exit }
  ' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --projects)         PROJECTS="$2"; shift 2 ;;
    --projects=*)       PROJECTS="${1#*=}"; shift ;;
    --tools)            TOOLS="$2"; shift 2 ;;
    --tools=*)          TOOLS="${1#*=}"; shift ;;
    --mvn-bin)          MVN_BIN="$2"; shift 2 ;;
    --mvn-bin=*)        MVN_BIN="${1#*=}"; shift ;;
    --mvnd-bin)         MVND_BIN="$2"; shift 2 ;;
    --mvnd-bin=*)       MVND_BIN="${1#*=}"; shift ;;
    --mitmdump-bin)     MITMDUMP_BIN="$2"; shift 2 ;;
    --mitmdump-bin=*)   MITMDUMP_BIN="${1#*=}"; shift ;;
    --output-root)      OUTPUT_ROOT="$2"; shift 2 ;;
    --output-root=*)    OUTPUT_ROOT="${1#*=}"; shift ;;
    --timeout-seconds)  TIMEOUT_SECONDS="$2"; shift 2 ;;
    --timeout-seconds=*) TIMEOUT_SECONDS="${1#*=}"; shift ;;
    -h|--help)          print_help; exit 0 ;;
    *)
      echo "error: unknown argument: $1" >&2
      echo "try: $0 --help" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$PROJECTS" || -z "$TOOLS" ]]; then
  echo "error: --projects and --tools are both required" >&2
  echo "try: $0 --help" >&2
  exit 2
fi

if ! command -v "$MITMDUMP_BIN" >/dev/null 2>&1; then
  echo "error: mitmdump not found: $MITMDUMP_BIN" >&2
  echo "install via: brew install mitmproxy   (or pipx install mitmproxy)" >&2
  exit 1
fi

# ---------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------

# Resolve the version string the given tool will report. Trimmed and
# free of any "WARNING:" prefix lines that newer JDKs emit.
tool_version() {
  local tool="$1"
  case "$tool" in
    mvn)
      "$MVN_BIN" --version 2>/dev/null \
        | awk 'NR==1{ for (i=1; i<=NF; i++) if ($i ~ /^[0-9]/) { print $i; exit } }'
      ;;
    mvnd)
      "$MVND_BIN" --version 2>/dev/null \
        | awk '/^Apache Maven Daemon/ { print $5; exit }'
      ;;
    *)
      echo "unknown"
      ;;
  esac
}

# Tool subdirectory name including the resolved version, so multiple
# baseline runs at different versions don't overwrite each other.
tool_dirname() {
  local tool="$1" ver
  ver="$(tool_version "$tool")"
  echo "${tool}-${ver:-unknown}"
}

# Allocate an unused TCP port. Mirrors the approach taken by
# `barista-netcap::session::allocate_free_port`: bind ephemeral 0,
# read back the kernel assignment, release. The TOCTOU window is
# unobservable in practice on a single-tenant capture host.
allocate_free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

# Spawn mitmdump in the background and echo its PID. Caller is
# responsible for SIGTERMing it.
start_proxy() {
  local port="$1" har_out="$2" log_out="$3"
  "$MITMDUMP_BIN" \
      --listen-port "$port" \
      --set "hardump=$har_out" \
      --set termlog_verbosity=warn \
      --ssl-insecure \
      >"$log_out" 2>&1 &
  echo $!
}

# Wait until the given port accepts a TCP connection, or fail after a
# few seconds. mitmdump does not emit a structured "ready" signal so
# polling is the canonical readiness check.
wait_for_port() {
  local port="$1" deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    if (echo >"/dev/tcp/127.0.0.1/$port") >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}

# Run one (project, tool) cell end-to-end. Failures here don't abort
# sibling cells — each cell records its exit code in metadata.toml so
# the operator can review the matrix as a whole.
run_one_cell() {
  local project="$1" tool="$2"
  local checkout="$REPO_ROOT/test-corpus/$project/checkout"
  if [[ ! -d "$checkout" ]]; then
    echo "[skip] $project / $tool: checkout missing — run materialize-corpus.sh first" >&2
    return 1
  fi

  local timestamp
  timestamp="$(date -u +"%Y-%m-%dT%H-%M-%SZ")"
  local tool_dir
  tool_dir="$(tool_dirname "$tool")"
  local out_dir="$OUTPUT_ROOT/$project/$tool_dir/$timestamp"
  mkdir -p "$out_dir"

  local har_out="$out_dir/capture.har"
  local meta_out="$out_dir/metadata.toml"
  local build_log="$out_dir/build.log"
  local proxy_log="$out_dir/mitmdump.log"
  local cold_repo
  cold_repo="$(mktemp -d "${TMPDIR:-/tmp}/barista-netcap-cold-repo.XXXXXX")"

  local port
  port="$(allocate_free_port)"

  echo "[$project/$tool_dir] starting proxy on 127.0.0.1:$port"
  local proxy_pid
  proxy_pid="$(start_proxy "$port" "$har_out" "$proxy_log")"

  # Ensure the proxy is reaped even on caller-level failures.
  # shellcheck disable=SC2064
  trap "kill -TERM $proxy_pid 2>/dev/null || true; rm -rf '$cold_repo'" RETURN

  if ! wait_for_port "$port"; then
    echo "[$project/$tool_dir] proxy never came up on port $port" >&2
    kill -KILL "$proxy_pid" 2>/dev/null || true
    return 1
  fi

  local start_epoch end_epoch exit_code
  start_epoch="$(date -u +%s)"

  # Build command: `clean install -DskipTests`. Captures should not
  # include test execution — tests inflate the HAR by 1-2 orders of
  # magnitude without adding any resolver-traffic value.
  #
  # Routing Maven through the proxy is non-trivial: Maven Resolver
  # (Aether) does NOT honour the JVM-level `https.proxyHost` /
  # `http.proxyHost` system properties — it ignores them by design and
  # only consults the `<proxies>` block of `settings.xml`. We
  # synthesize a one-shot `settings.xml` per session and feed it via
  # `--settings`, which is the only reliable proxy mechanism for both
  # `mvn 3.9.x` and `mvnd 2.x` (which embeds Maven 4.x).
  local settings_xml="$out_dir/settings.xml"
  cat >"$settings_xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<settings xmlns="http://maven.apache.org/SETTINGS/1.0.0">
  <localRepository>$cold_repo</localRepository>
  <proxies>
    <proxy>
      <id>barista-netcap-https</id>
      <active>true</active>
      <protocol>https</protocol>
      <host>127.0.0.1</host>
      <port>$port</port>
    </proxy>
    <proxy>
      <id>barista-netcap-http</id>
      <active>true</active>
      <protocol>http</protocol>
      <host>127.0.0.1</host>
      <port>$port</port>
    </proxy>
  </proxies>
</settings>
EOF

  local mvn_args=(
    "-B" "clean" "install"
    "--settings" "$settings_xml"
    "-Dmaven.repo.local=$cold_repo"
    "-DskipTests"
  )

  set +e
  case "$tool" in
    mvn)
      ( cd "$checkout" && \
        timeout "$TIMEOUT_SECONDS" \
        "$MVN_BIN" "${mvn_args[@]}" \
      ) >"$build_log" 2>&1
      exit_code=$?
      ;;
    mvnd)
      # `mvnd` forwards user-mode args to the daemon's embedded Maven,
      # so the same `--settings` flag works.
      ( cd "$checkout" && \
        timeout "$TIMEOUT_SECONDS" \
        "$MVND_BIN" "${mvn_args[@]}" \
      ) >"$build_log" 2>&1
      exit_code=$?
      ;;
    *)
      echo "[$project/$tool_dir] unknown tool: $tool" >&2
      exit_code=99
      ;;
  esac
  set -e

  end_epoch="$(date -u +%s)"

  # SIGTERM the proxy and wait for it to flush the HAR. mitmproxy's
  # `hardump` addon registers an atexit-style handler, so SIGTERM is
  # sufficient — but we give it up to 5 seconds before escalating to
  # SIGKILL, because flushing a large HAR (tens of MB on big corpora)
  # is not instantaneous.
  kill -TERM "$proxy_pid" 2>/dev/null || true
  local kill_deadline=$((SECONDS + 5))
  while kill -0 "$proxy_pid" 2>/dev/null && (( SECONDS < kill_deadline )); do
    sleep 0.2
  done
  if kill -0 "$proxy_pid" 2>/dev/null; then
    kill -KILL "$proxy_pid" 2>/dev/null || true
  fi
  wait "$proxy_pid" 2>/dev/null || true

  local har_size=0
  if [[ -f "$har_out" ]]; then
    har_size="$(wc -c <"$har_out" | tr -d ' \n')"
  fi

  # Write the metadata sidecar. Keep keys in stable order so a `diff`
  # across runs is easy to read.
  cat >"$meta_out" <<EOF
# Auto-generated by scripts/run-baseline-captures.sh. Do not edit by
# hand; re-run the capture to refresh.

corpus_id     = "$project"
tool          = "$tool"
tool_version  = "$(tool_version "$tool")"
jdk           = "$(java -version 2>&1 | awk -F\" '/version/ { print $2; exit }')"
host_os       = "$(uname -srm)"
mitmdump      = "$($MITMDUMP_BIN --version 2>&1 | awk '/^Mitmproxy/ { print $2; exit }')"

start_utc     = "$(date -u -r "$start_epoch" +"%Y-%m-%dT%H:%M:%SZ")"
end_utc       = "$(date -u -r "$end_epoch"   +"%Y-%m-%dT%H:%M:%SZ")"
wall_seconds  = $((end_epoch - start_epoch))

build_command = "${MVN_BIN##*/} -B clean install -DskipTests"
exit_code     = $exit_code
har_bytes     = $har_size
EOF

  echo "[$project/$tool_dir] done in $((end_epoch - start_epoch))s — exit=$exit_code, har=${har_size}B"
  trap - RETURN
  rm -rf "$cold_repo"
  return 0
}

# ---------------------------------------------------------------------
# Matrix driver
# ---------------------------------------------------------------------

IFS=',' read -ra PROJECT_LIST <<< "$PROJECTS"
IFS=',' read -ra TOOL_LIST <<< "$TOOLS"

# Materialize all listed projects up front so a missing corpus entry
# fails the run before the first proxy is spawned.
for proj in "${PROJECT_LIST[@]}"; do
  bash "$REPO_ROOT/scripts/materialize-corpus.sh" --filter "$proj" >&2
done

RC=0
for proj in "${PROJECT_LIST[@]}"; do
  for tool in "${TOOL_LIST[@]}"; do
    run_one_cell "$proj" "$tool" || RC=$?
  done
done

echo
echo "summary: $(( ${#PROJECT_LIST[@]} * ${#TOOL_LIST[@]} )) cells run, output root: $OUTPUT_ROOT"
exit "$RC"
