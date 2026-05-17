#!/usr/bin/env bash
# scripts/compare-perf-results.sh
#
# Tier-2 perf-gate comparison logic per PRD §17.10.
#
# Reads two directories of `barista.bench.results/v1` JSON documents
# (the baseline tree and the current/PR tree) and the
# accepted-regressions markdown file. For each pair of documents
# matching on `manifest_id`, compares `summary.median_wall_ms` and
# classifies the change against the per-dimension threshold table.
#
# Outcomes:
#   pass        — no regressions over threshold
#   pass+wins   — at least one improvement >5%; no regressions
#   warn        — regressions <=threshold (or exempted via accepted-
#                  regressions.md); does not fail the gate
#   fail        — at least one un-exempted regression >threshold
#
# Exit codes: 0 on pass / pass+wins / warn; 1 on fail.
#
# Inputs (all flags required except --summary and --report):
#   --baseline <dir>    Directory of baseline results.json blobs.
#   --current  <dir>    Directory of PR results.json blobs.
#   --accepted <path>   docs/perf/accepted-regressions.md (markdown
#                       with a `## Entries` table — see the file).
#   --summary  <path>   Optional. Path to $GITHUB_STEP_SUMMARY (or any
#                       file) — receives a human-readable markdown
#                       summary of the comparison.
#   --report   <path>   Optional. Path receives the same markdown so
#                       downstream steps can attach it to a PR comment.
#
# Format reference:
#   - Each results.json validates against
#     `crates/barista-bench/schema/results.schema.json`.
#   - The dimension this run belongs to is read from
#     `metadata.dimension` (one of D1..D7). Documents without a
#     dimension fall back to the conservative D1 cap (+10%).
#   - The corpus project is read from `metadata.project`.
#
# Matching rule: documents pair on `manifest_id`. Documents present in
# only one side are surfaced in the report but do not gate (the gate
# is "no regressions on tracked manifests", not "manifest set is
# identical"). Onboarding/offboarding manifests is handled by A.2 T3.

set -euo pipefail

BASELINE_DIR=""
CURRENT_DIR=""
ACCEPTED_PATH=""
SUMMARY_PATH=""
REPORT_PATH=""

while [ $# -gt 0 ]; do
  case "$1" in
    --baseline) BASELINE_DIR="$2"; shift 2 ;;
    --current)  CURRENT_DIR="$2"; shift 2 ;;
    --accepted) ACCEPTED_PATH="$2"; shift 2 ;;
    --summary)  SUMMARY_PATH="$2"; shift 2 ;;
    --report)   REPORT_PATH="$2"; shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$BASELINE_DIR" ] || [ -z "$CURRENT_DIR" ] || [ -z "$ACCEPTED_PATH" ]; then
  echo "usage: $0 --baseline <dir> --current <dir> --accepted <md> [--summary <path>] [--report <path>]" >&2
  exit 2
fi

if [ ! -d "$BASELINE_DIR" ]; then
  echo "error: baseline dir not found: $BASELINE_DIR" >&2; exit 2
fi
if [ ! -d "$CURRENT_DIR" ]; then
  echo "error: current dir not found: $CURRENT_DIR" >&2; exit 2
fi
# Accepted-regressions file is allowed to be absent; the gate
# behaves as if it had zero entries in that case.

# Verify python3 is on PATH — the JSON + markdown parsing rides on
# the stdlib (no third-party deps required).
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 not on PATH" >&2; exit 2
fi

export BASELINE_DIR CURRENT_DIR ACCEPTED_PATH SUMMARY_PATH REPORT_PATH

python3 - <<'PYEOF'
"""Tier-2 perf-gate comparison.

All numerics + markdown parsing happen in this single Python block.
No third-party dependencies; stdlib only.
"""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# PRD §17.10 threshold table. Allowed regression (%) per dimension.
# Improvements >5% are flagged as wins; |delta| <=1% is reported as
# neutral. Regressions over the per-dimension cap fail unless an
# accepted-regressions.md entry exempts the (project, dimension) pair.
# ---------------------------------------------------------------------------
THRESHOLDS_BY_DIMENSION = {
    "D1": 10.0,  # Cold-cache resolve
    "D2": 15.0,  # Warm-cache resolve
    "D3": 10.0,  # Lock-verified resolve
    "D4": 15.0,  # Cold build
    "D5": 10.0,  # Warm shot test no-change
    "D6": 20.0,  # Warm shot test 1-file
    "D7": 15.0,  # Reactor parallel
}
DEFAULT_THRESHOLD = 10.0  # Conservative fallback for missing/unknown dimension
IMPROVEMENT_HEADLINE = 5.0  # Improvements above this surface as wins
NEUTRAL_BAND = 1.0  # |delta%| under this is reported as neutral

