#!/usr/bin/env bash
# -----------------------------------------------------------------------------
# Syntax-only smoke check for the worker protocol schema.
#
# Runs `protoc` against `proto/barista/v1/worker.proto` and discards the
# descriptor output. A clean exit code (0) confirms the schema parses
# cleanly and is suitable for downstream binding generation.
#
# This is the lightest possible verification — it does NOT confirm that
# the Rust prost / Java protoc-gen-java bindings produce usable types
# (those checks live in the respective binding-generation crates).
#
# Usage:
#   ./tests/verify-schema.sh
#
# Prerequisites:
#   protoc (any libprotoc >= 3.21 will do; verified locally with 34.1).
#   Install on macOS via `brew install protobuf`.
# -----------------------------------------------------------------------------
set -euo pipefail

# Resolve the proto root regardless of where the script is invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# tests/ → v1/ → barista/ → proto/
PROTO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

if ! command -v protoc >/dev/null 2>&1; then
  echo "error: protoc not found on PATH" >&2
  echo "       install via 'brew install protobuf' (macOS) or your distro's package manager" >&2
  exit 127
fi

echo "protoc: $(protoc --version)"
echo "proto root: ${PROTO_ROOT}"
echo "schema: proto/barista/v1/worker.proto"

protoc \
  --descriptor_set_out=/dev/null \
  --proto_path="${PROTO_ROOT}" \
  "${PROTO_ROOT}/barista/v1/worker.proto"

echo "ok: schema parses cleanly"
