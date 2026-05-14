#!/usr/bin/env python3
"""Weekly security-summary issue generator.

Queries the issue tracker for every `security-bot`-labeled finding,
computes a rollup (open by severity, oldest open finding's age,
week-over-week delta, auto-remediation PR success rate over the past
seven days), and opens a new issue titled
`Security weekly summary — YYYY-WW` with the rollup in its body.

Inputs (env vars, all read with sensible defaults so the script is
trivially testable):

  GH_REPO                       owner/repo (set by GitHub Actions)
  REMEDIATION_BOT_LOGIN         GitHub login used by the auto-remediation
                                agent's PR author identity. Defaults to
                                `github-actions[bot]` if not set; the
                                workflow can override when the bot
                                identity is finalized (see C.5 T4).
  WEEKLY_SUMMARY_RETENTION_WEEKS
                                Older weekly-summary issues this many
                                weeks back get closed with a
                                "superseded" comment. Default 6.
  REPORT_DATE                   ISO date (YYYY-MM-DD) used as the
                                anchor for the current week. Defaults
                                to today (UTC). Mostly a testing knob.
  GH_DRY_RUN                    When `1`, the script prints the issue
                                body it would have created and skips
                                all mutating `gh` calls. The fixture
                                harness sets this off by default
                                because the shim already captures
                                writes.

Design notes:
- Pure stdlib + the `gh` CLI. No third-party deps to keep the workflow
  setup-free.
- The script never reaches outside the issue tracker: every metric is
  derived from `gh issue list` / `gh pr list` output. No code-scanning
  REST calls, no external services.
- Severity is parsed from the issue title or labels (in priority
  order). The filer at `.github/scripts/security_finding_to_issue.py`
  doesn't add a severity label today, so the title parse is the
  primary path; the label parse exists so a future filer change
  drops in without touching this script.
- Week numbering uses ISO 8601 (`%G-%V`) so the rollover always lands
  on Monday and the year boundary doesn't double-count.
"""
from __future__ import annotations

import datetime
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Iterable


# Label applied to every weekly-summary issue. Used to query prior
# summaries for the week-over-week delta and the retention sweep.
SUMMARY_LABEL = "weekly-summary"
SUMMARY_LABELS_ALWAYS = ["security", SUMMARY_LABEL]

# Label applied to every finding-issue by `security_finding_to_issue.py`.
FINDING_LABEL = "security-bot"

# Severity buckets we report on, in the order they appear in the body.
SEVERITIES = ["critical", "high", "medium", "low", "unknown"]


# ---------------------------------------------------------------------------
# Data classes


@dataclass
class Issue:
    number: int
    title: str
    state: str  # "OPEN" / "CLOSED"
    labels: list[str]
    created_at: datetime.datetime
    closed_at: datetime.datetime | None
    body: str = ""


@dataclass
class PullRequest:
    number: int
    state: str  # "OPEN" / "MERGED" / "CLOSED"
    created_at: datetime.datetime
    merged_at: datetime.datetime | None
    closed_at: datetime.datetime | None


@dataclass
class Rollup:
    open_by_severity: dict[str, int] = field(default_factory=dict)
    open_total: int = 0
    oldest_open_age_days: int | None = None
    auto_remediation_attempted: int = 0
    auto_remediation_merged: int = 0
    week_over_week_delta: int | None = None
    prior_summary_number: int | None = None
    snapshot_iso_week: str = ""

    @property
    def auto_remediation_success_pct(self) -> float | None:
        if self.auto_remediation_attempted == 0:
            return None
        return 100.0 * self.auto_remediation_merged / self.auto_remediation_attempted


# ---------------------------------------------------------------------------
# `gh` CLI wrapper (kept narrow so the fixture shim covers exactly what
# we use, no more).