baseline_dir = Path(os.environ["BASELINE_DIR"])
current_dir = Path(os.environ["CURRENT_DIR"])
accepted_path = Path(os.environ["ACCEPTED_PATH"]) if os.environ["ACCEPTED_PATH"] else None
summary_path = os.environ.get("SUMMARY_PATH") or ""
report_path = os.environ.get("REPORT_PATH") or ""


def load_results(d: Path) -> dict[str, dict]:
    """Load every *.json under `d` and index by manifest_id."""
    out: dict[str, dict] = {}
    for p in sorted(d.glob("*.json")):
        try:
            doc = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as e:
            print(f"::warning::failed to parse {p}: {e}", file=sys.stderr)
            continue
        mid = doc.get("manifest_id")
        if not mid:
            print(f"::warning::{p} missing manifest_id; skipping", file=sys.stderr)
            continue
        out[mid] = doc
    return out


def parse_accepted(md_path: Path | None) -> set[tuple[str, str]]:
    """Parse accepted-regressions.md.

    Recognized format: a single markdown table with at minimum the
    columns `Project | Dimension | ...`. Any row whose Project and
    Dimension cells are non-empty (and Dimension matches D1..D7)
    becomes an exemption. Comment lines (`<!-- ... -->`) and rows
    whose Project cell starts with `(` are ignored — that lets the
    placeholder row in a freshly-created file say `(no entries yet —
    first will land via A.2 T2)`.

    Returns a set of (project, dimension) tuples that are exempted.
    """
    if md_path is None or not md_path.exists():
        return set()
    text = md_path.read_text(encoding="utf-8")
    exempt: set[tuple[str, str]] = set()
    in_table = False
    header_cols: list[str] = []
    project_col = -1
    dim_col = -1
    for line in text.splitlines():
        s = line.strip()
        if not s.startswith("|"):
            in_table = False
            header_cols = []
            project_col = -1
            dim_col = -1
            continue
        # Strip outer pipes; split on remaining pipes.
        cells = [c.strip() for c in s.strip("|").split("|")]
        if not in_table:
            # Header row.
            header_cols = [c.lower() for c in cells]
            try:
                project_col = header_cols.index("project")
                dim_col = header_cols.index("dimension")
            except ValueError:
                in_table = False
                continue
            in_table = True
            continue
        # Separator row (all dashes/colons).
        if all(set(c) <= set("-:") and c for c in cells):
            continue
        if project_col >= len(cells) or dim_col >= len(cells):
            continue
        project = cells[project_col]
        dim = cells[dim_col].upper()
        if not project or project.startswith("("):
            continue
        if not re.fullmatch(r"D[1-7]", dim):
            continue
        exempt.add((project, dim))
    return exempt


def classify(delta_pct: float, threshold: float) -> str:
    """Map a percent delta to one of: win / neutral / warn / fail.

    Convention: positive `delta_pct` means the new run is SLOWER (a
    regression). Negative means faster (an improvement).
    """
    if delta_pct <= -IMPROVEMENT_HEADLINE:
        return "win"
    if abs(delta_pct) <= NEUTRAL_BAND:
        return "neutral"
    if delta_pct > threshold:
        return "fail"
    if delta_pct > NEUTRAL_BAND:
        return "warn"
    # -IMPROVEMENT_HEADLINE < delta_pct < -NEUTRAL_BAND is a small
    # improvement — pass quietly.
    return "win-small"


def fmt_delta(delta_pct: float) -> str:
    sign = "+" if delta_pct >= 0 else ""
    return f"{sign}{delta_pct:.2f}%"


baseline = load_results(baseline_dir)
current = load_results(current_dir)
exemptions = parse_accepted(accepted_path)

rows: list[dict] = []
all_manifests = sorted(set(baseline) | set(current))
for mid in all_manifests:
    b = baseline.get(mid)
    c = current.get(mid)
    if b is None or c is None:
        rows.append({
            "manifest_id": mid,
            "status": "missing-baseline" if b is None else "missing-current",
            "project": (c or b).get("metadata", {}).get("project", "?"),
            "dimension": (c or b).get("metadata", {}).get("dimension", "?"),
            "baseline_ms": None if b is None else b["summary"]["median_wall_ms"],
            "current_ms": None if c is None else c["summary"]["median_wall_ms"],
            "delta_pct": None,
            "threshold": None,
            "exempted": False,
        })
        continue
    bms = float(b["summary"]["median_wall_ms"])
    cms = float(c["summary"]["median_wall_ms"])
    if bms <= 0:
        # Cannot compute a percentage against a zero baseline. Flag
        # but don't gate.
        rows.append({
            "manifest_id": mid,
            "status": "skipped-zero-baseline",
            "project": c.get("metadata", {}).get("project", "?"),
            "dimension": c.get("metadata", {}).get("dimension", "?"),
            "baseline_ms": bms,
            "current_ms": cms,
            "delta_pct": None,
            "threshold": None,
            "exempted": False,
        })
        continue
    delta_pct = (cms - bms) / bms * 100.0
    project = c.get("metadata", {}).get("project", "?")
    dimension = c.get("metadata", {}).get("dimension", "?")
    threshold = THRESHOLDS_BY_DIMENSION.get(dimension, DEFAULT_THRESHOLD)
    cls = classify(delta_pct, threshold)
    exempted = (project, dimension) in exemptions
    if cls == "fail" and exempted:
        cls = "warn"  # exemption demotes fail → warn
    rows.append({
        "manifest_id": mid,
        "status": cls,
        "project": project,
        "dimension": dimension,
        "baseline_ms": bms,
        "current_ms": cms,
        "delta_pct": delta_pct,
        "threshold": threshold,
        "exempted": exempted,
    })

