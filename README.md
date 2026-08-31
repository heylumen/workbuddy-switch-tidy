# workbuddy-switch-tidy

WorkBuddy（腾讯 AI 编程助手）多账号切换工具。

本项目 fork 自 [changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch)，在保留上游全部功能的基础上，修复了多账号使用场景中的若干实际问题。

<p align="center">
  <img src="public/icon-transparent.png" alt="Switch Tidy 图标" width="128" />
</p>

<p align="center">
  <strong>workbuddy-switch-tidy</strong><br />
  WorkBuddy 多账号切换工具
</p>

---

## 下载

### 桌面 App（Windows 单 EXE）

前往 [Releases](https://github.com/heylumen/workbuddy-switch-tidy/releases/latest) 下载最新版：

| 平台 | 文件 | 使用方式 |
| --- | --- | --- |
| Windows x64 | `workbuddy-switch-tidy_<版本>.exe` | 双击直接运行，无需安装 |

**首次运行**：Windows SmartScreen 可能提示「Windows 已保护你的电脑」，点击「更多信息」→「仍要运行」即可。

> 本项目不做应用内自动更新，请到 Releases 手动下载新版本替换。

### npm / webui（跨平台）

macOS / Linux 暂无预编译 EXE，可通过 npm 使用：

```bash
npm i -g workbuddy-switch
workbuddy-switch              # 启动本地服务 + 自动打开浏览器
workbuddy-switch status       # 终端查看当前账号
```

webui 界面与桌面 App 一致。

---

## 功能

| 模块 | 说明 |
| --- | --- |
| 账号管理 | OAuth 扫码登录、从本机导入、手动添加 token、删除账号 |
| 账号切换 | 备份认证文件 → 关闭 WorkBuddy → 写入目标账号 → 重启，切换过程实时进度反馈 |
| 会话复制 | 将勾选会话复制给目标账号；**复制前自动去重**，目标已存在等价会话则跳过 |
| 清理重复会话 | 按「工作目录 + jsonl 正文逐字一致」合并副本，每组保留最近更新的一条 |
| 折叠同名会话 | 按「工作区 + 标题」收起视图冗余，专治切换账号反复复制导致的同名重复 |
| 自动签到 | 默认开启；启动时立即检查，运行期间每 30 分钟自动补签；30 天签到日志 |
| Token 保活 | 惰性刷新 + 每日保活，避免 refresh token 过期 |
| 积分到期查询 | 查询各账号积分资源、剩余量与到期时间；7 天内到期高亮并按到期优先排序 |
| 积分统计 | 汇总官方请求用量，展示每日趋势、模型分布、账号消耗与请求明细 |
| CodeBuddy CLI | 与 WorkBuddy 复用账号库，默认账号独立；Windows 通过 `settings.json.env.CODEBUDDY_AUTH_TOKEN` 设置 |
| 自动轮换 | 后台定时把 CLI 后续启动账号设为积分最紧迫的账号 |
| 权限检测 | macOS 授权引导（App 管理 / 完全磁盘访问） |

### 两个整理功能的区别

| | 清理重复会话 | 折叠同名会话 |
| --- | --- | --- |
| 判定依据 | 工作目录相同 **且** 正文逐字一致 | 工作区 + 标题（同标题不同内容**也会折叠**） |
| 处理对象 | 切换复制产生的完全相同的副本 | 同一对话被反复复制产生的同名冗余 |
| 数据风险 | **较高**：会删除 jsonl 正文，不可恢复 | **低**：仅软隐藏，正文留盘可找回 |
| 数据处理 | 软删除（标记 `deleted_at`）+ 删除正文 | 软隐藏（标记 `deleted_at`），**jsonl 正文原样留盘** |
| 结果 | 列表中的重复项消失 | 左栏「空间 / 任务」每个同名分组只显示最新一份 |

> 折叠之所以敢按「同标题」放宽，是因为它**不删正文**、随时可恢复；清理会真删文件，因此必须要求正文逐字一致才动手。

> 两者都只作用于**所选账号**，都需**先关闭 WorkBuddy** 再执行。

---

## 使用

1. **添加账号**：账号页 →「扫码登录」或「从本机导入」「手动添加」
2. **切换账号**：账号卡片 →「切换」，可勾选复制当前会话
3. **查看积分**：账号页自动查询；点「刷新积分」手动更新，临期账号排最前并标记「建议优先」
4. **整理重复会话**：点账号卡片右上角的 **⋮ 菜单**，选择「清理重复会话」或「折叠同名会话」（**执行前请关闭 WorkBuddy**）
5. **CodeBuddy CLI**：账号页一键接入；切换只影响后续会话，当前会话需重新加载或重启 CLI
6. **更新版本**：本项目不做应用内自动更新，请到 [Releases](https://github.com/heylumen/workbuddy-switch-tidy/releases/latest) 手动下载新版本替换

---

## 界面预览

### 账号管理

账号卡片集中展示登录状态、签到状态、积分余额与到期资源。临期积分直接标注在卡片内，并按紧迫程度优先排列。

<table>
  <thead>
    <tr><th>浅色模式</th><th>深色模式</th></tr>
  </thead>
  <tbody>
    <tr>
      <td><img src="docs/images/accounts-overview-light.png" alt="账号管理页面（浅色模式，账号信息已脱敏）" /></td>
      <td><img src="docs/images/accounts-overview-dark.png" alt="账号管理页面（深色模式，账号信息已脱敏）" /></td>
    </tr>
  </tbody>
</table>

### 积分统计

展示官方请求用量、每日趋势、模型分布、账号消耗与请求明细，并明确标注数据来源与更新时间。

<table>
  <thead>
    <tr><th>浅色模式</th><th>深色模式</th></tr>
  </thead>
  <tbody>
    <tr>
      <td><img src="docs/images/credit-statistics-light.png" alt="积分统计趋势页面（浅色模式）" /></td>
      <td><img src="docs/images/credit-statistics-dark.png" alt="积分统计趋势页面（深色模式）" /></td>
    </tr>
  </tbody>
</table>

> 以上截图取自上游项目，功能与布局一致；本版侧边栏品牌名为 `Switch Tidy`。

---

## 更新日志

### v1.0.1（最新）

- 修复官方服务端增加 User-Agent 校验后，全部账号积分查询失败的问题（HTTP 403 / code=10085）
- 修复「折叠同名会话」在多数情况下完全不生效：判定口径由「同工作区 + 同标题 + 同正文」放宽为「同工作区 + 同标题」。折叠仅软隐藏、jsonl 正文原样留盘可恢复，因此可以按标题收起；「清理重复会话」会真删正文，仍保持「正文逐字一致」的严格口径
- 修复清理/折叠在数据库写入失败时仍报告成功的问题：写库失败立即中止并返回明确错误（如 WorkBuddy 未关闭导致库被占用）
- 修复数据库写入失败时仍会删除 jsonl 正文的数据丢失隐患：仅在 `deleted_at` 写入成功后才清理正文
- 复制会话到目标账号前自动去重：目标已存在等价会话则跳过，不重复写入、不注册孤儿云端映射；并支持一键清理历史重复会话
- 统一 11 处版本号至 1.0.1

### v1.0.0

- 修复折叠误伤真实会话：分组键加入正文指纹，仅「同工作区 + 同标题 + 同内容」才收起，同标题不同内容的会话全部保留
- 修复多账号往返切换导致的会话重复累积（切换复制改为 upsert + 复制前去重 + 一键清理）
- 版本号调整至 1.0.0，软件更名为 `workbuddy-switch-tidy`
- 发布形态改为 Windows 单 EXE 便携版

---

## 从源码构建

需要 Node.js 22+ 与 Rust 工具链（Windows 另需 MSVC Build Tools 与 WebView2）。

```bash
npm install
npm run tauri build
```

产物位于 `target/release/`。

---

## 数据目录

- 账号与配置：`~/.wb-switch/`
- WorkBuddy 会话数据：`~/.workbuddy/`

> 本工具不改动数据表结构，仅在整理会话时标记会话可见性（`deleted_at`）。
> ⚠️ 执行「清理重复会话」「折叠同名会话」等写库操作前，请务必关闭 WorkBuddy 客户端，避免 SQLite 锁冲突。

---

## 致谢

- 上游项目 [changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch) 及 [Linux.do](https://linux.do) 社区

## 许可

[MIT](./LICENSE)
