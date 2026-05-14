#!/usr/bin/env python3
"""Drop-in `gh` replacement for the security-finding-to-issue test suite.

The real `gh` CLI hits GitHub. This shim records every call and replies
with canned data sourced from `$GH_SHIM_ALERTS` (alerts fixture) and
`$GH_SHIM_ISSUES` (existing-issues fixture). New issues get appended to
`$GH_SHIM_ISSUES_OUT` (one JSON object per line) so the verification
script can assert on what would have been created.

Supported invocations (the only ones `security_finding_to_issue.py`
emits):

  gh api --paginate --method GET /repos/{owner}/{repo}/code-scanning/alerts \
         -f state=open -f per_page=100
      → prints the JSON-array contents of $GH_SHIM_ALERTS.

  gh issue list --label security-bot --state all --limit 1000 \
                --json number,state,title,body,labels
      → prints the JSON-array contents of $GH_SHIM_ISSUES.

  gh issue create --title <T> --body-file - --label <L> [--label ...]
      → reads body from stdin; appends {number, title, body, labels} to
        $GH_SHIM_ISSUES_OUT; prints the synthesized issue URL.

  gh issue comment <N> --body-file -
      → reads body from stdin; appends {comment_on: N, body} to
        $GH_SHIM_ISSUES_OUT.

Anything else is logged and exits non-zero so a regression in the
production script that introduces a new `gh` call surfaces loudly.
"""
from __future__ import annotations

import json
import os
import pathlib
import sys


def die(msg: str, rc: int = 2) -> int:
    sys.stderr.write(f"gh_shim: {msg}\n")
    return rc


def load_json_array(env_var: str) -> list[dict]:
    path = os.environ.get(env_var)
    if not path:
        return []
    p = pathlib.Path(path)
    if not p.exists():
        return []
    text = p.read_text().strip()
    if not text:
        return []
    return json.loads(text)


def append_record(record: dict) -> None:
    out = os.environ.get("GH_SHIM_ISSUES_OUT")
    if not out:
        return
    pathlib.Path(out).parent.mkdir(parents=True, exist_ok=True)
    with open(out, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(record) + "\n")


def next_issue_number() -> int:
    """Counter persisted to a sidecar file alongside ISSUES_OUT."""
    out = os.environ.get("GH_SHIM_ISSUES_OUT")
    if not out:
        return 9000
    counter_path = pathlib.Path(out + ".counter")
    if counter_path.exists():
        n = int(counter_path.read_text().strip())
    else:
        # Start above any fixture-baseline issue numbers (those use
        # 1-999 so a shim-created issue is unambiguous).
        n = 1000
    n += 1
    counter_path.write_text(str(n))
    return n


def cmd_api(args: list[str]) -> int:
    # `gh api --paginate --method GET <path> -f k=v ...`
    if "code-scanning/alerts" in " ".join(args):
        alerts = load_json_array("GH_SHIM_ALERTS")
        sys.stdout.write(json.dumps(alerts))
        return 0
    return die(f"unhandled `gh api` call: {args}")


def cmd_issue(args: list[str]) -> int:
    if not args:
        return die("`gh issue` with no subcommand")
    sub = args[0]
    rest = args[1:]
    if sub == "list":
        # Filter to `--label security-bot` (the only filter the
        # production script applies).
        issues = load_json_array("GH_SHIM_ISSUES")
        sys.stdout.write(json.dumps(issues))
        return 0
    if sub == "create":
        title = ""
        labels: list[str] = []
        body = ""
        i = 0
        while i < len(rest):
            a = rest[i]
            if a == "--title":
                title = rest[i + 1]
                i += 2
            elif a == "--label":
                labels.append(rest[i + 1])
                i += 2
            elif a == "--body-file":
                src = rest[i + 1]
                body = sys.stdin.read() if src == "-" else pathlib.Path(src).read_text()
                i += 2
            else:
                i += 1
        number = next_issue_number()
        append_record({
            "action": "create",
            "number": number,
            "title": title,
            "labels": labels,
            "body": body,
        })
        sys.stdout.write(f"https://github.com/example/example/issues/{number}\n")
        return 0
    if sub == "comment":
        if not rest:
            return die("`gh issue comment` with no issue number")
        number = int(rest[0])
        body = ""
        i = 1
        while i < len(rest):
            a = rest[i]
            if a == "--body-file":
                src = rest[i + 1]
                body = sys.stdin.read() if src == "-" else pathlib.Path(src).read_text()
                i += 2
            else:
                i += 1
        append_record({
            "action": "comment",
            "number": number,
            "body": body,
        })
        return 0
    return die(f"unhandled `gh issue {sub}` call: {rest}")


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        return die("expected subcommand")
    sub = argv[1]
    rest = argv[2:]
    # Log every call to a trace file (handy when a test fails).
    trace = os.environ.get("GH_SHIM_TRACE")
    if trace:
        with open(trace, "a", encoding="utf-8") as fh:
            fh.write(" ".join([sub, *rest]) + "\n")
    if sub == "api":
        return cmd_api(rest)
    if sub == "issue":
        return cmd_issue(rest)
    return die(f"unhandled gh subcommand: {sub}")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
