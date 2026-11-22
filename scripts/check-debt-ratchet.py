#!/usr/bin/env python3
"""债务棘轮：已降下来的指标不许再涨。

为什么用棘轮而不是一次性大清理：本项目存量吞错/超长文件是历史积累，
一次性清完风险高、收益低；但**回涨**完全可以拦住（每涨一处都是新欠的债）。

指标（默认只统计生产代码，测试文件不计）：
1. `let _ = ` 数量：静默丢弃结果的典型写法；
2. 生产代码 `.unwrap()` 数量：硬性要求 0（panic 会打断用户操作）；
3. 单文件最大行数：防止又长出一个 4k 行的巨石模块；
4. `TODO` / `FIXME` 标记数量。

用法：
    python scripts/check-debt-ratchet.py            # 与基线比较，超标即失败
    python scripts/check-debt-ratchet.py --update   # 指标下降后刷新基线
退出码：0 通过；1 有指标回涨或硬性规则不满足。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "scripts" / "debt-baseline.json"
PROD_ROOTS = [ROOT / "crates", ROOT / "src-tauri" / "src", ROOT / "src"]
CODE_SUFFIXES = {".rs", ".ts", ".tsx"}
TEST_MARKERS = ("tests.rs", "/tests/", ".test.", ".spec.", "test_")


def is_test(path: Path) -> bool:
    text = path.as_posix()
    return any(marker in text for marker in TEST_MARKERS)


def prod_lines(text: str) -> list[str]:
    """去掉 #[cfg(test)] 块的每一行（按花括号配对，而不是从首次出现处切到底）。

    本仓库的单元测试既有同文件末尾的 mod tests，也有文件靠前的小测试模块
    （session.rs 第 77 行就有一个）——从首次出现处切会漏统计后面几千行生产代码。
    """
    lines = text.split("\n")
    skip = [False] * len(lines)
    for index, line in enumerate(lines):
        if line.strip() != "#[cfg(test)]":
            continue
        depth = 0
        started = False
        for cursor in range(index, len(lines)):
            for char in lines[cursor]:
                if char == "{":
                    depth += 1
                    started = True
                elif char == "}":
                    depth -= 1
            skip[cursor] = True
            if started and depth <= 0:
                break
    return [line for line, dropped in zip(lines, skip) if not dropped]


def iter_files():
    for base in PROD_ROOTS:
        if not base.exists():
            continue
        for path in base.rglob("*"):
            if path.suffix in CODE_SUFFIXES and path.is_file() and "node_modules" not in path.parts:
                yield path


def collect() -> tuple[dict[str, int], dict[str, str]]:
    silent = 0
    unwrap = 0
    todos = 0
    largest = (0, "")
    for path in iter_files():
        raw = path.read_text(encoding="utf-8", errors="ignore")
        # 静默丢弃/ unwrap 只统计生产代码：#[cfg(test)] 块整体跳过（花括号配对）。
        code = "\n".join(prod_lines(raw)) if path.suffix == ".rs" else raw
        if not is_test(path):
            # 注释里提到代码写法不算债务（否则文档会被误判）。
            code_only = "\n".join(
                line
                for line in code.split("\n")
                if not line.lstrip().startswith(("//", "///", "*"))
            )
            silent += len(re.findall(r"let _ = ", code_only))
            unwrap += len(re.findall(r"\.unwrap\(\)", code_only))
            # 行数用文件全文：文件多大就是多大，测试也占维护成本。
            lines = raw.count("\n") + 1
            if lines > largest[0]:
                largest = (lines, path.relative_to(ROOT).as_posix())
        todos += len(re.findall(r"\bTODO\b|\bFIXME\b", code))
    return (
        {
            "silent_discard_let_underscore": silent,
            "prod_unwrap": unwrap,
            "largest_prod_file_lines": largest[0],
            "todo_markers": todos,
        },
        {"largest_file": largest[1]},
    )


def load_baseline() -> dict:
    if not BASELINE.is_file():
        return {}
    return json.loads(BASELINE.read_text(encoding="utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser(description="债务棘轮")
    parser.add_argument("--update", action="store_true", help="把当前值写成新基线")
    args = parser.parse_args()

    metrics, extra = collect()
    baseline = load_baseline()
    base_metrics = baseline.get("metrics", {})

    print("指标（生产代码）:")
    for key, value in metrics.items():
        old = base_metrics.get(key)
        delta = "" if old is None else f"（基线 {old}，{value - old:+d}）"
        print(f"  {key}: {value}{delta}")
    print(f"  最大文件: {extra['largest_file']}")

    if args.update:
        payload = {
            "version": 1,
            "note": "指标只能下降；新增债务请先把基线调高的评估写进提交信息。",
            "metrics": metrics,
            "largestFile": extra["largest_file"],
        }
        BASELINE.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(f"✅ 基线已更新：{BASELINE.relative_to(ROOT)}")
        return 0

    problems: list[str] = []
    if not base_metrics:
        problems.append("缺少基线文件；先跑 --update 生成")
    else:
        for key, value in metrics.items():
            old = base_metrics.get(key)
            if old is not None and value > old:
                problems.append(f"{key} 从 {old} 涨到 {value}（棘轮只允许下降）")
    # 硬性规则：生产代码不允许 unwrap
    if metrics["prod_unwrap"] > 0:
        problems.append(f"生产代码出现 {metrics['prod_unwrap']} 处 .unwrap()（要求 0）")

    if problems:
        print("❌ 债务棘轮未通过：", file=sys.stderr)
        for item in problems:
            print(f"   - {item}", file=sys.stderr)
        return 1
    print("✅ 债务棘轮通过（无回涨）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
