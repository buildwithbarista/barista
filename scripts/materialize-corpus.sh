#!/usr/bin/env bash
#
# Materialize the test corpus.
#
# For each test-corpus/<id>/corpus.lock.toml, clone the upstream project
# at its pinned ref into test-corpus/<id>/checkout/. Idempotent: skips
# projects whose checkout/ already exists unless --update is given.
#
# Usage:
#   scripts/materialize-corpus.sh [--jobs N] [--filter PATTERN] [--update] [--help]
#
# Flags:
#   --jobs N        Number of parallel clones (default: 4, or $JOBS).
#   --filter PAT    Only materialize projects whose id matches PAT (shell glob).
#   --update        Re-fetch already-materialized projects.
#   --help, -h      Show this message.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORPUS_DIR="$REPO_ROOT/test-corpus"

JOBS="${JOBS:-4}"
FILTER="*"
UPDATE=0

print_help() {
  # Print the leading comment block (lines starting with '#'), stripped of
  # the leading '# '. Stops at the first non-comment, non-blank line.
  awk '
    NR == 1 { next }                              # skip shebang
    /^#/    { sub(/^# ?/, ""); print; next }
    /^$/    { print ""; next }
    { exit }
  ' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --jobs)    JOBS="$2"; shift 2 ;;
    --jobs=*)  JOBS="${1#*=}"; shift ;;
    --filter)  FILTER="$2"; shift 2 ;;
    --filter=*) FILTER="${1#*=}"; shift ;;
    --update)  UPDATE=1; shift ;;
    -h|--help) print_help; exit 0 ;;
    *)
      echo "error: unknown argument: $1" >&2
      echo "try: $0 --help" >&2
      exit 2
      ;;
  esac
done

if [[ ! -d "$CORPUS_DIR" ]]; then
  echo "error: $CORPUS_DIR does not exist" >&2
  exit 1
fi

# Minimal TOML reader: grabs `key = "value"` lines for a fixed key set.
# Tolerant of unknown keys and of `notes = """..."""` blocks (which it
# simply skips because we don't ask for them).
read_lock_value() {
  local file="$1" key="$2"
  awk -v k="$key" '
    BEGIN { FS="="; in_multi=0 }
    {
      # skip multi-line string values
      if (in_multi) { if ($0 ~ /"""[[:space:]]*$/) in_multi=0; next }
      if ($0 ~ /=[[:space:]]*"""/ && $0 !~ /""".*"""/) { in_multi=1; next }
      if ($0 ~ /^[[:space:]]*#/) next
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", $1)
      if ($1 != k) next
      # rejoin the value side in case it contained "="
      $1=""; v=$0; sub(/^=/, "", v)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", v)
      # strip surrounding quotes
      if (v ~ /^".*"$/) { v=substr(v, 2, length(v)-2) }
      print v
      exit
    }
  ' "$file"
}

# Build the worklist: tab-separated id\turl\tref\tref_kind\tdir
WORKLIST=()
while IFS= read -r -d '' lockfile; do
  dir="$(dirname "$lockfile")"
  id="$(read_lock_value "$lockfile" id)"
  url="$(read_lock_value "$lockfile" git_url)"
  ref="$(read_lock_value "$lockfile" ref)"
  kind="$(read_lock_value "$lockfile" ref_kind)"

  if [[ -z "$id" || -z "$url" || -z "$ref" || -z "$kind" ]]; then
    echo "[skip] $lockfile: missing required key (id/git_url/ref/ref_kind)" >&2
    continue
  fi

  # shellcheck disable=SC2053
  if [[ "$id" != $FILTER ]]; then
    continue
  fi

  WORKLIST+=("$id"$'\t'"$url"$'\t'"$ref"$'\t'"$kind"$'\t'"$dir")
done < <(find "$CORPUS_DIR" -mindepth 2 -maxdepth 2 -name corpus.lock.toml -print0)

if [[ ${#WORKLIST[@]} -eq 0 ]]; then
  echo "no projects matched filter: $FILTER" >&2
  exit 0
fi

materialize_one() {
  local line="$1"
  IFS=$'\t' read -r id url ref kind dir <<< "$line"
  local checkout="$dir/checkout"

  if [[ -d "$checkout/.git" ]]; then
    if [[ "$UPDATE" -eq 0 ]]; then
      echo "[$id] up-to-date (checkout/ already exists)"
      return 0
    fi
    echo "[$id] updating existing checkout"
    git -C "$checkout" fetch --depth 1 origin "$ref" >/dev/null 2>&1 || true
    git -C "$checkout" checkout -q "FETCH_HEAD" 2>/dev/null \
      || git -C "$checkout" checkout -q "$ref"
    echo "[$id] updated -> $kind $ref"
    return 0
  fi

  rm -rf "$checkout"
  case "$kind" in
    tag|branch)
      git clone --depth 1 --branch "$ref" "$url" "$checkout" \
        >/dev/null 2>&1
      ;;
    commit)
      git clone --filter=blob:none --no-checkout "$url" "$checkout" \
        >/dev/null 2>&1
      git -C "$checkout" checkout -q "$ref"
      ;;
    *)
      echo "[$id] error: unknown ref_kind: $kind" >&2
      return 1
      ;;
  esac
  echo "[$id] materialized -> $kind $ref"
}

# Run with a simple bounded-parallel loop. Each entry is tab-separated;
# preserving tabs through xargs is awkward, so we drive parallelism here.
RC=0
PIDS=()
for line in "${WORKLIST[@]}"; do
  ( materialize_one "$line" ) &
  PIDS+=($!)
  # cap concurrency at $JOBS
  if (( ${#PIDS[@]} >= JOBS )); then
    wait "${PIDS[0]}" || RC=$?
    PIDS=("${PIDS[@]:1}")
  fi
done
for pid in "${PIDS[@]}"; do
  wait "$pid" || RC=$?
done

echo
echo "summary: ${#WORKLIST[@]} project(s) processed (filter: $FILTER, jobs: $JOBS)"
exit "$RC"
