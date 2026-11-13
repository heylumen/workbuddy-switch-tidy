use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use crate::modules::auth_file;
use crate::modules::variant::WbVariant;

/// 打开数据库并设置 busy_timeout（对照 Python `sqlite3.connect(timeout=5)`）。
pub(crate) fn open_db(path: &Path, read_only: bool) -> Option<Connection> {
    let conn = if read_only {
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?
    } else {
        Connection::open(path).ok()?
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    Some(conn)
}

/// 客户端数据根下的会话数据库（按档位取根）。
pub fn workbuddy_db_path(variant: WbVariant) -> PathBuf {
    variant.data_root().join("workbuddy.db")
}

/// 映射库文件名解析：`edge-sync-mapping.db` 记 0，`edge-sync-mapping-vN.db` 记 N
/// （N 为整数）。其它名字（含 `-shm` / `-wal` 伴生文件，它们不以 `.db` 结尾）返回 None。
pub(crate) fn edge_sync_db_version(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let middle = name
        .strip_prefix("edge-sync-mapping")?
        .strip_suffix(".db")?;
    if middle.is_empty() {
        return Some(0);
    }
    let digits = middle.strip_prefix("-v")?;
    // `u64::parse` also accepts a leading `+`; WorkBuddy's filename contract is
    // digits only, so reject non-canonical names instead of treating them as candidates.
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// 没有任何候选时返回的默认文件名（保留各档位既有的「云端映射库 xxx 不存在」文案）。
fn edge_sync_db_default_name(variant: WbVariant) -> &'static str {
    match variant {
        WbVariant::Cn => "edge-sync-mapping-v2.db",
        WbVariant::Ai => "edge-sync-mapping-v4.db",
    }
}

/// 云端映射库解析：扫描数据根下所有 `edge-sync-mapping*.db`，返回版本号最大的一个。
///
/// 写死任何版本都会再次失效：WorkBuddy 客户端自行演进文件名，本机实测 v2（迁移残留）、
/// v3、v4 并存，而 v3 从未出现在本工具任何代码历史里（2026-09 实测）。
/// 判据不用 mtime——本工具自身写入会刷新 mtime，按它选会自我强化错误结果；
/// 也不用行数——需逐个打开数据库，还要处理损坏与锁。两个档位走同一套发现逻辑。
/// 目录不存在、读取失败或没有任何候选时回落到默认文件名，不得 panic。
pub(crate) fn edge_sync_db_path(root: &Path, variant: WbVariant) -> PathBuf {
    let best = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let version = edge_sync_db_version(&path)?;
            path.is_file().then_some((version, path))
        })
        // 同版本号时按路径定序，保证结果与目录遍历顺序无关。
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_path().cmp(b.1.as_path())));
    match best {
        Some((_, path)) => path,
        None => root.join(edge_sync_db_default_name(variant)),
    }
}

/// 会话复制能力探测：数据根同时具备 `projects/` 目录与 `workbuddy.db` 的
/// `sessions` 表才算可用（design D6）。
///
/// 为什么必须探测而不是按档位写死：国际版数据根与国内版**不同构**——本机实测
/// 国际版数据根下没有 `projects/`、edge-sync 为 v4。若直接套用国内版假设，
/// 会写出「有 db 记录但没有正文」的半成品会话。
///
/// 纯函数，接受根路径参数以便用临时目录做单元测试。
pub fn session_copy_supported_at(root: &Path) -> bool {
    if !root.join("projects").is_dir() {
        return false;
    }
    let db = root.join("workbuddy.db");
    if !db.is_file() {
        return false;
    }
    let Some(conn) = open_db(&db, true) else {
        return false;
    };
    table_exists(&conn, "sessions")
}

/// 当前认证账号的 uid（该档位认证文件的 account.uid）。
pub fn current_user_uid(variant: WbVariant) -> Option<String> {
    current_user_uid_at(&variant.auth_file_path())
}

/// 从指定登录态文件读取 uid（单测注入临时文件用）。
pub fn current_user_uid_at(auth_file: &Path) -> Option<String> {
    let auth = auth_file::read_auth_file_at(auth_file)?;
    auth.get("account")
        .and_then(|a| a.get("uid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub(crate) fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        == 1
}

pub(crate) fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let names: Vec<String> = iter.flatten().collect();
    names.iter().any(|name| name == column)
}

pub(crate) fn nonempty_text(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
