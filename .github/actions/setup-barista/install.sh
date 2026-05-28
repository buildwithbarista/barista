#!/usr/bin/env bash
# Download, verify, and install the Barista CLI. This is the core of the
# setup-barista composite action, factored out so it can be tested standalone
# (run it directly with INPUT_VERSION / INPUT_REPOSITORY set).
#
# Behaviour:
#   - Resolves the platform target from RUNNER_OS/RUNNER_ARCH (falling back to
#     uname for local runs). Linux and macOS only for now.
#   - Resolves "latest" to the most recent release (prereleases included, since
#     v0.1 ships as alpha/preview).
#   - Downloads the release's build-manifest.json and the platform archive,
#     then verifies the archive's sha256 against the manifest before extracting.
#   - Extracts with --strip-components=1 so bin/ and share/ land at the install
#     root; the CLI resolves its bundled Maven/barback tree relative to its own
#     executable path, so the tree must stay intact.
#   - When run inside GitHub Actions, prepends <dir>/bin to $GITHUB_PATH and
#     writes the `version` and `install-dir` outputs to $GITHUB_OUTPUT.
#
# Env:
#   INPUT_VERSION      release version without leading v, or "latest" (default)
#   INPUT_REPOSITORY   owner/repo (default buildwithbarista/barista)
#   INPUT_INSTALL_DIR  install location (default $RUNNER_TEMP/barista or a mktemp)
#   GH_TOKEN           optional token for the releases API (avoids rate limits)
set -euo pipefail

repo="${INPUT_REPOSITORY:-buildwithbarista/barista}"
version="${INPUT_VERSION:-latest}"

log() { printf '%s\n' "setup-barista: $*" >&2; }
fail() {
  # ::error:: is rendered as an annotation in GitHub Actions; harmless locally.
  printf '::error::setup-barista: %s\n' "$*" >&2
  exit 1
}

# --- Resolve the platform target -------------------------------------------
os="${RUNNER_OS:-$(uname -s)}"
arch="${RUNNER_ARCH:-$(uname -m)}"
case "$os" in
  Linux | linux) plat="unknown-linux-gnu" ;;
  macOS | Darwin | darwin) plat="apple-darwin" ;;
  *) fail "unsupported OS '$os' (setup-barista supports Linux and macOS for now)" ;;
esac
case "$arch" in
  X64 | x86_64 | amd64) cpu="x86_64" ;;
  ARM64 | arm64 | aarch64) cpu="aarch64" ;;
  *) fail "unsupported CPU architecture '$arch'" ;;
esac
target="${cpu}-${plat}"
log "target = ${target}"

# --- curl with optional auth -----------------------------------------------
auth=()
if [ -n "${GH_TOKEN:-}" ]; then
  auth=(-H "Authorization: Bearer ${GH_TOKEN}")
fi

# --- Resolve "latest" (prereleases included) -------------------------------
if [ "$version" = "latest" ]; then
  log "resolving latest release of ${repo}"
  tag="$(
    curl -fsSL "${auth[@]}" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/${repo}/releases?per_page=1" |
      python3 -c 'import json,sys; r=json.load(sys.stdin); print(r[0]["tag_name"] if r else "")'
  )"
  [ -n "$tag" ] || fail "no releases found for ${repo}"
  version="${tag#v}"
fi
log "version = ${version}"

base="https://github.com/${repo}/releases/download/v${version}"
manifest="barista-${version}-build-manifest.json"
archive="barista-${version}-${target}.tar.gz"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- Download manifest + archive -------------------------------------------
log "downloading ${manifest}"
curl -fsSL "${auth[@]}" "${base}/${manifest}" -o "${work}/manifest.json" ||
  fail "could not download ${manifest} (is v${version} a published release?)"
log "downloading ${archive}"
curl -fsSL "${auth[@]}" "${base}/${archive}" -o "${work}/${archive}" ||
  fail "could not download ${archive} for target ${target}"

# --- Verify sha256 against the manifest ------------------------------------
expected="$(
  python3 - "${work}/manifest.json" "$target" <<'PY'
import json, sys
manifest, target = sys.argv[1], sys.argv[2]
with open(manifest) as f:
    data = json.load(f)
for a in data.get("artifacts", []):
    if a.get("target") == target:
        print(a.get("archive_sha256", ""))
        break
PY
)"
[ -n "$expected" ] || fail "manifest has no entry for target ${target}"

if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${work}/${archive}" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "${work}/${archive}" | cut -d' ' -f1)"
fi

if [ "$expected" != "$actual" ]; then
  fail "sha256 mismatch for ${archive}: expected ${expected}, got ${actual}"
fi
log "sha256 verified (${actual})"

# --- Extract ---------------------------------------------------------------
install_dir="${INPUT_INSTALL_DIR:-${RUNNER_TEMP:-$(mktemp -d)}/barista}"
mkdir -p "${install_dir}"
# --strip-components=1 drops the leading barista-<v>-<target>/ component.
tar -xzf "${work}/${archive}" -C "${install_dir}" --strip-components=1
[ -x "${install_dir}/bin/barista" ] || fail "extracted tree has no bin/barista"
log "installed to ${install_dir}"

# --- Wire into the GitHub Actions environment ------------------------------
if [ -n "${GITHUB_PATH:-}" ]; then
  printf '%s\n' "${install_dir}/bin" >>"$GITHUB_PATH"
fi
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    printf 'version=%s\n' "$version"
    printf 'install-dir=%s\n' "$install_dir"
  } >>"$GITHUB_OUTPUT"
fi

# Always print the resolved location so standalone runs are useful too.
printf '%s\n' "${install_dir}/bin/barista"
