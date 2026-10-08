#!/usr/bin/env python
"""从版本更新日志文件提取「Release 正文」——发布流程的强制入口。

规则（2026-10-08 固化；用户已两次强调，勿再违反）：
1. **正文不得出现 `# vX.Y.Z 更新日志` 这类一级标题**：Release 页面与更新弹窗上方
   已经显示版本号，再带标题属重复冗余。正文应直接从 `## 修复` / `## 新功能` 等内容开始。
2. 只取 `# README 更新日志` 之前的部分（README 更新日志段不进入 Release 正文）。
3. 正文首行必须是二级标题或列表项（不得是 `#` 一级标题、不得是空）。

用法：
    python scripts/release-body.py <更新日志.md>            # 输出可用于 Release 的正文
    python scripts/release-body.py <更新日志.md> --check     # 仅校验（不合规退出码 1）
"""
import io
import re
import sys


def extract_body(text: str) -> str:
    """取正文：截断 README 段 + 去掉开头的一级标题。"""
    body = text.split("# README 更新日志", 1)[0]
    # 去掉形如 `# v1.0.10 更新日志` / `# 更新日志` 的开头标题（含其后空行）
    body = re.sub(r"^\s*#\s*v?\d+\.\d+\.\d*\s*更新日志\s*\n+", "", body, count=1)
    body = re.sub(r"^\s*#\s*更新日志\s*\n+", "", body, count=1)
    return body.strip()


def first_content_line(body: str) -> str:
    for line in body.split("\n"):
        if line.strip():
            return line.strip()
    return ""


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    path = sys.argv[1]
    check_only = "--check" in sys.argv[2:]
    text = io.open(path, encoding="utf-8").read()
    body = extract_body(text)
    first = first_content_line(body)

    problems = []
    if first.startswith("# "):
        problems.append("正文首行是一级标题（应直接从 ## 内容开始）")
    if not first:
        problems.append("正文为空")

    if check_only:
        print(f"首行: {first}")
        for p in problems:
            print(f"❌ {p}")
        return 1 if problems else 0

    if problems:
        print("⚠️ " + "；".join(problems), file=sys.stderr)
    print(body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
