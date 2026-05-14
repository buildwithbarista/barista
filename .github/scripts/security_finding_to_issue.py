#!/usr/bin/env python3
"""Open GitHub issues for net-new code-scanning findings.

Drives the `security-finding-to-issue.yml` workflow. Pulls open alerts
from the GitHub code scanning REST API, fingerprints each one, dedupes
against existing `security-bot`-labeled issues (open AND closed), and
opens at most `MAX_ISSUES_PER_RUN` new issues per invocation. Findings
beyond the cap roll up into a single tracker issue so the suppression
itself is visible.

Design notes:

  - The fingerprint is `sha256({tool} | {rule_id} | {path} |
    {start_line} | {snippet})`. SARIF's native `fingerprints` field
    would be preferable but it isn't exposed through the code-scanning
    REST API — the API gives us the resolved alert, not the raw SARIF
    properties bag. Computing our own fingerprint keeps the dedup logic
    self-contained and lets us re-derive it from any data source.

  - The fingerprint is embedded in the issue body inside an HTML
    comment block (`<!-- sec-fingerprint: <hash> -->`). The next run
    reads existing issues' bodies, regexes out the comment, and stores
    the set of seen fingerprints in memory. No external database; the
    issue tracker itself is the state store.

  - Both open and closed `security-bot`-labeled issues count as
    "previously seen". Closing an issue is the human triage decision
    (resolved / wontfix / duplicate); we don't reopen.

  - The script intentionally talks to GitHub through the `gh` CLI, not
    the REST API directly. `gh` handles auth, pagination, and rate
    limiting; the `verify.sh` test harness intercepts `gh` calls via a
    shim on PATH, so the production code and the tests share one
    surface.

  - `GH_DRY_RUN=1` makes the script run end-to-end but never actually
    create or comment on issues. The verification script doesn't use
    this mode (the shim is more faithful), but it's useful for manual
    debugging.

"""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Iterable

# ---------------------------------------------------------------------------
# Constants

# Labels applied to every issue this script opens. The `security-bot`
# label is what the script keys off when querying existing issues for
# the dedup pass; the `security` label is the human-facing rollup that
# groups every security issue in the tracker.
LABELS_ALWAYS = ("security", "security-bot")

# Pattern matching the fingerprint HTML comment in an issue body.
FINGERPRINT_RE = re.compile(
    r"<!--\s*sec-fingerprint:\s*([0-9a-f]{64})\s*-->",
    re.IGNORECASE,
)

# Pattern marking the tracker issue used for rate-limit overflow.
TRACKER_MARKER = "<!-- sec-tracker: deferred-findings -->"


# ---------------------------------------------------------------------------
# Types


@dataclass
class Finding:
    """A normalized code-scanning alert ready to be turned into an issue."""

    tool: str
    rule_id: str
    rule_description: str
    severity: str
    path: str
    start_line: int
    end_line: int
    snippet: str
    message: str
    help_text: str
    html_url: str
    tool_version: str = ""

    @property
    def fingerprint(self) -> str:
        """Stable hash over the load-bearing identity fields."""
        payload = "|".join((
            self.tool,
            self.rule_id,
            self.path,
            str(self.start_line),
            self.snippet,
        ))
        return hashlib.sha256(payload.encode("utf-8")).hexdigest()

    def issue_title(self) -> str:
        return (
            f"[security] {self.tool}: {self.rule_id} "
            f"in {self.path}:{self.start_line}"
        )

    def labels(self) -> list[str]:
        """Issue labels: the always-on pair plus the scanner short name.

        The scanner label lets a reviewer filter "every gitleaks finding"
        from the Issues UI; it's redundant with the tool name in the
        title but redundancy on labels is cheap and the filter UI is
        worth it.
        """
        # GitHub labels can't contain certain characters; normalize.
        scanner_label = re.sub(r"[^a-z0-9._-]", "-", self.tool.lower()).strip("-")
        return [*LABELS_ALWAYS, scanner_label] if scanner_label else list(LABELS_ALWAYS)


