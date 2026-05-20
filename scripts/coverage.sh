#!/usr/bin/env bash
# Workspace test-coverage gate.
#
# Runs `cargo llvm-cov` over the whole workspace, aggregates per-crate
# line / function / region coverage, prints a table, and enforces the
# coverage policy described below. The companion report (methodology,
# the latest recorded numbers, and the gap analysis) lives at
# `docs/perf/coverage.md`.
#
# Usage:
#   bash scripts/coverage.sh            # run coverage + enforce the gate
#   bash scripts/coverage.sh --report-only   # print the table, never fail
#
# Optional environment:
#   COVERAGE_TIMEOUT   wall-clock bound for the instrumented build+test
#                      run, in seconds. Default 1200 (20 min). Coverage is
#                      an instrumented build of every crate plus the full
#                      test run, so it is far slower than a normal build.
#   COVERAGE_JSON      where to write the machine-readable LLVM export.
#                      Default: a temp file cleaned up on exit.
#
# --------------------------------------------------------------------------
# GATE POLICY (v0.1)
#
# Targets, per crate:   line >= 80%   function >= 80%   (region reported)
#
#   - PRIORITY modules are a HARD gate. If any priority module is below
#     target on line OR function coverage, this script exits non-zero.
#     The priority modules are the core correctness surface:
#         barista-resolver  barista-cache  barista-lockfile
#
#   - NON-PRIORITY crates are ADVISORY at v0.1. A miss is printed (and
#     flagged FAIL in the table) but does NOT fail the gate. This keeps
#     the gate honest about the whole tree while only blocking on the
#     crates whose correctness most matters today. Tighten to hard later.
#
# BRANCH coverage:
#   True branch coverage (`-Z coverage-options=branch`) is a nightly-only
#   rustc feature. The workspace pins STABLE rust (see rust-toolchain.toml),
#   so this gate measures line + function + region coverage and does NOT
#   gate on branch %. The 70% branch target is recorded in the report as a
#   forward-looking goal; measuring it requires a nightly run and is out of
#   scope for the stable CI gate. Region coverage (a finer-grained,
#   stable-toolchain proxy for control-flow coverage) is reported alongside.
#
# --------------------------------------------------------------------------
# Toolchain note (asdf / no rustup):
#   cargo-llvm-cov normally locates `llvm-cov` / `llvm-profdata` via
#   `rustup which`. On a rustup-less toolchain (e.g. asdf-managed rust)
#   we point it at the tools shipped inside the active toolchain's
#   sysroot by exporting LLVM_COV / LLVM_PROFDATA. This is a no-op when
#   the vars are already set or when the tools are otherwise discoverable.
# --------------------------------------------------------------------------

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
export REPO_ROOT
cd "${REPO_ROOT}"

# Targets + priority set. `-rx` = read-only AND exported, so the Python
# roll-up below inherits them from the environment without re-assignment
# (re-assigning a read-only var on a command prefix would be an error).
declare -rx LINE_TARGET=80
declare -rx FUNC_TARGET=80
declare -rx BRANCH_TARGET=70   # recorded goal; not gated on stable (see header)
declare -rx PRIORITY_MODULES="barista-resolver barista-cache barista-lockfile"

COVERAGE_TIMEOUT="${COVERAGE_TIMEOUT:-1200}"

MODE="gate"
case "${1:-}" in
  --report-only) MODE="report" ;;
  "")            MODE="gate" ;;
  *)
    echo "usage: $0 [--report-only]" >&2
    exit 64  # EX_USAGE
    ;;
esac

# --------------------------------------------------------------------------
# Locate a `timeout` binary so the instrumented run is always bounded.
# coreutils ships it as `timeout` (Linux, Homebrew) or `gtimeout`.
# --------------------------------------------------------------------------
TIMEOUT_BIN=""
for cand in timeout gtimeout /opt/homebrew/bin/timeout; do
  if command -v "${cand}" >/dev/null 2>&1; then
    TIMEOUT_BIN="${cand}"
    break
  fi
done
if [[ -z "${TIMEOUT_BIN}" ]]; then
  echo "::error::no 'timeout'/'gtimeout' found; refusing to run coverage \
unbounded. Install coreutils." >&2
  exit 1
fi

# --------------------------------------------------------------------------
# Ensure cargo-llvm-cov is available.
# --------------------------------------------------------------------------
if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "::error::cargo-llvm-cov is not installed. Install the pinned \
version with:" >&2
  echo "    cargo install cargo-llvm-cov --version 0.8.7 --locked" >&2
  exit 1
fi

# --------------------------------------------------------------------------
# Point cargo-llvm-cov at the toolchain's llvm tools if they are not
# already discoverable (rustup-less toolchains). Derived portably from
# `rustc --print sysroot` + the host triple.
# --------------------------------------------------------------------------
if [[ -z "${LLVM_COV:-}" || -z "${LLVM_PROFDATA:-}" ]]; then
  sysroot="$(rustc --print sysroot)"
  host="$(rustc -vV | sed -n 's/^host: //p')"
  toolbin="${sysroot}/lib/rustlib/${host}/bin"
  if [[ -x "${toolbin}/llvm-cov" && -x "${toolbin}/llvm-profdata" ]]; then
    export LLVM_COV="${toolbin}/llvm-cov"
    export LLVM_PROFDATA="${toolbin}/llvm-profdata"
  fi
