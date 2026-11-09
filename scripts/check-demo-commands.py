#!/usr/bin/env python3
"""Ensure every demo read command has a screenshot-demo switch branch.

The demo/screenshot layer is easy to break when a new read command is added to
DEMO_READ_COMMANDS but its switch branch is forgotten; that throws
"演示模式缺少只读数据" only when a user opens the corresponding page.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
API = ROOT / "src/lib/api.ts"
DEMO = ROOT / "src/lib/screenshot-demo.ts"


def main() -> int:
    api = API.read_text(encoding="utf-8")
    demo = DEMO.read_text(encoding="utf-8")
    match = re.search(r"const DEMO_READ_COMMANDS = new Set\(\[(.*?)\]\);", api, re.S)
    if not match:
        print("cannot find DEMO_READ_COMMANDS", file=sys.stderr)
        return 1
    commands = set(re.findall(r'"([^"]+)"', match.group(1)))
    cases = set(re.findall(r'case "([^"]+)"', demo))
    missing = sorted(commands - cases)
    if missing:
        print("demo commands missing switch branch:", file=sys.stderr)
        for command in missing:
            print(f"  - {command}", file=sys.stderr)
        return 1
    print(f"demo command coverage OK ({len(commands)} commands)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
