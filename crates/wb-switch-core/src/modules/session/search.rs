//! 会话搜索：按标题走数据库 LIKE；正文匹配可选，带上限避免拖死界面。
//!
//! 设计取舍：正文搜索不做索引（会话正文是客户端私有格式，建索引会引入新的失配面），
//! 改为「最近 N 条会话 + 单文件/总量上限」的保守扫描；命中即返回，超限标注 truncated。

use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::variant::WbVariant;

use super::store::open_db;
use super::SessionPaths;

/// LIKE 模式转义：`%` `_` `\\` 都按字面匹配，避免用户输入被当成通配符。
fn like_pattern(raw: &str) -> String {
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// 正文扫描上限。
const BODY_MAX_FILES: usize = 200;
const BODY_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const BODY_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// 搜索会话。
///
/// - `query` 为空直接返回空结果（不扫描）；
/// - `include_body = false` 只匹配标题（快）；
/// - `include_body = true` 额外扫描最近会话正文，命中即标 `body`。
pub fn search_sessions(variant: WbVariant, query: &str, limit: usize, include_body: bool) -> Value {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return json!({ "ok": true, "items": [], "total": 0, "bodyScanned": 0, "truncated": false });
    }
    let limit = limit.clamp(1, 200);
    let paths = SessionPaths::for_variant(variant);
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return json!({ "ok": false, "reason": "workbuddy.db 不存在", "items": [] });
    }
    let Some(conn) = open_db(&db, true) else {
        return json!({ "ok": false, "reason": "无法打开 workbuddy.db", "items": [] });
    };
    let pattern = like_pattern(trimmed);
    let sql = "SELECT id, user_id, COALESCE(NULLIF(custom_title, ''), title) AS display_title, \
               cwd, COALESCE(last_activity_at, updated_at) AS ts \
               FROM sessions \
               WHERE deleted_at IS NULL \
                 AND (COALESCE(NULLIF(custom_title, ''), title) LIKE ?1 ESCAPE '\\' \
                      OR title LIKE ?1 ESCAPE '\\') \
               ORDER BY ts DESC LIMIT ?2";
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            return json!({ "ok": false, "reason": format!("查询失败：{error}"), "items": [] });
        }
    };
    let rows = stmt.query_map(rusqlite::params![pattern, limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            row.get::<_, Option<i64>>(4)?.unwrap_or(0),
        ))
    });
    let Ok(rows) = rows else {
        return json!({ "ok": false, "reason": "读取结果失败", "items": [] });
    };
    let accounts = account::load_accounts();
    let name_of = |uid: &str| -> Value {
        accounts
            .iter()
            .find(|acc| account::get_str(acc, "uid").as_deref() == Some(uid))
            .map(|acc| {
                json!({
                    "accountId": account::get_str(acc, "id").unwrap_or_default(),
                    "name": account::get_str(acc, "nickname")
                        .or_else(|| account::get_str(acc, "email"))
                        .unwrap_or_else(|| uid.to_string()),
                })
            })
            .unwrap_or_else(|| json!({ "accountId": Value::Null, "name": uid }))
    };

    let mut items: Vec<Value> = Vec::new();
    for row in rows.flatten() {
        let (session_id, uid, title, cwd, ts) = row;
        let owner = name_of(&uid);
        items.push(json!({
            "sessionId": session_id,
            "uid": uid,
            "title": title,
            "cwd": cwd,
            "updatedAt": ts,
            "accountId": owner.get("accountId").cloned().unwrap_or(Value::Null),
            "accountName": owner.get("name").cloned().unwrap_or(Value::Null),
            "match": "title",
        }));
    }

    let mut body_scanned = 0usize;
    let mut truncated = false;
    if include_body {
        // 正文匹配忽略大小写（用户不会记得原始大小写）。
        let needle = trimmed.to_lowercase();
        // 已按标题命中的会话不再扫正文；只补扫描最近未命中的会话。
        let mut seen: std::collections::HashSet<String> = items
            .iter()
            .filter_map(|item| {
                item.get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let projects = paths.projects_dir();
        let mut budget = BODY_MAX_TOTAL_BYTES;
        let mut candidates: Vec<(String, std::path::PathBuf, i64)> = Vec::new();
        if let Ok(dirs) = std::fs::read_dir(&projects) {
            for dir in dirs.flatten() {
                let Ok(files) = std::fs::read_dir(dir.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Ok(meta) = file.metadata() else { continue };
                    if meta.len() > BODY_MAX_FILE_BYTES {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    if seen.contains(stem) {
                        continue;
                    }
                    let ts = meta
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    candidates.push((stem.to_string(), path, ts));
                }
            }
        }
        candidates.sort_by_key(|(_, _, ts)| std::cmp::Reverse(*ts));
        for (stem, path, ts) in candidates {
            if body_scanned >= BODY_MAX_FILES {
                truncated = true;
                break;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > budget {
                truncated = true;
                break;
            }
            budget = budget.saturating_sub(meta.len());
            body_scanned += 1;
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !text.to_lowercase().contains(&needle) {
                seen.insert(stem.clone());
                continue;
            }
            // 正文命中：补齐账号与标题（标题不在 DB 里也给出占位）
            let mut uid = String::new();
            let mut title = String::new();
            if let Ok(mut stmt) = conn.prepare(
                "SELECT user_id, COALESCE(NULLIF(custom_title, ''), title) FROM sessions WHERE id = ?1",
            ) {
                if let Ok(mut rows) = stmt.query(rusqlite::params![stem]) {
                    if let Ok(Some(row)) = rows.next() {
                        uid = row.get::<_, Option<String>>(0).ok().flatten().unwrap_or_default();
                        title = row.get::<_, Option<String>>(1).ok().flatten().unwrap_or_default();
                    }
                }
            }
            let owner = name_of(&uid);
            seen.insert(stem.clone());
            items.push(json!({
                "sessionId": stem,
                "uid": uid,
                "title": title,
                "cwd": "",
                "updatedAt": ts,
                "accountId": owner.get("accountId").cloned().unwrap_or(Value::Null),
                "accountName": owner.get("name").cloned().unwrap_or(Value::Null),
                "match": "body",
            }));
            if items.len() >= limit {
                truncated = true;
                break;
            }
        }
    }

    let total = items.len();
    json!({
        "ok": true,
        "items": items,
        "total": total,
        "bodyScanned": body_scanned,
        "truncated": truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::like_pattern;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("abc"), "%abc%");
        assert_eq!(like_pattern("50%"), "%50\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
    }

    #[test]
    fn empty_query_returns_empty_result() {
        let value = super::search_sessions(crate::modules::variant::WbVariant::Cn, "  ", 20, false);
        assert_eq!(value["ok"], serde_json::json!(true));
        assert_eq!(value["total"], serde_json::json!(0));
    }
}
