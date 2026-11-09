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
    print(f"版本一致性通过：{versions[0]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
