//! 跨账号「同一会话」的等价判定：目标账号里是否已存在与源副本逐字等价的会话。
//!
//! 从 `session.rs` 拆出（4.2 拆分第 3 步，纯搬移、行为不变）。
//! 依赖：目标档位数据库（只读）+ 项目正文索引 + identity 的归一函数。

use std::path::Path;

use super::identity::{
    index_project_jsonls_in, is_claw_workspace, normalize_cwd, normalize_jsonl,
    session_display_title,
};
use super::store::{column_exists, open_db, table_exists};
use super::CopyContext;
use crate::modules::session_link;

/// 目标账号是否已存在与源会话「等价」的会话。
///
/// 等价判定：cwd 一致，且正文（抹平各自 id 后）一致。仅当双方都有正文时
/// 做内容比对；若双方都无正文（空会话），退化为「同 cwd + 同展示标题」判定，
/// 仅在目标也无正文时才跳过，避免误跳正常会话。
///
/// **失败即阻止**：目标数据库、正文索引或任一候选正文无法读取时返回 `Err`，
/// 由复制入口终止本次复制；绝不以“无法证明重复”为理由新建副本。
#[allow(clippy::too_many_arguments)] // 参数语义独立；抽结构体会牵动全部调用点
pub(crate) fn target_has_equivalent_session(
    db: &Path,
    _source_cid: &str,
    _source_uid: &str,
    target_uid: &str,
    source_cwd: &str,
    source_title: &str,
    source_jsonl_norm: &str,
    source_has_jsonl: bool,
) -> Result<Option<String>, String> {
    let conn = open_db(db, true).ok_or_else(|| "无法打开目标会话数据库，已阻止复制".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("目标会话数据库缺少 sessions 表，已阻止复制".to_string());
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, cwd, title, custom_title FROM sessions \
             WHERE user_id = ?1 AND deleted_at IS NULL",
        )
        .map_err(|_| "无法读取目标会话列表，已阻止复制".to_string())?;
    let rows = stmt
        .query_map([target_uid], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|_| "无法读取目标会话列表，已阻止复制".to_string())?;

    let data_root = db.parent().unwrap_or_else(|| Path::new("."));
    let index = index_project_jsonls_in(&data_root.join("projects"));
    for r in rows {
        let (cid, cwd, title, custom_title) =
            r.map_err(|_| "目标会话记录读取失败，已阻止复制".to_string())?;
        let cid = cid.unwrap_or_default();
        let cwd = cwd.unwrap_or_default();
        if is_claw_workspace(&cwd) {
            continue;
        }
        if normalize_cwd(&cwd) != normalize_cwd(source_cwd) {
            continue;
        }
        let target_has_jsonl = index.contains_key(&cid);
        if source_has_jsonl && target_has_jsonl {
            if let Some(p) = index.get(&cid) {
                let text = std::fs::read_to_string(p)
                    .map_err(|_| "目标会话正文不可读，已阻止复制".to_string())?;
                if normalize_jsonl(&text, &cid) == source_jsonl_norm {
                    return Ok(Some(cid.clone()));
                }
            }
        } else if !source_has_jsonl
            && !target_has_jsonl
            && session_display_title(title, custom_title) == source_title
        {
            return Ok(Some(cid.clone()));
        }
    }
    Ok(None)
}

/// 目标账号是否已存在与源会话「逐字等价」的会话；有则返回其 id。
///
/// 本 fork 的内容级去重：上游的「已关联则跳过」依赖关联组表，若某会话没有关联记录
/// （旧版本复制留下的副本、关联表缺失等），上游会再次新建副本导致重复行累积。
/// 这里按「同 cwd + 同标题 + 正文抹平各自 id 后一致」再判一次，命中即跳过新增。
///
/// 跨档复制时必须从**源档**数据库读取源 cwd/title，不能用目标档数据库。
/// 正文使用调用方已经校验过的源快照文本，避免二次读取产生 TOCTOU。
pub(crate) fn equivalent_target_session_id(
    context: &CopyContext,
    cid: &str,
    snapshot: &session_link::ContentSnapshot,
) -> Result<Option<String>, String> {
    if context.paths.data_root.as_os_str().is_empty()
        || context.source_paths.data_root.as_os_str().is_empty()
    {
        return Err("无法确定会话数据目录，已阻止复制".to_string());
    }
    let db = context.paths.data_root.join("workbuddy.db");
    let source_db = context.source_paths.data_root.join("workbuddy.db");
    let conn =
        open_db(&source_db, true).ok_or_else(|| "无法打开源会话数据库，已阻止复制".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("源会话数据库缺少 sessions 表，已阻止复制".to_string());
    }
    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let sql = if has_custom {
        "SELECT cwd, title, custom_title FROM sessions WHERE id = ?1 AND user_id = ?2"
    } else {
        "SELECT cwd, title, NULL FROM sessions WHERE id = ?1 AND user_id = ?2"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|_| "无法读取源会话元数据，已阻止复制".to_string())?;
    let (cwd, title, custom_title) = stmt
        .query_row([cid, context.source_uid], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|_| "源会话元数据不存在，已阻止复制".to_string())?;
    let source_cwd = cwd.unwrap_or_default();
    let source_title = session_display_title(title, custom_title);
    let source_norm = normalize_jsonl(&snapshot.text, cid);
    target_has_equivalent_session(
        &db,
        cid,
        context.source_uid,
        context.target_uid,
        &source_cwd,
        &source_title,
        &source_norm,
        true,
    )
}

/// 目标账号中是否存在与 (归一化 cwd, 展示标题) 相同的未删除会话。
///
/// 用于切换复制时的「按标题 upsert」：只要同工作区下同名，即视为同一会话，
/// 据此把内容同步为源端最新，而非新增重复行。返回其 `(cid, updated_at)`；
/// 不存在或读取失败返回 `None`。
#[allow(dead_code)] // 复制路径的事后查重已被上游幂等复制取代（保留以备回归）
pub(crate) fn find_target_session_by_cwd_title(
    db: &Path,
    target_uid: &str,
    norm_cwd: &str,
    display_title: &str,
) -> Option<(String, i64)> {
    let conn = open_db(db, true)?;
    if !table_exists(&conn, "sessions") {
        return None;
    }
    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let sql = if has_custom {
        "SELECT id, updated_at, cwd, title, custom_title FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL"
    } else {
        "SELECT id, updated_at, cwd, title, NULL FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL"
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return None;
    };
    let Ok(rows) = stmt.query_map([target_uid], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    }) else {
        return None;
    };
    for r in rows.flatten() {
        let (cid, ua, cwd, title, custom_title) = r;
        let cid = cid.unwrap_or_default();
        let cwd = cwd.unwrap_or_default();
        if is_claw_workspace(&cwd) {
            continue;
        }
        if normalize_cwd(&cwd) != norm_cwd {
            continue;
        }
        if session_display_title(title, custom_title) == display_title {
            return Some((cid, ua.unwrap_or(0)));
        }
    }
    None
}
