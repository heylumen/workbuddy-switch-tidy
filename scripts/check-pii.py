#!/usr/bin/env python
"""发布前 PII 检查：更新日志 / README 等公开文本不得包含个人身份信息。

为什么需要：更新日志会原样成为 GitHub Release 正文（对全网可见）。
曾把用户截图里的真实账号 ID 前缀写进草稿，靠人工发现才避免泄露。

用法：
    python scripts/check-pii.py <文件> [文件...]
命中即退出码 1，可接入发布流程做闸门。
"""
import io
import re
import sys

PATTERNS = {
    "疑似截断账号 ID（8 位十六进制）": r"\b[0-9a-f]{8}\b",
    "UUID": r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}",
    "邮箱": r"[\w.+-]+@[\w-]+\.[a-z]{2,}",
    "Windows 用户路径": r"[A-Za-z]:\\{1,2}Users\\{1,2}[^\\\s]+",
    "手机号": r"\b1[3-9]\d{9}\b",
}
# 白名单：常见非 PII 的十六进制串（版本号、颜色、占位符、hash 前缀示例）
ALLOW = {
    "0xdeadbe", "cafebabe", "deadbeef", "xxxxxxxx", "12345678",
}


def scan(path: str) -> list[str]:
    text = io.open(path, encoding="utf-8", errors="ignore").read()
    problems = []
    for label, pattern in PATTERNS.items():
        for m in re.finditer(pattern, text):
            value = m.group(0)
            if value.lower() in ALLOW or value.isdigit():
                continue
            if label.startswith("疑似截断") and not re.search(r"[a-f]", value):
                continue
            line = text[: m.start()].count("\n") + 1
            problems.append(f"{path}:{line} [{label}] {value}")
    return problems


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    all_problems = []
    for path in sys.argv[1:]:
        all_problems.extend(scan(path))
    if all_problems:
        print("发现疑似隐私信息（发布前必须处理）：")
        for item in all_problems:
            print("  -", item)
        return 1
    print("PII 检查通过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
