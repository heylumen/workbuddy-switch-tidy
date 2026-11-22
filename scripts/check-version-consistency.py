#!/usr/bin/env python
"""版本一致性门禁：README / package.json / Cargo / Tauri 配置必须同版本。

用法：python scripts/check-version-consistency.py
退出码：0 一致；1 不一致或字段缺失；2 用法/解析错误。
"""
from __future__ import annotations

import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def package_version() -> str:
    return str(json.loads(read("package.json"))["version"])


def cargo_version(path: str) -> str:
    with (ROOT / path).open("rb") as handle:
        data = tomllib.load(handle)
    return str(data.get("package", {}).get("version", ""))


def tauri_version() -> str:
    return str(json.loads(read("src-tauri/tauri.conf.json"))["version"])


def readme_version() -> str:
    match = re.search(r"版本号独立于上游（当前 \*\*([0-9]+\.[0-9]+\.[0-9]+)\*\*）", read("README.md"))
    return match.group(1) if match else ""


def repository_slugs() -> dict[str, str]:
    """收集各处写死的 GitHub owner/repo，必须完全一致。

    背景：2026-11-22 发现 index.html 的内联兜底脚本仍指向上游仓库
    （changexbc/workbuddy-switch），而实际发版在 heylumen/workbuddy-switch-tidy。
    后果是启动失败页的「前往 GitHub 下载」把用户带到别人的仓库、且看不到新版本。
    这类「同一个地址在多个文件里各写一份」最容易漂移，因此纳入门禁。
    """
    slugs: dict[str, str] = {}

    front = read("src/lib/update.ts")
    owner = re.search(r'GITHUB_OWNER\s*=\s*"([^"]+)"', front)
    repo = re.search(r'GITHUB_REPO\s*=\s*"([^"]+)"', front)
    if owner and repo:
        slugs["src/lib/update.ts"] = f"{owner.group(1)}/{repo.group(1)}"

    rust = read("crates/wb-switch-core/src/modules/update.rs")
    owner = re.search(r'GITHUB_OWNER: &str = "([^"]+)"', rust)
    repo = re.search(r'GITHUB_REPO: &str = "([^"]+)"', rust)
    if owner and repo:
        slugs["crates/wb-switch-core/src/modules/update.rs"] = f"{owner.group(1)}/{repo.group(1)}"

    html = read("index.html")
    match = re.search(r"github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/releases", html)
    if match:
        slugs["index.html"] = match.group(1)

    conf = read("src-tauri/tauri.conf.json")
    match = re.search(r"github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/releases", conf)
    if match:
        slugs["src-tauri/tauri.conf.json"] = match.group(1)

    return slugs


def check_repository_slugs() -> int:
    slugs = repository_slugs()
    if len(slugs) < 2:
        print("仓库地址检查跳过（未找到足够样本）")
        return 0
    unique = sorted(set(slugs.values()))
    if len(unique) != 1:
        print("仓库地址不一致：")
        for path, value in slugs.items():
            print(f" - {path}: {value}")
        return 1
    print(f"仓库地址一致：{unique[0]}（{len(slugs)} 处）")
    return 0


def main() -> int:
    fields = {
        "package.json": package_version(),
        "crates/wb-switch-core/Cargo.toml": cargo_version("crates/wb-switch-core/Cargo.toml"),
        "crates/wb-switch-server/Cargo.toml": cargo_version("crates/wb-switch-server/Cargo.toml"),
        "src-tauri/Cargo.toml": cargo_version("src-tauri/Cargo.toml"),
        "src-tauri/tauri.conf.json": tauri_version(),
        "README.md": readme_version(),
    }
    missing = [path for path, value in fields.items() if not value]
    versions = sorted(set(fields.values()))
    if missing:
        print("版本字段缺失：")
        for path in missing:
            print(f" - {path}")
        return 1
    if len(versions) != 1:
        print("版本号不一致：")
        for path, value in fields.items():
            print(f" - {path}: {value}")
        return 1
    if check_repository_slugs() != 0:
        return 1
    print(f"版本一致性通过：{versions[0]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
