#!/usr/bin/env python3
"""重生成 README 用的界面截图（演示模式数据，天然不含真实账号信息）。

背景：`docs/images/*.png` 是手工截的，功能一多就过期；演示模式（VITE_DEMO_MODE=1）
自带"演示模式"角标与合成账号（测试 A/B/C），不会泄露真实账号 / 邮箱 / 路径。

做法：起一个 vite（演示模式）→ 用系统浏览器 + CDP 逐路由截图。
不引新依赖，复用 scripts/smoke-webui.py 里的 MiniWs 客户端。

用法：
    python scripts/capture-docs-screenshots.py          # 生成 docs/images 下的 5 张
    python scripts/capture-docs-screenshots.py --port 5199
前置：需先 npm ci；脚本会自己拉起 vite 并在结束后关闭。
"""

from __future__ import annotations

import argparse
import base64
import importlib.util
import json
import os
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "docs" / "images"

# 路由 → 输出文件名（与 README 里的引用保持一致）
# (路由, 输出文件名, 点击目标文字 —— 为 None 表示直接截列表页)
# 演示数据里的会话标题是固定的合成标题，点开它即可截到详情/分支图。
PAGES = [
    ("/", "accounts-overview.png", None),
    ("/sessions", "session-links.png", None),
    ("/sessions", "session-links-branches.png", "整理长时间未完成的任务"),
    ("/credit-stats", "credit-statistics.png", None),
    ("/token-stats", "token-statistics.png", None),
]
VIEWPORT = (1080, 760)
SETTLE_SECONDS = 3.0


def load_ws_client():
    spec = importlib.util.spec_from_file_location("smoke", ROOT / "scripts" / "smoke-webui.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def wait_port(port: int, timeout: float = 90.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.6):
                return True
        except OSError:
            time.sleep(0.4)
    return False


def wait_cdp(port: int, timeout: float = 40.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1.5):
                return True
        except Exception:
            time.sleep(0.4)
    return False


def cdp_call(ws, message_id: int, method: str, params: dict):
    ws.send_text(json.dumps({"id": message_id, "method": method, "params": params}))
    for _ in range(400):
        raw = ws.recv_text()
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if message.get("id") == message_id:
            if "error" in message:
                raise RuntimeError(f"{method} 失败: {message['error']}")
            return message.get("result", {})
    raise RuntimeError(f"{method} 超时")


def main() -> int:
    parser = argparse.ArgumentParser(description="生成 README 界面截图（演示模式）")
    parser.add_argument("--port", type=int, default=5199, help="vite 端口")
    parser.add_argument("--debug-port", type=int, default=9333, help="浏览器 CDP 端口")
    args = parser.parse_args()

    smoke = load_ws_client()
    browser = smoke.find_browser()
    if not browser:
        print("❌ 未找到浏览器（Edge/Chrome），无法截图", file=sys.stderr)
        return 1

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    node = os.environ.get("WB_NODE") or "node"
    vite = subprocess.Popen(
        [node, str(ROOT / "node_modules" / "vite" / "bin" / "vite.js"),
         "--port", str(args.port), "--strictPort", "--host", "127.0.0.1"],
        cwd=str(ROOT),
        env={**os.environ, "VITE_DEMO_MODE": "1"},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    profile = os.path.join(os.environ.get("TEMP", "/tmp"), f"wb-shots-{os.getpid()}")
    chrome = subprocess.Popen(
        [browser, "--headless=new", "--disable-gpu", "--no-sandbox", "--no-first-run",
         "--hide-scrollbars", f"--remote-debugging-port={args.debug_port}",
         f"--user-data-dir={profile}", "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    ws = None
    try:
        if not wait_port(args.port):
            print("❌ vite 未就绪", file=sys.stderr)
            return 1
        if not wait_cdp(args.debug_port):
            print("❌ 浏览器 CDP 未就绪", file=sys.stderr)
            return 1
        base = f"http://127.0.0.1:{args.port}"
        urllib.request.urlopen(
            urllib.request.Request(
                f"http://127.0.0.1:{args.debug_port}/json/new?{base}{PAGES[0][0]}", method="PUT"
            ),
            timeout=10,
        ).read()
        time.sleep(1.0)
        targets = json.loads(
            urllib.request.urlopen(f"http://127.0.0.1:{args.debug_port}/json/list", timeout=10).read()
        )
        page = next((t for t in targets if t.get("type") == "page"), None)
        if not page:
            print("❌ 未取到页面目标", file=sys.stderr)
            return 1
        ws = smoke.MiniWs(page["webSocketDebuggerUrl"], timeout=60)
        cdp_call(ws, 1, "Page.enable", {})
        cdp_call(ws, 2, "Emulation.setDeviceMetricsOverride", {
            "width": VIEWPORT[0], "height": VIEWPORT[1],
            "deviceScaleFactor": 2, "mobile": False,
        })

        message_id = 10
        for route, filename, click_text in PAGES:
            message_id += 1
            cdp_call(ws, message_id, "Page.navigate", {"url": f"{base}{route}"})
            time.sleep(SETTLE_SECONDS)
            if click_text:
                # 点开指定标题的会话卡片/链接（演示数据标题是合成的，稳定可点）
                message_id += 1
                cdp_call(ws, message_id, "Runtime.evaluate", {
                    "expression": (
                        "(() => {"
                        f"  const target = {click_text!r};"
                        "  const nodes = Array.from(document.querySelectorAll('button,a,div,h3,span'));"
                        "  const hit = nodes.filter((el) => el.textContent && el.textContent.trim() === target && el.children.length === 0).pop();"
                        "  if (!hit) { return 'miss'; }"
                        "  (hit.closest('button,a,div[role=button]') || hit).click();"
                        "  return 'clicked';"
                        "})()"
                    ),
                    "returnByValue": True,
                })
                time.sleep(2.5)
            message_id += 1
            result = cdp_call(ws, message_id, "Page.captureScreenshot", {"format": "png"})
            data = base64.b64decode(result["data"])
            path = OUT_DIR / filename
            path.write_bytes(data)
            print(f"✅ {filename}  {len(data) / 1024:.0f} KB  ({route})")
        return 0
    finally:
        if ws:
            ws.close()
        for proc in (chrome, vite):
            proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()


if __name__ == "__main__":
    sys.exit(main())
