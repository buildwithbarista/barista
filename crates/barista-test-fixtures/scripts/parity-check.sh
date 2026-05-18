#!/usr/bin/env bash
#
# parity-check.sh — artifact-divergence test harness.
#
# For each corpus project, run the project's `build_cmd` twice:
#
#   1. Reference path: bare `mvn` (the pinned reference Maven from
#      `.tool-versions`; currently 3.9.9 on JDK 21).
#   2. Barista path:   `barista verify --no-daemon`, which forks the
#      same `mvn` binary downstream. In v0.1 the `--no-daemon` mode
#      is the unconditional surface that always works on arbitrary
#      corpus projects; the daemon path's parity coverage is a v0.2
#      follow-up (gated on staged Maven 4 distribution per the M4.0
#      spike rationale).
#
# Both runs target independent project checkouts and use independent
# local-repo dirs so cache state doesn't leak across paths. After both
# runs finish, every regular file under each path's `target/` tree is
# SHA-256-hashed and compared. The exit code is non-zero iff any file
# differs, accounting for the documented ignore list (see IGNORE_GLOBS
# below).
#
# Usage:
#
#   crates/barista-test-fixtures/scripts/parity-check.sh \
#       [--corpus-dir DIR] \
#       [--filter PATTERN] \
#       [--barista BIN] \
#       [--keep-work] \
#       [--compare-only MVN_TARGET BARISTA_TARGET] \
#       [--help]
#
# Flags:
#
#   --corpus-dir DIR
#       Directory containing `<id>/corpus.lock.toml` entries to
#       parity-check. Defaults to `<repo-root>/test-corpus`. The
#       meta-test under `scripts/test-parity-check.sh` points this at
#       a self-contained fixture set to exercise the harness without
#       building the full corpus.
#
#   --filter PATTERN
#       Shell glob matched against each entry's `id`; only matching
#       entries are checked. Default: `*` (all).
#
#   --barista BIN
#       Path to the `barista` binary. Defaults to
#       `$BARISTA_BIN`, falling back to `cargo run -p barista-cli --
#       barista` when unset. Set this in CI to point at the
#       prebuilt release binary so the harness doesn't trigger a
#       compile.
#
#   --keep-work
#       Don't delete the per-project work tree on exit. Useful for
#       inspecting divergent `target/` trees after a FAIL.
#
#   --compare-only MVN_TARGET BARISTA_TARGET
#       Skip both builds; just SHA-256-diff the two pre-built target
#       trees as if they were produced by paths 1 and 2 above. Used
#       by the meta-test to assert the comparison logic flags a
#       deliberately-perturbed byte. Exits 0 on byte-equality, 3 on
#       divergence (same as the comparison path of a real run).
#
#   --help, -h
#       Show this message.
#
# Output:
#
#   One status line per project: `[<id>] PASS|FAIL|SKIP <reason>`.
#   On FAIL, the diverging path set is printed underneath (one line
#   per file, with the mvn-side hash and the barista-side hash).
#   The footer is a single summary line `<N> projects checked, <P>
#   PASS, <F> FAIL, <S> SKIP`.
#
# Exit codes:
#
#   0   All checked projects byte-equal across both paths.
#   1   Usage error (bad flag, missing required argument).
#   2   Environment error (no `mvn`/`barista` on PATH, missing
#       corpus directory).
#   3   At least one project diverged.
#
# Status (v0.1):
#
#   The harness's full value comes online with the daemon-path
#   parity (v0.2). Today, `barista verify --no-daemon` delegates the
#   entire build to upstream `mvn`, so reference-mvn and barista-mvn
#   produce byte-equal `target/` trees by construction (modulo the
#   ignore list). This is still load-bearing as a regression gate:
#   it locks in the v0.1 guarantee that `--no-daemon` is a safe
#   fallback that doesn't perturb build outputs.

set -euo pipefail

# Resolve the repo root from this script's location so the harness can
# be invoked from anywhere (CI's checkout-root, a developer's pwd, the
# meta-test's tempdir wrapper, etc.).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

CORPUS_DIR="${REPO_ROOT}/test-corpus"
FILTER="*"
BARISTA_BIN="${BARISTA_BIN:-}"
KEEP_WORK=0
COMPARE_ONLY_MVN=""
COMPARE_ONLY_BARISTA=""

