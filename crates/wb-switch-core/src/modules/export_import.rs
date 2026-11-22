//! 账号导出/导入：解析导入文件、按 uid 去重合并、计数。
//!
//! 纯逻辑（`parse_accounts_json` / `merge_import_records` / `select_export_records`）
//! 不依赖文件系统，便于无 UI 环境单测；`export_accounts` / `import_accounts`
//! 负责读写账号库（`~/.wb-switch/accounts.json`）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::account;

/// 导入结果计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct ImportResult {
    /// 成功合入账号库的数量（含覆盖与新增）。
    pub imported: usize,
    /// 未导入的数量（缺凭据 / 索引越界）。
    pub skipped: usize,
    /// 其中覆盖了同 uid 本地账号的数量。
    pub overwritten: usize,
}

/// 把已解析的 JSON 转成账号记录数组（转移所有权，避免为预览再克隆一整份）。
fn into_account_records(parsed: Value) -> Result<Vec<Value>, String> {
    let Value::Array(items) = parsed else {
        return Err("文件内容应为 JSON 数组（账号列表）".to_string());
    };
    for (index, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(format!("文件第 {} 项不是合法的账号对象", index + 1));
        }
    }
    Ok(items)
}

/// 解析导出/导入文件文本：必须是 JSON 数组，且每项为 JSON 对象。
///
/// 失败时返回带位置的明确错误文案（非法 JSON / 非数组 / 元素不是对象）。
pub fn parse_accounts_json(text: &str) -> Result<Vec<Value>, String> {
    if text.trim().is_empty() {
        return Err("文件内容为空".to_string());
    }
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("文件不是合法的 JSON：{e}"))?;
    into_account_records(parsed)
}

/// 生成导入文件的脱敏预览（含文件内索引，不含 token）。
///
/// 展示字段（uid/nickname/email）一律走 `account::display_value` 折叠，与账号
/// 列表接口同口径：WorkBuddy 5.6 加密信封等对象/数组不得漏进前端渲染路径，
/// 否则会被当作 React 子节点渲染导致整树卸载（issue #100）。
/// `encrypted` 标记 access_token 为加密信封：可导入，但仅切换可用。
pub fn preview_accounts(text: &str) -> Result<Value, String> {
    let array = parse_accounts_json(text)?;
    Ok(preview_from_records(&array))
}

/// 由已解析的记录生成预览（文本版与文件版共用）。
fn preview_from_records(array: &[Value]) -> Value {
    let items: Vec<Value> = array
        .iter()
        .enumerate()
        .map(|(index, item)| {
            json!({
                "index": index,
                "uid": account::display_value(item, "uid"),
                "nickname": account::display_value(item, "nickname"),
                "email": account::display_value(item, "email"),
                "hasToken": account::secret_value(item, "access_token").is_some(),
                "encrypted": account::is_envelope(item, "access_token"),
            })
        })
        .collect();
    json!({ "accounts": items, "total": array.len() })
}

