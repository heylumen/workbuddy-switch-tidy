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

# 两种调用模式：
#   A) 发布前（本地有产物）：check-release-assets.py <tag> <本地exe> <本地latest.json>
#      逐个核对线上 name / label / size 与本地一致；
#   B) 发版后验收（只有线上文件）：check-release-assets.py --structural --tag <tag> --dir <目录>
#      按 tag 推导应有形状，不依赖本地产物。
REPO = "heylumen/workbuddy-switch-tidy"


def structural_check(tag: str, directory: Path) -> tuple[list[str], dict]:
    """结构校验：不依赖本地构建产物，只看「一个版本应当发什么」。

    为什么需要：发版后验收时我们只能拿到**线上**资产，若把下载下来的文件当作
    期望值去比对，name/label/size 三项都会恒真（等于没检查）。此模式改为按 tag
    推导应有的资产结构。
    """
    import re as _re

    version = tag[1:] if tag.startswith("v") else tag
    problems: list[str] = []
    facts: dict = {"tag": tag, "version": version, "mode": "structural"}
    files = sorted(p for p in directory.iterdir() if p.is_file())
    exes = [p for p in files if p.suffix.lower() == ".exe"]
    facts["files"] = [p.name for p in files]

    pattern = _re.compile(r"^workbuddy-switch-tidy_(\d+\.\d+\.\d+)\.exe$")
    if len(exes) != 1:
        problems.append(f"应当恰好 1 个 .exe，实际 {len(exes)} 个：{[p.name for p in exes]}")
    for path in exes:
        match = pattern.match(path.name)
        if not match:
            problems.append(f"EXE 命名不符合 workbuddy-switch-tidy_<版本>.exe：{path.name}")
        elif match.group(1) != version:
            problems.append(f"EXE 版本 {match.group(1)} 与 tag {version} 不一致")
        size = path.stat().st_size
        facts["exe_bytes"] = size
        if size < 20 * 1024 * 1024:
            problems.append(f"EXE 体积 {size} 字节偏小，疑似构建残次")

    manifest = directory / "latest.json"
    if not manifest.is_file():
        problems.append("缺少 latest.json")
    else:
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            problems.append(f"latest.json 不是合法 JSON：{error}")
            data = {}
        facts["manifest_version"] = data.get("version")
        if str(data.get("version", "")) != version:
            problems.append(f"latest.json version={data.get('version')!r} 与 tag {version} 不一致")

    forbidden = [p.name for p in files if p.suffix.lower() in {".sig", ".msi", ".dmg", ".deb", ".appimage"}]
    if forbidden:
        problems.append(f"出现不应发布的资产：{forbidden}")
    return problems, facts


def main() -> int:
    # --structural --tag vX.Y.Z --dir <目录>：发版后验收用（只看结构，不比对本地文件）
    if "--structural" in sys.argv:
        import argparse as _argparse

        parser = _argparse.ArgumentParser(description="发布资产结构校验")
        parser.add_argument("--structural", action="store_true")
        parser.add_argument("--tag", required=True)
        parser.add_argument("--dir", default=".")
        parser.add_argument("--json", action="store_true")
        args = parser.parse_args()
        problems, facts = structural_check(args.tag, Path(args.dir))
        if args.json:
            print(json.dumps({"ok": not problems, "problems": problems, "facts": facts},
                             ensure_ascii=False, indent=2))
        elif problems:
            print("❌ 发布资产结构不合规：")
            for item in problems:
                print(" -", item)
        else:
            print(f"✅ 发布资产结构合规：{facts.get('exe_bytes', 0):,} B + latest.json v{facts['version']}")
        return 1 if problems else 0

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
