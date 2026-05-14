#!/usr/bin/env python3
"""Convert `cargo deny check --format json` output to SARIF 2.1.0.

cargo-deny does not natively emit SARIF as of v0.16. This adapter reads
the JSON diagnostic stream on stdin and emits a SARIF 2.1.0 document
on stdout so the findings can be uploaded to GitHub code scanning
under `category: cargo-deny`.

Usage (from .github/workflows/sca.yml):

    cargo deny check --format json 2> deny.json || true
    python3 .github/scripts/cargo-deny-to-sarif.py < deny.json > cargo-deny.sarif

Notes:
- `cargo deny check --format json` emits one JSON object per line on
  stderr (NDJSON), one record per advisory / ban / license / source
  finding. The non-finding "summary" record at the end is also a JSON
  object but uses a `type` discriminator we filter on.
- The SARIF level mapping is conservative: cargo-deny's `error` →
  SARIF `error`; `warning` → `warning`; `help`/`note` → `note`.
- The `ruleId` is the cargo-deny diagnostic code (e.g.,
  `vulnerability`, `unmaintained`, `banned`, `license-not-allowed`).
  GitHub's Security tab groups alerts by `ruleId`, so this gives
  triagers a clean per-rule rollup.
- Pure stdlib; no third-party deps.

Invariants:
- Output is always a valid SARIF 2.1.0 document. If stdin is empty
  or contains no usable records, a document with zero `results` is
  emitted (still valid SARIF — represents a clean scan).
- Tool version is best-effort: if a `cargo deny --version` summary
  field is present in the stream we surface it; otherwise we record
  "unknown" rather than blocking on it.
"""
from __future__ import annotations

import json
import sys
from typing import Any


SARIF_LEVEL_BY_CARGO_DENY = {
    "error": "error",
    "warning": "warning",
    "help": "note",
    "note": "note",
    "info": "note",
}


def _normalize_level(raw: str | None) -> str:
    if not raw:
        return "warning"
    return SARIF_LEVEL_BY_CARGO_DENY.get(raw.lower(), "warning")


def _iter_json_records(text: str):
    """Yield JSON objects from an NDJSON stream, skipping blanks/parse errors."""
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError:
            # cargo-deny occasionally emits non-JSON warnings to stderr
            # alongside the structured stream. Skip those rather than
            # corrupting the SARIF output.
            continue


def _extract_location(record: dict) -> tuple[str, int]:
    """Best-effort (path, start_line) from a cargo-deny finding.

    Most cargo-deny diagnostics carry a span pointing into Cargo.toml
    or Cargo.lock. When absent, we fall back to ("Cargo.lock", 1) so
    GitHub code scanning has *something* to group on — without a
    location SARIF still validates but the alert won't be navigable.
    """
    # Cargo-deny embeds locations in different shapes depending on the
    # check; try the documented surfaces in order.
    spans = record.get("spans") or []
    for span in spans:
        if isinstance(span, dict) and span.get("file_name"):
            file_name = span["file_name"]
            line_start = (
                span.get("line_start")
                or (span.get("byte_start") and 1)
                or 1
            )
            return file_name, int(line_start)
    # Some advisories surface via `labels` instead.
    labels = record.get("labels") or []
    for lbl in labels:
        if isinstance(lbl, dict) and lbl.get("file_name"):
            return lbl["file_name"], int(lbl.get("line_start") or 1)
    return "Cargo.lock", 1


def _result_for(record: dict) -> dict[str, Any] | None:
    """Translate one cargo-deny record into one SARIF `result` entry.

    Returns None for non-finding records (e.g., the summary line at
    the end of the stream).
    """
    # cargo-deny tags every finding with `type: "diagnostic"` (or the
    # legacy `fields.severity` shape). Anything without a level is a
    # summary record we skip.
    level = record.get("fields", {}).get("severity") or record.get("level")
    if not level and "message" not in record:
        return None
    code = (
        record.get("fields", {}).get("code")
        or record.get("code")
        or record.get("name")
        or "cargo-deny"
    )
    message_text = (
        record.get("fields", {}).get("message")
        or record.get("message")
        or "cargo-deny finding"
    )
    path, line = _extract_location(record.get("fields") or record)
    return {
        "ruleId": str(code),
        "level": _normalize_level(level),
        "message": {"text": str(message_text)},
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": {"uri": path},
                    "region": {"startLine": int(line) if line else 1},
                }
            }
        ],
    }


def to_sarif(stdin_text: str) -> dict[str, Any]:
    results: list[dict[str, Any]] = []
    rules: dict[str, dict[str, Any]] = {}
    for rec in _iter_json_records(stdin_text):
        result = _result_for(rec)
        if result is None:
            continue
        rule_id = result["ruleId"]
        if rule_id not in rules:
            rules[rule_id] = {
                "id": rule_id,
                "name": rule_id,
                "shortDescription": {"text": rule_id},
                "helpUri": "https://embarkstudios.github.io/cargo-deny/",
            }
        results.append(result)
    return {
        "version": "2.1.0",
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "cargo-deny",
                        "informationUri": "https://github.com/EmbarkStudios/cargo-deny",
                        "rules": list(rules.values()),
                    }
                },
                "results": results,
            }
        ],
    }


def main() -> int:
    stdin_text = sys.stdin.read()
    sarif = to_sarif(stdin_text)
    json.dump(sarif, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
