# 开发指南

> **本 fork 的差异（2026-11-06）**：本项目是 `workbuddy-switch-tidy` 便携版，构建以 `build.sh` 为准（Windows 单 EXE、`bundle.active=false`），**不提供签名安装包/应用内自动升级**；下文的 `build:app`、签名密钥、`.sig` 与 updater manifest 流程属于上游安装版，仅供追溯。更新检查只做版本检测，入口是下载页。
>
> 当前验证基线：core 793 passed / 7 ignored；server 11；auth 集成 6；frontend 70；clippy `-D warnings`、fmt、release build 通过。CI 的 macOS/lint 若被 runner 取消，不得当作通过。

## 环境要求

Node.js ≥ 20、Rust stable、macOS（或 Windows/Linux）。

## 开发命令

```bash
npm install
npm run tauri dev        # 开发模式
npm run build:app        # 构建 debug .app（含前端资源补丁）
npm run build:app:release  # 构建 release .app + 签名更新包
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

## 发布新版本

签名密钥（自动更新用）存放于 `~/.wb-switch/wb-switch-updater.key`，构建脚本通过
`TAURI_SIGNING_PRIVATE_KEY` 注入。发布新版本时：

1. `npm run build:app:release` 生成 `.app.tar.gz` + `.sig`（Windows NSIS 构建会额外生成当前版本的 `*_x64-setup.exe` + `.exe.sig`）。CI 会先清掉 `target/**/release/bundle`，避免 cargo cache 把旧安装包带进 Release。
2. macOS：`UPDATE_OS=macos UPDATE_ARCH=aarch64 sh scripts/gen-update-json.sh` 生成 `latest-macos-aarch64.json`；Intel 用 `UPDATE_ARCH=x86_64`
3. Windows：`UPDATE_OS=windows UPDATE_ARCH=x86_64 sh scripts/gen-update-json.sh` 生成 `latest-windows-x86_64.json`
4. `python3 scripts/merge-update-manifests.py <产物目录>` 合并为 `latest.json`，并把 Windows 平台项写入 `latest-macos-x86_64.json`（兼容已安装的 Windows 客户端）
5. 将安装包、签名更新包、`latest*.json` 一并上传到 GitHub Release

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
