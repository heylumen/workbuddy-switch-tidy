#!/usr/bin/env python3
"""契约门禁：前端调用的 Tauri 命令 / HTTP 接口必须真实存在。

为什么需要：Tauri 里命令没注册、或 HTTP 路由改名后前端没跟上，这类问题
`tsc` 查不出（字符串）、Rust 编译也查不出（两侧独立），只有在真机上点一下才暴露。
2026-11-22 引入本门禁，把「字符串契约」变成可机械校验的对象。

检查四项：
1. 前端 call("x") 调用的命令必须已注册在 generate_handler![...]；
2. 注册的命令必须有对应的 #[tauri::command] 函数（防拼错/漏属性）；
3. 声明但未注册的命令只告警（可能是死代码）；
4. 前端 /api/... 字面量必须能在 server 的 .route(...) 里找到（动态段归一化后比较）。

用法：python scripts/check-command-routes.py [--quiet]
退出码：0 通过；1 有硬性不一致。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CMD_DECL = re.compile(r"#\[tauri::command[^\]]*\]\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)")
HANDLER_BLOCK = re.compile(r"generate_handler!\s*\[(.*?)\]", re.S)
FRONTEND_CALL = re.compile(r"""call(?:<[^>]*>)?\(\s*["']([A-Za-z0-9_]+)["']""")
SERVER_ROUTE = re.compile(r"""\.route\(\s*["']([^"']+)["']""")
FRONTEND_API = re.compile(r"""["'`](/api/[^"'`\s]*)["'`]""")
DYNAMIC = re.compile(r"\{[^}]*\}|\$\{[^}]*\}")

# WebUI 专用：这些命令只存在于 HTTP 服务端（桌面端走事件或无此功能），
# 前端调用点都有 isWebui() 守卫，因此不计入「未注册命令」。
WEBUI_ONLY_COMMANDS = {"switch_progress"}


def rust_files(*parts: str) -> list[Path]:
    base = ROOT.joinpath(*parts)
    return sorted(p for p in base.rglob("*.rs") if "target" not in p.parts)


def normalize_path(path: str) -> str:
    """把 {id} / ${id} 之类的动态段统一成 {}，并去掉 query。"""
    path = path.split("?")[0]
    path = DYNAMIC.sub("{}", path)
    return path.rstrip("/") or "/"


def collect_commands() -> tuple[set[str], set[str], set[str]]:
    registered: set[str] = set()
    declared: set[str] = set()

    lib = ROOT / "src-tauri" / "src" / "lib.rs"
    if lib.is_file():
        text = lib.read_text(encoding="utf-8")
        for block in HANDLER_BLOCK.findall(text):
            for raw in block.split(","):
                name = raw.strip().split("::")[-1].strip()
                if name and re.fullmatch(r"[A-Za-z0-9_]+", name):
                    registered.add(name)

    for path in rust_files("src-tauri", "src"):
        for name in CMD_DECL.findall(path.read_text(encoding="utf-8")):
            declared.add(name)

    called: set[str] = set()
    for path in sorted(ROOT.joinpath("src").rglob("*.ts")):
        called |= set(FRONTEND_CALL.findall(path.read_text(encoding="utf-8")))
    for path in sorted(ROOT.joinpath("src").rglob("*.tsx")):
        called |= set(FRONTEND_CALL.findall(path.read_text(encoding="utf-8")))

    return registered, declared, called


def collect_routes() -> tuple[set[str], set[str]]:
    routes: set[str] = set()
    api_file = ROOT / "crates" / "wb-switch-server" / "src" / "api.rs"
    if api_file.is_file():
        for literal in SERVER_ROUTE.findall(api_file.read_text(encoding="utf-8")):
            routes.add(normalize_path(literal))

    fetched: set[str] = set()
    for pattern in ("*.ts", "*.tsx"):
        for path in sorted(ROOT.joinpath("src").rglob(pattern)):
            for literal in FRONTEND_API.findall(path.read_text(encoding="utf-8")):
                fetched.add(normalize_path(literal))
    return routes, fetched


def main() -> int:
    parser = argparse.ArgumentParser(description="命令/路由契约校验")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()

    registered, declared, called = collect_commands()
    routes, fetched = collect_routes()

    problems: list[str] = []
    warnings: list[str] = []

    missing_cmd = sorted(called - registered - WEBUI_ONLY_COMMANDS)
    if missing_cmd:
        problems.append(
            "前端调用了未注册的 Tauri 命令："
            + ", ".join(missing_cmd)
            + "（要么在 generate_handler![] 里补上，要么前端改名）"
        )

    untyped = sorted(registered - declared)
    if untyped:
        problems.append(
            "注册了但没有 #[tauri::command] 函数（拼错或漏属性）：" + ", ".join(untyped)
        )

    dead = sorted(declared - registered)
    if dead:
        warnings.append("声明了但未注册（可能是死代码）：" + ", ".join(dead))

    unused_called = sorted(registered - called)
    if unused_called and not args.quiet:
        warnings.append("已注册但前端未见调用：" + ", ".join(unused_called))

    missing_route = sorted(fetched - routes)
    if missing_route:
        problems.append(
            "前端请求了服务端不存在的接口：" + ", ".join(missing_route)
        )

    print(
        f"命令：注册 {len(registered)} / 声明 {len(declared)} / 前端调用 {len(called)}；"
        f"路由：{len(routes)} 条 / 前端引用 {len(fetched)} 个"
    )
    for item in warnings:
        print(f"  ⚠️  {item}")
    if problems:
        print("❌ 契约不一致：", file=sys.stderr)
        for item in problems:
            print(f"   - {item}", file=sys.stderr)
        return 1
    print("✅ 命令与路由契约一致")
    return 0


if __name__ == "__main__":
    sys.exit(main())
