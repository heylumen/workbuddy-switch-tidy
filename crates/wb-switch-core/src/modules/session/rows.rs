//! 会话数据库的行级操作：备份、读取源行、插入副本行、列探测与写入校验。
//!
//! 从 `session.rs` 拆出（4.2 拆分第 4 步，纯搬移、行为不变）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::identity::is_claw_workspace;
use super::store::{open_db, table_exists};
use super::{snapshot_workbuddy_db, SessionPaths};
use crate::modules::config::now_ms;
use crate::modules::session_backup;

/// 备份 workbuddy.db（一致性快照），返回主库备份路径。
///
/// 复制允许在非当前登录账号运行时发生，顺序复制主库/WAL/SHM 无法保证同一时点；
/// 这里与同步路径共用 SQLite Backup API，失败即中止写入。
pub(crate) fn backup_workbuddy_db(
    paths: &SessionPaths,
    backup_root: &Path,
) -> Result<PathBuf, String> {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return Err("会话数据不存在，未复制".to_string());
    }
    std::fs::create_dir_all(backup_root).map_err(|error| format!("备份目录创建失败：{error}"))?;
    let dest = backup_root.join("workbuddy.db");
    snapshot_workbuddy_db(&db, &dest)?;
    Ok(dest)
}

/// 数据库插入结果：`No*` 与 `SourceRowMissing` 都不允许被当成成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbCopyOutcome {
    Inserted,
    SourceRowMissing,
    NoSessionsTable,
    NoDb,
}

