#!/usr/bin/env python3
"""前端运行时冒烟：构建产物必须在**真实浏览器**里挂载成功（断言 window.__WB_MOUNTED__）。

为什么需要：`vite build` 成功只证明「能构建」，不证明「能运行」。
2026-11-22 的事故正是如此——分包拆出空壳 chunk 后产物照常构建成功，但入口模块求值失败，
界面 10 秒后显示「应用启动失败」，而当时全部门禁（单测 / clippy / tsc / vite build /
6 个质量脚本）都是绿的。

实现（零新依赖）：启动系统浏览器的 headless 模式并开启 DevTools 协议（CDP），
用标准库实现最小 WebSocket 客户端执行 Runtime.evaluate：
  1. 轮询 `window.__WB_MOUNTED__ === true`（最多 --timeout 秒）；
  2. 顺带收集 `window.__WB_BOOT_ERRORS__` / 兜底文案，失败时打印原因。

用法：
    python scripts/smoke-webui.py                       # 校验 dist/
    python scripts/smoke-webui.py --dist dist
    python scripts/smoke-webui.py --exe target/release/wb-switch.exe   # 校验内嵌产物
    python scripts/smoke-webui.py --require-browser      # CI：没有浏览器即失败
环境变量：
    WB_SMOKE_BROWSER  指定浏览器可执行文件
"""

from __future__ import annotations

import argparse
import base64
import functools
import http.server
import json
import os
import platform
import re
import shutil
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse

MOUNT_EXPR = "window.__WB_MOUNTED__ === true"
DIAG_EXPR = """JSON.stringify({
  mounted: window.__WB_MOUNTED__ === true,
  bootErrors: (window.__WB_BOOT_ERRORS__ || []).slice(0, 5),
  title: document.title,
  text: (document.body ? document.body.innerText : '').slice(0, 400),
  readyState: document.readyState,
  resources: (performance.getEntriesByType('resource') || [])
    .filter(function (r) { return r.name.indexOf('/assets/') >= 0 && r.transferSize === 0 && r.decodedBodySize === 0; })
    .map(function (r) { return r.name; })
    .slice(0, 5)
})"""


