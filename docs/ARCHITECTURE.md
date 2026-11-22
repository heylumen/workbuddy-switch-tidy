# 架构说明

本文说明代码分层、模块职责与关键数据流。改动模块职责时请同步更新本文件。

## 1. 分层

```
┌─────────────────────────────────────────────┐
│ 前端 src/（React + TS + Tailwind）           │  页面 / 组件 / 类型 / HTTP 与 Tauri 双通道
└───────────────┬─────────────────────────────┘
                │  call(cmd, args)
      ┌─────────┴──────────┐
      │                    │
┌─────▼──────┐      ┌──────▼────────┐
│ src-tauri  │      │ wb-switch-    │   桌面：Tauri 命令（117 个）
│ (桌面壳)    │      │ server        │   WebUI：HTTP 路由（99 条）+ token 鉴权
└─────┬──────┘      └──────┬────────┘
      └─────────┬──────────┘
        ┌───────▼──────────────────────────┐
        │ wb-switch-core（业务核心，纯 Rust）│
        └──────────────────────────────────┘
```

- **同源双通道**：前端只调 `src/lib/api.ts` 的 `call()`；桌面走 `invoke`，WebUI 走
  `ROUTES` 表映射到 HTTP。两侧命令名必须一致，由 `scripts/check-command-routes.py` 强制。

## 2. 核心模块（`crates/wb-switch-core/src/modules/`）

| 分组 | 模块 | 职责 |
| --- | --- | --- |
| 账号 | `account` `account_memory` `account_merge` `variant` `oauth` `refresh` | 账号库读写、人格/记忆合并、档位（国内版/国际版）、登录与令牌刷新 |
| 切换 | `switch` `auth_file` `auth_transaction` `process` `rate_limit_events` `rotate` | 切号主流程（备份→关客户端→写认证→启动）、切号互斥、频率保护、自动轮换 |
| 会话 | `session`（+ `session/{list,store,rows,identity,equivalent,export,search,canonical,divergence,copy_progress}`） | 列表、读取、等价判定、复制、导出、搜索、逻辑会话、差异比对 |
| 关联与分组 | `session_link` `session_groups` `session_pinned` `session_backup` | 跨账号关联、同名分组、置顶同步、DB 快照备份 |
| 用量 | `credits` `credit_usage` `official_usage` `token_stats` `limits` `travel` | 积分到期、积分消耗统计、官方用量、Token 统计（增量读 + 缓存）、限额台账 |
| 宿主适配 | `codebuddy_cli` `codebuddy_ide` `codebuddy_ide_session*` `codebuddy_cn_ide` `vscode_ext` `vscode_session*` `vscode_cn_inject` `jetbrains` `linux_keyring` | CodeBuddy CLI / IDE / VS Code 插件 / JetBrains 的凭据写入与运行进程处理 |
| 运维 | `storage` `config` `error_log` `export_import` `encrypted_backup` `update` `notifications` `http_response` | 存储与回收站、路径与原子写、错误日志、导入导出、加密备份、更新检查 |

`session.rs` 仍保留编排逻辑；纯函数与数据访问已下沉到 `session/` 子模块。`session.rs`
行数由债务棘轮锁定，只减不增。

## 3. 关键数据流

### 切号（`switch_account`）
```
频率保护 → 取 auth 事务锁 → 读目标账号 → 预检登录态
  → 备份当前认证文件（失败即中止）
  → [restart] 关客户端 → 恢复未完成会话写入（阻塞则暂停启动）
  → 写认证文件 → [restart] 启动客户端
  → 可选：复制会话 / 同步关联副本 → 记录最近切换
```

### 跨账号复制会话（`session::copy_*` + `session/copy_progress.rs`）
```
读源行（源库）→ 生成新 session id → insert_session_copy()
  ├ 归一：id/user_id/created_at/updated_at/deleted_at/project_id/space_id/status
  ├ 正文：复制 JSONL 并建立映射
  └ 失败：标记 stale / 回滚，绝不留下半份数据
```
`project_id` 与 `space_id` **一律归一为空**：它们属于源账号的云端数据，继承会导致目标端
分组错乱或整组会话被客户端丢弃。

### 用量统计（`token_stats` / `credit_usage`）
```
源目录扫描 → append-only 增量读（offset 续读）→ 聚合 → 磁盘缓存（按 source）
官方接口可用 → 走官方口径；不可用 → 本地观察口径（超出覆盖范围显示「—」）
```

## 4. 安全边界

| 边界 | 措施 |
| --- | --- |
| WebUI 监听 | 仅绑定 `127.0.0.1`，并为每次启动生成随机 token |
| DNS rebinding | 校验 `Host` / `Origin` 必须为回环，否则 403（含单测） |
| 备份导出 | WebUI 禁用导出；桌面导出可选口令加密（PBKDF2-HMAC-SHA256 210k + AES-256-GCM） |
| 日志 | 账号标识脱敏（昵称首字符 / uid 前 6 位），不打印邮箱与令牌 |
| 删除 | 统一进回收站，二次确认，默认保留 30 天 |

## 5. 质量门禁（`scripts/`）

| 门禁 | 作用 |
| --- | --- |
| `check-version-consistency.py` | 6 处版本号 + 4 处仓库地址一致 |
| `check-command-routes.py` | Tauri 命令 ↔ 前端调用 ↔ HTTP 路由三方一致 |
| `check-debt-ratchet.py` | 静默丢弃数 / 生产 unwrap / 最大文件行数 / TODO 只减不增 |
| `check-release-assets.py` | 发布资产命名与体积（`--structural` 供发版后验收） |
| `check-portability.py` `check-demo-commands.py` `check-legacy-colors.py` `check-pii.py` `check-repair-ledger.py` | 可移植性、截图演示命令覆盖、旧色回退、隐私扫描、修复台账一致性 |
| `smoke-webui.py` | 真实浏览器断言 `__WB_MOUNTED__`（`--dist` / `--exe`），防白屏 |

CI（`.github/workflows/`）：`test.yml`（门禁 + 三平台测试）、`build.yml`（单文件 EXE）、
`release-smoke.yml`（发版后自动验收：前端挂载 / 资产结构 / EXE 启动存活）。
