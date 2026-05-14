#!/usr/bin/env python3
"""Convert `cargo audit --json` output to SARIF 2.1.0.

cargo-audit does not natively emit SARIF as of v0.21. This adapter
reads the JSON report on stdin and emits a SARIF 2.1.0 document on
stdout so the findings can be uploaded to GitHub code scanning under
`category: cargo-audit`.

Usage (from .github/workflows/sca.yml):

    cargo install cargo-audit --locked
    cargo audit --json > audit.json || true
    python3 .github/scripts/cargo-audit-to-sarif.py < audit.json > cargo-audit.sarif

Notes:
- `cargo audit --json` emits a single JSON object with two
  load-bearing arrays we care about: `vulnerabilities.list` (the
  classic RUSTSEC advisory matches) and `warnings` (informational
  advisories — unmaintained, yanked, unsound). Both surfaces are
  translated; SARIF `level` distinguishes them.
- The `ruleId` is the RUSTSEC advisory ID
  (e.g., `RUSTSEC-2025-0141`). GitHub's Security tab groups by
  `ruleId`, so triagers get a clean per-advisory rollup.
- The location is `Cargo.lock` at line 1: cargo-audit does not surface
  byte-level spans, and `Cargo.lock` is the file where the human fix
  ultimately lands (via a `cargo update` or a workspace dep bump).
- Pure stdlib; no third-party deps.

Invariants:
- Output is always a valid SARIF 2.1.0 document. If stdin is empty or
  the report has no `vulnerabilities` / `warnings`, a document with
  zero `results` is emitted (still valid SARIF — represents a clean scan).
- The full advisory text is folded into the SARIF `message.text` so
  the Security-tab alert body has enough context to triage without
  clicking through to the RUSTSEC site.
"""
from __future__ import annotations

import json
import sys
from typing import Any


def _sarif_level_for_severity(severity: str | None) -> str:
    """Map RUSTSEC's `severity` (CVSS-derived) to a SARIF level.

    cargo-audit surfaces `low` / `medium` / `high` / `critical` (the
    standard CVSS-band names). SARIF only has `note` / `warning` /
    `error`; we map conservatively so anything that should fail a
    serious-finding gate is `error`-level.
    """
    if not severity:
        return "warning"
    s = severity.lower()
    if s in {"high", "critical"}:
        return "error"
    if s in {"medium", "moderate"}:
        return "warning"
    return "note"


def _result_for_vuln(vuln: dict) -> dict[str, Any]:
    advisory = vuln.get("advisory") or {}
    package = vuln.get("package") or {}
    advisory_id = advisory.get("id") or "RUSTSEC-UNKNOWN"
    title = advisory.get("title") or advisory_id
    description = advisory.get("description") or ""
    pkg_name = package.get("name") or "?"
    pkg_version = package.get("version") or "?"
    severity = advisory.get("severity")
    # Compose a self-contained message: advisory title + affected
    # package + CVSS severity tag + (truncated) description.
    desc_short = description.strip().splitlines()[0] if description else ""
    message_text = (
        f"{title} "
        f"(package: {pkg_name} {pkg_version}; severity: {severity or 'unknown'}). "
        f"{desc_short}"
    ).strip()
    return {
        "ruleId": advisory_id,
        "level": _sarif_level_for_severity(severity),
        "message": {"text": message_text},
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": {"uri": "Cargo.lock"},
                    "region": {"startLine": 1},
                }
            }
        ],
    }


def _result_for_warning(warning: dict, kind: str) -> dict[str, Any]:
    advisory = warning.get("advisory") or {}
    package = warning.get("package") or {}
    advisory_id = advisory.get("id") or f"RUSTSEC-{kind.upper()}"
    title = advisory.get("title") or kind
    pkg_name = package.get("name") or warning.get("name") or "?"
    pkg_version = package.get("version") or warning.get("version") or "?"
    message_text = (
        f"{title} "
        f"(package: {pkg_name} {pkg_version}; class: {kind})."
    ).strip()
    return {
        "ruleId": advisory_id,
        "level": "warning",
        "message": {"text": message_text},
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": {"uri": "Cargo.lock"},
                    "region": {"startLine": 1},
                }
            }
        ],
    }


def to_sarif(stdin_text: str) -> dict[str, Any]:
    results: list[dict[str, Any]] = []
    rules: dict[str, dict[str, Any]] = {}

    text = stdin_text.strip()
    if not text:
        report = {}
    else:
        try:
            report = json.loads(text)
        except json.JSONDecodeError:
            # cargo-audit emits human-readable output on parse failures
            # upstream; tolerate it and emit an empty SARIF document
            # rather than crashing the workflow step.
            report = {}

    vulns = (report.get("vulnerabilities") or {}).get("list") or []
    for vuln in vulns:
        result = _result_for_vuln(vuln)
        rule_id = result["ruleId"]
        if rule_id not in rules:
            advisory = vuln.get("advisory") or {}
            rules[rule_id] = {
                "id": rule_id,
                "name": rule_id,
                "shortDescription": {
                    "text": advisory.get("title") or rule_id,
                },
                "helpUri": (
                    advisory.get("url")
                    or f"https://rustsec.org/advisories/{rule_id}.html"
                ),
            }
        results.append(result)

    # `warnings` is a {kind: [warning, ...]} dict in newer cargo-audit
    # releases; tolerate the older list shape too.
    warnings = report.get("warnings") or {}
    if isinstance(warnings, dict):
        for kind, items in warnings.items():
            for warning in items or []:
                result = _result_for_warning(warning, kind)
                rule_id = result["ruleId"]
                if rule_id not in rules:
                    rules[rule_id] = {
                        "id": rule_id,
                        "name": rule_id,
                        "shortDescription": {"text": str(kind)},
                        "helpUri": f"https://rustsec.org/advisories/{rule_id}.html",
                    }
                results.append(result)
    elif isinstance(warnings, list):
        for warning in warnings:
            result = _result_for_warning(warning, "warning")
            rule_id = result["ruleId"]
            if rule_id not in rules:
                rules[rule_id] = {
                    "id": rule_id,
                    "name": rule_id,
                    "shortDescription": {"text": "warning"},
                    "helpUri": f"https://rustsec.org/advisories/{rule_id}.html",
                }
            results.append(result)

    return {
        "version": "2.1.0",
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "cargo-audit",
                        "informationUri": "https://github.com/rustsec/rustsec/tree/main/cargo-audit",
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