# ---------------------------------------------------------------------
# Ignore list — paths under `target/` that are known to be non-
# byte-reproducible across runs even on a deterministic toolchain.
# Each entry is a bash extglob applied via `[[ "$rel" == $pattern ]]`
# (the loop sets `extglob` once at start). Adding to this list
# requires a one-line comment justifying why the path is excluded.
# ---------------------------------------------------------------------
IGNORE_GLOBS=(
  # Surefire/Failsafe report files embed wall-clock build times and
  # per-test elapsed-ms numbers; SOURCE_DATE_EPOCH does not flow
  # through to these. Documented as a known gap in M4.3 T6's IT
  # module docstring; v0.1 byte-equality scope is `target/classes/**`
  # + `target/*.jar`, not surefire reports.
  'surefire-reports/*'
  'failsafe-reports/*'

  # Maven-jar-plugin temp staging directories (`maven-archiver/`,
  # `maven-status/`) contain plugin-internal state that's a function
  # of the build's wall-clock timestamps when the archiver/timestamp
  # plumbing isn't fully wired. Their contents don't affect the
  # produced JAR; we hash the JAR itself.
  'maven-status/*'

  # Per-execution timing logs from various plugins (animal-sniffer,
  # build-helper, etc.). These are diagnostic dumps, not artifacts.
  '*.log'
  '*.tmp'
)

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
    --corpus-dir)    CORPUS_DIR="$2"; shift 2 ;;
    --corpus-dir=*)  CORPUS_DIR="${1#*=}"; shift ;;
    --filter)        FILTER="$2"; shift 2 ;;
    --filter=*)      FILTER="${1#*=}"; shift ;;
    --barista)       BARISTA_BIN="$2"; shift 2 ;;
    --barista=*)     BARISTA_BIN="${1#*=}"; shift ;;
    --keep-work)     KEEP_WORK=1; shift ;;
    --compare-only)
      if [[ $# -lt 3 ]]; then
        echo "error: --compare-only needs MVN_TARGET BARISTA_TARGET" >&2
        exit 1
      fi
      COMPARE_ONLY_MVN="$2"
      COMPARE_ONLY_BARISTA="$3"
      shift 3
      ;;
    -h|--help)       print_help; exit 0 ;;
    *)
      echo "error: unknown argument: $1" >&2
      echo "try: $0 --help" >&2
      exit 1
      ;;
  esac
done

shopt -s extglob nullglob