def browser_candidates() -> list[str]:
    env = os.environ.get("WB_SMOKE_BROWSER")
    if env:
        return [env]
    system = platform.system()
    if system == "Windows":
        return [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    if system == "Darwin":
        return [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    return ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]


def find_browser() -> str | None:
    for candidate in browser_candidates():
        if os.path.isabs(candidate):
            if os.path.isfile(candidate):
                return candidate
        else:
            found = shutil.which(candidate)
            if found:
                return found
    return None


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass


def serve_directory(directory: Path) -> tuple[socketserver.TCPServer, int]:
    handler = functools.partial(QuietHandler, directory=str(directory))
    server = socketserver.TCPServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def http_json(url: str, method: str = "GET", timeout: float = 5.0):
    request = urllib.request.Request(url, method=method)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        body = response.read()
    return json.loads(body or b"{}")


def wait_cdp(port: int, timeout: float = 30.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            http_json(f"http://127.0.0.1:{port}/json/version", timeout=1.5)
            return True
        except Exception:
            time.sleep(0.3)
    return False


class MiniWs:
    """最小 WebSocket 客户端：够用即可（文本帧、单次请求/响应）。"""

    def __init__(self, ws_url: str, timeout: float = 30.0):
        parsed = urlparse(ws_url)
        host = parsed.hostname or "127.0.0.1"
        port = parsed.port or 80
        path = parsed.path + (f"?{parsed.query}" if parsed.query else "")
        self.sock = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("CDP WebSocket 握手失败")
            response += chunk
        if b"101" not in response.split(b"\r\n", 1)[0]:
            raise RuntimeError(f"CDP 握手未升级: {response[:80]!r}")

    def send_text(self, payload: str) -> None:
        data = payload.encode()
        header = bytearray([0x81])
        length = len(data)
        if length < 126:
            header.append(0x80 | length)
        elif length < (1 << 16):
            header.append(0x80 | 126)
            header += struct.pack(">H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", length)
        mask = os.urandom(4)
        header += mask
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(data))
        self.sock.sendall(bytes(header) + masked)

    def recv_text(self) -> str:
        def read_exact(count: int) -> bytes:
            buffer = b""
            while len(buffer) < count:
                chunk = self.sock.recv(count - len(buffer))
                if not chunk:
                    raise RuntimeError("CDP 连接已断开")
                buffer += chunk
            return buffer

        first = read_exact(2)
        opcode = first[0] & 0x0F
        masked = bool(first[1] & 0x80)
        length = first[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", read_exact(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", read_exact(8))[0]
        mask = read_exact(4) if masked else b""
        payload = read_exact(length)
        if masked:
            payload = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        if opcode == 0x8:
            raise RuntimeError("CDP 连接被关闭")
        return payload.decode("utf-8", "ignore")

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def evaluate(ws: MiniWs, message_id: int, expression: str):
    ws.send_text(json.dumps({
        "id": message_id,
        "method": "Runtime.evaluate",
        "params": {"expression": expression, "returnByValue": True, "awaitPromise": True},
    }))
    deadline_guard = 0
    while True:
        raw = ws.recv_text()
        deadline_guard += 1
        if deadline_guard > 200:
            raise RuntimeError("CDP 响应超时")
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if message.get("id") == message_id:
            return message.get("result", {}).get("result", {}).get("value")


def check_url(url: str, browser: str, timeout: float) -> int:
    port = free_port()
    profile = tempfile.mkdtemp(prefix="wb-smoke-profile-")
    proc = subprocess.Popen(
        [
            browser,
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--no-first-run",
            "--disable-extensions",
            f"--remote-debugging-port={port}",
            f"--user-data-dir={profile}",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    ws = None
    try:
        if not wait_cdp(port):
            print("❌ 浏览器 DevTools 端口未就绪", file=sys.stderr)
            return 1
        try:
            http_json(f"http://127.0.0.1:{port}/json/new?{url}", method="PUT")
        except Exception:
            http_json(f"http://127.0.0.1:{port}/json/new?{url}")
        deadline = time.time() + 10
        ws_url = None
        while time.time() < deadline and not ws_url:
            for target in http_json(f"http://127.0.0.1:{port}/json/list") or []:
                if target.get("type") == "page" and url.split("?")[0] in (target.get("url") or ""):
                    ws_url = target.get("webSocketDebuggerUrl")
                    break
            if not ws_url:
                time.sleep(0.3)
        if not ws_url:
            print("❌ 未获取到目标页的 CDP 地址", file=sys.stderr)
            return 1
        ws = MiniWs(ws_url, timeout=timeout)
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                if evaluate(ws, 1, MOUNT_EXPR) is True:
                    print("✅ 前端已挂载（window.__WB_MOUNTED__ === true）")
                    return 0
            except Exception:
                pass
            time.sleep(0.5)
        try:
            diagnosis = evaluate(ws, 2, DIAG_EXPR)
        except Exception as error:
            diagnosis = f"（诊断失败：{error}）"
        print("❌ 前端在超时前未挂载", file=sys.stderr)
        print(f"   诊断: {diagnosis}", file=sys.stderr)
        return 1
    finally:
        if ws:
            ws.close()
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()


def run_static(dist: Path, browser: str, timeout: float) -> int:
    server, port = serve_directory(dist)
    try:
        url = f"http://127.0.0.1:{port}/"
        print(f"静态服务 {dist} → {url}")
        return check_url(url, browser, timeout)
    finally:
        server.shutdown()


def run_exe(binary: Path, browser: str, timeout: float) -> int:
    log = Path(tempfile.gettempdir()) / "wb-smoke-exe.log"
    handle = open(log, "w", encoding="utf-8")
    proc = subprocess.Popen([str(binary)], cwd=str(binary.parent), stdout=handle,
                            stderr=subprocess.STDOUT)
    try:
        deadline = time.time() + 45
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", 57890), timeout=0.6):
                    break
            except OSError:
                time.sleep(0.4)
        token = None
        deadline = time.time() + 20
        while time.time() < deadline and not token:
            text = log.read_text(encoding="utf-8", errors="ignore")
            match = re.search(r"token=([A-Za-z0-9_\-]+)", text)
            token = match.group(1) if match else None
            time.sleep(0.4)
        url = "http://127.0.0.1:57890/"
        if token:
            url += f"?token={token}"
        print(f"内嵌产物 {binary.name}")
        return check_url(url, browser, timeout)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
        handle.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="前端运行时冒烟（真实浏览器 + CDP）")
    target = parser.add_mutually_exclusive_group()
    target.add_argument("--dist", type=Path, default=Path("dist"))
    target.add_argument(
        "--exe",
        type=Path,
        help="校验可执行文件内嵌的前端产物；需是启动 WebUI 的二进制（wb-switch.exe）。"
        " 桌面端窗口走 Tauri 内嵌协议、不监听 HTTP，本脚本无法覆盖。",
    )
    parser.add_argument("--timeout", type=float, default=60.0, help="等待挂载的最长秒数")
    parser.add_argument("--require-browser", action="store_true")
    args = parser.parse_args()

    browser = find_browser()
    if not browser:
        message = "未找到可用浏览器（Edge/Chrome/Chromium），已跳过运行时冒烟"
        if args.require_browser:
            print(f"❌ {message}", file=sys.stderr)
            return 1
        print(f"⚠️ {message}")
        return 0
    print(f"浏览器: {browser}")

    if args.exe:
        if not args.exe.is_file():
            print(f"❌ 可执行文件不存在: {args.exe}", file=sys.stderr)
            return 1
        return run_exe(args.exe, browser, args.timeout)
    if not (args.dist / "index.html").is_file():
        print(f"❌ 找不到 {args.dist / 'index.html'}，请先 npm run build", file=sys.stderr)
        return 1
    return run_static(args.dist, browser, args.timeout)


if __name__ == "__main__":
    sys.exit(main())