def gh(args: list[str], stdin: str | None = None, check: bool = True) -> str:
    proc = subprocess.run(
        ["gh", *args],
        input=stdin,
        capture_output=True,
        text=True,
        check=False,
    )
    if check and proc.returncode != 0:
        sys.stderr.write(
            f"gh {' '.join(args)} failed (rc={proc.returncode})\n"
            f"stdout: {proc.stdout}\nstderr: {proc.stderr}\n"
        )
        raise SystemExit(proc.returncode)
    return proc.stdout


def list_finding_issues() -> list[Issue]:
    out = gh([
        "issue", "list",
        "--label", FINDING_LABEL,
        "--state", "all",
        "--limit", "500",
        "--json", "number,state,title,labels,createdAt,closedAt,body",
    ])
    return _parse_issues(out)


def list_summary_issues() -> list[Issue]:
    out = gh([
        "issue", "list",
        "--label", SUMMARY_LABEL,
        "--state", "all",
        "--limit", "200",
        "--json", "number,state,title,labels,createdAt,closedAt,body",
    ])
    return _parse_issues(out)


def list_bot_pull_requests(bot_login: str, since: datetime.datetime) -> list[PullRequest]:
    # `gh pr list` accepts `--search` with the same syntax GitHub
    # itself uses. We constrain to author + created-since.
    search = f"author:{bot_login} created:>={since.strftime('%Y-%m-%d')}"
    out = gh([
        "pr", "list",
        "--state", "all",
        "--limit", "200",
        "--search", search,
        "--json", "number,state,createdAt,mergedAt,closedAt",
    ])
    if not out.strip():
        return []
    return [
        PullRequest(
            number=int(p["number"]),
            state=str(p.get("state", "")).upper(),
            created_at=_parse_dt(p.get("createdAt")),
            merged_at=_parse_dt(p.get("mergedAt")),
            closed_at=_parse_dt(p.get("closedAt")),
        )
        for p in json.loads(out)
    ]


def create_issue(title: str, body: str, labels: Iterable[str]) -> str:
    if os.environ.get("GH_DRY_RUN") == "1":
        sys.stdout.write(f"--- DRY RUN: would create issue ---\n{title}\n\n{body}\n")
        return "dry-run://0"
    args = ["issue", "create", "--title", title, "--body-file", "-"]
    for lbl in labels:
        args.extend(["--label", lbl])
    return gh(args, stdin=body).strip()


def comment_and_close_issue(number: int, body: str) -> None:
    if os.environ.get("GH_DRY_RUN") == "1":
        sys.stdout.write(f"--- DRY RUN: would close #{number} with comment ---\n{body}\n")
        return
    gh(["issue", "comment", str(number), "--body-file", "-"], stdin=body)
    gh(["issue", "close", str(number)])


# ---------------------------------------------------------------------------
# Parsing helpers


def _parse_dt(value: str | None) -> datetime.datetime | None:
    if not value:
        return None
    # GitHub timestamps are ISO 8601 with a trailing `Z`; Python's
    # `fromisoformat` only accepts `+00:00` in <3.11, so we normalize.
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    return datetime.datetime.fromisoformat(value)


def _parse_issues(raw: str) -> list[Issue]:
    if not raw.strip():
        return []
    payload = json.loads(raw)
    issues: list[Issue] = []
    for item in payload:
        created = _parse_dt(item.get("createdAt"))
        if created is None:
            continue
        issues.append(Issue(
            number=int(item["number"]),
            title=item.get("title", ""),
            state=str(item.get("state", "")).upper(),
            labels=[lbl.get("name", "") if isinstance(lbl, dict) else str(lbl)
                    for lbl in item.get("labels", [])],
            created_at=created,
            closed_at=_parse_dt(item.get("closedAt")),
            body=item.get("body", "") or "",
        ))
    return issues


SEVERITY_PATTERNS = [
    (re.compile(r"\bcritical\b", re.IGNORECASE), "critical"),
    (re.compile(r"\bhigh\b|\berror\b", re.IGNORECASE), "high"),
    (re.compile(r"\bmedium\b|\bmoderate\b|\bwarning\b", re.IGNORECASE), "medium"),
    (re.compile(r"\blow\b|\bnote\b|\binfo\b", re.IGNORECASE), "low"),
]


