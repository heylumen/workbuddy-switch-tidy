# Repair Ledger — 2026-11-22

> 上一轮（1.0.13 时代）的台账见 [`repair-ledger-20261108.md`](./repair-ledger-20261108.md)。
> 本文件记录 1.0.16 → 1.0.17 这一轮的审查发现与处置，是**仓库内的紧凑索引**，
> 详细证据在提交信息与工作区发布记录里。

Baseline: `7ef4823`（1.0.16 发布提交）→ `7e85843`（1.0.17 构建来源）
Verification at close: core 883 / integration 5 / server 15 / frontend 9 files / Tauri 0；
clippy `-D warnings` 0 告警；`cargo audit` 0 漏洞；CodeQL open alerts 0。

## Fixed

| ID | Issue | Commit | Verification |
|---|---|---|---|
| WHITE_SCREEN | 前端 manualChunks 分包导致入口模块求值失败：构建成功但界面全白 | `65a9776` | `smoke-webui.py` 断言 `__WB_MOUNTED__`；release-smoke 的 frontend-mount job |
| DESKTOP_CMD_DRIFT | 桌面端 4 个 Tauri 命令名与前端调用漂移（重复审计 / 逻辑会话 / 导入账号不可用） | `65a9776` | `check-command-routes.py`：117 注册 / 117 声明 / 118 调用一致 |
| SPACE_ID_INHERIT | 跨账号复制沿用源账号 `space_id`，目标端分组错乱 | `65a9776` | `insert_session_copy_drops_source_space_id`；旧 schema（无 space_id 列）复制用例 |
| BACKUP_FAIL_OPEN | 切号前备份认证文件失败仍继续，切坏无法回滚 | `65a9776` | `backup_failure_returns_error_with_actionable_hint`；`missing_auth_file_is_not_an_error` |
| SWALLOWED_WRITES | 8 处写路径失败被静默丢弃，用户以为已保存 | `65a9776` | 统一记入 error.log；`check-debt-ratchet.py` 锁定 `let _ =` 数量 |
| CREDIT_RANGE_FALLBACK | 官方用量不可用时「近 7 天 / 本月」退化为今日值（三个数字相同） | `65a9776` | `coveredOrDash()`：超出本地覆盖显示「—」并在卡片下标注覆盖起点 |
| CRYPTO_ITERATION_DOS | 解密直接信任信封里的 KDF 轮数，恶意文件可拖死 CPU | `65a9776` | `absurd_iteration_count_is_rejected_fast`；`short_salt_is_rejected` |
| RELEASE_ASSET_VACUOUS | 发版验收把下载到的文件当期望值，资产校验恒真 | `65a9776` | `check-release-assets.py --structural` 正例/版本不匹配负例 |
| LOG_FLOOD | 限流状态落盘失败时每秒写一条日志（消费循环 1 秒一轮） | `65a9776` | `should_log_failure` 节流用例（同原因仅一次 / 不同原因独立计时） |
| PASSWORD_MIN_LENGTH | 加密备份口令仅校验非空，弱口令等于没加密 | `65a9776` | `short_password_is_rejected`（8 位下限；历史短口令仍可解密） |
| DEMO_VERSION_DRIFT | 演示数据写死版本 `0.1.24`，且更新提示地址指向上游仓库 | `00ed656` | `vite.config.ts` 注入 `__APP_VERSION__`；`app-version.ts` 兜底；截图侧栏显示 1.0.17 |
| DOC_SCREENSHOTS_STALE | README 截图为 9–10 月老图，缺新入口；另有 8 张未引用旧图 | `00ed656` | `capture-docs-screenshots.py` 重生成 5 张；删除未引用图 |
| DEP_ADVISORIES | tokio 1.53.1（unsound）、quinn-udp 0.5.15、url 2.5.8 三个漏洞 + 2 个 yanked 包 | `d30d839` | `cargo audit`：0 vulnerabilities |
| CLIPPY_DENY_WARNINGS | CI 以 `-D warnings` 运行 clippy 报错（本地未加该参数所以漏检） | `d30d839` | CI 同款 clippy 命令通过；补充到开发指南 |
| RELEASE_SMOKE_RACE | release published 比资产上传早触发，下载 404 | `d30d839` | 轮询等待资产；手动重跑 release-smoke 4 job 全绿 |
| TERMINAL_EMAIL_LOG | `--status` 在无昵称时把**邮箱**打到终端 | `7e85843` | `mask_label` / `mask_uid`：昵称首字符、uid 前 6 位 |
| CODEQL_FP_DOC | CodeQL 同一批误报反复出现，无判定记录 | `90a9c26` | `docs/codeql-false-positives.md`；open alerts 27 → 0 |

## Remaining

| ID | Issue | Notes |
|---|---|---|
| IDE_TOKEN_STATS | JetBrains / IDE 插件请求未计入 Token 统计 | `token_stats` 仅 workbuddy / workbuddy-ai / codebuddy-cli 三个源；需插件实际落盘样本 |
| CODEX_LINK_SCOPE | 关联会话不含 CodeBuddy CLI 会话 | `session_link` / `canonical` 只覆盖 WorkBuddy 两档位；需产品决策 |
| JETBRAINS_2026_3 | IDEA 2026.3 插件切换未验证 | 目录前缀可命中，失败更可能来自插件状态文件格式；需真机样本 |
| SESSION_RS_TAIL | `session.rs` 仍 4.4k 行 | 已拆 5 个子模块，剩余部分按行数棘轮锁定不再增长 |
