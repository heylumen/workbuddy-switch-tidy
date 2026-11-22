# CodeQL 误报处置记录

记录本仓库对 GitHub CodeQL（默认扫描，Rust 规则）告警的判定结论，避免每次扫描被同一批误报反复打断，也便于审阅者复核判断依据。

## rust/hard-coded-cryptographic-value（误报，按实例 dismiss）

**涉及文件**：`crates/wb-switch-core/src/modules/encrypted_backup.rs`、`crates/wb-switch-core/src/modules/export_import.rs`

**规则在找什么**：把常量字符串/字节数组当作密钥、盐、nonce 或口令传给加密 API。

**为什么是误报**：本项目的加密实现里，所有密钥材料都来自运行期：

| 材料 | 来源 | 是否有常量 |
| --- | --- | --- |
| 对称密钥 | `pbkdf2_hmac(SHA-256, 口令, 随机盐, 210000 轮)` 派生 | 无 |
| 盐 | `OsRng.fill_bytes` 生成 16 字节 | 无 |
| nonce | `OsRng.fill_bytes` 生成 12 字节 | 无 |
| 口令 | 用户输入 | 无 |

规则命中的具体位置是两类，都不构成风险：

1. **零初始化缓冲区**：`let mut key = [0u8; 32]` / `let mut salt = [0u8; 16]`。
   写成零初始化只是「先分配、随后由 CSPRNG 或 KDF 填满」的惯用写法，
   紧接着就被 `fill_bytes` / `pbkdf2_hmac` 覆盖；不存在硬编码密钥。
2. **测试代码里的固定输入**：`#[cfg(test)]` 模块中的样例 JSON、断言用的固定轮数与盐长度、
   以及测试口令的种子字符串。测试口令已改为按进程 id 动态生成
   （`format!("{tag}-{}", std::process::id())`），进一步减少这类命中。

**处置**：按实例 `Dismiss` → 理由 `false positive` / `used in tests`，并附具体说明。

**如果这批告警反复出现**：优先修正测试输入（让它们不在加密调用点出现常量），
其次再考虑改用 CodeQL 高级配置（advanced setup）对该规则做仓库级排除 —— 后者会失去
这条规则对真实代码的保护，**不建议默认开启**。

## rust/cleartext-logging（1 条误报 + 1 条真问题）

| 位置 | 判定 | 说明 |
| --- | --- | --- |
| `crates/wb-switch-core/src/modules/switch.rs` | 误报 | 被记日志的 progress 文案来自 `precheck_target_account` 返回的**固定中文串**，不含账号标识；属污点传递误判 |
| `crates/wb-switch-server/src/main.rs` | **真问题，已修** | `--status` 输出「当前账号」时曾回退到**邮箱**。现改为昵称只显示首字符、无昵称时显示 uid 前 6 位，不再输出邮箱 |

## 维护约定

- 新增加密相关代码时，避免把常量直接传给加密 API（尤其是测试）——用变量或运行期生成值。
- 每次 dismiss 都要写清理由；空理由的 dismiss 无法审计。
- 本文件只在判定规则发生变化时更新，不要在每次扫描后追加流水账。