@dataclass
class Context:
    """Workflow-run metadata threaded into every issue body."""

    upstream_workflow: str = ""
    upstream_run_id: str = ""
    upstream_run_url: str = ""
    upstream_head_sha: str = ""

    @classmethod
    def from_env(cls) -> "Context":
        return cls(
            upstream_workflow=os.environ.get("UPSTREAM_WORKFLOW", ""),
            upstream_run_id=os.environ.get("UPSTREAM_RUN_ID", ""),
            upstream_run_url=os.environ.get("UPSTREAM_RUN_URL", ""),
            upstream_head_sha=os.environ.get("UPSTREAM_HEAD_SHA", ""),
        )


@dataclass
class GhClient:
    """Thin wrapper over the `gh` CLI for the operations we need.

    Kept narrow on purpose so the test shim only has to mock four
    subcommands: `gh api`, `gh issue list`, `gh issue create`, and
    `gh issue comment`.
    """

    dry_run: bool = False
    seen_calls: list[list[str]] = field(default_factory=list)

    def _run(self, args: list[str], stdin: str | None = None) -> str:
        """Invoke `gh` and return stdout. Surfaces stderr on failure."""
        self.seen_calls.append(["gh", *args])
        if self.dry_run and args and args[0] in {"issue"} and len(args) > 1 and args[1] in {"create", "comment", "close", "edit"}:
            return ""
        proc = subprocess.run(
            ["gh", *args],
            input=stdin,
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            sys.stderr.write(
                f"gh {' '.join(args)} failed (rc={proc.returncode})\n"
                f"stdout: {proc.stdout}\n"
                f"stderr: {proc.stderr}\n"
            )
            raise SystemExit(proc.returncode)
        return proc.stdout

    def list_alerts(self) -> list[dict]:
        """Return every open code-scanning alert on the default branch."""
        # `--paginate` walks `Link:` headers; `gh` will issue many
        # requests if the alert count is large. We filter to open
        # alerts because closed ones are out of scope here — the human
        # already made a decision on those.
        out = self._run([
            "api",
            "--paginate",
            "--method", "GET",
            "/repos/{owner}/{repo}/code-scanning/alerts",
            "-f", "state=open",
            "-f", "per_page=100",
        ])
        if not out.strip():
            return []
        # `--paginate` concatenates JSON arrays back-to-back when the
        # response is a list. We re-parse by walking the JSON stream
        # (jq would be cleaner but adds a dep).
        return _concat_json_arrays(out)

    def list_security_bot_issues(self) -> list[dict]:
        """All `security-bot`-labeled issues (open + closed)."""
        out = self._run([
            "issue", "list",
            "--label", "security-bot",
            "--state", "all",
            "--limit", "1000",
            "--json", "number,state,title,body,labels",
        ])
        if not out.strip():
            return []
        return json.loads(out)

    def create_issue(self, title: str, body: str, labels: Iterable[str]) -> str:
        """Open a new issue and return its URL/number string."""
        args = ["issue", "create", "--title", title, "--body-file", "-"]
        for label in labels:
            args.extend(["--label", label])
        return self._run(args, stdin=body).strip()

    def comment_issue(self, number: int, body: str) -> None:
        self._run(
            ["issue", "comment", str(number), "--body-file", "-"],
            stdin=body,
        )


# ---------------------------------------------------------------------------
# Helpers


def _concat_json_arrays(blob: str) -> list[dict]:
    """Decode a stream of concatenated JSON arrays into one flat list.

    `gh api --paginate` emits each page as its own JSON document
    (typically a list). Concatenated, the output is `[...][...]`, which
    `json.loads` rejects. We use `JSONDecoder.raw_decode` to walk the
    string one document at a time.
    """
    decoder = json.JSONDecoder()
    out: list[dict] = []
    idx = 0
    blob = blob.strip()
    while idx < len(blob):
        # Skip whitespace between docs.
        while idx < len(blob) and blob[idx].isspace():
            idx += 1
        if idx >= len(blob):
            break
        doc, length = decoder.raw_decode(blob, idx)
        if isinstance(doc, list):
            out.extend(doc)
        else:
            out.append(doc)
        idx += length
    return out


def normalize_alert(alert: dict) -> Finding | None:
    """Turn a code-scanning API alert into a Finding, or None if skip."""
    rule = alert.get("rule") or {}
    instance = alert.get("most_recent_instance") or {}
    tool = (alert.get("tool") or {}).get("name") or "unknown"
    tool_version = (alert.get("tool") or {}).get("version") or ""
    location = instance.get("location") or {}
    path = location.get("path") or "(unknown)"
    start_line = int(location.get("start_line") or 0)
    end_line = int(location.get("end_line") or start_line)
    # Code-scanning's "snippet" is informational only; some alerts
    # populate it, others don't. The fingerprint should still be
    # stable for those without one, so empty-string is fine.
    snippet = ""
    region_snippet = (
        (instance.get("location") or {}).get("snippet")
        if isinstance(instance.get("location"), dict)
        else None
    )
    if isinstance(region_snippet, dict):
        snippet = region_snippet.get("text") or ""
    return Finding(
        tool=tool,
        tool_version=tool_version,
        rule_id=rule.get("id") or "(no-rule-id)",
        rule_description=rule.get("description") or rule.get("name") or "",
        severity=(rule.get("severity") or rule.get("security_severity_level") or "unknown"),
        path=path,
        start_line=start_line,
        end_line=end_line,
        snippet=snippet,
        message=(instance.get("message") or {}).get("text") or "",
        help_text=rule.get("help") or "",
        html_url=alert.get("html_url") or "",
    )


def existing_fingerprints(issues: list[dict]) -> set[str]:
    """Pull every fingerprint hash out of the existing issue bodies."""
    seen: set[str] = set()
    for issue in issues:
        body = issue.get("body") or ""
        for match in FINGERPRINT_RE.finditer(body):
            seen.add(match.group(1).lower())
    return seen


def find_tracker_issue(issues: list[dict]) -> dict | None:
    """Return the existing rate-limit tracker issue, if any."""
    for issue in issues:
        body = issue.get("body") or ""
        if TRACKER_MARKER in body and issue.get("state") == "OPEN":
            return issue
    return None


def render_issue_body(finding: Finding, ctx: Context) -> str:
    """Markdown body for a per-finding issue. Order matters for skimming."""
    lines: list[str] = []
    # Fingerprint comment first so the dedup regex finds it without
    # having to walk the whole body.
    lines.append(f"<!-- sec-fingerprint: {finding.fingerprint} -->")
    lines.append("")
    lines.append(f"## {finding.tool}: `{finding.rule_id}`")
    lines.append("")
    lines.append(f"**Severity:** {finding.severity}")
    if finding.rule_description:
        lines.append("")
        lines.append(f"**Rule:** {finding.rule_description}")
    lines.append("")
    lines.append("### Location")
    lines.append("")
    line_range = (
        f"{finding.start_line}"
        if finding.end_line in (0, finding.start_line)
        else f"{finding.start_line}-{finding.end_line}"
    )
    lines.append(f"- File: `{finding.path}`")
    lines.append(f"- Lines: {line_range}")
    if finding.snippet:
        lines.append("")
        lines.append("```")
        lines.append(finding.snippet.rstrip())
        lines.append("```")
    if finding.message:
        lines.append("")
        lines.append("### Finding")
        lines.append("")
        lines.append(finding.message.strip())
    if finding.help_text:
        lines.append("")
        lines.append("### Remediation hint")
        lines.append("")
        lines.append(finding.help_text.strip())
    lines.append("")
    lines.append("### Scanner")
    lines.append("")
    lines.append(f"- Tool: `{finding.tool}`"
                 + (f" `{finding.tool_version}`" if finding.tool_version else ""))
    if finding.html_url:
        lines.append(f"- [View in Security tab]({finding.html_url})")
    if ctx.upstream_workflow:
        lines.append("")
        lines.append("### Provenance")
        lines.append("")
        lines.append(f"- Upstream workflow: `{ctx.upstream_workflow}`")
        if ctx.upstream_run_url:
            lines.append(f"- Upstream run: {ctx.upstream_run_url}")
        if ctx.upstream_head_sha:
            lines.append(f"- HEAD sha: `{ctx.upstream_head_sha}`")
    lines.append("")
    lines.append("### Auto-remediation")
    lines.append("")
    # Placeholder section. The auto-remediation agent (separate
    # workflow) will append its plan + diff when it picks this issue
    # up. Left explicitly blank rather than omitted so the structure
    # is obvious to humans scanning a fresh issue.
    lines.append("_The auto-remediation agent has not yet attempted this finding._")
    lines.append("")
    return "\n".join(lines)


def render_tracker_body(deferred_count: int, ctx: Context) -> str:
    lines: list[str] = []
    lines.append(TRACKER_MARKER)
    lines.append("")
    lines.append("## Deferred security findings")
    lines.append("")
    lines.append(
        f"The latest scanner run produced more findings than the per-run cap "
        f"allowed; **{deferred_count} additional finding(s) were deferred** to "
        f"avoid spamming the issue tracker."
    )
    lines.append("")
    lines.append(
        "Re-running `Security finding → issue` (via the `workflow_dispatch` "
        "trigger) after triaging the existing batch will pick the rest up."
    )
    if ctx.upstream_run_url:
        lines.append("")
        lines.append(f"Source run: {ctx.upstream_run_url}")
    lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Main


def run() -> int:
    max_per_run = int(os.environ.get("MAX_ISSUES_PER_RUN", "25"))
    dry_run = os.environ.get("GH_DRY_RUN", "").lower() in {"1", "true", "yes"}
    gh = GhClient(dry_run=dry_run)
    ctx = Context.from_env()

    alerts = gh.list_alerts()
    sys.stdout.write(f"Fetched {len(alerts)} open code-scanning alert(s).\n")

    findings: list[Finding] = []
    for alert in alerts:
        f = normalize_alert(alert)
        if f is not None:
            findings.append(f)
    sys.stdout.write(f"Normalized {len(findings)} finding(s).\n")

    existing_issues = gh.list_security_bot_issues()
    sys.stdout.write(
        f"Found {len(existing_issues)} existing security-bot issue(s) (open + closed).\n"
    )
    seen = existing_fingerprints(existing_issues)
    sys.stdout.write(f"Known fingerprint(s): {len(seen)}\n")

    new_findings = [f for f in findings if f.fingerprint not in seen]
    # Deterministic order: by tool, then rule, then path:line. Keeps
    # issue numbers reproducible on test fixtures.
    new_findings.sort(key=lambda f: (f.tool, f.rule_id, f.path, f.start_line))
    sys.stdout.write(f"Net-new finding(s): {len(new_findings)}\n")

    to_open = new_findings[:max_per_run]
    deferred = new_findings[max_per_run:]

    opened = 0
    for finding in to_open:
        body = render_issue_body(finding, ctx)
        url = gh.create_issue(
            title=finding.issue_title(),
            body=body,
            labels=finding.labels(),
        )
        sys.stdout.write(f"Opened issue for {finding.tool}/{finding.rule_id}: {url}\n")
        opened += 1

    if deferred:
        tracker = find_tracker_issue(existing_issues)
        tracker_body = render_tracker_body(len(deferred), ctx)
        if tracker is None:
            url = gh.create_issue(
                title=f"[security] {len(deferred)} finding(s) deferred by rate limit",
                body=tracker_body,
                labels=[*LABELS_ALWAYS, "rate-limited"],
            )
            sys.stdout.write(f"Opened tracker issue: {url}\n")
        else:
            # Append a comment so the history of overflow events is
            # preserved on a single issue rather than fragmented.
            gh.comment_issue(tracker["number"], tracker_body)
            sys.stdout.write(
                f"Commented on existing tracker issue #{tracker['number']} "
                f"with {len(deferred)} deferred finding(s).\n"
            )

    sys.stdout.write(
        f"Summary: {opened} issue(s) opened, {len(deferred)} deferred, "
        f"{len(findings) - len(new_findings)} skipped as duplicates.\n"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(run())
