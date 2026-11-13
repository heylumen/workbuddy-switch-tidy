#!/usr/bin/env python3
"""Ensure every OKLCH theme variable has a legacy HEX/RGBA fallback.

Modern browsers use the later `oklch()` declaration. Old WebKit ignores it and
falls back to the declarations inside `@supports not (color: oklch(0 0 0))`.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

PATH = Path("src/index.css")
SCOPE_RE = re.compile(r"^\s*(:root|\.dark)\s*\{")
OKLCH_RE = re.compile(r"^\s*(--[\w-]+):\s*oklch\(")
FALLBACK_RE = re.compile(
    r"^\s*(--[\w-]+):\s*(#[0-9a-fA-F]{6}|rgba\([^)]*\));\s*$"
)


def main() -> int:
    if not PATH.is_file():
        print(f"missing {PATH}", file=sys.stderr)
        return 1
    lines = PATH.read_text(encoding="utf-8").splitlines()
    theme_vars: set[str] = set()
    fallback_vars: set[str] = set()
    scope = ""
    in_supports = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("@supports not (color: oklch("):
            in_supports = True
            continue
        if in_supports and stripped == "}":
            in_supports = False
            continue
        match = SCOPE_RE.match(line)
        if match:
            scope = match.group(1)
            continue
        if scope in {":root", ".dark"}:
            ok = OKLCH_RE.match(line)
            if ok:
                theme_vars.add(ok.group(1))
            fb = FALLBACK_RE.match(line)
            if fb:
                fallback_vars.add(fb.group(1))
    # 回退块本身也以 :root/.dark 开头，上面的扫描会覆盖；取并集即可。
    missing = sorted(theme_vars - fallback_vars)
    if missing:
        print("missing legacy color fallbacks:", file=sys.stderr)
        for name in missing:
            print(f"  - {name}", file=sys.stderr)
        return 1
    print(f"legacy color fallbacks OK: {len(theme_vars)} variables")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
