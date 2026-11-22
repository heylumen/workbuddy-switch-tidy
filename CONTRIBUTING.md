# 贡献与质量门禁

本项目是本地账号/会话工具，任何涉及认证文件、SQLite、`.jsonl` 或关联表的改动都必须先过门禁。

## 合并前必须通过的检查

分支保护已于 2026-11-08 配置在 `workbuddy-switch-tidy`，以下 job 是 **required status checks**：

- `quality-gates`
- `cargo test (macos-latest)`
- `cargo test (ubuntu-latest)`
- `cargo test (windows-latest)`
- `lint (tsc + fmt + clippy + server tests)`
- `frontend-tests`

其中：

| 检查 | 作用 |
|---|---|
| `quality-gates` | 版本一致性、修复台账、legacy 颜色回退、PII、命令/路由契约、债务棘轮、workflow YAML 语法、前端构建与运行时冒烟、`npm audit`、`cargo audit` |
| `rust-tests` | 三平台 core 测试（ubuntu 额外跑 linux cfg 的 clippy） |
| `lint` | tsc、fmt、clippy（`-D warnings`）、server/Tauri 测试 |
| `frontend-tests` | 前端逻辑回归 |

另有非阻塞的 `release-smoke`（发版后自动验收：前端挂载 / 资产结构 / EXE 启动存活）。

## 跨平台规则

本项目开源到 GitHub，不能假设用户使用 Windows、固定盘符或固定用户目录：

- 不把文件 mtime 当作“内容年龄”；复制/启动触碰都会改变 mtime，范围筛选必须用记录内 timestamp 或等价索引。
- 测试必须使用 `std::env::temp_dir()` 或注入路径，不得读写真实 `~/.wb-switch`、`~/.workbuddy`、`~/Library`、`%APPDATA%`。
- 平台命令（`tasklist`、`ps`、`mdfind`、`osascript`、`xdg-open`）必须放在对应 `#[cfg(target_os = ...)]` / `#[cfg(windows)]` 分支。
- Windows 文件替换不能用裸 `rename` 覆盖已存在目标；先移除或使用平台安全替换。
- 路径通过 `config::home_dir()` / `store_dir()` 拼接，不在生产代码写死盘符或用户名。
- Token 大文件缓存默认 128MB；低内存环境可用 `WB_TOKEN_CACHE_MB`（16–512）调整，不要硬编码机器预算。
- Token 扫描诊断可设 `WB_TOKEN_PROFILE=1`，输出各来源耗时/文件计数；不得输出路径、正文或账号。
- 同步写入诊断可设 `WB_SYNC_PROFILE=1`，输出逐项/总耗时；不得输出会话 ID、账号或路径。
- 新增/修改平台相关行为，必须同时在 Windows、macOS、Linux 至少通过编译/单测；三平台矩阵见 `.github/workflows/test.yml`。
- 本地维护脚本若只支持 Windows，必须在文档标明，不能作为跨平台产品路径的依赖。

## 本地提交前

```bash
npx tsc --noEmit
npm test -- --run
npm run build                      # 构建产物 + 下面用 --dist 冒烟

cargo fmt --all -- --check
# 必须带 -D warnings：CI 就是这么跑的，少了它会出现「本地绿、CI 红」（2026-11-22 踩过）
cargo clippy -p wb-switch-core -p wb-switch-server -p wb-switch-rust \
  --all-targets --no-deps -- -D warnings
cargo test -p wb-switch-core --lib
cargo test -p wb-switch-core --tests
cargo test -p wb-switch-server

cargo audit                        # 要求 0 vulnerabilities

python scripts/smoke-webui.py --dist dist        # 前端挂载冒烟（防白屏）
python scripts/check-version-consistency.py
python scripts/check-command-routes.py           # 改了命令/接口必跑
python scripts/check-debt-ratchet.py             # 想加 TODO 或长文件会在这里被拦
python scripts/check-repair-ledger.py docs/repair-ledger-20261122.md
python scripts/check-portability.py
python scripts/check-demo-commands.py
python scripts/check-legacy-colors.py
python scripts/check-pii.py README.md docs/*.md
```

> 文档改动：`docs/*.md` 的结构描述要跟着实现走；改了模块职责请同步
> `docs/ARCHITECTURE.md`，改了流程请同步 `docs/DEVELOPMENT.md`。

## 涉及关联表/会话数据的改动

1. 先备份 `~/.wb-switch/session_links.json` 及相关目录；
2. 优先使用 `workbuddy-switch repair-links` 修复，不要手工改主表；
3. 变更后跑一次 `repair-links`，确认 `changed=false` 或检查报告；
4. 不得为了“状态好看”批量合并单副本组。

## 发布前

- 推送并通过全部 required checks
- 版本一致性：`python scripts/check-version-consistency.py`
- 隐私：`python scripts/check-pii.py <更新日志> README.md`
- 资产：发布后 `python scripts/check-release-assets.py <tag> <本地资产...>`
- 更新清单：`scripts/gen-update-json.sh` 默认仓库已是 `heylumen/workbuddy-switch-tidy`

## 当前限制

- `.github/workflows/build.yml` 已存在三平台矩阵；安装包仍以 CI artifact 为准，发布需单独签名/校验；
- macOS/lint runner 偶发被平台取消，必须重跑而不是跳过；
- 本地提交未推送前，不能宣称 CI 已通过。
