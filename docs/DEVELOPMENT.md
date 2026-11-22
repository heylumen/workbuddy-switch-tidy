# 开发指南

> **本 fork 的形态**：`workbuddy-switch-tidy` 便携版，**只发单文件 EXE**（Windows）。
> 不做签名安装包、不做应用内下载安装；更新检查只检测版本并打开发布页。
> 上游安装版的签名密钥 / `.sig` / updater manifest 流程在本仓库**不使用**，相关脚本仅作历史保留。
>
> 当前验证基线（2026-11-22，1.0.17）：core 883 passed / 7 ignored；集成 5；server 15；
> 前端 9 个测试文件；Tauri crate 无单测。clippy `-D warnings`、`tsc`、`fmt`、`cargo audit`（0 漏洞）、
> 运行时冒烟全部通过；CodeQL open alerts 0。

## 环境要求

Node.js ≥ 20、Rust stable、macOS（或 Windows/Linux）。

## 开发命令

```bash
npm ci                      # 安装前端依赖（CI 同款）
npm run tauri dev           # 桌面开发模式
npm run tauri build         # 构建单文件 EXE（产物 target/release/wb-switch-rust.exe）

npm run typecheck           # tsc --noEmit
npm test                    # 前端单测（vitest）
npm run build               # tsc + vite build → dist/

cargo fmt --all -- --check
# 注意：本地必须带 -D warnings，否则 CI 会红而本地是绿的（2026-11-22 踩过）
cargo clippy -p wb-switch-core -p wb-switch-server -p wb-switch-rust \
  --all-targets --no-deps -- -D warnings
cargo test -p wb-switch-core --lib
cargo test -p wb-switch-core --tests
cargo test -p wb-switch-server
cargo audit                 # 依赖漏洞；要求 0 vulnerabilities
```

### 质量门禁（提交前建议全跑）

```bash
python scripts/check-version-consistency.py   # 版本号 6 处 + 仓库地址 4 处
python scripts/check-command-routes.py        # Tauri 命令 / 前端调用 / HTTP 路由三方一致
python scripts/check-debt-ratchet.py          # 静默丢弃 / 生产 unwrap / 最大文件行数 只减不增
python scripts/check-pii.py README.md docs/*.md
python scripts/check-repair-ledger.py docs/repair-ledger-20261122.md
python scripts/check-portability.py
python scripts/check-demo-commands.py
python scripts/check-legacy-colors.py
```

### 前端运行时冒烟（必做）

`vite build` 成功只说明「能构建」，不代表「能运行」。2026-11-22 就因为一次分包改动
（`manualChunks`）导致产物构建成功、但入口模块求值失败，桌面端直接白屏，
而当时单测 / clippy / tsc / build / 质量脚本全绿。

交付前必须跑：

```bash
npm run build            # tsc + vite build
npm run smoke            # 真实浏览器断言 window.__WB_MOUNTED__ === true
npm run smoke:exe        # 可选：直接校验可执行文件内嵌的前端产物
```

冒烟脚本（`scripts/smoke-webui.py`）用系统浏览器 + CDP，不引入新依赖；失败时会打印
资源 404 / 脚本异常详情。CI 的 `quality-gates` 已包含该步骤。

**桌面窗口无法脚本化验证**：Tauri 窗口走内嵌协议、不监听 HTTP，且 Tauri 屏蔽了 WebView2
的远程调试端口；`PrintWindow` 抓 WebView2 只能拿到空白图（实测 16×16）。所以桌面端
**每次交付前必须人工打开一次**（本机与 CI 都做不到自动判定）。好在这类失败会由下面的
「错误详情」直接说明原因，不再是黑盒白屏。

启动失败时，兜底页会显示「错误详情（可截图反馈）」，内容来自 `window.__WB_BOOT_ERRORS__`
（资源加载失败 / 脚本错误 / 未处理拒绝），可直接用于定位。

## 发布新版本（单文件 EXE）

1. `bash scripts/bump-version.sh <版本>`：同步 package.json / 三个 Cargo.toml / Cargo.lock /
   tauri.conf.json / README 版本标记，随后 `cargo check` 让 lockfile 落地。
2. `python scripts/check-version-consistency.py` 确认 6 处版本与 4 处仓库地址一致。
3. 构建：`npm run build && cargo build --release -p wb-switch-rust -p wb-switch-server`。
   - 本机偶发 `rustc STATUS_ACCESS_VIOLATION`，重试或 `-j 2` 降并发即可，不是代码问题。
4. 验证：
   - `./target/release/wb-switch.exe --version` 输出目标版本；
   - `python scripts/smoke-webui.py --exe target/release/wb-switch.exe` 断言内嵌前端已挂载；
   - 全部门禁 + `cargo test` 通过。
5. 归档资产：把 EXE 复制为 `releases/workbuddy-switch-tidy_<版本>.exe`，
   并写 `releases/latest.json`（字段 `version` / `pub_date` / `notes`）。
6. 打 tag（`v<版本>`）→ 建 Release（正文用 `scripts/release-body.py --check` 校验过的更新日志，
   首行不得是版本标题）→ 上传 EXE 与 `latest.json`。
   资产命名含版本、**不带 x64**、label 留空。
7. 发版后验收（自动）：`.github/workflows/release-smoke.yml` 会
   - 用该 tag 的源码重建前端并跑挂载冒烟（防白屏）；
   - `python scripts/check-release-assets.py --structural --tag <tag> --dir <目录>` 校验资产结构；
   - 下载线上 EXE 实跑 20 秒（参考级）；
   - 结果写入运行摘要。
   若失败，先看是"资产未上传完"（该 job 已内置轮询等待）还是真实问题。
8. 收尾核对：`/releases/latest` 指向该 tag，下载资产与本地 SHA256 一致。

> 已废弃：上游的签名更新包（`.sig`）、多平台 `latest-*.json` 合并流程不再使用。
> `scripts/gen-update-json.sh`、`scripts/merge-update-manifests.py` 仅作历史保留。

### npm 版（webui）发布

1. 编译 server 二进制并上传 GitHub Release（`.github/workflows/build.yml` 自动执行）
2. `cd npm && npm publish`（包名 `workbuddy-switch`，postinstall 按平台从 Release 下载二进制）

## 目录结构

```
src-tauri/
  src/
    commands.rs      # Tauri command 薄包装（对应 Python 版 HTTP API）
    modules/         # 已抽离到 crates/wb-switch-core（三宿主复用）
crates/
  wb-switch-core/    # 核心逻辑：account/auth_file/oauth/process/switch/session/checkin/refresh/update/config
  wb-switch-server/  # HTTP server + CLI：axum API + rust-embed 前端
src/                 # 前端：components/pages/lib（api.ts 双通道：Tauri invoke / HTTP fetch）
npm/                 # npm 包：package.json + bin + scripts/install.js
```

## 隐私注意事项

- 仓库不提交本地数据（accounts.json、认证文件、密钥、token 由 `.gitignore` 排除）
- 发布前用 `git grep` 扫描 token 模式（`ghp_`/`npm_`/`gho_` 等）