def issue_severity(issue: Issue) -> str:
    """Best-effort severity extraction.

    Priority order:
      1. A label like `severity:high` (future-proofing — the filer
         doesn't add these today).
      2. Standalone severity labels (`critical`, `high`, etc.).
      3. The issue title.
      4. The issue body's first 4 KB.
    """
    for lbl in issue.labels:
        lbl_norm = lbl.lower()
        if lbl_norm.startswith("severity:"):
            tail = lbl_norm.split(":", 1)[1].strip()
            if tail in SEVERITIES:
                return tail
        if lbl_norm in SEVERITIES:
            return lbl_norm
    haystack = f"{issue.title}\n{issue.body[:4096]}"
    for pattern, severity in SEVERITY_PATTERNS:
        if pattern.search(haystack):
            return severity
    return "unknown"


# ---------------------------------------------------------------------------
# Rollup computation


def compute_rollup(
    findings: list[Issue],
    prior_summaries: list[Issue],
    bot_prs: list[PullRequest],
    now: datetime.datetime,
) -> Rollup:
    open_findings = [i for i in findings if i.state == "OPEN"]
    open_by_severity: dict[str, int] = {sev: 0 for sev in SEVERITIES}
    for finding in open_findings:
        sev = issue_severity(finding)
        open_by_severity[sev] = open_by_severity.get(sev, 0) + 1

    oldest_age = None
    if open_findings:
        oldest = min(open_findings, key=lambda i: i.created_at)
        oldest_age = (now - oldest.created_at).days

    # Auto-remediation success rate over the past 7 days. Window
    # bounded by `since` (passed into the bot PR query); merged vs.
    # not-merged is enough signal for the rollup.
    attempted = len(bot_prs)
    merged = sum(1 for pr in bot_prs if pr.merged_at is not None)

    # Week-over-week delta: difference in *open finding count* between
    # this report and the most recent prior weekly-summary issue. We
    # parse the prior count out of the prior summary's body via a
    # stable HTML comment marker.
    prior_total: int | None = None
    prior_number: int | None = None
    if prior_summaries:
        prior = max(prior_summaries, key=lambda i: i.created_at)
        prior_number = prior.number
        m = re.search(r"<!--\s*open-total:\s*(\d+)\s*-->", prior.body)
        if m:
            prior_total = int(m.group(1))
    delta = (len(open_findings) - prior_total) if prior_total is not None else None

    return Rollup(
        open_by_severity=open_by_severity,
        open_total=len(open_findings),
        oldest_open_age_days=oldest_age,
        auto_remediation_attempted=attempted,
        auto_remediation_merged=merged,
        week_over_week_delta=delta,
        prior_summary_number=prior_number,
        snapshot_iso_week=now.strftime("%G-W%V"),
    )


# ---------------------------------------------------------------------------
# Rendering