fi

# --------------------------------------------------------------------------
# JSON export path (machine-readable; aggregated per crate below).
# --------------------------------------------------------------------------
CLEANUP_JSON=0
if [[ -n "${COVERAGE_JSON:-}" ]]; then
  json_path="${COVERAGE_JSON}"
else
  json_path="$(mktemp "${TMPDIR:-/tmp}/barista-cov.XXXXXX.json")"
  CLEANUP_JSON=1
fi
cleanup() { [[ "${CLEANUP_JSON}" -eq 1 ]] && rm -f "${json_path}"; }
trap cleanup EXIT

# --------------------------------------------------------------------------
# 1. Bounded instrumented build + test run. `--summary-only` keeps the
#    captured coverage data lean; we re-export JSON from the same profile
#    data afterwards (no rebuild). Docker-gated tests are `#[ignore]`d and
#    do not run here.
# --------------------------------------------------------------------------
echo "=== running bounded coverage (timeout ${COVERAGE_TIMEOUT}s) ==="
"${TIMEOUT_BIN}" "${COVERAGE_TIMEOUT}" \
  cargo llvm-cov --workspace --summary-only

# --------------------------------------------------------------------------
# 2. Re-export the same coverage data as JSON (reuses captured profdata;
#    does not re-instrument or re-run tests).
# --------------------------------------------------------------------------
"${TIMEOUT_BIN}" 300 \
  cargo llvm-cov report --json --output-path "${json_path}"

# --------------------------------------------------------------------------
# 3. Aggregate per crate, print the table, enforce the policy. The
#    per-crate roll-up + threshold logic is in Python for readable
#    integer-exact aggregation; the gate verdict is returned via exit code.
# --------------------------------------------------------------------------
# LINE_TARGET / FUNC_TARGET / BRANCH_TARGET / PRIORITY_MODULES / REPO_ROOT
# are already exported (see above); only GATE_MODE is passed inline.
GATE_MODE="${MODE}" \
python3 - "${json_path}" <<'PY'
import collections, json, os, re, sys

data = json.load(open(sys.argv[1]))
root = os.environ["REPO_ROOT"].rstrip("/") + "/"
line_t = int(os.environ["LINE_TARGET"])
func_t = int(os.environ["FUNC_TARGET"])
priority = set(os.environ["PRIORITY_MODULES"].split())
mode = os.environ["GATE_MODE"]

agg = collections.defaultdict(
    lambda: {"lc": 0, "lcov": 0, "fc": 0, "fcov": 0, "rc": 0, "rcov": 0}
)


def crate_of(path):
    rel = path[len(root):] if path.startswith(root) else path
    m = re.match(r"crates/([^/]+)/", rel)
    if m:
        return m.group(1)
    if rel.startswith("roastery/"):
        return "roastery"
    if rel.startswith("xtask/"):
        return "xtask"
    return None


for export in data["data"]:
    for f in export["files"]:
        crate = crate_of(f["filename"])
        if crate is None:
            continue
        s = f["summary"]
        a = agg[crate]
        a["lc"] += s["lines"]["count"]
        a["lcov"] += s["lines"]["covered"]
        a["fc"] += s["functions"]["count"]
        a["fcov"] += s["functions"]["covered"]
        a["rc"] += s["regions"]["count"]
        a["rcov"] += s["regions"]["covered"]


def pct(cov, cnt):
    return (100.0 * cov / cnt) if cnt else 0.0


hdr = f"{'crate':27} {'line%':>7} {'func%':>7} {'region%':>8}  {'pri':>3}  result"
print()
print(hdr)
print("-" * len(hdr))

priority_fail = False
nonpriority_fail = False
for crate in sorted(agg):
    a = agg[crate]
    lp = pct(a["lcov"], a["lc"])
    fp = pct(a["fcov"], a["fc"])
    rp = pct(a["rcov"], a["rc"])
    is_pri = crate in priority
    meets = lp >= line_t and fp >= func_t
    if not meets:
        if is_pri:
            priority_fail = True
        else:
            nonpriority_fail = True
    verdict = "PASS" if meets else ("FAIL(advisory)" if not is_pri else "FAIL")
    print(
        f"{crate:27} {lp:7.2f} {fp:7.2f} {rp:8.2f}  {'*' if is_pri else '':>3}"
        f"  {verdict}"
    )

print()
print(f"targets: line >= {line_t}%  function >= {func_t}%   (* = priority module, hard gate)")
print("branch coverage is nightly-only and not gated on the stable toolchain; see docs/perf/coverage.md")

if mode == "report":
    print("\n=== --report-only: not enforcing the gate ===")
    sys.exit(0)

if priority_fail:
    print("\n::error::a PRIORITY module is below the coverage target (hard gate).")
    sys.exit(1)

if nonpriority_fail:
    print("\n=== gate PASS: all priority modules meet target. "
          "Non-priority misses above are advisory at v0.1. ===")
else:
    print("\n=== gate PASS: every crate meets the coverage target. ===")
sys.exit(0)
PY
