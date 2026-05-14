#!/usr/bin/env bash
#
# test-secret-scan.sh
#
# Round-trip test for the repo's gitleaks configuration. Asserts:
#
#   Test A:  Running gitleaks against tests/fixtures/secrets/synthetic_aws_key.txt
#            with an empty .gitleaksignore fires at least one finding and
#            exits non-zero.
#
#   Test B:  Re-running gitleaks against the same fixture with the
#            fingerprint of the Test-A finding listed in .gitleaksignore
#            produces zero findings and exits zero.
#
# Together these prove (a) the scanner is configured and wired up,
# (b) the configured rule pack actually fires on a known-bad shape, and
# (c) the .gitleaksignore mechanism is honoured.
#
# Exit code:
#   0  — both tests passed
#   non-zero — at least one test failed; stderr explains which
#
# Usage:
#   bash scripts/test-secret-scan.sh
#
# Requirements:
#   - gitleaks on PATH (see CONTRIBUTING.md for install notes)
#   - python3 on PATH (used to parse the JSON report)
#
# This script does not mutate any tracked file. The repo's real
# `.gitleaksignore` is left untouched; the script writes a temporary
# ignore-file in a scratch directory and points gitleaks at it via the
# upstream-supported flag.

set -euo pipefail

# Resolve repo root from this script's location so the script works from
# any working directory.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

FIXTURE="tests/fixtures/secrets/synthetic_aws_key.txt"
CONFIG=".gitleaks.toml"

if [[ ! -f "${FIXTURE}" ]]; then
    echo "FAIL: fixture not found at ${FIXTURE}" >&2
    exit 2
fi
if [[ ! -f "${CONFIG}" ]]; then
    echo "FAIL: gitleaks config not found at ${CONFIG}" >&2
    exit 2
fi
if ! command -v gitleaks >/dev/null 2>&1; then
    echo "FAIL: gitleaks is not on PATH. Install it (see CONTRIBUTING.md)." >&2
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 is not on PATH (needed to parse the JSON report)." >&2
    exit 2
fi

WORK="$(mktemp -d -t barista-secret-scan-XXXXXX)"
trap 'rm -rf "${WORK}"' EXIT

# Empty ignore-file used by Test A. The flag --gitleaks-ignore-path is the
# upstream-supported way to point gitleaks at a non-default ignore file
# without mutating the tracked .gitleaksignore.
EMPTY_IGNORE="${WORK}/empty.gitleaksignore"
: > "${EMPTY_IGNORE}"

PRIMED_IGNORE="${WORK}/primed.gitleaksignore"
REPORT_A="${WORK}/report-a.json"
REPORT_B="${WORK}/report-b.json"

# ----------------------------------------------------------------------------
# Test A: scanner fires on the synthetic key when nothing is allowlisted.
# ----------------------------------------------------------------------------
echo "[test A] running gitleaks against ${FIXTURE} with empty ignore file..."
set +e
gitleaks detect \
    --no-git \
    --source="${FIXTURE}" \
    --config="${CONFIG}" \
    --gitleaks-ignore-path="${EMPTY_IGNORE}" \
    --report-format=json \
    --report-path="${REPORT_A}" \
    --redact \
    >/dev/null 2>&1
EXIT_A=$?
set -e

if [[ "${EXIT_A}" -eq 0 ]]; then
    echo "FAIL [test A]: gitleaks exited 0 — expected non-zero (a finding was expected)." >&2
    echo "  Report:" >&2
    cat "${REPORT_A}" >&2 || true
    exit 1
fi

FINDINGS_A=$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
print(len(data))
' "${REPORT_A}")

if [[ "${FINDINGS_A}" -lt 1 ]]; then
    echo "FAIL [test A]: report contained no findings — expected at least one." >&2
    cat "${REPORT_A}" >&2 || true
    exit 1
fi

# Capture the first finding's fingerprint for use in Test B.
FINGERPRINT=$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
print(data[0]["Fingerprint"])
' "${REPORT_A}")

echo "[test A] ok — gitleaks reported ${FINDINGS_A} finding(s), exit=${EXIT_A}, fingerprint=${FINGERPRINT}"

# ----------------------------------------------------------------------------
# Test B: same scan with the fingerprint allowlisted produces zero findings.
# ----------------------------------------------------------------------------
printf '%s\n' "${FINGERPRINT}" > "${PRIMED_IGNORE}"

echo "[test B] re-running gitleaks with fingerprint allowlisted..."
set +e
gitleaks detect \
    --no-git \
    --source="${FIXTURE}" \
    --config="${CONFIG}" \
    --gitleaks-ignore-path="${PRIMED_IGNORE}" \
    --report-format=json \
    --report-path="${REPORT_B}" \
    --redact \
    >/dev/null 2>&1
EXIT_B=$?
set -e

if [[ "${EXIT_B}" -ne 0 ]]; then
    echo "FAIL [test B]: gitleaks exited ${EXIT_B} — expected 0 with fingerprint allowlisted." >&2
    cat "${REPORT_B}" >&2 || true
    exit 1
fi

FINDINGS_B=$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
print(len(data))
' "${REPORT_B}")

if [[ "${FINDINGS_B}" -ne 0 ]]; then
    echo "FAIL [test B]: report contained ${FINDINGS_B} finding(s) — expected 0." >&2
    cat "${REPORT_B}" >&2 || true
    exit 1
fi

echo "[test B] ok — gitleaks reported 0 finding(s), exit=${EXIT_B}"
echo "PASS — secret-scan round-trip works (config fires; .gitleaksignore respected)."