def render_body(rollup: Rollup, now: datetime.datetime, bot_login: str) -> str:
    lines: list[str] = []
    lines.append(f"# Security weekly summary — {rollup.snapshot_iso_week}")
    lines.append("")
    lines.append(
        f"Snapshot taken at `{now.strftime('%Y-%m-%dT%H:%M:%SZ')}` UTC. "
        f"This issue is generated by `security-weekly-summary.yml`; do not "
        f"edit by hand — re-running the workflow regenerates it."
    )
    lines.append("")
    lines.append("## Open findings by severity")
    lines.append("")
    lines.append("| Severity | Count |")
    lines.append("|----------|------:|")
    for sev in SEVERITIES:
        lines.append(f"| {sev} | {rollup.open_by_severity.get(sev, 0)} |")
    lines.append(f"| **total** | **{rollup.open_total}** |")
    lines.append("")
    lines.append("## Oldest open finding")
    lines.append("")
    if rollup.oldest_open_age_days is None:
        lines.append("No open findings. The tracker is empty.")
    else:
        lines.append(f"Oldest open finding is **{rollup.oldest_open_age_days}** day(s) old.")
    lines.append("")
    lines.append("## Auto-remediation PRs (past 7 days)")
    lines.append("")
    if rollup.auto_remediation_attempted == 0:
        lines.append(
            f"No PRs authored by `{bot_login}` in the past 7 days."
        )
    else:
        pct = rollup.auto_remediation_success_pct or 0.0
        lines.append(
            f"`{bot_login}` opened **{rollup.auto_remediation_attempted}** PR(s); "
            f"**{rollup.auto_remediation_merged}** merged "
            f"(success rate: **{pct:.1f}%**)."
        )
    lines.append("")
    lines.append("## Week-over-week")
    lines.append("")
    if rollup.week_over_week_delta is None:
        lines.append(
            "No prior weekly-summary issue with a parseable open-total marker; "
            "delta unavailable for this run."
        )
    else:
        sign = "+" if rollup.week_over_week_delta >= 0 else ""
        lines.append(
            f"Open-finding count changed by **{sign}{rollup.week_over_week_delta}** "
            f"vs. the prior summary (#{rollup.prior_summary_number})."
        )
    lines.append("")
    lines.append("## How this is computed")
    lines.append("")
    lines.append(
        "- Severity comes from each finding-issue's title or labels.\n"
        "- The oldest-open age is the wall-clock days since the issue's `createdAt`.\n"
        "- The auto-remediation rate counts PRs authored by the bot account in "
        "the past 7 days; merged-vs-not-merged is the success signal.\n"
        "- The week-over-week delta reads the prior summary's `open-total` "
        "HTML-comment marker; the field below is what next week reads."
    )
    lines.append("")
    # Stable machine-readable marker the *next* week's run reads.
    lines.append(f"<!-- open-total: {rollup.open_total} -->")
    lines.append(f"<!-- iso-week: {rollup.snapshot_iso_week} -->")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Retention sweep


def retire_old_summaries(
    summaries: list[Issue],
    retention_weeks: int,
    now: datetime.datetime,
) -> int:
    """Close summary issues older than `retention_weeks` weeks.

    Returns the number of issues closed.
    """
    cutoff = now - datetime.timedelta(weeks=retention_weeks)
    closed = 0
    for issue in summaries:
        if issue.state != "OPEN":
            continue
        if issue.created_at >= cutoff:
            continue
        comment_and_close_issue(
            issue.number,
            "Superseded by a newer weekly summary; closing as part of "
            f"the {retention_weeks}-week retention sweep.",
        )
        closed += 1
    return closed


# ---------------------------------------------------------------------------
# Entry point


def main() -> int:
    bot_login = os.environ.get("REMEDIATION_BOT_LOGIN", "github-actions[bot]")
    retention_weeks = int(os.environ.get("WEEKLY_SUMMARY_RETENTION_WEEKS", "6"))
    report_date_env = os.environ.get("REPORT_DATE")
    if report_date_env:
        anchor = datetime.datetime.fromisoformat(report_date_env).replace(
            tzinfo=datetime.timezone.utc
        )
    else:
        anchor = datetime.datetime.now(datetime.timezone.utc)

    since = anchor - datetime.timedelta(days=7)

    findings = list_finding_issues()
    prior_summaries = list_summary_issues()
    bot_prs = list_bot_pull_requests(bot_login, since)

    rollup = compute_rollup(findings, prior_summaries, bot_prs, anchor)
    body = render_body(rollup, anchor, bot_login)
    title = f"Security weekly summary — {rollup.snapshot_iso_week}"

    url = create_issue(title, body, SUMMARY_LABELS_ALWAYS)
    sys.stdout.write(f"created: {url}\n")

    closed = retire_old_summaries(prior_summaries, retention_weeks, anchor)
    if closed:
        sys.stdout.write(f"retired {closed} prior summary issue(s)\n")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
