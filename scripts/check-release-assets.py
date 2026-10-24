#!/usr/bin/env python
"""发布后资产显示名/大小校验（机械闸门）。

背景（2026-10-24 复发）：用户早在 2026-08-30 就强调「上传 releases 注意命名和
版本结合」「不需要带 X64」；发布手册也要求资产名 ASCII、label 只能放原名。
v1.0.12 首次上传把 label 写成「Windows x64 便携版 / 更新清单」，GitHub 页面
优先显示 label，导致 x64 重新出现、版本号文件名被隐藏。

规则：
1. Release 中**上传资产**的 name 必须与本地文件名完全一致（ASCII、含版本）；
2. label 必须为空或与 name 完全一致；禁止中文描述、禁止额外 x64 字样；
3. 本地文件大小必须与线上一致。

用法：
    GITHUB_TOKEN=... python scripts/check-release-assets.py v1.0.12 \
        releases/workbuddy-switch-tidy_1.0.12.exe releases/latest.json

不带 --check 时同样只校验并以退出码表示结果（可直接放 CI/发布脚本）。
"""
import json
import os
import sys
import urllib.request
from pathlib import Path

REPO = "heylumen/workbuddy-switch-tidy"


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    tag = sys.argv[1].strip()
    expected = {Path(p).name: Path(p) for p in sys.argv[2:]}
    token = os.environ.get("GITHUB_TOKEN", "").strip()
    if not token:
        print("缺少 GITHUB_TOKEN")
        return 2

    req = urllib.request.Request(
        f"https://api.github.com/repos/{REPO}/releases/tags/{tag}",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "User-Agent": "wb-switch-release-check",
        },
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        release = json.load(resp)

    assets = release.get("assets", [])
    actual = {a["name"]: a for a in assets}
    problems = []

    # 上传资产必须与 expected 一一对应；GitHub 自动生成的 Source code 不在
    # assets API 里，因此这里只比较上传资产。
    if set(actual) != set(expected):
        problems.append(
            f"资产集合不一致：线上={sorted(actual)} 期望={sorted(expected)}"
        )

    for name, local in expected.items():
        asset = actual.get(name)
        if not asset:
            continue
        label = (asset.get("label") or "").strip()
        if label and label != name:
            problems.append(f"{name}: label 必须为空或等于 name，实际={label!r}")
        if "x64" in label.lower() or "x64" in asset["name"].lower():
            problems.append(f"{name}: 资产显示名不得带 x64（用户已明确要求）")
        if asset.get("size") != local.stat().st_size:
            problems.append(
                f"{name}: 大小不一致 线上={asset.get('size')} 本地={local.stat().st_size}"
            )
        if asset.get("state") != "uploaded":
            problems.append(f"{name}: 状态不是 uploaded，实际={asset.get('state')}")

    if problems:
        print("❌ Release 资产校验失败：")
        for p in problems:
            print(" -", p)
        return 1

    print(f"✅ Release {tag} 资产校验通过：{', '.join(sorted(expected))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