# ---------------------------------------------------------------------------
# Aggregate verdict.
# ---------------------------------------------------------------------------
any_fail = any(r["status"] == "fail" for r in rows)
any_warn = any(r["status"] == "warn" for r in rows)
any_win = any(r["status"] in ("win", "win-small") for r in rows)
all_neutral_or_better = all(r["status"] in ("neutral", "win", "win-small") for r in rows)

if any_fail:
    verdict = "FAIL"
elif any_warn:
    verdict = "WARN"
elif any_win and all_neutral_or_better:
    verdict = "PASS+WINS"
else:
    verdict = "PASS"

# ---------------------------------------------------------------------------
# Render markdown report.
# ---------------------------------------------------------------------------
lines: list[str] = []
lines.append(f"## Perf-gate verdict — {verdict}")
lines.append("")
lines.append("Per PRD §17.10. Tracked metric: `summary.median_wall_ms`.")
lines.append("Per-dimension thresholds: D1+10%, D2+15%, D3+10%, D4+15%, D5+10%, D6+20%, D7+15%.")
lines.append("Improvements >5% are flagged as wins. Accepted-regressions.md exemptions demote fail → warn.")
lines.append("")
lines.append("| Manifest | Project | Dim | Baseline (ms) | Current (ms) | Δ | Threshold | Status | Exempt |")
lines.append("|---|---|---|---:|---:|---:|---:|---|---|")
for r in rows:
    bms = "—" if r["baseline_ms"] is None else f"{r['baseline_ms']:.1f}"
    cms = "—" if r["current_ms"] is None else f"{r['current_ms']:.1f}"
    delta = "—" if r["delta_pct"] is None else fmt_delta(r["delta_pct"])
    thr = "—" if r["threshold"] is None else f"+{r['threshold']:.1f}%"
    exempt = "yes" if r["exempted"] else ""
    lines.append(
        f"| `{r['manifest_id']}` | {r['project']} | {r['dimension']} | "
        f"{bms} | {cms} | {delta} | {thr} | {r['status']} | {exempt} |"
    )
lines.append("")

# Surface fails + warns as GitHub annotations so they appear in the
# PR's `Files changed` view, not just in the job log.
for r in rows:
    if r["status"] == "fail":
        msg = (f"perf-gate FAIL: {r['manifest_id']} ({r['project']}/{r['dimension']}) "
               f"regressed {fmt_delta(r['delta_pct'])} vs +{r['threshold']:.1f}% cap")
        print(f"::error::{msg}")
    elif r["status"] == "warn":
        why = "exempted via accepted-regressions.md" if r["exempted"] else "below threshold"
        msg = (f"perf-gate warn: {r['manifest_id']} ({r['project']}/{r['dimension']}) "
               f"regressed {fmt_delta(r['delta_pct'])} ({why})")
        print(f"::warning::{msg}")
    elif r["status"].startswith("missing"):
        print(f"::warning::perf-gate {r['status']}: {r['manifest_id']}")

report_md = "\n".join(lines) + "\n"

if summary_path:
    with open(summary_path, "a", encoding="utf-8") as f:
        f.write(report_md)
if report_path:
    Path(report_path).parent.mkdir(parents=True, exist_ok=True)
    Path(report_path).write_text(report_md, encoding="utf-8")

# Print verdict to stdout so the workflow logs surface it without
# needing to scroll through the markdown.
print(f"perf-gate verdict: {verdict}")
print(f"rows: {len(rows)} (fail={sum(1 for r in rows if r['status']=='fail')}, "
      f"warn={sum(1 for r in rows if r['status']=='warn')}, "
      f"win={sum(1 for r in rows if r['status'].startswith('win'))}, "
      f"neutral={sum(1 for r in rows if r['status']=='neutral')})")

sys.exit(1 if verdict == "FAIL" else 0)
PYEOF
