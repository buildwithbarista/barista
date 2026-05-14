#!/usr/bin/env python3
"""
count-unsafe.py — count `unsafe` constructs per crate.

Walks the workspace's Rust source files and counts the number of lines that
introduce an `unsafe` construct (block, fn, trait, impl, extern). Comments
and string contents are ignored. The result is grouped by crate (the first
path component under `crates/` or the literal `roastery`).

Output is JSON on stdout, sorted by crate name, suitable for diffing
against a checked-in baseline. The schema is:

    {
      "schema": 1,
      "tool": "count-unsafe.py",
      "totals": { "<crate>": <count>, ... }
    }

Usage:
    scripts/count-unsafe.py              # print current counts
    scripts/count-unsafe.py --check      # compare against baseline
    scripts/count-unsafe.py --baseline   # write a new baseline

A counted line matches one of:
    unsafe {                  # unsafe block
    unsafe fn ...             # unsafe function
    unsafe impl ...           # unsafe impl
    unsafe trait ...          # unsafe trait
    unsafe extern ...         # unsafe extern block (FFI)
    pub unsafe fn ...         # visibility-prefixed variants
    pub(...) unsafe fn ...
    async unsafe fn ...

The intent is to ratchet: the baseline records what is currently in tree;
adding a new `unsafe` construct in a crate must be paired with a baseline
bump (which forces the diff to land in the same PR and surface in review).

Why not cargo-geiger? cargo-geiger requires a nightly toolchain to give
fully accurate counts, its JSON output schema has churned across releases,
and its "lines of unsafe" metric depends on transitive-dependency
inspection that is not what we want here (we only care about first-party
code under `crates/` and `roastery/`). A line-level grep over our own
sources gives a stable, deterministic signal and is trivial to reproduce
on any contributor's machine.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# Match an `unsafe` keyword that begins a construct: block, fn, impl, trait,
# or extern. Vis modifiers (`pub`, `pub(crate)`, `pub(super)`, `pub(in
# path)`) and `async`/`const` qualifiers are allowed in front.
#
# We require either `{` (block) or one of the construct keywords to follow.
# This explicitly excludes:
#   - the word "unsafe" inside a comment (handled by the comment stripper
#     below)
#   - the word "unsafe" inside a string literal
#   - identifiers like `unsafe_foo`
UNSAFE_PATTERN = re.compile(
    r"""
    (?<![A-Za-z0-9_])             # left word boundary (no underscore prefix)
    (?:pub\s*(?:\([^)]*\))?\s+)?  # optional vis modifier
    (?:async\s+|const\s+)?        # optional async/const qualifier
    unsafe
    \s+
    (?:fn|impl|trait|extern)      # construct keyword
    \b
    |
    (?<![A-Za-z0-9_])
    unsafe
    \s*
    \{                            # unsafe block
    """,
    re.VERBOSE,
)


def strip_line_comments_and_strings(line: str) -> str:
    """Return `line` with `//` line comments stripped and string contents
    (`"..."`) replaced by empty quotes so we don't match `unsafe` inside
    them. Block comments (`/* ... */`) are handled by the caller (whole-
    file pass) because they can span multiple lines.
    """
    # Strip `//` line comments. We do a simple character walk so escaped
    # quotes inside strings don't confuse us.
    out = []
    i = 0
    n = len(line)
    in_str = False
    while i < n:
        c = line[i]
        if not in_str and c == "/" and i + 1 < n and line[i + 1] == "/":
            break
        if c == '"':
            if in_str:
                in_str = False
                out.append('"')
            else:
                in_str = True
                out.append('"')
                # consume the body without keeping it
                i += 1
                while i < n:
                    if line[i] == "\\" and i + 1 < n:
                        i += 2
                        continue
                    if line[i] == '"':
                        in_str = False
                        out.append('"')
                        i += 1
                        break
                    i += 1
                continue
            i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def strip_block_comments(src: str) -> str:
    """Remove `/* ... */` block comments (non-nesting; nesting is rare in
    practice and the few cases would only produce false negatives, which
    is the safe direction)."""
    return re.sub(r"/\*.*?\*/", "", src, flags=re.DOTALL)


def count_unsafe_in_file(path: Path) -> int:
    try:
        src = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return 0
    src = strip_block_comments(src)
    count = 0
    for raw_line in src.splitlines():
        line = strip_line_comments_and_strings(raw_line)
        if UNSAFE_PATTERN.search(line):
            count += 1
    return count


def crate_for(path: Path, repo_root: Path) -> str | None:
    rel = path.relative_to(repo_root)
    parts = rel.parts
    if len(parts) >= 2 and parts[0] == "crates":
        return parts[1]
    if len(parts) >= 1 and parts[0] == "roastery":
        return "roastery"
    return None


def walk(repo_root: Path) -> dict[str, int]:
    totals: dict[str, int] = {}
    roots = [repo_root / "crates", repo_root / "roastery"]
    for root in roots:
        if not root.is_dir():
            continue
        for p in root.rglob("*.rs"):
            # Skip target/ build artifacts and fuzz sub-crates (separate
            # workspaces; tracked by their own baseline if added later).
            if "/target/" in str(p) or "/fuzz/" in str(p):
                continue
            crate = crate_for(p, repo_root)
            if crate is None:
                continue
            n = count_unsafe_in_file(p)
            if n:
                totals[crate] = totals.get(crate, 0) + n
    return totals


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--check",
        action="store_true",
        help="Compare against the checked-in baseline; exit non-zero on growth.",
    )
    ap.add_argument(
        "--baseline",
        action="store_true",
        help="Write the current counts to the baseline file and exit.",
    )
    ap.add_argument(
        "--baseline-path",
        default="docs/ci/geiger-baseline.json",
        help="Path to the baseline file (default: docs/ci/geiger-baseline.json).",
    )
    args = ap.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    totals = walk(repo_root)
    payload = {
        "schema": 1,
        "tool": "count-unsafe.py",
        "totals": dict(sorted(totals.items())),
    }
    rendered = json.dumps(payload, indent=2) + "\n"

    baseline_path = repo_root / args.baseline_path

    if args.baseline:
        baseline_path.parent.mkdir(parents=True, exist_ok=True)
        baseline_path.write_text(rendered, encoding="utf-8")
        print(f"wrote baseline to {baseline_path}", file=sys.stderr)
        return 0

    if args.check:
        if not baseline_path.is_file():
            print(
                f"error: baseline file not found at {baseline_path}; "
                f"run `{sys.argv[0]} --baseline` to create it.",
                file=sys.stderr,
            )
            return 2
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
        base_totals = baseline.get("totals", {})
        regressions: list[tuple[str, int, int]] = []
        for crate, current in totals.items():
            prev = int(base_totals.get(crate, 0))
            if current > prev:
                regressions.append((crate, prev, current))
        if regressions:
            print("unsafe-line baseline regression detected:", file=sys.stderr)
            for crate, prev, current in regressions:
                print(
                    f"  {crate}: {prev} -> {current} (+{current - prev})",
                    file=sys.stderr,
                )
            print(
                "\nIf this is intentional (new vetted `unsafe` block), "
                "regenerate the baseline with:\n"
                f"  python3 {Path(sys.argv[0])} "
                "--baseline\n"
                "and include the diff in the same PR. Each `unsafe` site "
                "must carry an inline SAFETY comment.",
                file=sys.stderr,
            )
            return 1
        # Also surface decreases (good news, but baseline is stale).
        decreases: list[tuple[str, int, int]] = []
        for crate, prev in base_totals.items():
            current = totals.get(crate, 0)
            if current < int(prev):
                decreases.append((crate, int(prev), current))
        if decreases:
            print(
                "unsafe-line baseline could be tightened "
                "(counts decreased since baseline):",
                file=sys.stderr,
            )
            for crate, prev, current in decreases:
                print(f"  {crate}: {prev} -> {current}", file=sys.stderr)
            print(
                "\nThis is informational, not a failure. Run "
                f"`python3 {Path(sys.argv[0])} "
                "--baseline` to tighten the ratchet.",
                file=sys.stderr,
            )
        print(rendered, end="")
        return 0

    print(rendered, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
