#!/usr/bin/env python3
"""Validate the repository-tracked repair ledger.

Checks:
- IDs are unique across Fixed/Remaining.
- Every Fixed row has a commit hash that exists and is an ancestor of HEAD.
- Every Remaining row has no commit claim.

Exit code 1 on any inconsistency.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROW_RE = re.compile(r"^\|\s*([A-Z][A-Z0-9_]+)\s*\|")
COMMIT_RE = re.compile(r"`([0-9a-f]{7,40})`")
SECTION_RE = re.compile(r"^##\s+(Fixed|Remaining)\b")


def git_ok(*args: str) -> bool:
    return (
        subprocess.run(
            ["git", *args],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "path",
        nargs="?",
        default="docs/repair-ledger-20261108.md",
        type=Path,
    )
    args = parser.parse_args()
    if not args.path.is_file():
        print(f"repair ledger not found: {args.path}", file=sys.stderr)
        return 1

    section = ""
    seen: set[str] = set()
    problems: list[str] = []
    fixed = remaining = 0

    for lineno, line in enumerate(args.path.read_text(encoding="utf-8").splitlines(), 1):
        match = SECTION_RE.match(line)
        if match:
            section = match.group(1)
            continue
        if section not in {"Fixed", "Remaining"}:
            continue
        row = ROW_RE.match(line)
        if not row:
            continue
        issue_id = row.group(1)
        if issue_id == "ID":
            continue
        if issue_id in seen:
            problems.append(f"line {lineno}: duplicate issue id {issue_id}")
        seen.add(issue_id)
        if section == "Fixed":
            fixed += 1
            commit = COMMIT_RE.search(line)
            if not commit:
                problems.append(f"line {lineno}: fixed issue {issue_id} has no commit")
                continue
            sha = commit.group(1)
            if not git_ok("cat-file", "-e", f"{sha}^{{commit}}"):
                problems.append(f"line {lineno}: commit {sha} does not exist")
            elif not git_ok("merge-base", "--is-ancestor", sha, "HEAD"):
                problems.append(f"line {lineno}: commit {sha} is not reachable from HEAD")
        else:
            remaining += 1
            if COMMIT_RE.search(line):
                problems.append(f"line {lineno}: remaining issue {issue_id} claims a commit")

    if fixed == 0 and remaining == 0:
        problems.append("no ledger rows found")

    if problems:
        print("repair ledger check failed:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1

    print(f"repair ledger OK: fixed={fixed} remaining={remaining} unique={len(seen)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
