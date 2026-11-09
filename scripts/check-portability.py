#!/usr/bin/env python3
"""Portability gate for an open-source, cross-platform project.

This is intentionally narrow and deterministic:
- CI must cover Windows/macOS/Linux and the release build matrix must contain all targets.
- Token statistics must not use mtime as a content-age filter.
- The persistent token index must use the shared store dir and handle Windows rename.
- Tests must not read/write the real user store for the token index.
- New platform commands should be documented in CONTRIBUTING's portability section.

Exit code 1 on any violation.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEST_YML = ROOT / ".github/workflows/test.yml"
BUILD_YML = ROOT / ".github/workflows/build.yml"
TOKEN_RS = ROOT / "crates/wb-switch-core/src/modules/token_stats.rs"
CONTRIBUTING = ROOT / "CONTRIBUTING.md"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def main() -> int:
    problems: list[str] = []

    if not TEST_YML.is_file() or not BUILD_YML.is_file():
        print("missing CI workflow files", file=sys.stderr)
        return 1

    test_ci = read(TEST_YML)
    for os_name in ("windows-latest", "macos-latest", "ubuntu-latest"):
        if os_name not in test_ci:
            problems.append(f"test.yml missing CI OS: {os_name}")

    build_ci = read(BUILD_YML)
    for target in (
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ):
        if target not in build_ci:
            problems.append(f"build.yml missing target: {target}")

    token = read(TOKEN_RS)
    # mtime may be used for ordering, but never as a range cutoff (copies/touches lie).
    if re.search(r"paths\.retain\([^)]*modified|metadata\([^)]*\)[\s\S]{0,200}cutoff", token):
        problems.append("token_stats must not use file mtime as a cutoff filter")
    if "token-file-index.json" not in token or "store_dir()" not in token:
        problems.append("token file index must live under the shared store_dir")
    if "std::fs::rename" not in token or "json.bak" not in token:
        problems.append("token file index must preserve/replace the previous index safely (backup + rename)")
    if "#[cfg(test)]" not in token or "TokenFileIndex::default()" not in token:
        problems.append("token index tests must not touch the real user store")

    if not CONTRIBUTING.is_file() or "跨平台" not in read(CONTRIBUTING):
        problems.append("CONTRIBUTING.md missing cross-platform rules")

    if problems:
        print("portability gate failed:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print("portability gate OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