/// 导入路径护栏：只接受 `.json` 且 ≤64 MB 的文件。
///
/// WebUI 模式下该路径来自请求体，不加限制就等于给了"探测任意文件"的能力；
/// 限定扩展名与体积后，探测只能得到"是不是 JSON 备份"，且不会拖垮进程。
fn guard_import_path(path: &str) -> Result<(), String> {
    const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
    if !path.trim().to_ascii_lowercase().ends_with(".json") {
        return Err("只支持 .json 账号备份文件".to_string());
    }
    let meta = std::fs::metadata(path).map_err(|error| format!("打开文件失败：{error}"))?;
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(format!(
            "文件过大（{} MB），上限 {} MB",
            meta.len() / 1024 / 1024,
            MAX_IMPORT_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// 按路径读取导入文件：`BufReader` + `from_reader`，不再额外驻留一整份字符串。
///
/// 桌面端走此路径：解析发生在 Rust 的阻塞线程上，不会卡住 WebView 主线程
/// （同类项目 cockpit-tools #3178 就是大 JSON 在 WebView 内解析导致冻结）。
pub fn parse_accounts_file(path: &str) -> Result<Vec<Value>, String> {
    guard_import_path(path)?;
    let file = std::fs::File::open(path).map_err(|error| format!("打开文件失败：{error}"))?;
    let reader = std::io::BufReader::new(file);
    let parsed: Value =
        serde_json::from_reader(reader).map_err(|error| format!("文件不是合法的 JSON：{error}"))?;
    into_account_records(parsed)
}

/// 按路径生成导入预览。
pub fn preview_accounts_file(path: &str) -> Result<Value, String> {
    let array = parse_accounts_file(path)?;
    Ok(preview_from_records(&array))
}

/// 按路径导入：Rust 侧读取文本后复用文本版合并逻辑。
pub fn import_accounts_file(path: &str, indexes: &[usize]) -> Result<ImportResult, String> {
    guard_import_path(path)?;
    let text = std::fs::read_to_string(path).map_err(|error| format!("读取文件失败：{error}"))?;
    import_accounts(&text, indexes)
}

/// 导出并加密：与 `export_accounts_to_path` 同样的明文结构，整体加密后落盘。
///
/// 明文不会经过磁盘：先在内存里序列化再加密，最后一次性原子写入信封。
pub fn export_accounts_encrypted_to_path(
    ids: &[String],
    path: &str,
    password: &str,
) -> Result<String, String> {
    validate_export_path(path)?;
    if password.trim().is_empty() {
        return Err("口令不能为空".to_string());
    }
    let records = select_export_records(&account::load_accounts(), ids)?;
    let content = serde_json::to_string_pretty(&records).map_err(|error| error.to_string())?;
    let envelope = crate::modules::encrypted_backup::encrypt(&content, password)?;
    let path_buf = PathBuf::from(path.trim());
    crate::modules::config::atomic_write(&path_buf, &envelope)
        .map_err(|error| format!("写入加密备份失败：{error}"))?;
    Ok(path_buf.to_string_lossy().into_owned())
}

/// 读取备份文本：普通 JSON 原样返回；加密信封则用口令解密。
fn read_backup_text(path: &str, password: Option<&str>) -> Result<String, String> {
    guard_import_path(path)?;
    let text = std::fs::read_to_string(path).map_err(|error| format!("读取文件失败：{error}"))?;
    if !crate::modules::encrypted_backup::is_encrypted_envelope(&text) {
        return Ok(text);
    }
    match password {
        Some(value) if !value.is_empty() => crate::modules::encrypted_backup::decrypt(&text, value),
        _ => Err("该备份已加密，请输入口令".to_string()),
    }
}

/// 导入预览（自动识别明文 / 加密备份）。
pub fn preview_accounts_file_auto(path: &str, password: Option<&str>) -> Result<Value, String> {
    let text = read_backup_text(path, password)?;
    let array = parse_accounts_json(&text)?;
    Ok(preview_from_records(&array))
}

/// 按路径导入（自动识别明文 / 加密备份）。
pub fn import_accounts_file_auto(
    path: &str,
    password: Option<&str>,
    indexes: &[usize],
) -> Result<ImportResult, String> {
    let text = read_backup_text(path, password)?;
    import_accounts(&text, indexes)
}

/// 单条导入记录的合并动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeOutcome {
    /// 追加为新账号。
    Appended,
    /// 覆盖同 uid 的本地账号（保留导入记录原样）。
    Overwritten,
    /// 缺少可导入凭据（缺失 / 空串 / 普通对象），跳过。
    Skipped,
}

/// 纯函数：把一条导入记录合并进账号列表。
///
/// 按 uid 去重：同 uid 覆盖（保留导入记录原样）；uid 缺失或无法匹配则追加。
/// 凭据判定与采集口径一致（`account::secret_value`）：非空明文串或
/// `$wbEncrypted` 加密信封均可入库，缺失 / 空串 / 普通对象跳过。
fn merge_import_record(accounts: &mut Vec<Value>, item: &Value) -> MergeOutcome {
    if account::secret_value(item, "access_token").is_none() {
        return MergeOutcome::Skipped;
    }
    if let Some(uid) = account::get_str(item, "uid").as_deref() {
        if let Some(existing) = accounts
            .iter_mut()
            .find(|a| account::get_str(a, "uid").as_deref() == Some(uid))
        {
            let mut replaced = item.clone();
            // 本地展示字段（备注 / 显示选择）：备份未携带时保留本地值；
            // 备份自带则以备份为准（导入即恢复用户当时的展示配置）。
            if let Some(obj) = replaced.as_object_mut() {
                for key in ["note", "displayField", "lastUsedAt"] {
                    if !obj.contains_key(key) {
                        if let Some(v) = existing.get(key) {
                            obj.insert(key.to_string(), v.clone());
                        }
                    }
                }
            }
            // 账号库不允许出现无 id 记录（删除按 id、列表 key、导出选择都依赖
            // id）：优先保留本地 id；本地也是无 id 的历史脏数据时补一个。
            if account::get_str(&replaced, "id").is_none() {
                replaced["id"] = account::get_str(existing, "id")
                    .map(Value::String)
                    .unwrap_or_else(|| Value::String(uuid::Uuid::new_v4().to_string()));
            }
            *existing = replaced;
            return MergeOutcome::Overwritten;
        }
    }
    // 追加同样必须带 id：缺 id 的记录在刷新回写时无法被按 id 覆盖，会被
    // 反复追加副本（issue #111）。
    let mut appended = item.clone();
    if account::get_str(&appended, "id").is_none() {
        appended["id"] = Value::String(uuid::Uuid::new_v4().to_string());
    }
    accounts.push(appended);
    MergeOutcome::Appended
}

/// 纯函数：解析文件文本并按选中索引把记录合并进账号列表，返回计数。
///
/// 不做文件读写，便于单测。索引越界视为跳过。
pub fn merge_import_records(
    accounts: &mut Vec<Value>,
    text: &str,
    indexes: &[usize],
) -> Result<ImportResult, String> {
    let array = parse_accounts_json(text)?;
    let mut result = ImportResult::default();
    for &index in indexes {
        match array.get(index) {
            None => result.skipped += 1,
            Some(item) => match merge_import_record(accounts, item) {
                MergeOutcome::Appended => result.imported += 1,
                MergeOutcome::Overwritten => {
                    result.imported += 1;
                    result.overwritten += 1;
                }
                MergeOutcome::Skipped => result.skipped += 1,
            },
        }
    }
    Ok(result)
}

/// 导入：读账号库 → 合并 → 写回，返回计数。
pub fn import_accounts(text: &str, indexes: &[usize]) -> Result<ImportResult, String> {
    account::with_accounts_lock(|| {
        let mut accounts = account::load_accounts_for_update()?;
        let result = merge_import_records(&mut accounts, text, indexes)?;
        account::save_accounts_unlocked(&accounts).map_err(|e| format!("保存账号库失败：{e}"))?;
        Ok(result)
    })
}

/// 纯函数：从账号列表中挑出 id 命中的完整记录（含 token）。
pub fn select_export_records(accounts: &[Value], ids: &[String]) -> Result<Vec<Value>, String> {
    let ids: Vec<&str> = ids
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        return Err("请先选择要导出的账号".to_string());
    }
    let mut exported: Vec<Value> = Vec::new();
    for id in ids {
        if let Some(acc) = accounts
            .iter()
            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
        {
            exported.push(acc.clone());
        }
    }
    if exported.is_empty() {
        return Err("未找到要导出的账号".to_string());
    }
    Ok(exported)
}

/// 导出：按账号 id 列表返回完整记录（含 token）。
pub fn export_accounts(ids: &[String]) -> Result<Vec<Value>, String> {
    select_export_records(&account::load_accounts(), ids)
}

/// 导出文件名白名单：只允许 `wb-switch-accounts-YYYY-MM-DD.json` 这类由前端生成的文件名。
fn validate_export_file_name(file_name: &str) -> Result<(), String> {
    let name = file_name.trim();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || !name.ends_with(".json")
    {
        return Err("导出文件名不合法".to_string());
    }
    Ok(())
}

/// 校验导出目标路径：必须是绝对路径且以 `.json` 结尾（保存对话框产物）。
fn validate_export_path(path: &str) -> Result<(), String> {
    let p = Path::new(path.trim());
    if !p.is_absolute() {
        return Err("导出路径必须是绝对路径".to_string());
    }
    if !p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
    {
        return Err("导出文件名必须以 .json 结尾".to_string());
    }
    Ok(())
}

/// 纯函数：把记录数组写入指定目录下的 JSON 文件，返回完整路径。
pub fn write_records_to_file(
    dir: &Path,
    records: &[Value],
    file_name: &str,
) -> Result<String, String> {
    validate_export_file_name(file_name)?;
    let content = serde_json::to_string_pretty(records).map_err(|e| e.to_string())?;
    let path = dir.join(file_name);
    crate::modules::config::atomic_write(&path, &content)
        .map_err(|e| format!("写入导出文件失败：{e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// 导出：按账号 id 列表把完整记录写入用户选择的路径（保存对话框产物），返回该路径。
pub fn export_accounts_to_path(ids: &[String], path: &str) -> Result<String, String> {
    validate_export_path(path)?;
    let records = select_export_records(&account::load_accounts(), ids)?;
    let path_buf = PathBuf::from(path.trim());
    let content = serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?;
    crate::modules::config::atomic_write(&path_buf, &content)
        .map_err(|e| format!("写入导出文件失败：{e}"))?;
    Ok(path_buf.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        id: &str,
        uid: Option<&str>,
        nickname: &str,
        email: Option<&str>,
        token: bool,
    ) -> Value {
        json!({
            "id": id,
            "uid": uid,
            "nickname": nickname,
            "email": email,
            "access_token": if token { format!("token-{id}") } else { String::new() },
            "createdAt": 1,
        })
    }

    #[test]
    fn parse_rejects_empty_invalid_and_non_array() {
        assert!(parse_accounts_json("").is_err());
        assert!(parse_accounts_json("not json").is_err());
        assert!(parse_accounts_json(r#"{ "a": 1 }"#).is_err());
    }

    #[test]
    fn parse_rejects_non_object_element() {
        let err = parse_accounts_json(r#"[{ "uid": "u1" }, 42]"#).unwrap_err();
        assert!(err.contains("第 2 项"), "错误应带位置：{err}");
    }

    #[test]
    fn parse_accepts_object_array() {
        let parsed = parse_accounts_json(r#"[{ "uid": "u1" }, {}]"#).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn merge_overwrites_same_uid_preserving_imported_record() {
        let mut accounts = vec![record("local", Some("u1"), "旧名称", None, true)];
        let text = r#"[{ "id": "imported", "uid": "u1", "nickname": "新名称", "access_token": "tok-new" }]"#;
        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.overwritten, 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(accounts.len(), 1, "同 uid 应覆盖而不是新增");
        assert_eq!(accounts[0]["id"], "imported", "覆盖保留导入记录原样");
        assert_eq!(accounts[0]["nickname"], "新名称");
        assert_eq!(accounts[0]["access_token"], "tok-new");
    }

    #[test]
    fn merge_preserves_local_id_when_imported_record_has_none() {
        let mut accounts = vec![record("local-id", Some("u1"), "旧名称", None, true)];
        let text = r#"[{ "uid": "u1", "nickname": "新名称", "access_token": "tok" }]"#;
        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.overwritten, 1);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["id"], "local-id", "导入记录缺 id 时保留本地 id");
        assert_eq!(accounts[0]["nickname"], "新名称");
        assert_eq!(accounts[0]["access_token"], "tok");
    }

    #[test]
    fn merge_appends_when_uid_missing_or_unmatched() {
        let mut accounts = vec![record("local", Some("u1"), "甲", None, true)];
        let text = r#"[
            { "id": "n1", "nickname": "无uid", "access_token": "t1" },
            { "id": "n2", "uid": "u-other", "nickname": "uid不匹配", "access_token": "t2" }
        ]"#;
        let result = merge_import_records(&mut accounts, text, &[0, 1]).unwrap();
        assert_eq!(result.imported, 2);
        assert_eq!(result.overwritten, 0);
        assert_eq!(accounts.len(), 3);
    }

    #[test]
    fn merge_skips_missing_token() {
        let mut accounts: Vec<Value> = vec![];
        let text = r#"[{ "id": "no-token", "uid": "u9", "nickname": "缺token" }]"#;
        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 1);
        assert!(accounts.is_empty(), "缺 token 的记录不得进入账号库");
    }

    /// 回归 issue #100：加密信封凭据按采集口径放行，且导入后原样保留（不落空串）。
    #[test]
    fn merge_imports_envelope_token_as_is() {
        let mut accounts: Vec<Value> = vec![];
        let text = r#"[{
            "id": "env-1",
            "uid": "u-env",
            "nickname": {"$wbEncrypted": 1, "envelope": "nick"},
            "access_token": {"$wbEncrypted": 1, "envelope": "a"},
            "refresh_token": {"$wbEncrypted": 1, "envelope": "r"}
        }]"#;
        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();

        assert_eq!(result.imported, 1, "信封凭据应计入 imported");
        assert_eq!(result.skipped, 0);
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0]["access_token"],
            json!({"$wbEncrypted": 1, "envelope": "a"}),
            "信封凭据必须原样保留"
        );
        assert_eq!(accounts[0]["refresh_token"]["envelope"], "r");
    }

    /// 空串与普通对象（非信封）仍按无凭据处理：预览「缺少 token」、导入计 skipped。
    #[test]
    fn merge_still_skips_blank_and_plain_object_token() {
        let mut accounts: Vec<Value> = vec![];
        let text = r#"[
            { "id": "blank", "uid": "u-blank", "access_token": "   " },
            { "id": "object", "uid": "u-object", "access_token": { "token": "t" } }
        ]"#;
        let result = merge_import_records(&mut accounts, text, &[0, 1]).unwrap();

        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 2);
        assert!(accounts.is_empty(), "空串 / 普通对象不得进入账号库");
    }

    #[test]
    fn merge_counts_out_of_range_index_as_skipped() {
        let mut accounts: Vec<Value> = vec![];
        let result =
            merge_import_records(&mut accounts, r#"[{ "access_token": "t" }]"#, &[5]).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn merge_deduplicates_within_file() {
        let mut accounts: Vec<Value> = vec![];
        let text = r#"[
            { "id": "f1", "uid": "u9", "access_token": "t1" },
            { "id": "f2", "uid": "u9", "access_token": "t2" }
        ]"#;
        let result = merge_import_records(&mut accounts, text, &[0, 1]).unwrap();
        assert_eq!(result.imported, 2);
        assert_eq!(result.overwritten, 1);
        assert_eq!(accounts.len(), 1, "文件内重复 uid 也不得产生重复账号");
        assert_eq!(accounts[0]["id"], "f2");
    }

    /// 回归 issue #111：导入记录缺 id 时必须补一个，否则刷新回写会反复追加副本。
    #[test]
    fn merge_generates_id_for_imported_record_without_id() {
        let mut accounts: Vec<Value> = vec![];
        let text = r#"[{ "uid": "u-new", "nickname": "无id", "access_token": "t1" }]"#;
        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();

        assert_eq!(result.imported, 1);
        assert_eq!(accounts.len(), 1);
        assert!(
            accounts[0]["id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty()),
            "追加入库的记录必须带 id"
        );
    }

    /// 本地同 uid 记录也是无 id 的历史脏数据时，覆盖后同样要补 id。
    #[test]
    fn merge_generates_id_when_neither_imported_nor_local_record_has_one() {
        let mut accounts = vec![json!({
            "uid": "u1",
            "nickname": "旧名称",
            "access_token": "t-old",
        })];
        let text = r#"[{ "uid": "u1", "nickname": "新名称", "access_token": "t-new" }]"#;
        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();

        assert_eq!(result.overwritten, 1);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["nickname"], "新名称");
        assert!(
            accounts[0]["id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty()),
            "本地也无 id 时覆盖后必须补 id"
        );
    }

    #[test]
    fn merge_repairs_empty_local_id_when_imported_record_has_no_id() {
        let mut accounts = vec![json!({
            "id": "  ",
            "uid": "u1",
            "access_token": "t-old",
        })];
        let text = r#"[{ "uid": "u1", "access_token": "t-new" }]"#;

        let result = merge_import_records(&mut accounts, text, &[0]).unwrap();

        assert_eq!(result.overwritten, 1);
        assert_eq!(accounts.len(), 1);
        assert!(account::get_str(&accounts[0], "id").is_some());
        assert_eq!(accounts[0]["access_token"], "t-new");
    }

    #[test]
    fn export_requires_selection() {
        let accounts = vec![record("a1", Some("u1"), "甲", None, true)];
        assert!(select_export_records(&accounts, &[]).is_err());
        assert!(select_export_records(&accounts, &["missing".to_string()]).is_err());
    }

    #[test]
    fn export_returns_full_records_with_tokens() {
        let accounts = vec![
            record("a1", Some("u1"), "甲", Some("a@b.c"), true),
            record("a2", Some("u2"), "乙", None, true),
        ];
        let exported =
            select_export_records(&accounts, &["a1".to_string(), "missing".to_string()]).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0]["id"], "a1");
        assert_eq!(exported[0]["access_token"], "token-a1");
    }

    #[test]
    fn exported_records_roundtrip_import() {
        let accounts = vec![json!({
            "id": "a1",
            "uid": "u1",
            "nickname": "甲",
            "email": "a@b.c",
            "access_token": "tok-1",
            "refresh_token": "ref-1",
            "auth_raw": { "k": "v" },
            "createdAt": 1,
        })];
        let exported = select_export_records(&accounts, &["a1".to_string()]).unwrap();
        let text = serde_json::to_string(&exported).unwrap();
        let mut target: Vec<Value> = vec![];
        let result = merge_import_records(&mut target, &text, &[0]).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(target.len(), 1);
        assert_eq!(
            target[0]["access_token"], "tok-1",
            "round-trip 保留 access_token"
        );
        assert_eq!(target[0]["refresh_token"], "ref-1");
        assert_eq!(target[0]["auth_raw"]["k"], "v");
    }

    #[test]
    fn preview_exposes_only_desensitized_fields() {
        let text = r#"[{
            "id": "a1",
            "uid": "u1",
            "nickname": "小明",
            "email": "x@y.z",
            "access_token": "SECRET"
        }]"#;
        let preview = preview_accounts(text).unwrap();
        assert_eq!(preview["total"], 1);
        assert_eq!(preview["accounts"][0]["index"], 0);
        assert_eq!(preview["accounts"][0]["uid"], "u1");
        assert_eq!(preview["accounts"][0]["nickname"], "小明");
        assert_eq!(preview["accounts"][0]["email"], "x@y.z");
        assert_eq!(preview["accounts"][0]["hasToken"], true);
        assert!(
            preview["accounts"][0].get("access_token").is_none(),
            "预览不得泄露 token"
        );
    }

    /// 回归 issue #100：预览的对象字段必须折叠为 null（防 React error #31 白屏），
    /// 信封凭据标记 encrypted，明文凭据不标记。
    #[test]
    fn preview_folds_object_fields_and_marks_envelope_token() {
        let text = r#"[
            {
                "id": "env-1",
                "uid": "u-env",
                "nickname": {"$wbEncrypted": 1, "envelope": "nick"},
                "email": {"$wbEncrypted": 1, "envelope": "mail"},
                "access_token": {"$wbEncrypted": 1, "envelope": "a"}
            },
            {
                "id": "plain-1",
                "uid": "u-plain",
                "nickname": "明文昵称",
                "email": "x@y.z",
                "access_token": "SECRET"
            }
        ]"#;
        let preview = preview_accounts(text).unwrap();

        assert_eq!(preview["total"], 2);
        let env = &preview["accounts"][0];
        assert_eq!(env["uid"], "u-env");
        assert!(
            env["nickname"].is_null(),
            "信封 nickname 必须折叠为 null，不得透传对象"
        );
        assert!(
            env["email"].is_null(),
            "信封 email 必须折叠为 null，不得透传对象"
        );
        assert_eq!(env["hasToken"], true, "信封凭据应视为有凭据");
        assert_eq!(env["encrypted"], true, "信封凭据应标记 encrypted");
        assert!(env.get("access_token").is_none(), "预览不得泄露 token");

        let plain = &preview["accounts"][1];
        assert_eq!(plain["nickname"], "明文昵称");
        assert_eq!(plain["email"], "x@y.z");
        assert_eq!(plain["hasToken"], true);
        assert_eq!(plain["encrypted"], false, "明文凭据不得误标 encrypted");
    }

    #[test]
    fn export_file_name_rejects_traversal_and_non_json() {
        assert!(validate_export_file_name("../../etc/passwd.json").is_err());
        assert!(validate_export_file_name("a/b.json").is_err());
        assert!(validate_export_file_name("a\\b.json").is_err());
        assert!(validate_export_file_name("out.txt").is_err());
        assert!(validate_export_file_name("").is_err());
        assert!(validate_export_file_name("wb-switch-accounts-2026-08-21.json").is_ok());
    }

    #[test]
    fn export_path_validation() {
        assert!(
            validate_export_path("relative/out.json").is_err(),
            "必须绝对路径"
        );
        // 绝对路径的形态按平台不同（Windows 需盘符或 UNC 前缀），fixture 必须取自
        // 当前平台：写死 `/tmp/...` 会在 Windows 上被判为相对路径而误报。
        let dir = std::env::temp_dir();
        let txt = dir.join("out.txt").to_string_lossy().to_string();
        let upper = dir.join("out.JSON").to_string_lossy().to_string();
        let json = dir.join("out.json").to_string_lossy().to_string();
        assert!(validate_export_path(&txt).is_err(), "必须 .json");
        assert!(validate_export_path(&upper).is_ok(), "扩展名不区分大小写");
        assert!(validate_export_path(&json).is_ok());
    }

    #[test]
    fn export_accounts_to_file_writes_json_with_tokens() {
        let dir = std::env::temp_dir();
        let file_name = format!(
            "wb-switch-accounts-test-{}.json",
            uuid::Uuid::new_v4().simple()
        );
        let records = vec![record("a1", Some("u1"), "甲", None, true)];
        let path = write_records_to_file(&dir, &records, &file_name).unwrap();
        let written: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0]["access_token"], "token-a1", "导出文件保留 token");
    }

    #[test]
    fn write_records_to_file_rejects_bad_name() {
        let dir = std::env::temp_dir();
        let records = vec![record("a1", Some("u1"), "甲", None, true)];
        assert!(write_records_to_file(&dir, &records, "../escape.json").is_err());
        assert!(write_records_to_file(&dir, &records, "no-ext").is_err());
    }

    /// 导入备份：记录未携带本地展示字段（备注 / 显示选择）时保留本地值。
    #[test]
    fn merge_preserves_local_display_fields_when_absent() {
        let mut local = record("local", Some("u1"), "旧名称", None, true);
        local["note"] = json!("本地备注");
        local["displayField"] = json!("note");
        let mut accounts = vec![local];

        let text = r#"[{ "uid": "u1", "nickname": "新名称", "access_token": "tok-new" }]"#;
        merge_import_records(&mut accounts, text, &[0]).unwrap();
        assert_eq!(accounts[0]["note"], "本地备注");
        assert_eq!(accounts[0]["displayField"], "note");
        assert_eq!(accounts[0]["nickname"], "新名称");
    }

    /// 导入备份：记录自带展示字段时以导入为准（导入即恢复当时的展示配置）。
    #[test]
    fn merge_uses_imported_display_fields_when_present() {
        let mut local = record("local", Some("u1"), "旧名称", None, true);
        local["note"] = json!("本地备注");
        local["displayField"] = json!("note");
        let mut accounts = vec![local];

        let text = r#"[{ "uid": "u1", "nickname": "新名称", "access_token": "tok-new",
            "note": "备份备注", "displayField": "phone" }]"#;
        merge_import_records(&mut accounts, text, &[0]).unwrap();
        assert_eq!(accounts[0]["note"], "备份备注");
        assert_eq!(accounts[0]["displayField"], "phone");
    }

    // ---- 加密备份（口令 + 自动识别）----

    /// 测试口令动态生成（避免字面量口令被 CodeQL 判为硬编码加密值）。
    fn backup_password(tag: &str) -> String {
        format!("{tag}-pw-{}", std::process::id())
    }

    fn write_backup(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn encrypted_backup_requires_password_then_previews() {
        let dir = std::env::temp_dir().join(format!("wb-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plaintext =
            serde_json::to_string_pretty(&vec![record("a1", Some("u1"), "甲", None, true)])
                .unwrap();
        let envelope =
            crate::modules::encrypted_backup::encrypt(&plaintext, &backup_password("ok")).unwrap();
        let path = write_backup(&dir, "backup-enc.json", &envelope);

        // 不给口令：明确提示，而不是报「JSON 非法」
        let error = preview_accounts_file_auto(&path, None).unwrap_err();
        assert_eq!(error, "该备份已加密，请输入口令");

        // 口令错：统一文案
        let error =
            preview_accounts_file_auto(&path, Some(backup_password("bad").as_str())).unwrap_err();
        assert_eq!(error, "口令错误或文件已损坏");

        // 口令对：能解析出账号
        let preview = preview_accounts_file_auto(&path, Some(&backup_password("ok"))).unwrap();
        assert_eq!(preview["total"], 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plaintext_backup_still_works_with_auto_import() {
        let dir = std::env::temp_dir().join(format!("wb-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plaintext =
            serde_json::to_string_pretty(&vec![record("a2", Some("u2"), "乙", None, true)])
                .unwrap();
        let path = write_backup(&dir, "backup-plain.json", &plaintext);
        let preview = preview_accounts_file_auto(&path, None).unwrap();
        assert_eq!(preview["total"], 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_encrypted_backup_is_rejected_before_decrypt() {
        // 体积闸门必须在解密**之前**生效：否则一个 200MB 的加密文件会先被读进内存、
        // 再解密膨胀，形成内存放大攻击面。
        let dir = std::env::temp_dir().join(format!("wb-enc-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(65 * 1024 * 1024).unwrap();
        drop(file);

        let error =
            preview_accounts_file_auto(&path.to_string_lossy(), Some("pw-123456")).unwrap_err();
        assert!(error.contains("文件过大"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encrypted_export_rejects_empty_password() {
        let dir = std::env::temp_dir().join(format!("wb-enc2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.json").to_string_lossy().into_owned();
        let error =
            export_accounts_encrypted_to_path(&["missing".to_string()], &target, "").unwrap_err();
        assert_eq!(error, "口令不能为空");
        std::fs::remove_dir_all(&dir).ok();
    }
}