/// 会话行归属（未删除时）。
pub(crate) fn session_row_owner(paths: &SessionPaths, cid: &str) -> Option<String> {
    let db = paths.workbuddy_db();
    let conn = open_db(&db, true)?;
    conn.query_row(
        "SELECT user_id FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
        [cid],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// 在 workbuddy.db 中把源会话行复制为新 id（动态列，覆盖 id/user_id/updated_at，
/// created_at 透传源值）。
///
/// 跨档时源行与目标行分属两个库：源行从 `source_paths` 读取（跨档用只读连接），
/// 目标行按**目标表列序取交集**写入——目标存在而源没有的列不写，由库默认值补齐，
/// 不写 NULL 破坏约束（design §2.3；旧版客户端列少时靠这条兼容）。
/// 与旧实现不同：db/表/源行缺失都显式返回，不再静默 Ok。
pub(crate) fn insert_session_copy(
    source_paths: &SessionPaths,
    target_paths: &SessionPaths,
    new_cid: &str,
    cid: &str,
    source_uid: &str,
    target_uid: &str,
) -> Result<DbCopyOutcome, String> {
    let dst_db = target_paths.workbuddy_db();
    if !dst_db.is_file() {
        return Ok(DbCopyOutcome::NoDb);
    }
    let src_db = source_paths.workbuddy_db();
    let same_db = src_db == dst_db;
    let source_row = if src_db.is_file() {
        // 同档沿用读写连接（与改造前一致）；跨档对源档只读，不触碰源档 WAL 恢复。
        let Some(conn) = open_db(&src_db, !same_db) else {
            return Err("会话数据无法打开".to_string());
        };
        if !table_exists(&conn, "sessions") {
            return Ok(DbCopyOutcome::NoSessionsTable);
        }
        read_source_session_row(&conn, cid, source_uid)?
    } else {
        None
    };
    let Some(source_row) = source_row else {
        return Ok(DbCopyOutcome::SourceRowMissing);
    };

    let Some(conn) = open_db(&dst_db, false) else {
        return Err("会话数据无法打开".to_string());
    };
    if !table_exists(&conn, "sessions") {
        return Ok(DbCopyOutcome::NoSessionsTable);
    }
    // 写事务的提交必须可靠持久：在本次实际写连接上确认 synchronous ≥ FULL。
    session_backup::ensure_full_synchronous(&conn)?;
    let target_cols = session_table_columns(&conn)?;

    let mut cols: Vec<String> = Vec::with_capacity(target_cols.len());
    let mut vals: Vec<rusqlite::types::Value> = Vec::with_capacity(target_cols.len());
    for col in &target_cols {
        let Some((_, value)) = source_row.iter().find(|(name, _)| name == col) else {
            continue;
        };
        let value = value.clone();
        if col == "cwd" {
            if let rusqlite::types::Value::Text(ref path) = value {
                if is_claw_workspace(path) {
                    return Err("Claw 工作区绑定当前账号渠道，不支持复制".to_string());
                }
            }
        }
        let value = match col.as_str() {
            "id" => rusqlite::types::Value::Text(new_cid.to_string()),
            "user_id" => rusqlite::types::Value::Text(target_uid.to_string()),
            // created_at 是会话历史起点，复制不伪造：源值原样透传，只有源值为 NULL
            // （旧库没记）时才回退到当前时间，避免把 NULL 下传破坏客户端的时间排序。
            "created_at" => match value {
                rusqlite::types::Value::Null => rusqlite::types::Value::Integer(now_ms()),
                source => source,
            },
            // updated_at 恒写当前时间：它驱动「最近更新」排序，副本沿用源值会直接沉底，
            // 而且副本此刻确实刚被创建。
            "updated_at" => rusqlite::types::Value::Integer(now_ms()),
            "deleted_at" => rusqlite::types::Value::Null,
            // 项目归属属于源账号云端数据，不能跨账号继承；否则目标客户端按项目分组
            // 渲染时查不到该项目，会把整组会话静默丢弃（issue #200）。
            "project_id" => rusqlite::types::Value::Null,
            // 空间分组（WorkBuddy 5.11+ 的 spaces / space_folders，spaces 带 user_id）
            // 同样按账号隔离：沿用源 space_id 会让目标客户端把副本归到不存在的分组，
            // 表现为分组错乱、新建任务后从分组里消失（issue #219）。归一为 NULL 即默认分组
            // （本机实测 2,507/3,284 会话本就是 NULL），用户可在目标端重新分组。
            "space_id" => rusqlite::types::Value::Null,
            // 副本不是运行中的实例：terminated/working 等状态会让客户端归入非默认分类，
            // 归一为 completed，保留已有的终态/空闲态。
            "status" => match value {
                rusqlite::types::Value::Text(state)
                    if !matches!(state.as_str(), "archived" | "completed" | "active" | "idle") =>
                {
                    rusqlite::types::Value::Text("completed".to_string())
                }
                other => other,
            },
            // 客户端排序键为 COALESCE(last_activity_at, created_at)，源值为 NULL 时会沉底。
            "last_activity_at" => match value {
                rusqlite::types::Value::Null => rusqlite::types::Value::Integer(now_ms()),
                other => other,
            },
            // 客户端按 mode 渲染，缺字段会退化。
            "source_mode" => match value {
                rusqlite::types::Value::Null => rusqlite::types::Value::Text("craft".to_string()),
                other => other,
            },
            _ => value,
        };
        cols.push(col.clone());
        vals.push(value);
    }

    let placeholders = cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let colnames = cols.join(", ");
    // 用 INSERT 而不是 INSERT OR REPLACE：新 UUID 撞库时宁可失败，也不能悄悄覆盖既有会话。
    let sql = format!("INSERT INTO sessions ({colnames}) VALUES ({placeholders})");
    let params: Vec<&rusqlite::types::Value> = vals.iter().collect();
    conn.execute(&sql, rusqlite::params_from_iter(params))
        .map_err(|e| format!("会话记录保存失败：{e}"))?;
    Ok(DbCopyOutcome::Inserted)
}

/// 读取一行会话（列名 + 值，按源表列序），供跨库复制按目标列取交集。
pub(crate) fn read_source_session_row(
    conn: &Connection,
    cid: &str,
    uid: &str,
) -> Result<Option<Vec<(String, rusqlite::types::Value)>>, String> {
    let mut stmt = conn
        .prepare("SELECT * FROM sessions WHERE id = ?1 AND user_id = ?2")
        .map_err(|e| e.to_string())?;
    let cols: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
    let mut rows = stmt
        .query(rusqlite::params![cid, uid])
        .map_err(|e| e.to_string())?;
    let Some(row) = rows.next().map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let values = cols
        .into_iter()
        .enumerate()
        .map(|(i, col)| {
            let value = row
                .get::<_, rusqlite::types::Value>(i)
                .unwrap_or(rusqlite::types::Value::Null);
            (col, value)
        })
        .collect();
    Ok(Some(values))
}

/// 目标 sessions 表的列名（按表自身列序）。
pub(crate) fn session_table_columns(conn: &Connection) -> Result<Vec<String>, String> {
    let stmt = conn
        .prepare("SELECT * FROM sessions LIMIT 0")
        .map_err(|e| e.to_string())?;
    Ok(stmt.column_names().map(|s| s.to_string()).collect())
}

/// 写后校验：目标行必须存在、归属目标账号且未删除。
pub(crate) fn verify_session_row(
    paths: &SessionPaths,
    new_cid: &str,
    target_uid: &str,
) -> Result<(), String> {
    match session_row_owner(paths, new_cid) {
        Some(owner) if owner == target_uid => Ok(()),
        Some(owner) => Err(format!(
            "会话记录归属校验失败：期望 {target_uid}，实际 {owner}"
        )),
        None => Err("会话记录保存后不可见，未按成功处理".to_string()),
    }
}