# Minimal TOML reader for `key = "value"` lines. Same shape as the one
# in `scripts/materialize-corpus.sh`; duplicated rather than sourced
# because the harness must remain runnable from a plain checkout
# without sourcing siblings. Skips `"""..."""` multi-line strings and
# `#` comments. Unknown keys are returned as empty strings.
read_lock_value() {
  local file="$1" key="$2"
  awk -v k="$key" '
    BEGIN { FS="="; in_multi=0 }
    {
      if (in_multi) { if ($0 ~ /"""[[:space:]]*$/) in_multi=0; next }
      if ($0 ~ /=[[:space:]]*"""/ && $0 !~ /""".*"""/) { in_multi=1; next }
      if ($0 ~ /^[[:space:]]*#/) next
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", $1)
      if ($1 != k) next
      $1=""; v=$0; sub(/^=/, "", v)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
      if (v ~ /^".*"$/) { v=substr(v, 2, length(v)-2) }
      print v
      exit
    }
  ' "$file"
}

# SHA-256 helper — `shasum -a 256` on macOS, `sha256sum` on Linux.
# Mirrors `cmd_verify_ci_reproducibility.rs::sha256_file` so the harness
# uses the same hashing tool as the M4.3 T6 reproducibility AC.
sha256_file() {
  local path="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
    return
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
    return
  fi
  echo "error: neither shasum nor sha256sum on PATH" >&2
  exit 2
}

# Collect every regular file under `dir`, emit paths relative to `dir`,
# one per line, sorted. Used to produce a stable file set for cross-
# tree comparison.
collect_files() {
  local dir="$1"
  if [[ ! -d "$dir" ]]; then
    return 0
  fi
  ( cd "$dir" && find . -type f -print | sed 's|^\./||' | LC_ALL=C sort )
}

# Returns 0 if `rel` matches any pattern in IGNORE_GLOBS, else 1.
is_ignored() {
  local rel="$1" pat
  for pat in "${IGNORE_GLOBS[@]}"; do
    # shellcheck disable=SC2053 # intentional glob match, not literal
    if [[ "$rel" == $pat ]]; then
      return 0
    fi
    # Also match the pattern at any directory depth (e.g. `*.log`
    # should match both `foo.log` and `nested/dir/foo.log`).
    # shellcheck disable=SC2053
    if [[ "$rel" == */$pat ]]; then
      return 0
    fi
  done
  return 1
}

# Compare two target/ trees. Echoes any per-file divergences to stdout
# and sets RC=3 on any divergence, leaving RC unchanged on equality.
# Reads/writes the global RC; callers initialize to 0 and check after.
compare_target_trees() {
  local lhs="$1" rhs="$2" project_id="$3"

  # Build union of files, applying the ignore list. The harness compares
  # the file sets first; a file present on one side and absent on the
  # other is its own divergence class ("missing artifact").
  local lhs_files rhs_files
  lhs_files="$(collect_files "$lhs")"
  rhs_files="$(collect_files "$rhs")"

  local -A lhs_set=() rhs_set=()
  local f
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    if is_ignored "$f"; then continue; fi
    lhs_set["$f"]=1
  done <<< "$lhs_files"
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    if is_ignored "$f"; then continue; fi
    rhs_set["$f"]=1
  done <<< "$rhs_files"

  local divergences=0
  local -a union=()
  for f in "${!lhs_set[@]}"; do union+=("$f"); done
  for f in "${!rhs_set[@]}"; do
    [[ -z "${lhs_set[$f]:-}" ]] && union+=("$f")
  done

  # Sort the union for stable output.
  local sorted_union
  if [[ ${#union[@]} -gt 0 ]]; then
    sorted_union="$(printf '%s\n' "${union[@]}" | LC_ALL=C sort)"
  else
    sorted_union=""
  fi

  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    local in_lhs="${lhs_set[$f]:-}" in_rhs="${rhs_set[$f]:-}"
    if [[ -z "$in_lhs" ]]; then
      echo "  [${project_id}] missing on mvn side:     ${f}"
      divergences=$((divergences + 1))
      continue
    fi
    if [[ -z "$in_rhs" ]]; then
      echo "  [${project_id}] missing on barista side: ${f}"
      divergences=$((divergences + 1))
      continue
    fi
    local lhs_hash rhs_hash
    lhs_hash="$(sha256_file "$lhs/$f")"
    rhs_hash="$(sha256_file "$rhs/$f")"
    if [[ "$lhs_hash" != "$rhs_hash" ]]; then
      echo "  [${project_id}] hash mismatch on ${f}"
      echo "      mvn:     ${lhs_hash}"
      echo "      barista: ${rhs_hash}"
      divergences=$((divergences + 1))
    fi
  done <<< "$sorted_union"

  if (( divergences > 0 )); then
    RC=3
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------
# Compare-only mode: short-circuit before any build, just diff two
# pre-built target/ trees. Used by the meta-test to exercise the
# comparison logic without spawning Maven.
# ---------------------------------------------------------------------
if [[ -n "$COMPARE_ONLY_MVN" ]]; then
  if [[ ! -d "$COMPARE_ONLY_MVN" ]]; then
    echo "error: --compare-only MVN_TARGET not a directory: $COMPARE_ONLY_MVN" >&2
    exit 2
  fi
  if [[ ! -d "$COMPARE_ONLY_BARISTA" ]]; then
    echo "error: --compare-only BARISTA_TARGET not a directory: $COMPARE_ONLY_BARISTA" >&2
    exit 2
  fi
  RC=0
  if compare_target_trees "$COMPARE_ONLY_MVN" "$COMPARE_ONLY_BARISTA" "compare-only"; then
    echo "[compare-only] PASS"
  else
    echo "[compare-only] FAIL"
  fi
  exit "$RC"
fi

# ---------------------------------------------------------------------
# Environment validation for the build-and-compare path.
# ---------------------------------------------------------------------
if [[ ! -d "$CORPUS_DIR" ]]; then
  echo "error: corpus dir does not exist: $CORPUS_DIR" >&2
  exit 2
fi

if ! command -v mvn >/dev/null 2>&1; then
  echo "error: no \`mvn\` on PATH; the parity-check needs upstream Maven" >&2
  exit 2
fi

# Resolve the barista binary. If `--barista` / `$BARISTA_BIN` is unset,
# the harness assumes a developer-loop run and uses `cargo run` from
# the repo root. CI should always pass a prebuilt binary via
# `BARISTA_BIN=<path>` so the harness doesn't trigger a compile inside
# each project's shell.
if [[ -z "$BARISTA_BIN" ]]; then
  BARISTA_INVOKE="cargo run --quiet --release -p barista-cli --bin barista --manifest-path ${REPO_ROOT}/Cargo.toml --"
else
  if [[ ! -x "$BARISTA_BIN" ]]; then
    echo "error: BARISTA_BIN is not an executable file: $BARISTA_BIN" >&2
    exit 2
  fi
  BARISTA_INVOKE="$BARISTA_BIN"
fi

# Build the worklist. Each entry is tab-separated:
#   id\tref_kind\tbuild_cmd\tlock_dir
WORKLIST=()
while IFS= read -r -d '' lockfile; do
  dir="$(dirname "$lockfile")"
  id="$(read_lock_value "$lockfile" id)"
  kind="$(read_lock_value "$lockfile" ref_kind)"
  build_cmd="$(read_lock_value "$lockfile" build_cmd)"
  [[ -z "$build_cmd" ]] && build_cmd="mvn -B -DskipTests=false verify"

  if [[ -z "$id" || -z "$kind" ]]; then
    echo "[skip] $lockfile: missing required key (id/ref_kind)" >&2
    continue
  fi
  # shellcheck disable=SC2053
  if [[ "$id" != $FILTER ]]; then
    continue
  fi

  WORKLIST+=("$id"$'\t'"$kind"$'\t'"$build_cmd"$'\t'"$dir")
done < <(find "$CORPUS_DIR" -mindepth 2 -maxdepth 2 -name corpus.lock.toml -print0)

if [[ ${#WORKLIST[@]} -eq 0 ]]; then
  echo "no projects matched filter: $FILTER (corpus: $CORPUS_DIR)" >&2
  exit 0
fi

# Work-root: scratch space for per-project mvn-side / barista-side
# checkouts + isolated local Maven repos.
WORK_ROOT="$(mktemp -d -t parity-check.XXXXXX)"
# shellcheck disable=SC2329 # invoked via `trap cleanup EXIT` below
cleanup() {
  if (( KEEP_WORK == 0 )); then
    rm -rf "$WORK_ROOT"
  else
    echo "note: --keep-work; left work tree at $WORK_ROOT" >&2
  fi
}
trap cleanup EXIT

# Source-of-truth checkout: prefer an already-materialized `checkout/`
# (the workflow expects `materialize-corpus.sh` to have run first),
# else attempt to materialize. For `ref_kind = "vendored"` entries the
# materialize step is a recursive copy from `<id>/vendor/`; no network.
ensure_materialized() {
  local id="$1" dir="$2"
  if [[ -d "$dir/checkout" ]]; then
    return 0
  fi
  if ! "${REPO_ROOT}/scripts/materialize-corpus.sh" --filter "$id" >&2; then
    return 1
  fi
}

# Copy `src` to `dst` preserving file modes; used to give the mvn-side
# and barista-side builds their own scratch trees so neither
# observes the other's `target/` writes mid-build.
copy_tree() {
  local src="$1" dst="$2"
  mkdir -p "$dst"
  # Use cp -a where available (GNU); fall back to a BSD-compatible
  # form on macOS. Both behave identically on the inputs we hit.
  if cp -a "$src/." "$dst/" 2>/dev/null; then
    return 0
  fi
  cp -R "$src/." "$dst/"
}

# Counters for the summary footer.
N_PASS=0
N_FAIL=0
N_SKIP=0
RC=0

for line in "${WORKLIST[@]}"; do
  IFS=$'\t' read -r id kind build_cmd dir <<< "$line"

  # Materialize on demand. Failure to materialize is SKIP, not FAIL —
  # it's an environment issue (no network, upstream tag moved, etc.),
  # not a divergence. CI surfaces it but doesn't gate on it.
  if ! ensure_materialized "$id" "$dir"; then
    echo "[${id}] SKIP could not materialize"
    N_SKIP=$((N_SKIP + 1))
    continue
  fi

  src_checkout="$dir/checkout"
  proj_root="$WORK_ROOT/$id"
  mvn_dir="$proj_root/mvn"
  barista_dir="$proj_root/barista"
  mvn_repo="$proj_root/m2-mvn"
  barista_repo="$proj_root/m2-barista"

  if ! copy_tree "$src_checkout" "$mvn_dir"; then
    echo "[${id}] SKIP could not copy mvn-side tree"
    N_SKIP=$((N_SKIP + 1))
    continue
  fi
  if ! copy_tree "$src_checkout" "$barista_dir"; then
    echo "[${id}] SKIP could not copy barista-side tree"
    N_SKIP=$((N_SKIP + 1))
    continue
  fi
  mkdir -p "$mvn_repo" "$barista_repo"

  # Run the reference-mvn path. The corpus's `build_cmd` already starts
  # with `mvn`; append `-Dmaven.repo.local=<path>` so cold-cache hygiene
  # is preserved without rewriting the user's command.
  mvn_log="$proj_root/mvn.log"
  ( cd "$mvn_dir" && eval "$build_cmd -Dmaven.repo.local=$mvn_repo" ) \
    > "$mvn_log" 2>&1 || {
      echo "[${id}] FAIL reference mvn build failed (see $mvn_log)"
      N_FAIL=$((N_FAIL + 1))
      RC=3
      continue
    }

  # Run the barista path. The build_cmd is `mvn ... <phase>`; substitute
  # `mvn` with `barista` and append `--no-daemon` + the same local-repo
  # override. The phase ("verify", "package", "install") translates
  # 1:1 to barista's Maven-vocabulary lifecycle (M4.3 T2). We drop
  # the `-B` flag (barista output is non-interactive by default) but
  # preserve everything else.
  barista_args="${build_cmd#mvn }"
  barista_args="${barista_args/-B /}"
  barista_log="$proj_root/barista.log"
  ( cd "$barista_dir" \
      && eval "$BARISTA_INVOKE --no-daemon $barista_args -Dmaven.repo.local=$barista_repo" \
  ) > "$barista_log" 2>&1 || {
      echo "[${id}] FAIL barista --no-daemon build failed (see $barista_log)"
      N_FAIL=$((N_FAIL + 1))
      RC=3
      continue
    }

  # Compare target/ trees. For multi-module projects, compare each
  # module's target/ via a recursive find under the project root
  # (find every dir literally named `target` and walk it).
  mvn_targets=()
  while IFS= read -r -d '' t; do mvn_targets+=("$t"); done < \
    <(find "$mvn_dir" -type d -name target -print0)
  barista_targets=()
  while IFS= read -r -d '' t; do barista_targets+=("$t"); done < \
    <(find "$barista_dir" -type d -name target -print0)

  if [[ ${#mvn_targets[@]} -eq 0 || ${#barista_targets[@]} -eq 0 ]]; then
    echo "[${id}] FAIL no target/ produced (mvn=${#mvn_targets[@]}, barista=${#barista_targets[@]})"
    N_FAIL=$((N_FAIL + 1))
    RC=3
    continue
  fi

  project_rc=0
  for t in "${mvn_targets[@]}"; do
    rel="${t#"$mvn_dir/"}"
    counterpart="$barista_dir/$rel"
    if [[ ! -d "$counterpart" ]]; then
      echo "[${id}] FAIL barista side missing $rel"
      project_rc=1
      continue
    fi
    if ! compare_target_trees "$t" "$counterpart" "$id"; then
      project_rc=1
    fi
  done

  if (( project_rc == 0 )); then
    echo "[${id}] PASS"
    N_PASS=$((N_PASS + 1))
  else
    echo "[${id}] FAIL artifact divergence (see lines above)"
    N_FAIL=$((N_FAIL + 1))
  fi
done

N_TOTAL=$((N_PASS + N_FAIL + N_SKIP))
echo
echo "summary: ${N_TOTAL} project(s) checked, ${N_PASS} PASS, ${N_FAIL} FAIL, ${N_SKIP} SKIP"
exit "$RC"
