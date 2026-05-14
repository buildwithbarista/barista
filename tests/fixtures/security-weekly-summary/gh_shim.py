#!/usr/bin/env python3
"""Drop-in `gh` replacement for the weekly-summary test harness.

The real `gh` CLI hits GitHub. This shim records every call and
replies with canned data sourced from environment-pointed fixture
files. New issues / comments / closes are appended to
`$GH_SHIM_OUT` (one JSON object per line) so the verification
script can assert on what would have been written.

Supported invocations:

  gh issue list --label <L> --state <S> --limit <N> --json <fields>
      → prints the JSON-array contents of $GH_SHIM_FINDINGS
        (when --label security-bot) or $GH_SHIM_SUMMARIES
        (when --label weekly-summary). Empty for any other label.

  gh pr list --state all --limit <N> --search <Q> --json <fields>
      → prints the JSON-array contents of $GH_SHIM_PRS.

  gh issue create --title <T> --body-file - --label <L> [--label ...]
      → reads body from stdin; appends a `create` record.

  gh issue comment <N> --body-file -
      → reads body from stdin; appends a `comment` record.

  gh issue close <N>
      → appends a `close` record.

Anything else exits non-zero so a regression that adds an unknown
gh call fails loudly.
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
    out = os.environ.get("GH_SHIM_OUT")
    if not out:
        return
    pathlib.Path(out).parent.mkdir(parents=True, exist_ok=True)
    with open(out, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(record) + "\n")


def next_issue_number() -> int:
    out = os.environ.get("GH_SHIM_OUT")
    if not out:
        return 9000
    counter_path = pathlib.Path(out + ".counter")
    if counter_path.exists():
        n = int(counter_path.read_text().strip())
    else:
        n = 5000
    n += 1
    counter_path.write_text(str(n))
    return n


def _label_arg(args: list[str]) -> str | None:
    i = 0
    while i < len(args):
        if args[i] == "--label" and i + 1 < len(args):
            return args[i + 1]
        i += 1
    return None


def cmd_issue(args: list[str]) -> int:
    if not args:
        return die("`gh issue` with no subcommand")
    sub, rest = args[0], args[1:]
    if sub == "list":
        label = _label_arg(rest)
        if label == "security-bot":
            sys.stdout.write(json.dumps(load_json_array("GH_SHIM_FINDINGS")))
            return 0
        if label == "weekly-summary":
            sys.stdout.write(json.dumps(load_json_array("GH_SHIM_SUMMARIES")))
            return 0
        sys.stdout.write("[]")
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
            return die("`gh issue comment` with no number")
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
        append_record({"action": "comment", "number": number, "body": body})
        return 0
    if sub == "close":
        if not rest:
            return die("`gh issue close` with no number")
        number = int(rest[0])
        append_record({"action": "close", "number": number})
        return 0
    return die(f"unhandled `gh issue {sub}` call")


def cmd_pr(args: list[str]) -> int:
    if not args:
        return die("`gh pr` with no subcommand")
    sub = args[0]
    if sub == "list":
        sys.stdout.write(json.dumps(load_json_array("GH_SHIM_PRS")))
        return 0
    return die(f"unhandled `gh pr {sub}` call")


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        return die("expected subcommand")
    sub, rest = argv[1], argv[2:]
    trace = os.environ.get("GH_SHIM_TRACE")
    if trace:
        with open(trace, "a", encoding="utf-8") as fh:
            fh.write(" ".join([sub, *rest]) + "\n")
    if sub == "issue":
        return cmd_issue(rest)
    if sub == "pr":
        return cmd_pr(rest)
    return die(f"unhandled gh subcommand: {sub}")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
