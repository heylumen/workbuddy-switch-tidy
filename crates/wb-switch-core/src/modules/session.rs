//! 会话列表与按需复制（路径 B：生成新 id，云端可正常同步）。
//!
//! 对照 server.py `current_user_uid` / `list_sessions_for_user` /
//! `_find_project_jsonl` / `copy_session_to_user` / `_register_edge_sync_mapping` /
//! `copy_sessions_for_switch` / `backup_workbuddy_db` / `workbuddy_db_path`。
//!
//! WorkBuddy 5.x 数据三件套（缺一不可）：
//!   1) 正文：`~/.workbuddy/projects/{workspace}/{cid}.jsonl`（JSONL 含 sessionId 字段）
//!   2) 元数据：`~/.workbuddy/workbuddy.db` sessions 表（id = conversation id = UUID）
//!   3) 云端映射：`~/.workbuddy/edge-sync-mapping-v{N}.db` edge_sync_mapping
//!      （文件名版本号由客户端演进，按最大版本号动态发现）
//!      （session_id=conversation_id，msg_channel=convmsg:{uid} 决定云端归属）
//!      **不由本工具写入**：复制只写 1)、2)，映射行由 edge-sync 扩展在客户端下次
//!      启动迁移副本时自行写入（预写会让客户端判定"已迁移"跳过上传，云端缺会话）。
//!
//! 复制收口（design §4）：所有复制入口统一走 [`copy_sessions_for_switch`]，在同一把
//! 档位操作锁内先恢复未完成操作、再查询关联组；同一逻辑会话只保留一个有效副本，
//! 目标 UUID 在任何副本写入前持久化，任一阶段失败都不报告完整成功，恢复复用同一
//! UUID 且不产生第二个副本。
//!
//! 同步契约（design §5 / §6）：[`session_links_preview`] 只读预览双方共同参与的关联组并
//! 下发预览凭据；[`sync_sessions_for_switch`] 在执行前重新加载账号身份、成员、基线与
//! 正文并逐项核对凭据，然后按「备份 → 正文 → 数据库 → 组表」逐个阶段写入。
//!
//! 写入前置条件（design §5.4）：本次同步的备份必须成功且可核验（唯一目录、数据库
//! 一致性快照、目标正文逐文件备份 + 摘要、恢复清单）。备份不可信时**零写入**：不碰
//! 目标正文、不改数据库、不提交基线。任一阶段中断都保留未完成操作，下次切换按同一份
//! 清单补完（复用同一目标 UUID 与新基线引用），不把未完成的写入报告成 `synced`。

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::modules::account;
use crate::modules::config::now_ms;
use crate::modules::error_log;
use crate::modules::process;
use crate::modules::session_backup::{
    self, BackupLifecycle, CleanupOutcome, CLEANUP_STATE_SAFE_TERMINATED,
    OPERATION_LIFECYCLE_VERSION,
};
use crate::modules::session_link::{
    self, full_digest_of, BaselineState, ContentSnapshot, ContentState, LinkGroup, LinkMember,
    LinkStore, MemberState, NormalizedContent, OpPhase, Operation, OperationMember, PreviewBinding,
    RecoveryIssue, RecoveryReport, StoreState, SyncDecision, SyncMode, SyncVerdict,
    NORMALIZATION_VERSION, OPERATION_VERSION,
};
use crate::modules::session_pinned;
use crate::modules::variant::WbVariant;

mod paths;
pub use paths::*;

mod store;
pub(crate) use store::{column_exists, edge_sync_db_path, nonempty_text, open_db, table_exists};
pub use store::{
    current_user_uid, current_user_uid_at, session_copy_supported_at, workbuddy_db_path,
};

/// 档位不支持会话复制时的统一错误文案。
pub const SESSION_COPY_UNSUPPORTED: &str = "该档位暂不支持会话复制";

/// 目标账号是当前登录账号、且 WorkBuddy 正在运行时的统一错误文案。
///
/// 写"非当前登录账号"的副本实测免关安全（`research/verification-app-running-write.md`
/// 探针 5）：客户端不使用该账号的数据、切号后自然可见；写"当前登录账号"仍必须等
/// App 停止写入——运行实例看不到写入，继续对话会让会话状态分叉。
pub const SESSION_COPY_APP_RUNNING: &str =
    "目标账号正在 WorkBuddy 中使用，已阻止修改会话数据；请先退出 WorkBuddy 后重试";

/// 目标档客户端运行时的写入门禁：仅当目标账号是该档位**当前登录账号**时拦截。
///
/// 无法读取登录态时保守拦截（宁可要求退出，不做无法判定的写入）。
fn app_running_blocks_target_write(
    paths: &SessionPaths,
    variant: WbVariant,
    target_uid: &str,
    is_app_running: &impl Fn(WbVariant) -> bool,
) -> bool {
    if !is_app_running(variant) {
        return false;
    }
    match current_user_uid_at(&paths.auth_file) {
        Some(current) => current == target_uid,
        None => true,
    }
}

/// 操作日志无法解析时的原因前缀（恢复与复制共用，避免漏报后写出第二个副本）。
const UNPARSEABLE_OPERATION_REASON: &str = "操作记录无法解析";

/// 账号对象里的 uid；前后空白视为缺失。
fn account_uid(account: &Value) -> String {
    account
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// WorkBuddy 侧栏展示名：优先 custom_title（用户改名 / 定时任务名），否则 title。
fn session_display_title(title: Option<String>, custom_title: Option<String>) -> String {
    nonempty_text(custom_title)
        .or_else(|| nonempty_text(title))
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// Claw 是账号绑定的 IM 渠道工作区，复制会话行不够，目标账号也用不了。
fn is_claw_workspace(cwd: &str) -> bool {
    cwd.trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("claw"))
}

/// 归一化工作目录：去掉首尾空白与尾部路径分隔符，用于跨副本的 cwd 比对。
fn normalize_cwd(cwd: &str) -> String {
    cwd.trim().trim_end_matches(['/', '\\']).to_string()
}

/// 会话正文去标识：把「本会话自身 id」替换为中性占位符，
/// 使复制产生的不同 id 副本在比对时视为同一份内容。
/// 复制时会把源 cid 替换为新 cid，故正文里出现的是会话自身 id。
const NEUTRAL_ID: &str = "\u{0}WB_SWITCH_NEUTRAL_ID\u{0}";

fn normalize_jsonl(text: &str, cid: &str) -> String {
    text.replace(cid, NEUTRAL_ID)
}

/// 一次性索引指定 `projects/` 下所有 `{cid}.jsonl`，返回 cid -> 路径。
///
/// 查找范围与 `find_project_jsonl` 一致（直接文件 + 各 workspace 子目录）。
/// 多档位/测试注入都必须走这里，避免固定读当前用户主目录而漏掉另一档位的数据。
fn index_project_jsonls_in(projects: &Path) -> std::collections::HashMap<String, PathBuf> {
    let mut map = std::collections::HashMap::new();
    if !projects.is_dir() {
        return map;
    }
    let mut visit = |p: PathBuf| {
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            map.entry(stem.to_string()).or_insert(p);
        }
    };
    if let Ok(entries) = std::fs::read_dir(projects) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                visit(p);
            } else if p.is_dir() {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for f in sub.flatten() {
                        let fp = f.path();
                        if fp.is_file() {
                            visit(fp);
                        }
                    }
                }
            }
        }
    }
    map
}

/// 删除云端映射中指定会话的归属记录（会话被去重软删后清理孤儿映射）。
fn delete_edge_sync_mappings(db_path: &Path, cids: &[String]) {
    if !db_path.is_file() {
        return;
    }
    let Some(conn) = open_db(db_path, false) else {
        return;
    };
    if !table_exists(&conn, "edge_sync_mapping") {
        return;
    }
    for cid in cids {
        let _ = conn.execute(
            "DELETE FROM edge_sync_mapping WHERE session_id = ?1 OR conversation_id = ?1",
            rusqlite::params![cid],
        );
    }
}

/// 目标账号是否已存在与源会话「等价」的会话。
///
/// 等价判定：cwd 一致，且正文（抹平各自 id 后）一致。仅当双方都有正文时
/// 做内容比对；若双方都无正文（空会话），退化为「同 cwd + 同展示标题」判定，
/// 仅在目标也无正文时才跳过，避免误跳正常会话。
///
/// **失败即阻止**：目标数据库、正文索引或任一候选正文无法读取时返回 `Err`，
/// 由复制入口终止本次复制；绝不以“无法证明重复”为理由新建副本。
#[allow(clippy::too_many_arguments)] // 参数语义独立；抽结构体会牵动全部调用点
fn target_has_equivalent_session(
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
fn equivalent_target_session_id(
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
fn find_target_session_by_cwd_title(
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

mod list;
#[cfg(test)]
pub(crate) use list::audit_session_duplicates_for_user_at;
#[cfg(test)]
pub(crate) use list::dedup_sessions_for_user_at;
#[cfg(test)]
pub(crate) use list::hide_duplicate_group_for_user_at;
#[cfg(test)]
pub(crate) use list::list_duplicate_hides_for_user_at;
#[cfg(test)]
pub(crate) use list::list_sessions_for_user_at;
#[cfg(test)]
pub(crate) use list::restore_duplicate_hide_for_user_at;
pub(crate) use list::session_row_for_user;
pub use list::{
    audit_session_duplicates_for_user, collapse_sessions_for_user, dedup_sessions_for_user,
    hide_duplicate_group_for_user, list_duplicate_hides_for_user, list_sessions_for_account,
    list_sessions_for_user, restore_duplicate_hide_for_user,
};

/// 在 `{档位数据根}/projects/{workspace}/{cid}.jsonl` 定位会话正文。
struct ProjectJsonlIndex {
    modified: Option<SystemTime>,
    files: HashMap<String, PathBuf>,
}

static PROJECT_JSONL_INDEX: OnceLock<Mutex<HashMap<PathBuf, ProjectJsonlIndex>>> = OnceLock::new();

fn project_jsonl_for(paths: &SessionPaths, cid: &str) -> Option<PathBuf> {
    let projects = paths.projects_dir();
    if !projects.is_dir() {
        return None;
    }
    let direct = projects.join(format!("{cid}.jsonl"));
    if direct.is_file() {
        return Some(direct);
    }
    let modified = std::fs::metadata(&projects)
        .and_then(|meta| meta.modified())
        .ok();
    let cache = PROJECT_JSONL_INDEX.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = guard.get(&projects) {
        if index.modified == modified {
            if let Some(found) = index.files.get(cid) {
                if found.is_file() {
                    return Some(found.clone());
                }
            }
            // 目录时间戳精度可能不足以反映刚创建/删除的文件：miss 或文件已删时重建一次。
        }
    }
    let mut files = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(&projects) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Ok(children) = std::fs::read_dir(&dir) {
                for child in children.flatten() {
                    let path = child.path();
                    if path.extension().is_some_and(|ext| ext == "jsonl") {
                        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                            files.insert(stem.to_string(), path);
                        }
                    }
                }
            }
        }
    }
    let result = files.get(cid).cloned();
    guard.insert(projects, ProjectJsonlIndex { modified, files });
    // 避免长期运行/多工作区测试把索引无限累积；容量很小，清空后按需重建即可。
    if guard.len() > 32 {
        guard.clear();
    }
    result
}

pub(crate) fn find_project_jsonl(paths: &SessionPaths, cid: &str) -> Option<PathBuf> {
    project_jsonl_for(paths, cid)
}

/// 备份 workbuddy.db（一致性快照），返回主库备份路径。
///
/// 复制允许在非当前登录账号运行时发生，顺序复制主库/WAL/SHM 无法保证同一时点；
/// 这里与同步路径共用 SQLite Backup API，失败即中止写入。
fn backup_workbuddy_db(paths: &SessionPaths, backup_root: &Path) -> Result<PathBuf, String> {
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
enum DbCopyOutcome {
    Inserted,
    SourceRowMissing,
    NoSessionsTable,
    NoDb,
}

/// 会话行归属（未删除时）。
fn session_row_owner(paths: &SessionPaths, cid: &str) -> Option<String> {
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
fn insert_session_copy(
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
fn read_source_session_row(
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
fn session_table_columns(conn: &Connection) -> Result<Vec<String>, String> {
    let stmt = conn
        .prepare("SELECT * FROM sessions LIMIT 0")
        .map_err(|e| e.to_string())?;
    Ok(stmt.column_names().map(|s| s.to_string()).collect())
}

/// 写后校验：目标行必须存在、归属目标账号且未删除。
fn verify_session_row(paths: &SessionPaths, new_cid: &str, target_uid: &str) -> Result<(), String> {
    match session_row_owner(paths, new_cid) {
        Some(owner) if owner == target_uid => Ok(()),
        Some(owner) => Err(format!(
            "会话记录归属校验失败：期望 {target_uid}，实际 {owner}"
        )),
        None => Err("会话记录保存后不可见，未按成功处理".to_string()),
    }
}

/// 云端映射登记结果：失败必须上报，不能静默降级成成功。
/// 把勾选的会话复制到目标账号（路径 B）。返回复制报告。
///
/// 档位以**目标账号**自身为准：数据根、数据库、备份目录、认证文件都取该档位。
/// 国际版能力不满足时直接返回明确错误，绝不写半成品（design D6）。
/// App 正在运行时拒绝写入：独立复制 API 与桌面端共用同一条生命周期保护。
pub fn copy_sessions_for_switch(
    target_acc: &Value,
    session_ids: &[String],
) -> Result<Value, String> {
    let variant = account::variant_of(target_acc);
    let paths = SessionPaths::for_variant(variant);
    copy_sessions_for_switch_at(
        &paths,
        variant,
        target_acc,
        session_ids,
        process::is_workbuddy_running,
    )
}

/// 把勾选的会话从**显式源账号**复制到**显式目标账号**（跨档复制入口，design §2.1）。
///
/// 与 [`copy_sessions_for_switch`] 的差异只在来源判定：源 uid 取自 `source_acc`，
/// 不再从目标档登录态读取；源档与目标档可为不同档位。写入侧仍以目标账号为准
/// （数据根、数据库、映射库、备份目录），源档只读。
pub fn copy_sessions_cross(
    source_acc: &Value,
    target_acc: &Value,
    session_ids: &[String],
) -> Result<Value, String> {
    let source_variant = account::variant_of(source_acc);
    let target_variant = account::variant_of(target_acc);
    let source_paths = SessionPaths::for_variant(source_variant);
    let target_paths = SessionPaths::for_variant(target_variant);
    let source_uid = account_uid(source_acc);
    if source_uid.is_empty() {
        return Err("源账号缺少 uid，无法复制会话".to_string());
    }
    let target_uid = account_uid(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法复制会话".to_string());
    }
    let source = CopySide {
        paths: &source_paths,
        variant: source_variant,
        uid: source_uid,
        account_id: nonempty_text(
            source_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
    };
    let target = CopySide {
        paths: &target_paths,
        variant: target_variant,
        uid: target_uid,
        account_id: nonempty_text(
            target_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
    };
    copy_sessions_cross_at(
        source,
        target,
        session_ids,
        process::is_workbuddy_running,
        SessionPaths::for_variant,
    )
}

/// 可注入路径与「App 是否运行」探针的跨档复制入口（单测注入临时双档目录与假探针，
/// 不触碰真实路径、不探测真实进程）。
fn copy_sessions_cross_at(
    source: CopySide<'_>,
    target: CopySide<'_>,
    session_ids: &[String],
    is_app_running: impl Fn(WbVariant) -> bool,
    resolve_source_paths: impl Fn(WbVariant) -> SessionPaths,
) -> Result<Value, String> {
    // 快拒只针对**目标档**（写侧）：源档只读，运行中的源客户端不影响读取安全；
    // 目标账号不是当前登录账号时免关直接写（实测，见门禁函数注释）。
    if app_running_blocks_target_write(target.paths, target.variant, &target.uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    // 源档也要读（正文与源行），国际版数据根不同构时同样不支持（design D6）。
    ensure_session_copy_supported(source.paths, source.variant)?;
    ensure_session_copy_supported(target.paths, target.variant)?;
    if source.variant == target.variant && source.uid == target.uid {
        return Err("源账号与目标账号相同，无需复制会话".to_string());
    }
    run_copy(
        &source,
        &target,
        session_ids,
        is_app_running,
        resolve_source_paths,
    )
}

/// 可注入路径与「App 是否运行」探针的同档复制入口（单测注入临时目录与假探针，不触碰
/// 真实路径、不探测真实进程）。
///
/// App 运行检查做两次：拿档位操作锁之前先快速拒绝；拿锁之后再复查——锁前到拿锁之间
/// App 可能被启动，只有锁后复查才能保证会话写入发生在 App 停止写入之后（design §4.1）。
fn copy_sessions_for_switch_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_acc: &Value,
    session_ids: &[String],
    is_app_running: impl Fn(WbVariant) -> bool,
) -> Result<Value, String> {
    // 探测只对国际版生效（design D6 针对的是国际版数据根不同构）。国内版数据根与
    // 改造前同构，保留改造前的路径与返回结构，不让国内版看到「暂不支持」类新文案。
    ensure_session_copy_supported(paths, variant)?;
    let target_uid = account_uid(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法复制会话".to_string());
    }
    let source_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法确定来源账号".to_string())?;
    if source_uid == target_uid {
        return Err("当前账号与目标账号相同，无需复制会话".to_string());
    }
    // 同档路径的源恒为当前登录账号、且上面已拦同账号，因此这里对"目标=当前登录"
    // 恒为放行；保留统一门禁只为与其他写入入口共用同一条规则。
    if app_running_blocks_target_write(paths, variant, &target_uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    let source = CopySide {
        paths,
        variant,
        account_id: account_id_for_uid(paths, &source_uid),
        uid: source_uid,
    };
    let target = CopySide {
        paths,
        variant,
        account_id: nonempty_text(
            target_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
        uid: target_uid,
    };
    run_copy(
        &source,
        &target,
        session_ids,
        is_app_running,
        SessionPaths::for_variant,
    )
}

/// 国际版支持性探测的统一封装：只有国际版需要数据根同构检查（design D6）。
fn ensure_session_copy_supported(paths: &SessionPaths, variant: WbVariant) -> Result<(), String> {
    if variant == WbVariant::Ai && !session_copy_supported_at(&paths.data_root) {
        return Err(format!(
            "{SESSION_COPY_UNSUPPORTED}（档位 {}）",
            variant.as_str()
        ));
    }
    Ok(())
}

/// 从关联组提取 (源 uid → 目标 uid) 的 session_id 映射；跨档按成员 variant 不参与。
fn pinned_pairs_from_store(
    store: &LinkStore,
    source_uid: &str,
    target_uid: &str,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for group in &store.groups {
        let sources: Vec<&str> = group
            .members
            .iter()
            .filter(|m| m.uid == source_uid && m.state == MemberState::Active)
            .map(|m| m.session_id.as_str())
            .collect();
        let targets: Vec<&str> = group
            .members
            .iter()
            .filter(|m| m.uid == target_uid && m.state == MemberState::Active)
            .map(|m| m.session_id.as_str())
            .collect();
        for source in &sources {
            for target in &targets {
                if source != target {
                    let pair = (source.to_string(), target.to_string());
                    if !pairs.contains(&pair) {
                        pairs.push(pair);
                    }
                }
            }
        }
    }
    pairs
}

/// 复制的一方：档位数据根、档位名与账号身份（源侧只读、目标侧写入）。
struct CopySide<'a> {
    paths: &'a SessionPaths,
    variant: WbVariant,
    uid: String,
    /// 账号库中的账号 id（仅作展示，身份判定以 uid 为准）。
    account_id: Option<String>,
}

/// 同档与跨档复制的统一流程：双档锁 → 恢复 → 逐会话复制 → 报告。
///
/// `resolve_source_paths` 用于恢复未完成的**跨档**操作时解析其源档数据根；
/// 同档入口传 [`SessionPaths::for_variant`]，跨档入口可注入与两条 side 一致的映射，
/// 单测由此避免触碰真实目录。
fn run_copy(
    source: &CopySide<'_>,
    target: &CopySide<'_>,
    session_ids: &[String],
    is_app_running: impl Fn(WbVariant) -> bool,
    resolve_source_paths: impl Fn(WbVariant) -> SessionPaths,
) -> Result<Value, String> {
    // 档位操作锁覆盖「恢复 → 查询关联 → 写副本 → 提交关联」全过程，
    // 并发请求与中断重试因此不会各自写出第二个副本。
    // 跨档操作同时持有两档锁，获取顺序固定为枚举序（Cn → Ai），任何代码路径不得
    // 反序，避免两个方向的跨档操作互相等待（design §2.5）。
    let mut _ops_locks = Vec::new();
    for variant in WbVariant::ALL {
        if variant == source.variant {
            _ops_locks.push(session_link::try_acquire_variant_ops_lock(
                source.paths,
                variant,
            )?);
        } else if variant == target.variant {
            _ops_locks.push(session_link::try_acquire_variant_ops_lock(
                target.paths,
                variant,
            )?);
        }
    }
    // 复查：锁前未运行、拿锁后目标账号被登录进 App 的情况在这里被拦住，不写入任何产物。
    if app_running_blocks_target_write(target.paths, target.variant, &target.uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    let recovery = recover_pending_session_operations_at_with(
        target.paths,
        target.variant,
        resolve_source_paths,
    );
    let pending = session_link::pending_operations(target.paths, target.variant);

    let context = CopyContext {
        paths: target.paths,
        variant: target.variant,
        source_paths: source.paths,
        source_variant: source.variant,
        source_uid: &source.uid,
        source_account_id: source.account_id.clone(),
        target_uid: &target.uid,
        target_account_id: target.account_id.clone(),
        pending: &pending,
    };

    let mut copied: Vec<Value> = Vec::new();
    let mut already_linked: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    // 解析失败的操作日志无法对应到具体会话：继续复制会绕过 pending 去重，
    // 可能对同一请求再写出第二个副本。
    if let Some(issue) = recovery
        .needs_recovery
        .iter()
        .find(|issue| !issue.retryable && issue.reason.contains(UNPARSEABLE_OPERATION_REASON))
    {
        for cid in session_ids {
            errors.push(json!({"id": cid, "error": issue.reason.clone()}));
        }
    } else {
        for cid in session_ids {
            match copy_one_session(&context, cid) {
                Ok(CopyOutcome::Copied {
                    new_id,
                    group_id,
                    backup,
                    cleanup_state,
                    cleanup_error,
                }) => {
                    let mut item = json!({
                        "id": cid,
                        "newId": new_id,
                        "groupId": group_id,
                        "backup": backup,
                        "cleanupState": cleanup_state,
                    });
                    if let Some(error) = cleanup_error {
                        item["cleanupError"] = json!(error);
                    }
                    copied.push(item);
                }
                Ok(CopyOutcome::AlreadyLinked {
                    session_id,
                    group_id,
                }) => already_linked.push(json!({
                    "id": cid,
                    "sessionId": session_id,
                    "groupId": group_id,
                })),
                Err(error) => errors.push(json!({"id": cid, "error": error})),
            }
        }
    }

    // 本次请求之后仍存在未完成操作（含本次刚留下的）→ 必须提示恢复需求。
    let unfinished_after = session_link::pending_operations(target.paths, target.variant);
    let unusable = !recovery.is_clean() || !unfinished_after.is_empty();
    let mut report = json!({
        "sourceUid": &source.uid,
        "targetUid": &target.uid,
        "copied": copied,
        "alreadyLinked": already_linked,
    });
    // 跨档复制在报告里带两侧档位；同档报告保持原字段（逐字不变）。
    if source.variant != target.variant {
        report["sourceVariant"] = json!(source.variant.as_str());
        report["targetVariant"] = json!(target.variant.as_str());
    }
    if !errors.is_empty() {
        report["errors"] = json!(errors);
    }
    if unusable {
        report["needsRecovery"] = json!(true);
    }
    // 本轮复制之后再扫一遍：当前项的清理失败/保护残留必须出现在报告里，
    // 不能只用请求开始时的维护快照（否则成功项 pending 只在 copied[] 上）。
    // 维护可能补清成功：成功项上的 pending 路径必须改写成 null，避免虚假可还原位置。
    report["temporaryFiles"] = json!(session_backup::maintain(target.paths, target.variant));
    if let Some(items) = report.get_mut("copied").and_then(Value::as_array_mut) {
        reconcile_reported_cleanup(items);
    }
    let mut pinned_pairs = Vec::new();
    if let Some(items) = report.get("copied").and_then(Value::as_array) {
        for item in items {
            if let (Some(from), Some(to)) = (
                item.get("id").and_then(Value::as_str),
                item.get("newId").and_then(Value::as_str),
            ) {
                pinned_pairs.push((from.to_string(), to.to_string()));
            }
        }
    }
    if let Some(items) = report.get("alreadyLinked").and_then(Value::as_array) {
        for item in items {
            if let (Some(from), Some(to)) = (
                item.get("id").and_then(Value::as_str),
                item.get("sessionId").and_then(Value::as_str),
            ) {
                pinned_pairs.push((from.to_string(), to.to_string()));
            }
        }
    }
    report["pinned"] = session_pinned::sync_pairs(
        &source.paths.data_root,
        &target.paths.data_root,
        &source.uid,
        &target.uid,
        &pinned_pairs,
        &target.paths.store_root,
    );
    Ok(report)
}

/// 复制上下文中不变的输入（避免逐会话重复解析）。
///
/// 源 / 目标解耦：`paths` 恒为目标档（写侧），`source_paths` 恒为源档（读侧）；
/// 同档复制时两者相同，跨档复制时指向不同数据根。
struct CopyContext<'a> {
    paths: &'a SessionPaths,
    variant: WbVariant,
    source_paths: &'a SessionPaths,
    source_variant: WbVariant,
    source_uid: &'a str,
    source_account_id: Option<String>,
    target_uid: &'a str,
    target_account_id: Option<String>,
    pending: &'a [Operation],
}

/// 单个会话的一次复制结果。
enum CopyOutcome {
    Copied {
        new_id: String,
        group_id: String,
        /// 待清理位置（已清理为 None）；仅表示待清理，不是可撤销备份。
        backup: Option<String>,
        cleanup_state: String,
        cleanup_error: Option<String>,
    },
    AlreadyLinked {
        session_id: String,
        group_id: String,
    },
}

/// 目标解析：组内目标账号是否已有可复用的有效副本。
struct TargetResolution {
    group_id: Option<String>,
    existing_link: Option<String>,
    /// 目标账号在同组内被软隐藏的关联副本；复制时必须先恢复，不能新建。
    hidden_aliases: Vec<String>,
}

/// 目标账号在关联组内的 active 成员是否真实有效：正文可验证 + 会话行归属正确。
fn member_is_valid(paths: &SessionPaths, member: &LinkMember) -> bool {
    let Some(body) = find_project_jsonl(paths, &member.session_id) else {
        return false;
    };
    match session_link::read_content_snapshot(&body, &member.session_id) {
        ContentState::Ready(_) => {}
        ContentState::Missing | ContentState::Unavailable(_) => return false,
    }
    session_row_owner(paths, &member.session_id).is_some_and(|owner| owner == member.uid)
}

/// 解析（variant, 来源会话）所属组，以及目标账号是否已有有效副本。
fn resolve_target(context: &CopyContext, cid: &str) -> Result<TargetResolution, String> {
    let store = match session_link::load_store(context.paths) {
        StoreState::Missing => {
            return Ok(TargetResolution {
                group_id: None,
                existing_link: None,
                hidden_aliases: Vec::new(),
            })
        }
        StoreState::Ready(store) => store,
        StoreState::Unavailable(reason) => {
            return Err(format!("{reason}；已阻止复制"));
        }
    };
    // 身份查找按**源档**（跨档组的源成员记源档位，组级 variant 不参与判定）。
    let Some(group) = session_link::find_group_for_identity(
        &store,
        context.source_variant,
        context.source_uid,
        cid,
    ) else {
        return Ok(TargetResolution {
            group_id: None,
            existing_link: None,
            hidden_aliases: Vec::new(),
        });
    };
    let group_id = Some(group.id.clone());
    let hidden_aliases: Vec<String> = group
        .members
        .iter()
        .filter(|member| {
            member.state == session_link::MemberState::Stale
                && member.uid == context.target_uid
                && member.variant.unwrap_or(group.variant) == context.variant
        })
        .map(|member| member.session_id.clone())
        .collect();
    let Some(member) = session_link::active_member_for(group, context.target_uid) else {
        return Ok(TargetResolution {
            group_id,
            existing_link: None,
            hidden_aliases,
        });
    };
    if member_is_valid(context.paths, member) {
        return Ok(TargetResolution {
            group_id,
            existing_link: Some(member.session_id.clone()),
            hidden_aliases: Vec::new(),
        });
    }
    // 失效成员保留记录、不自动复活；存在隐藏别名时由调用方阻止新建。
    Ok(TargetResolution {
        group_id,
        existing_link: None,
        hidden_aliases,
    })
}

/// 目标正文路径：目标档 `projects` 下沿用源文件所在的工作区子目录。
///
/// 客户端按会话行的 `cwd` 推导工作区目录名（跨档实验已验证：同名目录下客户端可读），
/// 而 `cwd` 在复制时原样保留，所以跨档时目录名也必须保持一致（design §2.2）。
/// 同档时 `source_paths == target_paths`，结果与源文件同目录，与改造前逐字相同。
fn target_body_path(
    source_path: &Path,
    source_paths: &SessionPaths,
    target_paths: &SessionPaths,
    new_cid: &str,
) -> PathBuf {
    let file_name = format!("{new_cid}.jsonl");
    let relative = source_path
        .parent()
        .and_then(|parent| parent.strip_prefix(source_paths.projects_dir()).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    match relative {
        Some(relative) => target_paths.projects_dir().join(relative).join(file_name),
        None => target_paths.projects_dir().join(file_name),
    }
}

/// 写入副本正文并做写后校验（复用同一次源快照，避免 TOCTOU）。
fn write_copy_body(
    snapshot: &ContentSnapshot,
    source_path: &Path,
    source_paths: &SessionPaths,
    target_paths: &SessionPaths,
    cid: &str,
    new_cid: &str,
) -> Result<PathBuf, String> {
    let dest = target_body_path(source_path, source_paths, target_paths, new_cid);
    if dest.exists() {
        return Err("目标内容已存在同名文件，已停止复制".to_string());
    }
    // 跨档时目标档可能没有该工作区目录（源目录名跨档一致，见 `target_body_path`）。
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("目标工作区目录创建失败：{error}"))?;
    }
    let text = snapshot.text.replace(cid, new_cid);
    // 正文属于业务完成门禁：会话专用持久化写（sync_all + 父目录持久化）。
    session_backup::durable_write_str(&dest, &text)
        .map_err(|error| format!("复制后的内容保存失败：{error}"))?;
    match session_link::read_content_snapshot(&dest, new_cid) {
        ContentState::Ready(read_back)
            if read_back.normalized.total_digest == snapshot.normalized.total_digest =>
        {
            Ok(dest)
        }
        ContentState::Ready(_) => Err("复制后的内容保存后校验不一致，未按成功处理".to_string()),
        ContentState::Missing => Err("复制后的内容保存后不存在，未按成功处理".to_string()),
        ContentState::Unavailable(reason) => Err(format!("复制后的内容保存后无法确认：{reason}")),
    }
}

/// 首次使用时先落地空的关联存储。
///
/// 保证「操作日志出现」一定晚于「主文件存在」，否则刚预分配的操作日志会被
/// `load_store` 的残留痕迹规则误判成「主文件缺失但残留未完成操作」而自锁。
/// 存储损坏/未知版本/权限失败时同样在这里拒绝，绝不降级成空表。
fn ensure_link_store_ready(paths: &SessionPaths) -> Result<(), String> {
    match session_link::load_store(paths) {
        StoreState::Missing => {
            session_link::with_link_store_write(paths, |_| Ok(()))?;
            Ok(())
        }
        StoreState::Ready(_) => Ok(()),
        StoreState::Unavailable(reason) => Err(format!("{reason}；已阻止复制")),
    }
}

/// 复制单个会话：预分配 UUID → 持久化操作 → 正文 → 数据库 → 映射 → 关联/基线。
fn copy_one_session(context: &CopyContext, cid: &str) -> Result<CopyOutcome, String> {
    let paths = context.paths;
    // 上一次未完成的同一请求：只复用，不新建第二个副本。
    if let Some(operation) = session_link::find_pending_operation(
        context.pending,
        context.source_uid,
        cid,
        context.target_uid,
    ) {
        return Err(format!(
            "上一次复制尚未完成（操作 {}）：{}，这次不会重复创建",
            operation.operation_id,
            operation
                .last_error
                .clone()
                .unwrap_or_else(|| "等待恢复".to_string())
        ));
    }

    // 源正文与源行都在源档（跨档时与目标档不同）。
    let Some(source_path) = find_project_jsonl(context.source_paths, cid) else {
        return Err("会话内容不存在，未复制".to_string());
    };
    let snapshot = match session_link::read_content_snapshot(&source_path, cid) {
        ContentState::Ready(snapshot) => snapshot,
        ContentState::Missing => return Err("会话内容不存在，未复制".to_string()),
        ContentState::Unavailable(reason) => {
            return Err(format!("会话内容无法验证（{reason}），未复制"));
        }
    };
    let source_owner = session_row_owner(context.source_paths, cid);
    match source_owner.as_deref() {
        Some(owner) if owner == context.source_uid => {}
        Some(_) => return Err("源会话不属于当前账号，未复制".to_string()),
        None => return Err("数据库中找不到源会话记录，未复制".to_string()),
    }

    let resolution = resolve_target(context, cid)?;
    if let Some(session_id) = resolution.existing_link {
        return Ok(CopyOutcome::AlreadyLinked {
            session_id,
            group_id: resolution.group_id.unwrap_or_default(),
        });
    }
    if !resolution.hidden_aliases.is_empty() {
        return Err(format!(
            "目标账号存在 {} 个已隐藏的关联副本，请先在「重复审计」中恢复后再复制",
            resolution.hidden_aliases.len()
        ));
    }

    // 本 fork 的内容级去重（design：从源头杜绝重复行累积）。
    // 上游的「已关联则跳过」只覆盖有关联组记录的复制；目标账号若已存在「同 cwd + 同标题 +
    // 正文一致」的会话（旧版本遗留副本、关联表缺失等），这里同样跳过新增，返回 AlreadyLinked，
    // 由调用方计入 alreadyLinked（既不报错，也不产生第二个副本）。
    if let Some(existing_id) = equivalent_target_session_id(context, cid, &snapshot)? {
        // 目标已有等价副本但尚未入组：直接复用该副本建立关联与基线，不重复写正文/数据库。
        let group_id = resolution
            .group_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = now_ms();
        let operation = Operation {
            version: OPERATION_VERSION,
            operation_id: uuid::Uuid::new_v4().to_string(),
            kind: "copy".to_string(),
            variant: context.variant,
            source_variant: Some(context.source_variant),
            group_id: group_id.clone(),
            source: OperationMember {
                account_id: context.source_account_id.clone(),
                uid: context.source_uid.to_string(),
                session_id: cid.to_string(),
            },
            target: OperationMember {
                account_id: context.target_account_id.clone(),
                uid: context.target_uid.to_string(),
                session_id: existing_id.clone(),
            },
            expected_content_digest: snapshot.normalized.total_digest.clone(),
            expected_record_count: snapshot.normalized.record_count,
            phase: OpPhase::LinksCommitted,
            backup: None,
            lifecycle_version: None,
            cleanup_state: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let group_id = commit_links(paths, context.variant, &operation, &snapshot.normalized)?;
        return Ok(CopyOutcome::AlreadyLinked {
            session_id: existing_id,
            group_id,
        });
    }

    // 预分配身份：维护记录（allocating）先于任何备份与业务写入（design §3）。
    // 任何副本写入之前，生命周期身份必须已可靠落盘，恢复/补清理才有依据。
    let new_cid = uuid::Uuid::new_v4().to_string();
    let group_id = resolution
        .group_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut lifecycle = session_backup::begin_operation(
        paths,
        context.variant,
        OPERATION_KIND_COPY,
        Some(new_cid.clone()),
        None,
    )?;
    let backup_dir =
        session_backup::transaction_dir(paths, context.variant, &lifecycle.operation_id)?;
    let backup = match backup_workbuddy_db(paths, &backup_dir) {
        Ok(backup) => backup,
        Err(error) => {
            // 受控失败分支：契约保证 protected 之前不发生业务写入，可安全回收残留。
            session_backup::reclaim_unwritten(paths, context.variant, &mut lifecycle, &error);
            return Err(error);
        }
    };
    // 备份完备先转 protected：此步失败禁止任何业务写入，残留保守保留待下次维护。
    session_backup::mark_protected(paths, &mut lifecycle)?;
    ensure_link_store_ready(paths)?;

    let mut operation = Operation {
        version: OPERATION_VERSION,
        operation_id: lifecycle.operation_id.clone(),
        kind: OPERATION_KIND_COPY.to_string(),
        variant: context.variant,
        // 同档复制写 None（与旧记录逐字兼容）；跨档复制显式记录源档，恢复时据此读源。
        source_variant: (context.source_variant != context.variant)
            .then_some(context.source_variant),
        group_id: group_id.clone(),
        source: OperationMember {
            account_id: context.source_account_id.clone(),
            uid: context.source_uid.to_string(),
            session_id: cid.to_string(),
        },
        target: OperationMember {
            account_id: context.target_account_id.clone(),
            uid: context.target_uid.to_string(),
            session_id: new_cid.clone(),
        },
        expected_content_digest: snapshot.normalized.total_digest.clone(),
        expected_record_count: snapshot.normalized.record_count,
        phase: OpPhase::Prepared,
        backup: Some(backup.to_string_lossy().to_string()),
        lifecycle_version: Some(OPERATION_LIFECYCLE_VERSION),
        cleanup_state: None,
        last_error: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    session_link::save_operation(paths, &operation)?;

    if let Err(error) = finish_copy_from_body(
        paths,
        context.variant,
        context.source_paths,
        &mut operation,
        &snapshot,
        Some(&source_path),
    ) {
        fail_operation(paths, &mut operation, &error);
        return Err(error);
    }
    let _ = session_link::prune_operations(
        paths,
        context.variant,
        session_link::KEEP_COMPLETED_OPERATIONS,
    );
    // 业务可靠完成之后才授权清理；清理失败不回滚业务、不报告复制失败。
    let cleanup = finish_backup_cleanup(paths, context.variant, &mut lifecycle);
    let (backup, cleanup_state, cleanup_error) = report_cleanup(&cleanup, &backup_dir);
    Ok(CopyOutcome::Copied {
        new_id: new_cid,
        group_id,
        backup,
        cleanup_state: cleanup_state.to_string(),
        cleanup_error,
    })
}

/// 业务完成后的收尾：可靠转 cleanupPending 再回收；失败只报告，不改业务结果。
fn finish_backup_cleanup(
    paths: &SessionPaths,
    variant: WbVariant,
    lifecycle: &mut BackupLifecycle,
) -> CleanupOutcome {
    if let Err(error) = session_backup::mark_cleanup_pending(paths, lifecycle, None) {
        // 业务已完成、维护记录仍是 protected：下次维护按 Completed 补转后清理。
        return CleanupOutcome::Pending {
            reason: format!("清理状态推进失败（{error}）"),
        };
    }
    session_backup::cleanup_after_success(paths, variant, lifecycle)
}

/// 维护入口补清成功后，把仍写着 pending 路径的成功项改成 cleaned / null。
///
/// `symlink_metadata` 把符号链接也视为「还在」，避免把拒绝删除的链接目标标成已清理。
fn reconcile_reported_cleanup(items: &mut [Value]) {
    for item in items {
        if item.get("cleanupState").and_then(Value::as_str) != Some("pending") {
            continue;
        }
        let still_there = item
            .get("backup")
            .and_then(Value::as_str)
            .is_some_and(|path| std::fs::symlink_metadata(path).is_ok());
        if still_there {
            continue;
        }
        item["backup"] = json!(null);
        if item.get("backupManifest").is_some() {
            item["backupManifest"] = json!(null);
        }
        item["cleanupState"] = json!("cleaned");
        if let Some(object) = item.as_object_mut() {
            object.remove("cleanupError");
        }
    }
}

/// 清理结果到报告字段的投影：已清理不展示路径；待清理保留位置与原因。
fn report_cleanup(
    cleanup: &CleanupOutcome,
    dir: &Path,
) -> (Option<String>, &'static str, Option<String>) {
    match cleanup {
        CleanupOutcome::Cleaned => (None, "cleaned", None),
        CleanupOutcome::Pending { reason } => (
            Some(dir.to_string_lossy().to_string()),
            "pending",
            Some(reason.clone()),
        ),
        CleanupOutcome::Protected { reason } => (
            Some(dir.to_string_lossy().to_string()),
            "pending",
            Some(reason.clone()),
        ),
    }
}

/// 从「源快照已确认」开始推进复制：写正文 → 写数据库行 → 登记映射 → 提交关联与基线。
///
/// `body_source` 为 `Some(source_path)` 时先写正文；为 `None` 表示正文此前已写成
/// （恢复场景），直接继续数据库行与关联。源行读取与目标行写入的档位经
/// `source_paths` 分离（跨档复制时与 `paths` 不同，同档时相同）。
fn finish_copy_from_body(
    paths: &SessionPaths,
    variant: WbVariant,
    source_paths: &SessionPaths,
    operation: &mut Operation,
    snapshot: &ContentSnapshot,
    body_source: Option<&Path>,
) -> Result<(), String> {
    if let Some(source_path) = body_source {
        write_copy_body(
            snapshot,
            source_path,
            source_paths,
            paths,
            &operation.source.session_id,
            &operation.target.session_id,
        )?;
        advance_operation(paths, operation, OpPhase::BodyWritten)?;
    }

    match insert_session_copy(
        source_paths,
        paths,
        &operation.target.session_id,
        &operation.source.session_id,
        &operation.source.uid,
        &operation.target.uid,
    )? {
        DbCopyOutcome::Inserted => {}
        DbCopyOutcome::SourceRowMissing => {
            return Err("数据库中找不到源会话记录，未复制".to_string())
        }
        DbCopyOutcome::NoSessionsTable => return Err("会话数据缺少数据表，未复制".to_string()),
        DbCopyOutcome::NoDb => return Err("会话数据不存在，未复制".to_string()),
    }
    verify_session_row(paths, &operation.target.session_id, &operation.target.uid)?;
    advance_operation(paths, operation, OpPhase::DbWritten)?;

    // 云端登记交接给客户端：不预写 edge_sync_mapping——预写会让 edge-sync 扩展
    // 判定"已迁移"而跳过上传，导致云端没有会话、后续 RENAME/ACTIVITY 404。
    // 副本由客户端下次启动时自行迁移并写入映射行；阶段标记保留，语义为"已交接"。
    advance_operation(paths, operation, OpPhase::MappingWritten)?;

    let group_id = commit_links(paths, variant, operation, &snapshot.normalized)?;
    operation.group_id = group_id;
    advance_operation(paths, operation, OpPhase::LinksCommitted)?;
    advance_operation(paths, operation, OpPhase::Completed)?;
    Ok(())
}

/// 推进操作阶段。阶段只能前进：已经走到更靠后的阶段时不回写（恢复路径不得把
/// `LinksCommitted`/`Completed` 写回 `DbWritten`）。
///
/// 先保存候选副本、成功后才替换内存状态：保存失败时内存阶段不变，随后的
/// `fail_operation` 不会把未落盘的阶段写回磁盘（design §3）。
fn advance_operation(
    paths: &SessionPaths,
    operation: &mut Operation,
    phase: OpPhase,
) -> Result<(), String> {
    if operation.phase >= phase {
        return Ok(());
    }
    let mut candidate = operation.clone();
    candidate.phase = phase;
    candidate.updated_at = now_ms();
    session_link::save_operation(paths, &candidate)?;
    *operation = candidate;
    Ok(())
}

fn fail_operation(paths: &SessionPaths, operation: &mut Operation, error: &str) {
    operation.last_error = Some(error.to_string());
    operation.updated_at = now_ms();
    if let Err(save_error) = session_link::save_operation(paths, operation) {
        error_log::record(
            "session-operation",
            "保存失败状态失败",
            &format!(
                "operation={} phase={:?}: {save_error}",
                operation.operation_id, operation.phase
            ),
        );
    }
}

/// 原子提交关联与配对基线（含失效成员替换与基线继承）。
///
/// 整个读改写都在关联存储锁内完成；只有全部成功才推进 revision。
fn commit_links(
    paths: &SessionPaths,
    variant: WbVariant,
    operation: &Operation,
    normalized: &NormalizedContent,
) -> Result<String, String> {
    let source = &operation.source;
    let target = &operation.target;
    // 成员级档位：源成员记源档（跨档组的身份基础），目标成员记目标档（参数 variant）。
    let source_variant = operation.source_variant.unwrap_or(variant);
    let group_id = operation.group_id.clone();
    let group_id_out = group_id.clone();
    session_link::with_link_store_write(paths, move |store| {
        let index = match store.groups.iter().position(|group| group.id == group_id) {
            Some(index) => index,
            None => {
                store.groups.push(LinkGroup {
                    id: group_id.clone(),
                    variant,
                    created_at: now_ms(),
                    members: Vec::new(),
                    pair_bases: Vec::new(),
                });
                store.groups.len() - 1
            }
        };
        let group = &mut store.groups[index];

        let source_member_id =
            match session_link::find_member(group, &source.uid, &source.session_id) {
                Some(member) => member.member_id.clone(),
                None => {
                    let member_id = uuid::Uuid::new_v4().to_string();
                    session_link::add_active_member(
                        group,
                        LinkMember {
                            member_id: member_id.clone(),
                            account_id: source.account_id.clone(),
                            uid: source.uid.clone(),
                            session_id: source.session_id.clone(),
                            variant: Some(source_variant),
                            state: MemberState::Active,
                            linked_at: now_ms(),
                            last_synced_at: None,
                        },
                    );
                    member_id
                }
            };

        let target_member_id =
            match session_link::find_member(group, &target.uid, &target.session_id) {
                Some(member) => {
                    let member_id = member.member_id.clone();
                    session_link::set_member_state(group, &member_id, MemberState::Active);
                    member_id
                }
                None => {
                    let member_id = uuid::Uuid::new_v4().to_string();
                    // 同账号的失效 active 成员在这里被显式 supersede，保留记录。
                    session_link::add_active_member(
                        group,
                        LinkMember {
                            member_id: member_id.clone(),
                            account_id: target.account_id.clone(),
                            uid: target.uid.clone(),
                            session_id: target.session_id.clone(),
                            variant: Some(variant),
                            state: MemberState::Active,
                            linked_at: now_ms(),
                            last_synced_at: None,
                        },
                    );
                    member_id
                }
            };

        // 本次复制的正文即来源与目标的共同基线（定向更新，不动其它配对）。
        let pair_baseline_ref = uuid::Uuid::new_v4().to_string();
        session_link::save_baseline(paths, &pair_baseline_ref, normalized)?;
        session_link::set_pair_base(
            group,
            &source_member_id,
            &target_member_id,
            &pair_baseline_ref,
            NORMALIZATION_VERSION,
        );

        // 继承：来源与组内其它成员已有的历史共同基线，只有在「来源内容有序前缀
        // 包含该基线」时才能建立到新成员的基线；已有配对基线不覆盖。
        let others: Vec<String> = group
            .members
            .iter()
            .filter(|member| {
                member.member_id != source_member_id && member.member_id != target_member_id
            })
            .map(|member| member.member_id.clone())
            .collect();
        for other_id in others {
            if session_link::find_pair_base(group, &other_id, &target_member_id).is_some() {
                continue;
            }
            let Some(pair) =
                session_link::find_pair_base(group, &source_member_id, &other_id).cloned()
            else {
                continue;
            };
            if let Some(record) =
                session_link::inheritable_baseline(paths, &pair, &normalized.line_digests)
            {
                session_link::set_pair_base(
                    group,
                    &other_id,
                    &target_member_id,
                    &record.baseline_ref,
                    pair.normalization_version,
                );
            }
        }
        Ok(group_id_out)
    })
}

/// 操作已标成 `LinksCommitted` 时，核验关联组与双方成员仍在（只读，不写存储）。
fn committed_links_present(paths: &SessionPaths, operation: &Operation) -> Result<(), String> {
    match session_link::load_store(paths) {
        StoreState::Ready(store) => {
            let Some(group) = store
                .groups
                .iter()
                .find(|group| group.id == operation.group_id)
            else {
                return Err("会话的关联关系缺失，已停止恢复".to_string());
            };
            let has_source = session_link::find_member(
                group,
                &operation.source.uid,
                &operation.source.session_id,
            )
            .is_some();
            let has_target = session_link::find_member(
                group,
                &operation.target.uid,
                &operation.target.session_id,
            )
            .is_some();
            if !has_source || !has_target {
                return Err("对应的会话缺失，已停止恢复".to_string());
            }
            Ok(())
        }
        StoreState::Missing => Err("同步记录主文件缺失，已停止恢复".to_string()),
        StoreState::Unavailable(reason) => Err(reason),
    }
}

/// 当前账号库中按 uid 找账号 id（成员 accountId 仅作展示，身份判定仍以 uid 为准）。
fn account_id_for_uid(paths: &SessionPaths, uid: &str) -> Option<String> {
    let accounts = account::load_accounts_at(&account::accounts_file_in(&paths.store_root));
    accounts
        .iter()
        .find(|account| account.get("uid").and_then(Value::as_str) == Some(uid))
        .and_then(|account| account.get("id").and_then(Value::as_str))
        .map(String::from)
}

// ---------------------------------------------------------------------------
// 同步预览与执行契约（design §5 / §6）
// ---------------------------------------------------------------------------

/// 会话同步能力不足时的错误文案。
pub const SESSION_SYNC_UNSUPPORTED: &str = "该档位暂不支持会话同步";
/// 预览凭据过期/失配时的原因码（design §5.2：不继承用户旧选择）。
pub const REASON_PREVIEW_STALE: &str = "previewStale";

/// `sessions.status` 的归档取值。
const SESSION_STATUS_ARCHIVED: &str = "archived";
/// 允许被传导为归档的**目标终态**白名单。
///
/// 排除 `working`（生成中）：正在生成的会话不能被收进归档。精确匹配而非「非
/// working 即允许」——NULL、未知值与缺列都不授予归档权限（真实库存在 `Pending`
/// 这类合法但语义未定的默认值）。
///
/// `active`（当前激活）在列内：切号前客户端被关掉时，目标行常残留一个陈旧的
/// `active`，把它排除会导致该会话的归档状态永远无法与来源收敛。写入侧已有三重防线
/// 兜住真正的活跃会话——`app_running` 门禁拦住 App 在跑的目标档、`planned_status_change`
/// 写前重核、以及事务内按前置状态等值匹配的条件 UPDATE（前置值已变则零行命中）。
const ARCHIVE_TARGET_TERMINAL_STATUSES: [&str; 4] = ["completed", "error", "terminated", "active"];
/// 预览项与预览绑定里的归档动作取值。
const ARCHIVE_ACTION_STATUS_ONLY: &str = "statusOnly";

/// 一条 `syncSelections` 入参（取代只传 `syncLinkIds`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSelection {
    pub group_id: String,
    pub preview_token: String,
    pub mode: SyncMode,
}

impl SyncSelection {
    /// 解析单条入参；缺字段或未知模式直接拒绝（不静默跳过、不回落默认值）。
    pub fn parse(value: &Value) -> Result<Self, String> {
        let group_id = value
            .get("groupId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| "同步选择项缺少 groupId".to_string())?;
        let preview_token = value
            .get("previewToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("同步选择项缺少 previewToken（组 {group_id}）"))?;
        let mode = value
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("同步选择项缺少 mode（组 {group_id}）"))?;
        Ok(Self {
            group_id: group_id.to_string(),
            preview_token: preview_token.to_string(),
            mode: SyncMode::parse(mode)?,
        })
    }
}

/// 解析 `syncSelections`；缺省或 null 视为未勾选同步。
pub fn parse_sync_selections(value: Option<&Value>) -> Result<Vec<SyncSelection>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = value.as_array() else {
        return Err("syncSelections 必须是数组".to_string());
    };
    items.iter().map(SyncSelection::parse).collect()
}

/// 成员当前正文状态；`projects/` 下找不到正文一律按 Missing（不当作空正文）。
pub(crate) fn member_content_state(paths: &SessionPaths, session_id: &str) -> ContentState {
    match find_project_jsonl(paths, session_id) {
        Some(path) => session_link::read_content_snapshot(&path, session_id),
        None => ContentState::Missing,
    }
}

/// 只用于判定/统计的成员内容：走摘要缓存，不返回正文文本。
pub(crate) fn member_content_summary(paths: &SessionPaths, session_id: &str) -> ContentState {
    match find_project_jsonl(paths, session_id) {
        Some(path) => session_link::read_content_summary(&path, session_id),
        None => ContentState::Missing,
    }
}

/// 内容快照里的记录数（不可验证时为 0，仅用于展示）。
fn record_count_of(content: &ContentState) -> usize {
    match content {
        ContentState::Ready(snapshot) => snapshot.normalized.record_count,
        _ => 0,
    }
}

/// 会话行的展示名与目录（标题优先 custom_title，与侧栏一致）。
fn session_row_info(paths: &SessionPaths, cid: &str) -> Option<(String, String)> {
    let db = paths.workbuddy_db();
    let conn = open_db(&db, true)?;
    if !table_exists(&conn, "sessions") {
        return None;
    }
    let sql = if column_exists(&conn, "sessions", "custom_title") {
        "SELECT title, custom_title, cwd FROM sessions WHERE id = ?1 AND deleted_at IS NULL"
    } else {
        "SELECT title, NULL, cwd FROM sessions WHERE id = ?1 AND deleted_at IS NULL"
    };
    conn.query_row(sql, [cid], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })
    .ok()
    .map(|(title, custom_title, cwd)| {
        (
            session_display_title(title, custom_title),
            cwd.unwrap_or_default(),
        )
    })
}

/// 会话库中的一行身份信息（含已软删除行，用于识别旧关联）。
#[derive(Debug, Clone)]
struct SessionIdentityRow {
    uid: String,
    cwd: String,
    title: String,
    updated_at: i64,
    deleted: bool,
}

/// 关联成员对账结果（仅供诊断与测试观察）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LinkReconcileReport {
    relinked: usize,
    superseded: usize,
}

struct LinkMemberFix {
    group_id: String,
    member_id: String,
    /// 计算修复时的会话 id：写回前复核，避免覆盖并发复制/换绑后的新成员。
    expected_session_id: String,
    /// `Some` 表示同一账号下找到内容一致的存活会话，可原地换绑；`None` 表示置为失效。
    session_id: Option<String>,
}

/// 读取会话库身份行；库/表不可用时返回 None（调用方保持原行为）。
fn load_session_identity_rows(
    paths: &SessionPaths,
) -> Option<std::collections::HashMap<String, SessionIdentityRow>> {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return None;
    }
    let conn = open_db(&db, true)?;
    if !table_exists(&conn, "sessions") {
        return None;
    }
    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let sql = if has_custom {
        "SELECT id, user_id, cwd, title, custom_title, updated_at, deleted_at FROM sessions"
    } else {
        "SELECT id, user_id, cwd, title, NULL, updated_at, deleted_at FROM sessions"
    };
    let mut stmt = conn.prepare(sql).ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })
        .ok()?;
    let mut map = std::collections::HashMap::new();
    for (id, uid, cwd, title, custom_title, updated_at, deleted_at) in rows.flatten() {
        map.insert(
            id,
            SessionIdentityRow {
                uid: uid.unwrap_or_default(),
                cwd: cwd.unwrap_or_default(),
                title: session_display_title(title, custom_title),
                updated_at: updated_at.unwrap_or(0),
                deleted: deleted_at.is_some(),
            },
        );
    }
    Some(map)
}

/// 把「成员仍 active、但会话行已软删/缺失」的历史脏关联对账一次。
///
/// 规则完全通用、不依赖具体账号或路径：
/// 1. 同一账号下存在「归一化 cwd + 展示标题」相同、且正文归一化摘要一致的存活会话
///    → 成员原地换绑到该会话（内容一致，配对基线仍然有效）；
/// 2. 找不到可证明一致的存活会话 → 成员置为 `Superseded`（明确失效，不再误导）。
///
/// 返回诊断计数。存储/数据库不可用时不做任何写入。
fn reconcile_stale_link_members(paths: &SessionPaths, variant: WbVariant) -> LinkReconcileReport {
    reconcile_stale_link_members_scoped(paths, variant, None)
}

/// [] 的作用域版本：单组操作只对账该组，避免为一次
/// 单对预览扫描全部关联组（性能：组数越多越明显）。
fn reconcile_stale_link_members_scoped(
    paths: &SessionPaths,
    variant: WbVariant,
    only_group: Option<&str>,
) -> LinkReconcileReport {
    let Some(rows) = load_session_identity_rows(paths) else {
        return LinkReconcileReport::default();
    };
    let StoreState::Ready(store) = session_link::load_store(paths) else {
        return LinkReconcileReport::default();
    };

    type IdentityKey = (String, String, String);
    let mut candidates: std::collections::HashMap<IdentityKey, Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for (id, row) in rows.iter() {
        if row.deleted {
            continue;
        }
        candidates
            .entry((row.uid.clone(), normalize_cwd(&row.cwd), row.title.clone()))
            .or_default()
            .push((id.clone(), row.updated_at));
    }
    // 更新时间倒序、同值时按 id 定序，保证结果与目录/查询顺序无关。
    for list in candidates.values_mut() {
        list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }

    let linked_ids = session_link::active_session_ids(paths);
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut fixes: Vec<LinkMemberFix> = Vec::new();
    let mut report = LinkReconcileReport::default();

    // 跨档组里成员档位可能不同：必须按「成员自身档位」筛，不能按组档位，
    // 否则用 CN 会话库去核对 AI 成员会全部误判为缺失。
    for group in store.groups.iter() {
        if only_group.is_some_and(|id| group.id != id) {
            continue;
        }
        for member in group.members.iter().filter(|member| {
            member.state == MemberState::Active
                && session_link::member_variant(group, member) == variant
        }) {
            let Some(row) = rows.get(&member.session_id) else {
                // 行被物理删除：无法按「同工作区 + 同标题」找替代项，但只要正文仍完整，
                // 仍可作为只读来源；正文也没了才置失效。
                let readable = find_project_jsonl(paths, &member.session_id).is_some_and(|path| {
                    matches!(
                        session_link::read_content_snapshot(&path, &member.session_id),
                        ContentState::Ready(_)
                    )
                });
                if !readable {
                    report.superseded += 1;
                    fixes.push(LinkMemberFix {
                        group_id: group.id.clone(),
                        member_id: member.member_id.clone(),
                        expected_session_id: member.session_id.clone(),
                        session_id: None,
                    });
                }
                continue;
            };
            if !row.deleted && row.uid == member.uid {
                continue;
            }

            let key = (
                member.uid.clone(),
                normalize_cwd(&row.cwd),
                row.title.clone(),
            );
            let old_digest = find_project_jsonl(paths, &member.session_id).and_then(|path| {
                match session_link::read_content_snapshot(&path, &member.session_id) {
                    ContentState::Ready(snapshot) => Some(snapshot.normalized.total_digest),
                    _ => None,
                }
            });

            let mut replacement = None;
            if let (Some(digest), Some(list)) = (old_digest.as_deref(), candidates.get(&key)) {
                for (cid, _) in list {
                    if cid == &member.session_id
                        || claimed.contains(cid)
                        || linked_ids.contains(cid)
                    {
                        continue;
                    }
                    let Some(path) = find_project_jsonl(paths, cid) else {
                        continue;
                    };
                    if let ContentState::Ready(snapshot) =
                        session_link::read_content_snapshot(&path, cid)
                    {
                        if snapshot.normalized.total_digest == digest {
                            replacement = Some(cid.clone());
                            break;
                        }
                    }
                }
            }

            if let Some(cid) = replacement.clone() {
                claimed.insert(cid.clone());
                report.relinked += 1;
                fixes.push(LinkMemberFix {
                    group_id: group.id.clone(),
                    member_id: member.member_id.clone(),
                    expected_session_id: member.session_id.clone(),
                    session_id: Some(cid),
                });
            } else if old_digest.is_none() {
                // 正文已不可验证（被 dedup 删除或文件丢失）才置失效；只要正文仍完整，
                // 该成员仍可作为只读来源参与同步，是否可用交给预览按目标有效性判定。
                report.superseded += 1;
                fixes.push(LinkMemberFix {
                    group_id: group.id.clone(),
                    member_id: member.member_id.clone(),
                    expected_session_id: member.session_id.clone(),
                    session_id: None,
                });
            }
        }
    }

    if fixes.is_empty() {
        return report;
    }
    // 只改关联元数据（成员换绑/失效），不碰任何会话正文与数据库行。
    let _ = session_link::with_link_store_write(paths, |store| {
        for fix in &fixes {
            let Some(group) = store
                .groups
                .iter_mut()
                .find(|group| group.id == fix.group_id)
            else {
                continue;
            };
            let Some(member) = group
                .members
                .iter_mut()
                .find(|member| member.member_id == fix.member_id)
            else {
                continue;
            };
            // 并发写入后已换绑/失效的成员不再按旧快照修复。
            if member.state != MemberState::Active || member.session_id != fix.expected_session_id {
                continue;
            }
            match fix.session_id.as_deref() {
                Some(session_id) => member.session_id = session_id.to_string(),
                None => member.state = MemberState::Superseded,
            }
        }
        Ok(())
    });
    report
}

/// 会话行是否仍归属该成员账号（行被删除或改归属即成员失效，design §5）。
fn member_row_owned_by(paths: &SessionPaths, member: &LinkMember) -> bool {
    session_row_owner(paths, &member.session_id).is_some_and(|owner| owner == member.uid)
}

/// 会话行 `status` 的读取结果（区分「缺列」与「列为 NULL」）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionStatus {
    /// 会话表没有 `status` 列（旧库）：不支持状态读写，但正文同步照旧可用。
    Unsupported,
    /// 行不存在或已软删除：没有可读写状态的对象。
    Absent,
    /// 行存在且未删除；`None` 表示 `status` 为 NULL。
    Present(Option<String>),
}

/// 读取会话行的 `status`（行必须存在且未软删除）。
///
/// 读失败按 Err 上抛：状态是写入门禁，读不出来就不能声称「不具备归档资格」。
fn read_member_status(paths: &SessionPaths, cid: &str) -> Result<SessionStatus, String> {
    let conn = open_db(&paths.workbuddy_db(), true)
        .ok_or_else(|| "会话数据无法打开，无法读取会话状态".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("会话数据缺少数据表，无法读取会话状态".to_string());
    }
    if !column_exists(&conn, "sessions", "status") {
        return Ok(SessionStatus::Unsupported);
    }
    let row: Option<(Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT status, deleted_at FROM sessions WHERE id = ?1",
            [cid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| format!("会话状态读取失败：{error}"))?;
    match row {
        // 行不存在或已软删除：与 Absent 同义。
        None | Some((_, Some(_))) => Ok(SessionStatus::Absent),
        Some((status, None)) => Ok(SessionStatus::Present(status)),
    }
}

/// 单向粘滞归档资格：来源已归档，且目标处于允许被归档的终态。
///
/// 不反向传导取消归档；缺列、NULL、未知值与生成中（`working`）一律不授权。
/// 陈旧 `active` 视作可归档终态，由写入侧门禁兜底（见白名单注释）。
fn archive_qualifies(source: &SessionStatus, target: &SessionStatus) -> bool {
    let SessionStatus::Present(Some(source_status)) = source else {
        return false;
    };
    if source_status != SESSION_STATUS_ARCHIVED {
        return false;
    }
    let SessionStatus::Present(Some(target_status)) = target else {
        return false;
    };
    ARCHIVE_TARGET_TERMINAL_STATUSES.contains(&target_status.as_str())
}

/// 读取双方会话行状态并算出本次的归档动作；读取失败一律按「不授权」处理，
/// 不改变既有正文判定的结果（状态同步是附加能力，不是正文同步的前置条件）。
fn preview_archive_state(
    source_paths: &SessionPaths,
    source_session_id: &str,
    target_paths: &SessionPaths,
    target_session_id: &str,
) -> PreviewArchiveState {
    let source =
        read_member_status(source_paths, source_session_id).unwrap_or(SessionStatus::Absent);
    let target =
        read_member_status(target_paths, target_session_id).unwrap_or(SessionStatus::Absent);
    let source_status = match &source {
        SessionStatus::Present(status) => status.clone(),
        _ => None,
    };
    let target_status = match &target {
        SessionStatus::Present(status) => status.clone(),
        _ => None,
    };
    let action =
        archive_qualifies(&source, &target).then(|| ARCHIVE_ACTION_STATUS_ONLY.to_string());
    PreviewArchiveState {
        source_status,
        target_status,
        action,
    }
}

/// 预览时记入凭据的归档状态（双方 status 与本次授权的归档动作）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PreviewArchiveState {
    source_status: Option<String>,
    target_status: Option<String>,
    /// `Some("statusOnly")` 或 None。
    action: Option<String>,
}

/// 组内是否存在该账号的成员（任意状态）：双方都有成员才谈得上「共同参与」。
fn has_member_for(group: &LinkGroup, uid: &str) -> bool {
    group.members.iter().any(|member| member.uid == uid)
}

/// 成员摘要（只含身份与状态，不含正文）。
fn member_summary(member: Option<&LinkMember>) -> Value {
    match member {
        Some(member) => json!({
            "memberId": member.member_id,
            "uid": member.uid,
            "accountId": member.account_id,
            "sessionId": member.session_id,
            "state": member.state.as_str(),
        }),
        None => Value::Null,
    }
}

/// 由实时状态组装预览绑定（预览与执行前核对共用同一份装配逻辑）。
// 参数都是本次绑定的显式输入（含归档状态），与复制侧同口径不做结构装箱。
#[allow(clippy::too_many_arguments)]
fn live_preview_binding(
    group: &LinkGroup,
    source_member: &LinkMember,
    target_member: &LinkMember,
    source_content: &ContentState,
    target_content: &ContentState,
    baseline: &BaselineState,
    verdict: SyncVerdict,
    archive: &PreviewArchiveState,
) -> PreviewBinding {
    PreviewBinding {
        variant: group.variant,
        group_id: group.id.clone(),
        group_fingerprint: session_link::group_fingerprint(group),
        source: session_link::preview_member_binding(source_member, source_content),
        target: session_link::preview_member_binding(target_member, target_content),
        baseline_ref: session_link::find_pair_base(
            group,
            &source_member.member_id,
            &target_member.member_id,
        )
        .map(|pair| pair.baseline_ref.clone()),
        baseline_total_digest: baseline.ready().map(|record| record.total_digest.clone()),
        baseline_record_count: baseline.ready().map(|record| record.record_count),
        verdict,
        source_status: archive.source_status.clone(),
        target_status: archive.target_status.clone(),
        archive_action: archive.action.clone(),
    }
}

/// 预览「当前账号 → 目标账号」可同步的关联组（design §6）。
///
/// 只处理双方共同参与的组，不涉及第三方账号；不写任何会话正文或数据库行。
/// 预览前会对历史脏关联做一次核对：仅在关联存储内把成员换绑到内容一致的存活副本，
/// 或把正文已不可读的成员标为失效（不改变任何会话内容）。
/// 来源身份一律取该档位登录态文件，不接受前端传入。
pub fn session_links_preview(variant: WbVariant, target_acc: &Value) -> Result<Value, String> {
    session_links_preview_at(&SessionPaths::for_variant(variant), variant, target_acc)
}

fn session_links_preview_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_acc: &Value,
) -> Result<Value, String> {
    let target_uid = account_uid(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法同步会话".to_string());
    }
    // 历史脏关联（成员 active 但会话已被折叠/清理）先做一次通用对账：
    // 能按「同账号 + 同工作区 + 同标题 + 正文一致」换绑的换绑，否则明确置为失效。
    let _ = reconcile_stale_link_members(paths, variant);
    let source_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法确定来源账号".to_string())?;
    if source_uid == target_uid {
        return Err("当前账号与目标账号相同，无需同步会话".to_string());
    }
    // 能力探测与复制同口径：只对国际版生效（国内版数据根与改造前同构）。
    let supported = variant != WbVariant::Ai || session_copy_supported_at(&paths.data_root);

    let mut report = json!({
        "supported": supported,
        "sourceUid": source_uid,
        "targetUid": target_uid,
        "groups": [],
    });
    if !supported {
        report["storeStatus"] = json!("unsupported");
        return Ok(report);
    }
    match session_link::load_store(paths) {
        StoreState::Missing => report["storeStatus"] = json!("missing"),
        StoreState::Unavailable(reason) => {
            report["storeStatus"] = json!("unavailable");
            report["storeError"] = json!(reason);
        }
        StoreState::Ready(store) => {
            report["storeStatus"] = json!("ready");
            let groups: Vec<Value> = store
                .groups
                .iter()
                .filter(|group| {
                    group.variant == variant
                        && has_member_for(group, &source_uid)
                        && has_member_for(group, &target_uid)
                })
                .map(|group| {
                    let fixed_paths = |_: &LinkGroup, _: &LinkMember| paths.clone();
                    preview_group_item(paths, &fixed_paths, group, &source_uid, &target_uid)
                })
                .collect();
            report["groups"] = json!(groups);
        }
    }
    Ok(report)
}

/// 预览「显式来源账号 → 显式目标账号」可同步的关联组（会话管理页用；跨档支持）。
///
/// 与 [`session_links_preview`] 的差异只在来源判定：源 uid 取自 `source_acc`，
/// 不再从登录态读取；成员内容与行按**成员自身档位**读取（跨档组：源读源档、
/// 目标读目标档），关联存储与基线共享。
pub fn session_links_preview_cross(
    source_acc: &Value,
    target_acc: &Value,
) -> Result<Value, String> {
    let source_variant = account::variant_of(source_acc);
    let target_variant = account::variant_of(target_acc);
    session_links_preview_cross_at(
        &SessionPaths::for_variant(source_variant),
        source_variant,
        source_acc,
        &SessionPaths::for_variant(target_variant),
        target_variant,
        target_acc,
        SessionPaths::for_variant,
    )
}

/// [`session_links_preview_cross`] 的可注入版本（单测注入临时双档目录与解析器）。
fn session_links_preview_cross_at(
    source_paths: &SessionPaths,
    source_variant: WbVariant,
    source_acc: &Value,
    target_paths: &SessionPaths,
    target_variant: WbVariant,
    target_acc: &Value,
    resolve_source_paths: impl Fn(WbVariant) -> SessionPaths,
) -> Result<Value, String> {
    let source_uid = account_uid(source_acc);
    if source_uid.is_empty() {
        return Err("来源账号缺少 uid，无法预览会话".to_string());
    }
    let target_uid = account_uid(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法同步会话".to_string());
    }
    if source_variant == target_variant && source_uid == target_uid {
        return Err("来源账号与目标账号相同，无需同步会话".to_string());
    }
    // 两个档位各自做一次通用对账（关联存储共享，按组档位过滤，避免误处理另一档）。
    let _ = reconcile_stale_link_members(source_paths, source_variant);
    if target_variant != source_variant {
        let _ = reconcile_stale_link_members(target_paths, target_variant);
    }
    // 能力探测与复制同口径：成员数据根只有国际版需要同构检查（读也依赖）。
    let mut supported = true;
    for (paths, variant) in [
        (source_paths, source_variant),
        (target_paths, target_variant),
    ] {
        if variant == WbVariant::Ai && !session_copy_supported_at(&paths.data_root) {
            supported = false;
        }
    }

    let mut report = json!({
        "supported": supported,
        "sourceUid": source_uid,
        "targetUid": target_uid,
        "groups": [],
    });
    // 跨档预览在报告里带两侧档位；同档报告保持原字段（与现有入口逐字兼容）。
    if source_variant != target_variant {
        report["sourceVariant"] = json!(source_variant.as_str());
        report["targetVariant"] = json!(target_variant.as_str());
    }
    if !supported {
        report["storeStatus"] = json!("unsupported");
        return Ok(report);
    }
    // 关联存储与基线共享；用目标档路径承载（同档时两者相同）。
    match session_link::load_store(target_paths) {
        StoreState::Missing => report["storeStatus"] = json!("missing"),
        StoreState::Unavailable(reason) => {
            report["storeStatus"] = json!("unavailable");
            report["storeError"] = json!(reason);
        }
        StoreState::Ready(store) => {
            report["storeStatus"] = json!("ready");
            // 成员数据根：与请求两侧同档的成员直接用请求路径（含单测注入），
            // 其它档位（历史组）按解析器解析，避免误读。
            let member_paths = |group: &LinkGroup, member: &LinkMember| -> SessionPaths {
                let variant = session_link::member_variant(group, member);
                if variant == source_variant {
                    source_paths.clone()
                } else if variant == target_variant {
                    target_paths.clone()
                } else {
                    resolve_source_paths(variant)
                }
            };
            let groups: Vec<Value> = store
                .groups
                .iter()
                .filter(|group| {
                    has_member_for(group, &source_uid) && has_member_for(group, &target_uid)
                })
                .map(|group| {
                    preview_group_item(target_paths, &member_paths, group, &source_uid, &target_uid)
                })
                .collect();
            report["groups"] = json!(groups);
        }
    }
    Ok(report)
}

/// 只预览指定组的一对成员（会话管理页单对操作专用）。
///
/// 与 session_links_preview_cross 的区别：**只加载目标组、只对账目标组、只计算
/// 一对成员**，不再遍历所有包含这两个账号的组。返回结构与完整预览一致（groups 只有一项）。
pub fn session_links_preview_pair(
    source_acc: &Value,
    target_acc: &Value,
    group_id: &str,
) -> Result<Value, String> {
    let source_variant = account::variant_of(source_acc);
    let target_variant = account::variant_of(target_acc);
    let source_paths = SessionPaths::for_variant(source_variant);
    let target_paths = SessionPaths::for_variant(target_variant);
    session_links_preview_pair_at(
        &source_paths,
        source_variant,
        source_acc,
        &target_paths,
        target_variant,
        target_acc,
        group_id,
    )
}

fn session_links_preview_pair_at(
    source_paths: &SessionPaths,
    source_variant: WbVariant,
    source_acc: &Value,
    target_paths: &SessionPaths,
    target_variant: WbVariant,
    target_acc: &Value,
    group_id: &str,
) -> Result<Value, String> {
    let source_uid = account_uid(source_acc);
    let target_uid = account_uid(target_acc);
    if source_uid.is_empty() || target_uid.is_empty() {
        return Err("账号缺少 uid，无法预览会话".to_string());
    }
    if source_variant == target_variant && source_uid == target_uid {
        return Err("来源账号与目标账号相同，无需同步会话".to_string());
    }
    // 与完整预览同口径：国际版数据根不同构时不能给出可写预览，避免误操作。
    for (paths, variant) in [
        (source_paths, source_variant),
        (target_paths, target_variant),
    ] {
        if variant == WbVariant::Ai && !session_copy_supported_at(&paths.data_root) {
            return Ok(json!({
                "supported": false,
                "storeStatus": "unsupported",
                "groups": [],
                "sourceUid": source_uid,
                "targetUid": target_uid,
            }));
        }
    }
    // 只对账目标组：历史脏关联仍会被修复，但不再为一次单对操作扫描全部组。
    let _ = reconcile_stale_link_members_scoped(source_paths, source_variant, Some(group_id));
    if target_variant != source_variant {
        let _ = reconcile_stale_link_members_scoped(target_paths, target_variant, Some(group_id));
    }
    let store = match session_link::load_store(target_paths) {
        StoreState::Ready(store) => store,
        StoreState::Missing => return Err("会话关联组不存在".to_string()),
        StoreState::Unavailable(reason) => return Err(format!("会话关联组暂不可用：{reason}")),
    };
    let group = store
        .groups
        .iter()
        .find(|group| group.id == group_id)
        .ok_or_else(|| "会话关联组不存在".to_string())?;
    if !has_member_for(group, &source_uid) || !has_member_for(group, &target_uid) {
        return Err("该组不包含指定的来源或目标账号".to_string());
    }
    let member_paths = |group: &LinkGroup, member: &LinkMember| -> SessionPaths {
        let variant = session_link::member_variant(group, member);
        if variant == source_variant {
            source_paths.clone()
        } else if variant == target_variant {
            target_paths.clone()
        } else {
            SessionPaths::for_variant(variant)
        }
    };
    let item = preview_group_item(target_paths, &member_paths, group, &source_uid, &target_uid);
    Ok(json!({
        "storeStatus": "ready",
        "groups": [item],
        "sourceUid": source_uid,
        "targetUid": target_uid,
    }))
}

/// 单个关联组的预览项。
///
/// 记录数与差集只用于向用户解释；能否勾选只由判定结果决定（design §3.2）。
/// `member_paths` 解析成员所在档位的数据根：同档恒为同一份，跨档按成员档位
/// （源读源档、目标读目标档）；关联存储与基线与档位无关，统一走 `store_paths`。
fn preview_group_item(
    store_paths: &SessionPaths,
    member_paths: &dyn Fn(&LinkGroup, &LinkMember) -> SessionPaths,
    group: &LinkGroup,
    source_uid: &str,
    target_uid: &str,
) -> Value {
    let source_any = group.members.iter().find(|member| member.uid == source_uid);
    let target_any = group.members.iter().find(|member| member.uid == target_uid);
    // 组展示名取来源会话（同步保留目标的 sessionId、标题与自定义标题）。
    let (title, cwd) = source_any
        .and_then(|member| {
            let paths = member_paths(group, member);
            session_row_info(&paths, &member.session_id)
        })
        .unwrap_or_else(|| ("(无标题)".to_string(), String::new()));
    let source_summary = member_summary(source_any);
    let target_summary = member_summary(target_any);

    let (Some(source_member), Some(target_member)) = (
        session_link::active_member_for(group, source_uid),
        session_link::active_member_for(group, target_uid),
    ) else {
        // 任一方的有效成员缺失（失效/已被替换）→ 关联不确定，不提供任何写入动作。
        return json!({
            "groupId": group.id,
            "title": title,
            "cwd": cwd,
            "verdict": SyncVerdict::Unknown.as_str(),
            "extraA": 0,
            "extraB": 0,
            "common": 0,
            "defaultChecked": false,
            "availableModes": [],
            "reason": "对应的会话已失效或已被替换，需手动处理",
            // 记录数契约与其它不可验证路径一致：source/target 为 0、baseline 为 null
            // （前端的 `recordCount` 类型按此声明，不能发 null）。
            "recordCount": {"source": 0, "target": 0, "baseline": null},
            "source": source_summary,
            "target": target_summary,
        });
    };

    let source_paths = member_paths(group, source_member);
    let target_paths = member_paths(group, target_member);
    let baseline = session_link::load_pair_baseline(
        store_paths,
        group,
        &source_member.member_id,
        &target_member.member_id,
    );
    let source_content = member_content_state(&source_paths, &source_member.session_id);
    let target_content = member_content_state(&target_paths, &target_member.session_id);
    // 目标会话行必须有效：写入与展示都以它为准。来源只读，其会话行即使被折叠/软删，
    // 只要正文仍可完整校验就不影响同步（正文摘要才是内容权威）。
    let target_row_ok = member_row_owned_by(&target_paths, target_member);
    let decision = if target_row_ok {
        session_link::decide_sync(&source_content, &target_content, &baseline)
    } else {
        SyncDecision::unknown("目标账号的关联会话已被删除或归属异常，请重新复制后再同步")
    };
    let reason = decision.reason.clone();
    let modes: Vec<&str> = decision
        .verdict
        .available_modes()
        .iter()
        .map(|mode| mode.as_str())
        .collect();
    // 归档资格：独立于正文判定，只看双方会话行的状态。来源已归档 + 目标处于终态白名单
    // 才授权；缺列/NULL/生成中（working）一律不授权，陈旧 active 放行（写入侧门禁兜底），
    // 也不因此影响既有正文判定的结果。
    // 不可验证的判定不提供任何动作：此时既没有正文模式也不会下发凭据，若仍写出
    // archiveAction，前端摘要会显示「仅同步归档」而用户实际勾不了，等于误导。
    let mut archive = preview_archive_state(
        &source_paths,
        &source_member.session_id,
        &target_paths,
        &target_member.session_id,
    );
    if decision.verdict == SyncVerdict::Unknown {
        archive.action = None;
    }
    let archive_action = archive.action.clone();
    // 归档状态同步作为可选操作，默认不勾选（防误操作），由用户主动勾选授权；正文动作沿用自身默认值。
    let default_checked = decision.default_checked;
    // Keep ordinary sync unavailable for Ahead, but bind its verified snapshot so the
    // session-group "use this copy" flow can explicitly request UnifyOverwrite.
    // 内容一致本身不可勾选，但具备归档资格时要发凭据，否则归档动作无从授权。
    let actionable = !modes.is_empty()
        || decision.verdict == SyncVerdict::Ahead
        || (archive_action.is_some() && decision.verdict == SyncVerdict::Identical);

    let mut item = json!({
        "groupId": group.id,
        "title": title,
        "cwd": cwd,
        "verdict": decision.verdict.as_str(),
        "extraA": decision.extra_a,
        "extraB": decision.extra_b,
        "common": decision.common,
        "defaultChecked": default_checked,
        "availableModes": modes,
        "reason": reason.clone(),
        "recordCount": {
            "source": record_count_of(&source_content),
            "target": record_count_of(&target_content),
            "baseline": baseline.ready().map(|record| record.record_count),
        },
        "source": source_summary,
        "target": target_summary,
    });
    if let Some(action) = archive_action.as_deref() {
        item["archiveAction"] = json!(action);
    }
    if actionable {
        let binding = live_preview_binding(
            group,
            source_member,
            target_member,
            &source_content,
            &target_content,
            &baseline,
            decision.verdict,
            &archive,
        );
        match session_link::save_preview_token(store_paths, binding) {
            Ok(preview_id) => item["previewToken"] = json!(preview_id),
            Err(error) => {
                // 绑定存不下来就不能让用户勾选：不给出可执行动作，并说明原因。
                item["availableModes"] = json!([]);
                item["defaultChecked"] = json!(false);
                item["reason"] = json!(format!("{reason}（检查结果无法保存：{error}）"));
            }
        }
    }
    item
}

/// 校验通过后的执行计划：写入阶段与报告所需的全部已核对信息。
///
/// 正文快照来自同一次读取（在档位锁内、App 已停止写入之后），执行时不再回读来源，
/// 避免执行与校验之间的 TOCTOU。
struct SyncWritePlan {
    group_id: String,
    mode: SyncMode,
    verdict: SyncVerdict,
    source_member: LinkMember,
    target_member: LinkMember,
    source_snapshot: ContentSnapshot,
    target_snapshot: ContentSnapshot,
    /// 待写入目标的新正文：来源正文按本副本 sessionId 替换为目标 sessionId。
    incoming_text: String,
    /// 待写入正文的归一化内容（执行后据此复算摘要核验）。
    incoming: NormalizedContent,
    /// 提交前该成员对的基线引用（清单里记录，便于人工比对）。
    old_baseline_ref: Option<String>,
    /// 本次是否把目标对齐为 `archived`：状态模式下是唯一动作，正文模式下是副作用。
    archive_target: bool,
    /// 目标行在写入前的 `status`：事务内条件 UPDATE 的前置值；缺列时为 None。
    target_status_before: Option<String>,
    reason: String,
}

impl SyncWritePlan {
    fn source_records(&self) -> usize {
        self.source_snapshot.normalized.record_count
    }
}

/// 单条同步选择的校验结果。
enum SyncItemOutcome {
    /// 全部前置条件通过，可进入写入阶段（计划体较大，装箱避免枚举体积失衡）。
    Validated {
        mode: SyncMode,
        verdict: SyncVerdict,
        plan: Box<SyncWritePlan>,
    },
    /// 版本变化/前置条件不满足：跳过该项，不沿用用户旧选择。
    Skipped {
        reason: &'static str,
        message: String,
        verdict: Option<SyncVerdict>,
    },
    /// 入参或凭据非法、模式越权：拒绝，不静默执行。
    Rejected { message: String },
}

/// 重新校验单条选择：凭据、身份、成员、基线、正文逐项核对（design §5.2）。
fn plan_sync_selection(
    context: &SyncContext,
    store: Option<&LinkStore>,
    selection: &SyncSelection,
) -> SyncItemOutcome {
    let paths = context.paths;
    let source_paths = context.source_paths;
    let (source_uid, target_uid) = (context.source_uid, context.target_uid);
    // 凭据必须是我们服务端保存过的：伪造的 id 读不到，直接拒绝。
    let Some(token) = session_link::load_preview_token(paths, &selection.preview_token) else {
        return SyncItemOutcome::Rejected {
            message: "检查结果不存在或已失效，请重新检查后再操作".to_string(),
        };
    };
    let binding = &token.binding;
    // 组级 variant 是创建档位，不作为过滤（跨档组两方向都可同步）；
    // 身份由凭据的组成员绑定与指纹校验（verify_preview）逐项兜底。
    if binding.group_id != selection.group_id {
        return SyncItemOutcome::Rejected {
            message: "检查结果与所选会话不匹配，已拒绝".to_string(),
        };
    }
    let skip = |message: String| SyncItemOutcome::Skipped {
        reason: REASON_PREVIEW_STALE,
        message,
        verdict: Some(binding.verdict),
    };
    // 来源身份取登录态、目标身份取入参：与预览不一致说明账号已经变了。
    if binding.source.uid != source_uid || binding.target.uid != target_uid {
        return skip("账号已变化，检查结果已失效".to_string());
    }
    let Some(store) = store else {
        return skip("同步记录不存在或不可用，检查结果已失效".to_string());
    };
    // 组查找只按 id：跨档组的组级 variant 是创建档位，不作为身份过滤；
    // 两侧成员身份由 active_member_for 与凭据中的成员绑定逐项校验。
    let Some(group) = store
        .groups
        .iter()
        .find(|group| group.id == selection.group_id)
    else {
        return skip("会话的关联关系已不存在，检查结果已失效".to_string());
    };
    let (Some(source_member), Some(target_member)) = (
        session_link::active_member_for(group, source_uid),
        session_link::active_member_for(group, target_uid),
    ) else {
        return skip("对应的会话已失效，检查结果已失效".to_string());
    };
    // 只校验目标行：目标行决定写入对象与展示归属，必须仍然有效。
    // 来源只读，软删/折叠的源会话只要正文可校验仍可作为同步来源（与预览口径一致）。
    if !member_row_owned_by(paths, target_member) {
        return skip("目标会话记录已被删除或归属异常，检查结果已失效".to_string());
    }
    // 重新加载正文、基线与判定，再与凭据逐项核对；任一变化都跳过（含显式覆盖）。
    let source_content = member_content_state(source_paths, &source_member.session_id);
    let target_content = member_content_state(paths, &target_member.session_id);
    let baseline = session_link::load_pair_baseline(
        paths,
        group,
        &source_member.member_id,
        &target_member.member_id,
    );
    let decision = session_link::decide_sync(&source_content, &target_content, &baseline);
    let live = live_preview_binding(
        group,
        source_member,
        target_member,
        &source_content,
        &target_content,
        &baseline,
        decision.verdict,
        &preview_archive_state(
            source_paths,
            &source_member.session_id,
            paths,
            &target_member.session_id,
        ),
    );
    let mismatches = session_link::verify_preview(&token, &live);
    if !mismatches.is_empty() {
        return skip(format!("检查结果已失效：{}", mismatches.join("；")));
    }
    // 本次是否随写入把目标对齐为归档：凭据里授权过才允许，且必须在本次重新核验。
    let archive_authorized = binding.archive_action.as_deref() == Some(ARCHIVE_ACTION_STATUS_ONLY);
    // 判定已按当前内容重算：mode 必须仍然成立，unknown 不得被覆盖绕过。
    // `StatusOnly` 不属于任何正文判定（`allows` 恒为 false），走独立的归档资格门禁。
    if selection.mode == SyncMode::StatusOnly {
        if !archive_authorized {
            return SyncItemOutcome::Rejected {
                message: "该组本次不提供仅同步归档的操作，已拒绝".to_string(),
            };
        }
        if !matches!(
            decision.verdict,
            SyncVerdict::Identical | SyncVerdict::Ahead
        ) {
            return SyncItemOutcome::Rejected {
                message: format!("该组判定为 {}，不允许仅同步归档", decision.verdict.as_str()),
            };
        }
    } else if !decision.verdict.allows(selection.mode) {
        return SyncItemOutcome::Rejected {
            message: format!(
                "该组判定为 {}，不允许以 {} 模式同步",
                decision.verdict.as_str(),
                selection.mode.as_str()
            ),
        };
    }
    // 判定非 unknown 必然双方正文可验证（decide_sync 的前置条件），这里仍然显式兜底。
    // 预览绑定已在上方用只读引用构造完成；这里按值取出快照，避免把整份正文再克隆进
    // 写入计划（大会话下两份正文的克隆会显著抬高写入阶段峰值内存）。
    let (ContentState::Ready(source_snapshot), ContentState::Ready(target_snapshot)) =
        (source_content, target_content)
    else {
        return skip("双方内容不可验证，检查结果已失效".to_string());
    };
    // 归档资格在写入前重新核验（来源仍已归档、目标仍在终态白名单）；任一项不满足
    // 整体判为预览失效，不静默执行另一组副作用。
    let target_status_before = if archive_authorized {
        let source_status = match read_member_status(source_paths, &source_member.session_id) {
            Ok(status) => status,
            Err(error) => return SyncItemOutcome::Rejected { message: error },
        };
        let target_status = match read_member_status(paths, &target_member.session_id) {
            Ok(status) => status,
            Err(error) => return SyncItemOutcome::Rejected { message: error },
        };
        if !archive_qualifies(&source_status, &target_status) {
            return skip("会话的归档状态已变化，检查结果已失效".to_string());
        }
        match target_status {
            SessionStatus::Present(status) => status,
            _ => return skip("会话的归档状态已变化，检查结果已失效".to_string()),
        }
    } else {
        None
    };
    // 正文模式：目标正文 = 来源正文，只把本副本 sessionId 换成目标 sessionId。
    // 状态模式：正文零写入，待写入内容就是目标自己的快照（恢复据此核验「正文没被改」）。
    let (incoming_text, incoming) = if selection.mode == SyncMode::StatusOnly {
        (
            target_snapshot.text.clone(),
            target_snapshot.normalized.clone(),
        )
    } else {
        let incoming_text = source_snapshot
            .text
            .replace(&source_member.session_id, &target_member.session_id);
        let incoming =
            match session_link::normalize_jsonl(&incoming_text, &target_member.session_id) {
                Ok(normalized) => normalized,
                Err(reason) => {
                    return SyncItemOutcome::Rejected {
                        message: format!("目标内容无法按来源内容生成（{reason}），已拒绝"),
                    }
                }
            };
        (incoming_text, incoming)
    };
    SyncItemOutcome::Validated {
        mode: selection.mode,
        verdict: decision.verdict,
        plan: Box::new(SyncWritePlan {
            group_id: selection.group_id.clone(),
            mode: selection.mode,
            verdict: decision.verdict,
            source_member: source_member.clone(),
            target_member: target_member.clone(),
            source_snapshot,
            target_snapshot,
            incoming_text,
            incoming,
            old_baseline_ref: live.baseline_ref,
            archive_target: archive_authorized,
            target_status_before,
            reason: decision.reason,
        }),
    }
}

/// 重新校验勾选的同步项并返回 `sessionSync` 报告（design §5 / §6）。
///
/// 每项走「解析 → 重新加载身份/成员/基线/正文 → 核对预览凭据 → 重新判定 → 备份 →
/// 阶段化写入」：任一版本变化都跳过该项（原因码 [`REASON_PREVIEW_STALE`]），
/// 只在全部校验通过后才写目标；写入到 `completed` 才算 `synced`，
/// 中断的操作保留记录并置 `needsRecovery`，绝不报成成功。
///
/// 调用方需保证 App 已停止写入：本函数自行获取档位操作锁，并在加锁后复查一次。
pub fn sync_sessions_for_switch(
    target_acc: &Value,
    selections: &[SyncSelection],
) -> Result<Value, String> {
    let variant = account::variant_of(target_acc);
    let paths = SessionPaths::for_variant(variant);
    sync_sessions_for_switch_at(
        &paths,
        variant,
        target_acc,
        selections,
        process::is_workbuddy_running,
    )
}

/// 同步上下文中不变的输入（避免逐项重复解析身份）。
///
/// 源 / 目标解耦：`paths` 恒为目标档（写侧），`source_paths` 恒为源档（读侧）；
/// 同档同步时两者相同，跨档同步时指向不同数据根。
struct SyncContext<'a> {
    paths: &'a SessionPaths,
    variant: WbVariant,
    source_paths: &'a SessionPaths,
    source_variant: WbVariant,
    source_uid: &'a str,
    source_account_id: Option<String>,
    target_uid: &'a str,
    target_account_id: Option<String>,
    /// 本次进入写入前仍未完成的操作（同一目标上不得再写一次）。
    pending: &'a [Operation],
}

fn sync_sessions_for_switch_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_acc: &Value,
    selections: &[SyncSelection],
    is_app_running: impl Fn(WbVariant) -> bool,
) -> Result<Value, String> {
    let mut synced: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    if selections.is_empty() {
        return Ok(json!({ "synced": synced, "skipped": skipped, "errors": errors }));
    }
    if variant == WbVariant::Ai && !session_copy_supported_at(&paths.data_root) {
        return Err(format!(
            "{SESSION_SYNC_UNSUPPORTED}（档位 {}）",
            variant.as_str()
        ));
    }
    let target_uid = account_uid(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法同步会话".to_string());
    }
    let source_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法确定来源账号".to_string())?;
    if source_uid == target_uid {
        return Err("当前账号与目标账号相同，无需同步会话".to_string());
    }
    // 与复制同一条门禁：同档路径的源恒为当前登录账号，这里恒为放行（统一规则）。
    if app_running_blocks_target_write(paths, variant, &target_uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }

    // 档位操作锁与复制共用：预览后的校验与写入都不得与并发复制交错。
    let _ops_lock = session_link::try_acquire_variant_ops_lock(paths, variant)?;
    // 复查：锁前未运行、拿锁后目标账号被登录进 App 的情况在这里被拦住。
    if app_running_blocks_target_write(paths, variant, &target_uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    let recovery = recover_pending_session_operations_at(paths, variant);
    let pending = session_link::pending_operations(paths, variant);
    let (store, store_unavailable) = match session_link::load_store(paths) {
        StoreState::Ready(store) => (Some(store), None),
        StoreState::Missing => (None, None),
        StoreState::Unavailable(reason) => (None, Some(reason)),
    };
    // 关系表损坏/未知版本，或存在解析不出来的操作日志：不能当成没有关联继续校验。
    let store_broken = store_unavailable.is_some();
    let blocked = store_unavailable.or_else(|| {
        recovery
            .needs_recovery
            .iter()
            .find(|issue| !issue.retryable && issue.reason.contains(UNPARSEABLE_OPERATION_REASON))
            .map(|issue| issue.reason.clone())
    });
    let context = SyncContext {
        paths,
        variant,
        source_paths: paths,
        source_variant: variant,
        source_uid: &source_uid,
        source_account_id: account_id_for_uid(paths, &source_uid),
        target_uid: &target_uid,
        target_account_id: nonempty_text(
            target_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
        pending: &pending,
    };
    match blocked {
        Some(reason) => {
            for selection in selections {
                errors.push(json!({ "groupId": selection.group_id, "error": reason }));
            }
        }
        None => {
            let profile_started = Instant::now();
            let mut profile_count = 0usize;
            for selection in selections {
                let profile_item_started = Instant::now();
                match plan_sync_selection(&context, store.as_ref(), selection) {
                    SyncItemOutcome::Validated {
                        mode,
                        verdict,
                        plan,
                    } => match execute_sync_item(&context, &plan, mode, verdict) {
                        Ok(item) => synced.push(item),
                        Err(error) => {
                            errors.push(json!({ "groupId": selection.group_id, "error": error }))
                        }
                    },
                    SyncItemOutcome::Skipped {
                        reason,
                        message,
                        verdict,
                    } => skipped.push(json!({
                        "groupId": selection.group_id,
                        "status": "skipped",
                        "reasonCode": reason,
                        "message": message,
                        "verdict": verdict.map(SyncVerdict::as_str),
                    })),
                    SyncItemOutcome::Rejected { message } => {
                        errors.push(json!({ "groupId": selection.group_id, "error": message }))
                    }
                }
                profile_count += 1;
                if sync_profile_enabled() {
                    eprintln!(
                        "[sync-profile] item={profile_count} elapsed_ms={}",
                        profile_item_started.elapsed().as_millis()
                    );
                }
            }
            if sync_profile_enabled() {
                eprintln!(
                    "[sync-profile] total items={profile_count} elapsed_ms={}",
                    profile_started.elapsed().as_millis()
                );
            }
        }
    }

    let unfinished_after = session_link::pending_operations(paths, variant);
    let needs_recovery = store_broken || !recovery.is_clean() || !unfinished_after.is_empty();
    let mut report = json!({ "synced": synced, "skipped": skipped, "errors": errors });
    if needs_recovery {
        report["needsRecovery"] = json!(true);
    }
    // 本轮同步之后再扫一遍：当前项的清理失败/保护残留必须出现在报告里。
    report["temporaryFiles"] = json!(session_backup::maintain(paths, variant));
    if let Some(items) = report.get_mut("synced").and_then(Value::as_array_mut) {
        reconcile_reported_cleanup(items);
    }
    let pinned_pairs = store
        .as_ref()
        .map(|store| pinned_pairs_from_store(store, &source_uid, &target_uid))
        .unwrap_or_default();
    report["pinned"] = session_pinned::sync_pairs(
        &paths.data_root,
        &paths.data_root,
        &source_uid,
        &target_uid,
        &pinned_pairs,
        &paths.store_root,
    );
    Ok(report)
}

/// 把**显式来源账号**的新增内容同步到**显式目标账号**的关联会话（会话管理页；跨档支持）。
///
/// 与 [`sync_sessions_for_switch`] 的差异只在来源判定：源 uid 取自 `source_acc`，
/// 不再从登录态读取；源正文与源行按源档读取，写入侧仍以目标账号为准
/// （目标档数据根 + 备份 + 基线提交）。现有切号入口不受影响（同档语义逐字保留）。
pub fn sync_sessions_cross(
    source_acc: &Value,
    target_acc: &Value,
    selections: &[SyncSelection],
) -> Result<Value, String> {
    let source_variant = account::variant_of(source_acc);
    let target_variant = account::variant_of(target_acc);
    let source_paths = SessionPaths::for_variant(source_variant);
    let target_paths = SessionPaths::for_variant(target_variant);
    let source_uid = account_uid(source_acc);
    if source_uid.is_empty() {
        return Err("源账号缺少 uid，无法同步会话".to_string());
    }
    let target_uid = account_uid(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法同步会话".to_string());
    }
    let source = CopySide {
        paths: &source_paths,
        variant: source_variant,
        uid: source_uid,
        account_id: nonempty_text(
            source_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
    };
    let target = CopySide {
        paths: &target_paths,
        variant: target_variant,
        uid: target_uid,
        account_id: nonempty_text(
            target_acc
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        ),
    };
    sync_sessions_cross_at(
        &source,
        &target,
        selections,
        process::is_workbuddy_running,
        SessionPaths::for_variant,
    )
}

/// 可注入路径与「App 是否运行」探针的跨档同步入口（单测注入临时双档目录与假探针）。
fn sync_sessions_cross_at(
    source: &CopySide<'_>,
    target: &CopySide<'_>,
    selections: &[SyncSelection],
    is_app_running: impl Fn(WbVariant) -> bool,
    resolve_source_paths: impl Fn(WbVariant) -> SessionPaths,
) -> Result<Value, String> {
    let variant = target.variant;
    let mut synced: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    if selections.is_empty() {
        return Ok(json!({ "synced": synced, "skipped": skipped, "errors": errors }));
    }
    // 与复制同一条门禁：仅当目标账号是目标档当前登录账号时才要求关闭客户端
    // （源档只读，运行中的源客户端不影响读取安全）。
    if app_running_blocks_target_write(target.paths, target.variant, &target.uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    // 源档也要读（源正文与源行），国际版数据根不同构时同样不支持（与复制同口径）。
    if source.variant == WbVariant::Ai && !session_copy_supported_at(&source.paths.data_root) {
        return Err(format!(
            "{SESSION_SYNC_UNSUPPORTED}（档位 {}）",
            source.variant.as_str()
        ));
    }
    if target.variant == WbVariant::Ai && !session_copy_supported_at(&target.paths.data_root) {
        return Err(format!(
            "{SESSION_SYNC_UNSUPPORTED}（档位 {}）",
            target.variant.as_str()
        ));
    }
    if source.variant == target.variant && source.uid == target.uid {
        return Err("源账号与目标账号相同，无需同步会话".to_string());
    }

    // 双档操作锁：获取顺序固定为枚举序（Cn → Ai），任何代码路径不得反序（design §2.5）。
    let mut _ops_locks = Vec::new();
    for lock_variant in WbVariant::ALL {
        if lock_variant == source.variant {
            _ops_locks.push(session_link::try_acquire_variant_ops_lock(
                source.paths,
                lock_variant,
            )?);
        } else if lock_variant == target.variant {
            _ops_locks.push(session_link::try_acquire_variant_ops_lock(
                target.paths,
                lock_variant,
            )?);
        }
    }
    // 复查：锁前未运行、拿锁后目标账号被登录进 App 的情况在这里被拦住，不写入任何产物。
    if app_running_blocks_target_write(target.paths, target.variant, &target.uid, &is_app_running) {
        return Err(SESSION_COPY_APP_RUNNING.to_string());
    }
    let recovery =
        recover_pending_session_operations_at_with(target.paths, variant, resolve_source_paths);
    let pending = session_link::pending_operations(target.paths, variant);
    let (store, store_unavailable) = match session_link::load_store(target.paths) {
        StoreState::Ready(store) => (Some(store), None),
        StoreState::Missing => (None, None),
        StoreState::Unavailable(reason) => (None, Some(reason)),
    };
    // 关系表损坏/未知版本，或存在解析不出来的操作日志：不能当成没有关联继续校验。
    let store_broken = store_unavailable.is_some();
    let blocked = store_unavailable.or_else(|| {
        recovery
            .needs_recovery
            .iter()
            .find(|issue| !issue.retryable && issue.reason.contains(UNPARSEABLE_OPERATION_REASON))
            .map(|issue| issue.reason.clone())
    });
    let context = SyncContext {
        paths: target.paths,
        variant: target.variant,
        source_paths: source.paths,
        source_variant: source.variant,
        source_uid: &source.uid,
        source_account_id: source.account_id.clone(),
        target_uid: &target.uid,
        target_account_id: target.account_id.clone(),
        pending: &pending,
    };
    match blocked {
        Some(reason) => {
            for selection in selections {
                errors.push(json!({ "groupId": selection.group_id, "error": reason }));
            }
        }
        None => {
            let profile_started = Instant::now();
            let mut profile_count = 0usize;
            for selection in selections {
                let profile_item_started = Instant::now();
                match plan_sync_selection(&context, store.as_ref(), selection) {
                    SyncItemOutcome::Validated {
                        mode,
                        verdict,
                        plan,
                    } => match execute_sync_item(&context, &plan, mode, verdict) {
                        Ok(item) => synced.push(item),
                        Err(error) => {
                            errors.push(json!({ "groupId": selection.group_id, "error": error }))
                        }
                    },
                    SyncItemOutcome::Skipped {
                        reason,
                        message,
                        verdict,
                    } => skipped.push(json!({
                        "groupId": selection.group_id,
                        "status": "skipped",
                        "reasonCode": reason,
                        "message": message,
                        "verdict": verdict.map(SyncVerdict::as_str),
                    })),
                    SyncItemOutcome::Rejected { message } => {
                        errors.push(json!({ "groupId": selection.group_id, "error": message }))
                    }
                }
                profile_count += 1;
                if sync_profile_enabled() {
                    eprintln!(
                        "[sync-profile] item={profile_count} elapsed_ms={}",
                        profile_item_started.elapsed().as_millis()
                    );
                }
            }
            if sync_profile_enabled() {
                eprintln!(
                    "[sync-profile] total items={profile_count} elapsed_ms={}",
                    profile_started.elapsed().as_millis()
                );
            }
        }
    }

    let unfinished_after = session_link::pending_operations(target.paths, variant);
    let needs_recovery = store_broken || !recovery.is_clean() || !unfinished_after.is_empty();
    let mut report = json!({ "synced": synced, "skipped": skipped, "errors": errors });
    if needs_recovery {
        report["needsRecovery"] = json!(true);
    }
    report["temporaryFiles"] = json!(session_backup::maintain(target.paths, variant));
    if let Some(items) = report.get_mut("synced").and_then(Value::as_array_mut) {
        reconcile_reported_cleanup(items);
    }
    let pinned_pairs = store
        .as_ref()
        .map(|store| pinned_pairs_from_store(store, &source.uid, &target.uid))
        .unwrap_or_default();
    report["pinned"] = session_pinned::sync_pairs(
        &source.paths.data_root,
        &target.paths.data_root,
        &source.uid,
        &target.uid,
        &pinned_pairs,
        &target.paths.store_root,
    );
    Ok(report)
}

// ---------------------------------------------------------------------------
// 同步执行与备份（design §5.4 / §5.5）
// ---------------------------------------------------------------------------

/// 同步备份清单格式版本；读到其它版本一律视为不可用。
pub const SYNC_BACKUP_VERSION: u32 = 1;
/// 数据库快照方法（清单里如实记录，恢复方据此选择恢复方法）。
const DB_SNAPSHOT_METHOD: &str = "sqliteBackupApi";
/// 数据库快照的分页步长与超时：App 已关闭，超时说明库被其它进程占用。
const DB_BACKUP_PAGES_PER_STEP: i32 = 1024;

/// 可选诊断：`WB_SYNC_PROFILE=1` 输出逐项/总耗时，不输出会话 ID、账号或路径。
fn sync_profile_enabled() -> bool {
    matches!(
        std::env::var("WB_SYNC_PROFILE").ok().as_deref(),
        Some("1" | "true" | "yes")
    )
}
const DB_BACKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// 操作日志的 kind 取值。
const OPERATION_KIND_COPY: &str = "copy";
const OPERATION_KIND_SYNC: &str = "sync";

/// 一次同步的备份目录：`backups/session-transactions/{variant}/{operationId}`。
///
/// 身份由调用方（生命周期记录）预分配，目录用 `create_dir` 拒绝复用。
fn sync_backup_dir(
    paths: &SessionPaths,
    variant: WbVariant,
    operation_id: &str,
) -> Result<PathBuf, String> {
    session_backup::transaction_dir(paths, variant, operation_id)
}

fn sync_manifest_file(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

/// 清单里记录的单个成员：身份、覆盖前正文摘要与备份位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBackupMember {
    member_id: String,
    uid: String,
    session_id: String,
    /// 覆盖前正文的备份位置（相对备份目录）；来源侧不写目标，为 None。
    body_file: Option<String>,
    body_raw_digest: String,
    body_normalized_digest: String,
    record_count: usize,
}

/// 本次待写入目标的新正文：恢复补写按这份内容重放，不回读可能已变化的来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBackupPayload {
    body_file: String,
    body_raw_digest: String,
    body_normalized_digest: String,
    record_count: usize,
}

/// 目标会话行的覆盖前快照（恢复「目标行」用，design §5.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBackupRow {
    session_id: String,
    user_id: String,
    title: Option<String>,
    custom_title: Option<String>,
    /// 覆盖前的 `status`；旧清单或旧库缺列时缺省为 None（恢复不得据此改状态）。
    #[serde(default)]
    status: Option<String>,
    updated_at: Option<i64>,
    deleted_at: Option<i64>,
}

/// 数据库备份：一致性快照位置、目标行覆盖前快照与本次写入的时间戳。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBackupDb {
    snapshot_file: String,
    method: String,
    target_row: Option<SyncBackupRow>,
    /// 本次写入目标行的 updated_at（恢复时据此判断这一步是否已应用）。
    new_updated_at: i64,
    /// 本次要写入的 `status`；旧清单与不做状态同步的操作缺省为 None。
    #[serde(default)]
    new_status: Option<String>,
}

/// 同步备份清单：路径、目标行、备份位置与恢复方法（design §5.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBackupManifest {
    version: u32,
    operation_id: String,
    variant: WbVariant,
    group_id: String,
    mode: SyncMode,
    verdict: SyncVerdict,
    created_at: i64,
    /// 目标正文的绝对路径（恢复补写用；使用时必须校验落在本项目录内）。
    target_body_path: String,
    source: SyncBackupMember,
    target: SyncBackupMember,
    incoming: SyncBackupPayload,
    db: SyncBackupDb,
    /// 本次要提交的新配对基线引用（恢复补完复用同一个引用，不重复新建）。
    /// 仅同步归档不产生新基线，为 None；旧清单的字符串仍读为 Some。
    #[serde(default)]
    new_baseline_ref: Option<String>,
    old_baseline_ref: Option<String>,
    /// 提交成功后目标成员的 lastSyncedAt。
    last_synced_at: i64,
    /// 恢复方法（人类可读；清单只在未完成/待清理期间保留，成功清理后随目录删除）。
    restore_steps: Vec<String>,
}

/// 一次同步的备份结果：目录、清单位置与清单本体。
struct SyncBackup {
    dir: PathBuf,
    manifest_file: PathBuf,
    manifest: SyncBackupManifest,
}

/// 备份单个文件并按摘要核验（不一致即报错，不宣称备份成功）。
fn backup_file_with_digest(
    source: &Path,
    dest: &Path,
    expected_raw_digest: &str,
) -> Result<(), String> {
    let bytes = std::fs::read(source).map_err(|error| format!("备份读取失败：{error}"))?;
    if full_digest_of(&bytes) != expected_raw_digest {
        return Err("备份内容与读取时不一致（内容在读取后被改动），已停止保存".to_string());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("备份目录创建失败：{error}"))?;
    }
    session_backup::durable_write(dest, &bytes)
        .map_err(|error| format!("备份保存失败：{error}"))?;
    let read_back = std::fs::read(dest).map_err(|error| format!("备份回读失败：{error}"))?;
    if full_digest_of(&read_back) != expected_raw_digest {
        return Err("备份保存后核验不一致，未按备份成功处理".to_string());
    }
    Ok(())
}

/// 用 SQLite 在线备份 API 生成一致性快照（design §5.4：不直接 cp 活动库）。
///
/// 活动库可能带 WAL/SHM，直接复制会拿到半写状态；backup API 产出的是自洽的独立
/// 数据库文件（同时自动带上未 checkpoint 的 WAL 内容），并做写后核验。
fn snapshot_workbuddy_db(source: &Path, dest: &Path) -> Result<(), String> {
    if !source.is_file() {
        return Err("会话数据不存在，无法备份".to_string());
    }
    if dest.exists() {
        return Err("备份数据库已存在同名文件，未覆盖".to_string());
    }
    // 源连接用读写打开：WAL 库在缺 -shm 时无法只读打开，而备份是写入门禁，
    // 不能因此失败。App 已关闭且持有档位锁，读写打开不会改动会话内容。
    let src = open_db(source, false).ok_or_else(|| "会话数据无法打开，未同步".to_string())?;
    {
        let mut dst =
            Connection::open(dest).map_err(|error| format!("备份数据库创建失败：{error}"))?;
        let backup = Backup::new(&src, &mut dst)
            .map_err(|error| format!("数据库快照初始化失败：{error}"))?;
        let deadline = Instant::now() + DB_BACKUP_TIMEOUT;
        loop {
            match backup.step(DB_BACKUP_PAGES_PER_STEP) {
                Ok(StepResult::Done) => break,
                Ok(StepResult::Busy) | Ok(StepResult::Locked) => {
                    if Instant::now() >= deadline {
                        return Err("数据库快照超时（数据库被占用），未同步".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(_) => {}
                Err(error) => return Err(format!("数据库快照失败：{error}")),
            }
        }
    }
    verify_db_snapshot(dest)
}

/// 快照核验：可回读、完整性检查通过、会话表存在。
fn verify_db_snapshot(path: &Path) -> Result<(), String> {
    let conn =
        open_db(path, true).ok_or_else(|| "备份数据库无法回读，未按备份成功处理".to_string())?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| format!("备份数据库完整性校验失败：{error}"))?;
    if check != "ok" {
        return Err(format!("备份数据库完整性校验未通过：{check}"));
    }
    if !table_exists(&conn, "sessions") {
        return Err("备份数据库缺少数据表，未按备份成功处理".to_string());
    }
    Ok(())
}

/// 读取一行会话的覆盖前快照（含标题类列）。
fn read_session_row(conn: &Connection, cid: &str) -> Result<Option<SyncBackupRow>, String> {
    // Schema errors are not evidence that the target was deleted. Keep custom_title optional
    // for older databases, but propagate every failure while inspecting the schema.
    let mut statement = conn
        .prepare("PRAGMA table_info(sessions)")
        .map_err(|error| format!("会话表结构读取失败：{error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("会话表结构读取失败：{error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("会话表结构读取失败：{error}"))?;
    if columns.is_empty() {
        return Err("会话数据缺少数据表，无法读取目标会话记录".to_string());
    }
    // `custom_title` 与 `status` 都可能缺失：缺列时在 SELECT 里用 NULL 占位保持列数，
    // 避免旧库（含旧测试用例）抛「列名不存在」，也避免 row.get 索引错位。
    let has_custom_title = columns.iter().any(|column| column == "custom_title");
    let has_status = columns.iter().any(|column| column == "status");
    let sql = match (has_custom_title, has_status) {
        (true, true) => {
            "SELECT id, user_id, title, custom_title, status, updated_at, deleted_at \
             FROM sessions WHERE id = ?1"
        }
        (true, false) => {
            "SELECT id, user_id, title, custom_title, NULL, updated_at, deleted_at \
             FROM sessions WHERE id = ?1"
        }
        (false, true) => {
            "SELECT id, user_id, title, NULL, status, updated_at, deleted_at \
             FROM sessions WHERE id = ?1"
        }
        (false, false) => {
            "SELECT id, user_id, title, NULL, NULL, updated_at, deleted_at \
             FROM sessions WHERE id = ?1"
        }
    };
    conn.query_row(sql, [cid], |row| {
        Ok(SyncBackupRow {
            session_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            user_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            title: row.get(2)?,
            custom_title: row.get(3)?,
            status: row.get(4)?,
            updated_at: row.get(5)?,
            deleted_at: row.get(6)?,
        })
    })
    .optional()
    .map_err(|error| format!("目标会话记录读取失败：{error}"))
}

/// 会话行的覆盖前快照；只有成功查询且没有命中时才返回 None。
fn session_row_snapshot(paths: &SessionPaths, cid: &str) -> Result<Option<SyncBackupRow>, String> {
    let conn = open_db(&paths.workbuddy_db(), true)
        .ok_or_else(|| "会话数据无法打开，无法读取目标会话记录".to_string())?;
    read_session_row(&conn, cid)
}

/// 读取备份清单；缺失/损坏/版本不符一律返回 None（视为没有恢复依据）。
fn load_sync_manifest(dir: &Path) -> Option<SyncBackupManifest> {
    let text = std::fs::read_to_string(sync_manifest_file(dir)).ok()?;
    let manifest: SyncBackupManifest = serde_json::from_str(&text).ok()?;
    (manifest.version == SYNC_BACKUP_VERSION).then_some(manifest)
}

/// 备份完整性核验：覆盖前正文与待写入正文都能按清单摘要读回，数据库快照可回读。
fn verify_sync_backup(dir: &Path, manifest: &SyncBackupManifest) -> Result<(), String> {
    let Some(target_body_file) = manifest.target.body_file.as_deref() else {
        return Err("备份清单缺少目标内容备份位置".to_string());
    };
    for (relative, digest) in [
        (target_body_file, manifest.target.body_raw_digest.as_str()),
        (
            manifest.incoming.body_file.as_str(),
            manifest.incoming.body_raw_digest.as_str(),
        ),
    ] {
        let bytes = std::fs::read(dir.join(relative))
            .map_err(|error| format!("备份文件缺失或不可读（{relative}）：{error}"))?;
        if full_digest_of(&bytes) != digest {
            return Err(format!("备份文件摘要不一致（{relative}），已停止恢复"));
        }
    }
    let snapshot = dir.join(&manifest.db.snapshot_file);
    if !snapshot.is_file() {
        return Err("数据库快照缺失，已停止恢复".to_string());
    }
    let conn =
        open_db(&snapshot, true).ok_or_else(|| "数据库快照无法打开，已停止恢复".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("数据库快照缺少数据表，已停止恢复".to_string());
    }
    Ok(())
}

/// 目标正文路径：必须是档位 `projects/` 目录下 `{目标 sessionId}.jsonl`。
///
/// 清单可能被外部改动，恢复写入前必须重新确认路径落在本项目录内，不能按清单原样写。
fn validated_target_body_path(
    paths: &SessionPaths,
    manifest: &SyncBackupManifest,
) -> Result<PathBuf, String> {
    let path = PathBuf::from(&manifest.target_body_path);
    let expected_name = format!("{}.jsonl", manifest.target.session_id);
    let name_matches = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy() == expected_name);
    if !path.starts_with(paths.projects_dir()) || !name_matches {
        return Err("备份清单记录的目标内容路径不合法，已停止保存".to_string());
    }
    Ok(path)
}

/// 创建本次同步的备份：唯一目录 + 覆盖前正文 + 待写入正文 + 数据库一致性快照 + 清单。
///
/// 全过程只读目标、只写备份目录；任何一步失败都返回 Err，调用方必须零写入
/// （不碰目标正文、不改数据库、不提交基线）。`operation_id` 由生命周期记录预分配。
#[allow(clippy::too_many_arguments)] // 与 rotate.rs 同口径：参数都是本次备份的显式输入
fn create_sync_backup(
    paths: &SessionPaths,
    variant: WbVariant,
    operation_id: &str,
    plan: &SyncWritePlan,
    target_body_path: &Path,
    new_updated_at: i64,
    new_baseline_ref: Option<&str>,
    last_synced_at: i64,
) -> Result<SyncBackup, String> {
    let dir = sync_backup_dir(paths, variant, operation_id)?;
    if !dir.is_dir() {
        return Err("操作专属目录不存在，未创建备份".to_string());
    }
    std::fs::create_dir_all(dir.join("bodies"))
        .map_err(|error| format!("同步备份目录创建失败：{error}"))?;

    // 1) 覆盖前正文备份 + 摘要核验（恢复/回滚的唯一依据）。
    let original_rel = format!("bodies/original-{}.jsonl", plan.target_member.session_id);
    backup_file_with_digest(
        target_body_path,
        &dir.join(&original_rel),
        &plan.target_snapshot.full_digest,
    )?;
    // 2) 待写入正文备份 + 摘要核验（写入与恢复都只写这份字节）。
    let incoming_rel = format!("bodies/incoming-{}.jsonl", plan.target_member.session_id);
    let incoming_raw = plan.incoming_text.as_bytes();
    std::fs::write(dir.join(&incoming_rel), incoming_raw)
        .map_err(|error| format!("待保存内容备份失败：{error}"))?;
    let incoming_raw_digest = full_digest_of(incoming_raw);
    if full_digest_of(
        &std::fs::read(dir.join(&incoming_rel))
            .map_err(|error| format!("待保存内容回读失败：{error}"))?,
    ) != incoming_raw_digest
    {
        return Err("待保存内容备份保存后核验不一致，未按备份成功处理".to_string());
    }
    // 3) 数据库一致性快照。
    let db_rel = "workbuddy.db".to_string();
    snapshot_workbuddy_db(&paths.workbuddy_db(), &dir.join(&db_rel))?;
    let target_row = session_row_snapshot(paths, &plan.target_member.session_id)?;
    // 规划与备份之间状态被改动就不按「可归档」继续：条件 UPDATE 的前置值必须来自
    // 与规划时一致的现场，不能拿一份已经过期的前置值去写库。
    if plan.archive_target
        && target_row.as_ref().and_then(|row| row.status.clone()) != plan.target_status_before
    {
        return Err("目标会话的状态在校验后发生变化，已停止保存".to_string());
    }

    let manifest = SyncBackupManifest {
        version: SYNC_BACKUP_VERSION,
        operation_id: operation_id.to_string(),
        variant,
        group_id: plan.group_id.clone(),
        mode: plan.mode,
        verdict: plan.verdict,
        created_at: now_ms(),
        target_body_path: target_body_path.to_string_lossy().to_string(),
        source: SyncBackupMember {
            member_id: plan.source_member.member_id.clone(),
            uid: plan.source_member.uid.clone(),
            session_id: plan.source_member.session_id.clone(),
            body_file: None,
            body_raw_digest: plan.source_snapshot.full_digest.clone(),
            body_normalized_digest: plan.source_snapshot.normalized.total_digest.clone(),
            record_count: plan.source_records(),
        },
        target: SyncBackupMember {
            member_id: plan.target_member.member_id.clone(),
            uid: plan.target_member.uid.clone(),
            session_id: plan.target_member.session_id.clone(),
            body_file: Some(original_rel),
            body_raw_digest: plan.target_snapshot.full_digest.clone(),
            body_normalized_digest: plan.target_snapshot.normalized.total_digest.clone(),
            record_count: plan.target_snapshot.normalized.record_count,
        },
        incoming: SyncBackupPayload {
            body_file: incoming_rel,
            body_raw_digest: incoming_raw_digest,
            body_normalized_digest: plan.incoming.total_digest.clone(),
            record_count: plan.incoming.record_count,
        },
        db: SyncBackupDb {
            snapshot_file: db_rel,
            method: DB_SNAPSHOT_METHOD.to_string(),
            target_row,
            new_updated_at,
            new_status: plan
                .archive_target
                .then(|| SESSION_STATUS_ARCHIVED.to_string()),
        },
        new_baseline_ref: new_baseline_ref.map(str::to_string),
        old_baseline_ref: plan.old_baseline_ref.clone(),
        last_synced_at,
        restore_steps: vec![
            "目标内容：从 bodies/ 下 original-*.jsonl 写回目标路径（先校验当前内容是否为本次保存的内容）"
                .to_string(),
            "目标会话记录：把 sessions.updated_at 还原为清单 db.targetRow.updatedAt（先校验归属与当前值）"
                .to_string(),
            "数据库：workbuddy.db 为本次保存前的一致性快照，可用 SQLite 打开核对；不要整库覆盖当前库"
                .to_string(),
            "同步记录：把 manifest.newBaselineRef 对应的成员间同步记录还原为 oldBaselineRef（若未改动则无需处理）"
                .to_string(),
        ],
    };
    // 4) 清单落盘并回读核验：备份必须可验证恢复，读不回来的清单不算备份成功。
    let content = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    let manifest_file = sync_manifest_file(&dir);
    session_backup::durable_write_str(&manifest_file, &content)
        .map_err(|error| format!("同步备份清单保存失败：{error}"))?;
    match load_sync_manifest(&dir) {
        Some(read_back) if read_back == manifest => {}
        _ => return Err("同步备份清单保存后核验不一致，未按备份成功处理".to_string()),
    }
    verify_sync_backup(&dir, &manifest)?;
    Ok(SyncBackup {
        dir,
        manifest_file,
        manifest,
    })
}

/// 目标正文相对本次操作的状态。
enum SyncBodyState {
    /// 已经是本次写入的内容。
    Ours,
    /// 仍是覆盖前的内容（尚未写入，或已被还原）。
    PreSync,
    /// 正文不存在。
    Gone,
    /// 两者都不是：可能被其它程序改动，停止恢复，不覆盖未知内容。
    Unknown(String),
}

fn classify_sync_body(
    target_body_path: &Path,
    target_session_id: &str,
    pre_sync_raw_digest: &str,
    expected_digest: &str,
) -> SyncBodyState {
    match session_link::read_content_snapshot(target_body_path, target_session_id) {
        ContentState::Ready(content) if content.normalized.total_digest == expected_digest => {
            SyncBodyState::Ours
        }
        ContentState::Ready(content) if content.full_digest == pre_sync_raw_digest => {
            SyncBodyState::PreSync
        }
        ContentState::Ready(_) => SyncBodyState::Unknown(
            "目标内容与本次保存及覆盖前版本都不一致（可能被其它程序改动），已停止保存，不覆盖未知内容"
                .to_string(),
        ),
        ContentState::Missing => SyncBodyState::Gone,
        ContentState::Unavailable(reason) => SyncBodyState::Unknown(format!(
            "目标内容无法验证（{reason}），已停止保存"
        )),
    }
}

/// 原子替换目标正文（同目录临时文件 + rename），写后复算摘要核验。
fn write_sync_body(
    target_body_path: &Path,
    target_session_id: &str,
    text: &str,
    expected_digest: &str,
) -> Result<NormalizedContent, String> {
    // 正文属于业务完成门禁：会话专用持久化写（sync_all + 父目录持久化）。
    session_backup::durable_write_str(target_body_path, text)
        .map_err(|error| format!("同步内容保存失败：{error}"))?;
    match session_link::read_content_snapshot(target_body_path, target_session_id) {
        ContentState::Ready(read_back) if read_back.normalized.total_digest == expected_digest => {
            Ok(read_back.normalized)
        }
        ContentState::Ready(_) => Err("同步内容保存后校验不一致，未按成功处理".to_string()),
        ContentState::Missing => Err("同步内容保存后不存在，未按成功处理".to_string()),
        ContentState::Unavailable(reason) => Err(format!("同步内容保存后无法确认：{reason}")),
    }
}

/// 从备份目录取待写入正文并原子替换目标正文：写入的字节等于备份的字节。
fn apply_sync_body(
    paths: &SessionPaths,
    backup_dir: &Path,
    manifest: &SyncBackupManifest,
    expected_digest: &str,
) -> Result<NormalizedContent, String> {
    let target_body_path = validated_target_body_path(paths, manifest)?;
    let bytes = std::fs::read(backup_dir.join(&manifest.incoming.body_file))
        .map_err(|error| format!("待保存内容备份读取失败：{error}"))?;
    if full_digest_of(&bytes) != manifest.incoming.body_raw_digest {
        return Err("待保存内容备份与清单不一致，已停止保存".to_string());
    }
    let text =
        String::from_utf8(bytes).map_err(|_| "待保存内容不是合法 UTF-8，未保存".to_string())?;
    write_sync_body(
        &target_body_path,
        &manifest.target.session_id,
        &text,
        expected_digest,
    )
}

/// 事务内更新目标行 updated_at：命中归属（owner = 目标 uid）且未删除。
///
/// 目标行状态的条件写入要求：事务内按前置状态等值匹配，避免覆盖并发改动。
struct StatusChange<'a> {
    /// 覆盖前的 `status`（事务内 WHERE 条件）。
    before: String,
    /// 本次要写入的状态。
    after: &'a str,
}

/// 更新目标会话行：`updated_at` 恒写，状态只在本次获得授权时按前置值条件写入。
///
/// 只改 updated_at（与本次状态）；sessionId、标题与 custom_title 必须与覆盖前一致（R4）。
fn update_target_session_row(
    paths: &SessionPaths,
    target: &OperationMember,
    new_updated_at: i64,
    before: Option<&SyncBackupRow>,
    status_change: Option<&StatusChange<'_>>,
) -> Result<(), String> {
    let mut conn = open_db(&paths.workbuddy_db(), false)
        .ok_or_else(|| "会话数据无法打开，未同步".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("会话数据缺少数据表，未同步".to_string());
    }
    // 写事务的提交必须可靠持久：在本次实际写连接上确认 synchronous ≥ FULL。
    session_backup::ensure_full_synchronous(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("会话数据事务开启失败：{error}"))?;
    // 状态写入用条件 UPDATE：目标在事务内被改为活跃态/被取消归档时 affected 为 0，
    // 事务外的一次读取不能替代事务内的条件。
    let affected = match status_change {
        // `status = after` 这一支是幂等重放：数据库已提交但阶段标记未推进时，恢复会
        // 再跑一次本函数。前置值不匹配就不写——活跃态、未知值均被排除在外。
        Some(change) => tx.execute(
            "UPDATE sessions SET updated_at = ?1, status = ?2 \
             WHERE id = ?3 AND user_id = ?4 AND deleted_at IS NULL \
             AND (status = ?5 OR status = ?6)",
            rusqlite::params![
                new_updated_at,
                change.after,
                target.session_id,
                target.uid,
                change.before,
                change.after
            ],
        ),
        None => tx.execute(
            "UPDATE sessions SET updated_at = ?1 \
             WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
            rusqlite::params![new_updated_at, target.session_id, target.uid],
        ),
    }
    .map_err(|error| format!("目标会话记录更新失败：{error}"))?;
    if affected != 1 {
        return Err(
            "目标会话记录归属校验失败：会话不存在、不属于目标账号或已被删除，未按成功处理"
                .to_string(),
        );
    }
    match (read_session_row(&tx, &target.session_id)?, before) {
        (None, _) => return Err("目标会话记录保存后不可见，未按成功处理".to_string()),
        (Some(after), Some(before)) => {
            if after.session_id != before.session_id
                || after.user_id != before.user_id
                || after.title != before.title
                || after.custom_title != before.custom_title
            {
                return Err(
                    "目标会话记录的归属或标题在保存期间发生变化，已回滚本次更新".to_string()
                );
            }
            if after.updated_at != Some(new_updated_at) {
                return Err("目标会话记录更新时间未按本次保存生效，未按成功处理".to_string());
            }
            // 只在本次确实写状态时才校验状态。不写状态的正文同步里，目标被第三方
            // 从 completed 改成 active 属于与本次无关的変化，保留当前状态继续补完
            // 正文，不能把一次正常的正文恢复升级成人工处理。
            // （旧清单同样记不到 status：此处若拿 None 去比对，真实库上恒不相等。）
            if let Some(change) = status_change {
                if after.status.as_deref() != Some(change.after) {
                    return Err("目标会话记录的状态在保存期间发生变化，已回滚本次更新".to_string());
                }
            }
        }
        (Some(_), None) => {}
    }
    tx.commit()
        .map_err(|error| format!("会话数据提交失败：{error}"))?;
    Ok(())
}

/// 提交 A/B 新基线与目标成员 lastSyncedAt（定向更新，不触碰其它配对）。
///
/// 新基线用新的 ref，避免覆盖被其它成员对继承的历史基线（A/B 同步不代表 C 也同步）。
/// 组查找只按 id（跨档组的组级 variant 是创建档位，不作过滤）；成员身份由下面的
/// member_id 校验兜底。
fn commit_sync_baseline(
    paths: &SessionPaths,
    manifest: &SyncBackupManifest,
    normalized: &NormalizedContent,
) -> Result<(), String> {
    // 仅同步归档不产生新基线：缺引用即拒绝提交，不能用空引用或假引用蒙混。
    let Some(new_baseline_ref) = manifest.new_baseline_ref.as_deref() else {
        return Err("同步备份清单缺少新基线引用，未提交同步结果".to_string());
    };
    let source_member_id = manifest.source.member_id.as_str();
    let target_member_id = manifest.target.member_id.as_str();
    session_link::with_link_store_write(paths, |store| {
        let Some(group) = store
            .groups
            .iter_mut()
            .find(|group| group.id == manifest.group_id)
        else {
            return Err("会话的关联关系已不存在，未提交同步结果".to_string());
        };
        if !group
            .members
            .iter()
            .any(|member| member.member_id == source_member_id)
        {
            return Err("当前账号的会话已不存在，未提交同步结果".to_string());
        }
        {
            let Some(target) = group
                .members
                .iter_mut()
                .find(|member| member.member_id == target_member_id)
            else {
                return Err("目标账号的会话已不存在，未提交同步结果".to_string());
            };
            target.last_synced_at = Some(manifest.last_synced_at);
        }
        session_link::save_baseline(paths, new_baseline_ref, normalized)?;
        session_link::set_pair_base(
            group,
            source_member_id,
            target_member_id,
            new_baseline_ref,
            NORMALIZATION_VERSION,
        );
        Ok(())
    })
}

/// 核验 A/B 配对基线已提交为本次的新引用（已越过该阶段时不重放，只核验）。
fn verify_sync_baseline_committed(
    paths: &SessionPaths,
    manifest: &SyncBackupManifest,
) -> Result<(), String> {
    // 仅同步归档没有新基线：这类操作不会走到本函数，缺引用即为清单异常。
    let Some(new_baseline_ref) = manifest.new_baseline_ref.as_deref() else {
        return Err("同步备份清单缺少新基线引用，已停止恢复".to_string());
    };
    match session_link::load_store(paths) {
        StoreState::Ready(store) => {
            let Some(group) = store
                .groups
                .iter()
                .find(|group| group.id == manifest.group_id)
            else {
                return Err("会话的关联关系缺失，已停止恢复".to_string());
            };
            let Some(pair) = session_link::find_pair_base(
                group,
                &manifest.source.member_id,
                &manifest.target.member_id,
            ) else {
                return Err("同步记录缺失，已停止恢复".to_string());
            };
            if Some(pair.baseline_ref.as_str()) != manifest.new_baseline_ref.as_deref() {
                return Err("同步记录与本次保存不一致，已停止恢复".to_string());
            }
            if session_link::load_baseline(paths, new_baseline_ref).is_none() {
                return Err("同步记录缺失或内容不一致，已停止恢复".to_string());
            }
            Ok(())
        }
        StoreState::Missing => Err("同步记录主文件缺失，已停止恢复".to_string()),
        StoreState::Unavailable(reason) => Err(reason),
    }
}

/// 用备份里的覆盖前正文回滚目标正文（只在当前内容确为本次写入时调用）。
fn restore_sync_backup_body(
    paths: &SessionPaths,
    dir: &Path,
    manifest: &SyncBackupManifest,
) -> Result<(), String> {
    let Some(relative) = manifest.target.body_file.as_deref() else {
        return Err("备份清单缺少目标内容备份位置".to_string());
    };
    let bytes =
        std::fs::read(dir.join(relative)).map_err(|error| format!("备份内容读取失败：{error}"))?;
    if full_digest_of(&bytes) != manifest.target.body_raw_digest {
        return Err("备份内容摘要不一致，已停止回滚".to_string());
    }
    let text =
        String::from_utf8(bytes).map_err(|_| "备份内容不是合法 UTF-8，未回滚".to_string())?;
    let target_body_path = validated_target_body_path(paths, manifest)?;
    session_backup::durable_write_str(&target_body_path, &text)
        .map_err(|error| format!("目标内容回滚失败：{error}"))
}

/// 清单里的状态写入要求（新状态 + 覆盖前状态）；缺覆盖前状态即不可核验。
fn status_change_of(manifest: &SyncBackupManifest) -> Result<Option<StatusChange<'_>>, String> {
    let Some(after) = manifest.db.new_status.as_deref() else {
        return Ok(None);
    };
    let Some(before) = manifest
        .db
        .target_row
        .as_ref()
        .and_then(|row| row.status.clone())
    else {
        return Err("备份清单缺少目标会话的覆盖前状态，已停止保存".to_string());
    };
    Ok(Some(StatusChange { before, after }))
}

/// 正文模式下本次实际执行的状态写入。
///
/// 正文模式的归档是副作用：目标已不在可归档的状态（例如被打开后变成 active）时，
/// 不能为了归档把正文恢复卡成人工处理——跳过状态写入，正文照常补完。
/// 仅同步归档里状态就是全部动作，必须保留要求，由事务内条件 UPDATE 兜底。
fn planned_status_change<'a>(
    paths: &SessionPaths,
    operation: &Operation,
    manifest: &'a SyncBackupManifest,
) -> Result<Option<StatusChange<'a>>, String> {
    let Some(change) = status_change_of(manifest)? else {
        return Ok(None);
    };
    if manifest.mode == SyncMode::StatusOnly {
        return Ok(Some(change));
    }
    let current =
        session_row_snapshot(paths, &operation.target.session_id)?.and_then(|row| row.status);
    let still_archivable = current.as_deref() == Some(change.after)
        || current.as_deref() == Some(change.before.as_str());
    Ok(still_archivable.then_some(change))
}

/// 仅同步归档的阶段推进：`Prepared → DbWritten → Completed`。
///
/// 跳过 `BodyWritten` 与 `LinksCommitted`：本次没有正文写入，也不产生新基线，
/// 不能伪造「正文已写入」或「基线已提交」的含义。
fn run_status_only_phases(
    paths: &SessionPaths,
    operation: &mut Operation,
    manifest: &SyncBackupManifest,
    body: SyncBodyState,
) -> Result<(), String> {
    // 1) 正文：只核验仍是本次读取的目标原文；任何写入（含补写与回滚）都不允许。
    match body {
        SyncBodyState::Ours | SyncBodyState::PreSync => {
            let target_body_path = validated_target_body_path(paths, manifest)?;
            match session_link::read_content_snapshot(
                &target_body_path,
                &manifest.target.session_id,
            ) {
                ContentState::Ready(content)
                    if content.normalized.total_digest == operation.expected_content_digest => {}
                ContentState::Ready(_) => {
                    return Err("目标内容与本次保存不一致，状态同步未执行".to_string())
                }
                ContentState::Missing => return Err("目标内容丢失，状态同步未执行".to_string()),
                ContentState::Unavailable(reason) => {
                    return Err(format!("目标内容无法验证（{reason}），状态同步未执行"))
                }
            }
        }
        SyncBodyState::Gone => return Err("目标内容不存在，状态同步未执行".to_string()),
        SyncBodyState::Unknown(reason) => return Err(reason),
    }
    // 2) 状态：归档是本次唯一动作，必须带可核验的覆盖前状态。
    let Some(status_change) = status_change_of(manifest)? else {
        return Err("备份清单未记录本次归档状态，状态同步未执行".to_string());
    };
    update_target_session_row(
        paths,
        &operation.target,
        manifest.db.new_updated_at,
        manifest.db.target_row.as_ref(),
        Some(&status_change),
    )?;
    advance_operation(paths, operation, OpPhase::DbWritten)?;
    // 3) 不提交基线、不推进目标成员 lastSyncedAt：正文与配对基线保持原值。
    advance_operation(paths, operation, OpPhase::Completed)?;
    Ok(())
}

/// 阶段化写入：正文 → 数据库 → 组表 → completed；每步先落阶段再推进，可恢复。
///
/// 正常执行与恢复共用同一段代码：阶段只前进不回退，已越过的阶段不重放（只核验产物），
/// 因此恢复不会产生第二份写入。仅同步归档走 [`run_status_only_phases`]，不进正文分支。
fn run_sync_phases(
    paths: &SessionPaths,
    operation: &mut Operation,
    backup_dir: &Path,
    manifest: &SyncBackupManifest,
    body: SyncBodyState,
) -> Result<(), String> {
    // 仅同步归档：正文零写入，也不提交新基线。必须在这里分流——复用正文分支时
    // `Gone` 会把备份里的目标原文写回业务正文，等于补写正文。
    if manifest.mode == SyncMode::StatusOnly {
        return run_status_only_phases(paths, operation, manifest, body);
    }
    let normalized = match body {
        // 已写入：只核验现场，不重写。
        SyncBodyState::Ours => {
            let target_body_path = validated_target_body_path(paths, manifest)?;
            match session_link::read_content_snapshot(
                &target_body_path,
                &manifest.target.session_id,
            ) {
                ContentState::Ready(content)
                    if content.normalized.total_digest == operation.expected_content_digest =>
                {
                    content.normalized
                }
                ContentState::Ready(_) => {
                    return Err("目标内容与本次保存不一致，已停止恢复".to_string())
                }
                ContentState::Missing => return Err("目标内容丢失，已停止恢复".to_string()),
                ContentState::Unavailable(reason) => {
                    return Err(format!("目标内容无法验证（{reason}），已停止恢复"))
                }
            }
        }
        SyncBodyState::PreSync | SyncBodyState::Gone => {
            let normalized = apply_sync_body(
                paths,
                backup_dir,
                manifest,
                &operation.expected_content_digest,
            )?;
            advance_operation(paths, operation, OpPhase::BodyWritten)?;
            normalized
        }
        SyncBodyState::Unknown(reason) => return Err(reason),
    };

    // 数据库步骤必须**恰好执行一次**：本次时间戳已落库说明这一步已由本操作提交过，
    // 重跑会把首次执行时合法跳过的归档副作用补上（当时目标是 active，现在回到
    // completed 又变成了「可归档」）——等于在恢复里执行了用户当次没同意的动作。
    // 仅同步归档不受影响：它的 `db_applied` 已确认状态就是本次新值，重跑等价。
    let db_already_applied = manifest.mode != SyncMode::StatusOnly
        && session_row_snapshot(paths, &operation.target.session_id)?
            .and_then(|row| row.updated_at)
            == Some(manifest.db.new_updated_at);
    if db_already_applied {
        // 行仍归属目标账号且未删除即可视为本操作的产物，不写库、不改状态。
        let owner = session_row_owner(paths, &operation.target.session_id)
            .ok_or_else(|| "目标会话记录不存在或已被删除，已停止恢复".to_string())?;
        if owner != operation.target.uid {
            return Err("目标会话记录归属异常，已停止恢复".to_string());
        }
    } else {
        let status_change = planned_status_change(paths, operation, manifest)?;
        update_target_session_row(
            paths,
            &operation.target,
            manifest.db.new_updated_at,
            manifest.db.target_row.as_ref(),
            status_change.as_ref(),
        )?;
    }
    advance_operation(paths, operation, OpPhase::DbWritten)?;

    if operation.phase < OpPhase::LinksCommitted {
        commit_sync_baseline(paths, manifest, &normalized)?;
        advance_operation(paths, operation, OpPhase::LinksCommitted)?;
    } else {
        verify_sync_baseline_committed(paths, manifest)?;
    }
    advance_operation(paths, operation, OpPhase::Completed)?;
    Ok(())
}

/// 执行单条同步：备份 → 阶段化写入 → completed。
///
/// 任一阶段失败都保留未完成操作（同一目标 UUID 与新基线引用，下次切换按清单补完），
/// 绝不把未完成的写入报告成 `synced`。
fn execute_sync_item(
    context: &SyncContext,
    plan: &SyncWritePlan,
    mode: SyncMode,
    verdict: SyncVerdict,
) -> Result<Value, String> {
    let paths = context.paths;
    // 同一目标上仍有未完成写入：本轮不得再写一次（等恢复完成）。
    if let Some(operation) = context.pending.iter().find(|operation| {
        operation.phase.is_unfinished()
            && operation.target.session_id == plan.target_member.session_id
    }) {
        return Err(format!(
            "上一次会话保存尚未完成（操作 {}）：{}，本次未保存",
            operation.operation_id,
            operation
                .last_error
                .clone()
                .unwrap_or_else(|| "等待恢复".to_string())
        ));
    }
    let target_body_path = find_project_jsonl(paths, &plan.target_member.session_id)
        .ok_or_else(|| "目标内容不存在，未同步".to_string())?;
    let new_updated_at = now_ms();
    let last_synced_at = now_ms();
    // 仅同步归档不产生新正文，也就没有新配对基线：显式记为 None，不用空引用占位。
    let new_baseline_ref = (mode != SyncMode::StatusOnly).then(|| uuid::Uuid::new_v4().to_string());
    // 预分配身份：维护记录（allocating）先于备份与业务写入（design §3）。
    let target_title =
        session_row_info(paths, &plan.target_member.session_id).map(|(title, _)| title);
    let mut lifecycle = session_backup::begin_operation(
        paths,
        context.variant,
        OPERATION_KIND_SYNC,
        Some(plan.target_member.session_id.clone()),
        target_title,
    )?;
    // 备份是一道门禁：备份不可信就零写入（不碰正文、不改数据库、不提交基线）。
    let backup = match create_sync_backup(
        paths,
        context.variant,
        &lifecycle.operation_id,
        plan,
        &target_body_path,
        new_updated_at,
        new_baseline_ref.as_deref(),
        last_synced_at,
    ) {
        Ok(backup) => backup,
        Err(error) => {
            // 受控失败分支：protected 之前未写业务，可安全回收准备残留。
            session_backup::reclaim_unwritten(paths, context.variant, &mut lifecycle, &error);
            return Err(error);
        }
    };
    // 备份完备先转 protected：此步失败禁止业务写入，残留保守保留待下次维护。
    session_backup::mark_protected(paths, &mut lifecycle)?;

    let mut operation = Operation {
        version: OPERATION_VERSION,
        operation_id: lifecycle.operation_id.clone(),
        kind: OPERATION_KIND_SYNC.to_string(),
        variant: context.variant,
        // 同档同步写 None（与旧记录逐字兼容）；跨档同步显式记录源档（审计与恢复依据）。
        source_variant: (context.source_variant != context.variant)
            .then_some(context.source_variant),
        group_id: plan.group_id.clone(),
        source: OperationMember {
            account_id: context.source_account_id.clone(),
            uid: plan.source_member.uid.clone(),
            session_id: plan.source_member.session_id.clone(),
        },
        target: OperationMember {
            account_id: context.target_account_id.clone(),
            uid: plan.target_member.uid.clone(),
            session_id: plan.target_member.session_id.clone(),
        },
        expected_content_digest: plan.incoming.total_digest.clone(),
        expected_record_count: plan.incoming.record_count,
        phase: OpPhase::Prepared,
        backup: Some(backup.manifest_file.to_string_lossy().to_string()),
        lifecycle_version: Some(OPERATION_LIFECYCLE_VERSION),
        cleanup_state: None,
        last_error: None,
        created_at: now_ms(),
        updated_at: now_ms(),
    };
    session_link::save_operation(paths, &operation)?;

    let body = classify_sync_body(
        &target_body_path,
        &plan.target_member.session_id,
        &plan.target_snapshot.full_digest,
        &operation.expected_content_digest,
    );
    if let Err(error) = run_sync_phases(paths, &mut operation, &backup.dir, &backup.manifest, body)
    {
        fail_operation(paths, &mut operation, &error);
        return Err(error);
    }
    let _ = session_link::prune_operations(
        paths,
        context.variant,
        session_link::KEEP_COMPLETED_OPERATIONS,
    );
    // 业务可靠完成后才授权清理；清理失败不回滚业务、不报告同步失败。
    let cleanup = finish_backup_cleanup(paths, context.variant, &mut lifecycle);
    let (backup_path, cleanup_state, cleanup_error) = report_cleanup(&cleanup, &backup.dir);
    let manifest_path = backup_path
        .as_ref()
        .map(|path| format!("{path}/manifest.json"));
    let mut item = json!({
        "groupId": plan.group_id,
        "status": "synced",
        "verdict": verdict.as_str(),
        "mode": mode.as_str(),
        "sourceSessionId": plan.source_member.session_id,
        "targetSessionId": plan.target_member.session_id,
        "recordCount": {
            "source": plan.source_records(),
            "targetBefore": plan.target_snapshot.normalized.record_count,
            "target": plan.incoming.record_count,
        },
        "updatedAt": backup.manifest.db.new_updated_at,
        "backup": backup_path,
        "backupManifest": manifest_path,
        "cleanupState": cleanup_state,
        "message": plan.reason,
    });
    if let Some(error) = cleanup_error {
        item["cleanupError"] = json!(error);
    }
    Ok(item)
}

// ---------------------------------------------------------------------------
// 未完成操作恢复（design §4.2 / §4.5）
// ---------------------------------------------------------------------------

/// 恢复某档位全部未完成操作（需已持有档位操作锁）。
///
/// 恢复先检查实际状态再决定下一步，不盲目重放；中间产物被改动或丢失时只上报
/// needsRecovery，不覆盖未知内容。
///
/// 跨档操作的源档数据根按 [`SessionPaths::for_variant`] 解析；单测与需要注入
/// 临时目录的宿主用 [`recover_pending_session_operations_at_with`]。
pub fn recover_pending_session_operations_at(
    paths: &SessionPaths,
    variant: WbVariant,
) -> RecoveryReport {
    recover_pending_session_operations_at_with(paths, variant, SessionPaths::for_variant)
}

/// [`recover_pending_session_operations_at`] 的可注入版本：`resolve_source_paths`
/// 用于解析跨档操作（`sourceVariant` 与操作档不同）的源档数据根。
pub fn recover_pending_session_operations_at_with<F>(
    paths: &SessionPaths,
    variant: WbVariant,
    resolve_source_paths: F,
) -> RecoveryReport
where
    F: Fn(WbVariant) -> SessionPaths,
{
    let mut report = RecoveryReport::default();
    let scan = session_link::scan_operations(paths);
    // 扫描不完整（目录不可读/枚举失败）不能按「没有未完成操作」继续写：与解析失败
    // 同口径阻断，避免绕过 pending 去重后写出第二个副本。
    if !scan.complete {
        report.needs_recovery.push(RecoveryIssue {
            operation_id: "operation-scan".to_string(),
            reason: format!(
                "{UNPARSEABLE_OPERATION_REASON}（操作记录无法读取），已停止恢复以免产生重复复制"
            ),
            retryable: false,
        });
    }
    for problem in scan.problems {
        report.needs_recovery.push(RecoveryIssue {
            operation_id: problem.clone(),
            reason: format!(
                "{UNPARSEABLE_OPERATION_REASON}（{problem}），已停止恢复以免产生重复复制"
            ),
            retryable: false,
        });
    }
    for operation in scan
        .operations
        .into_iter()
        .filter(|operation| operation.variant == variant && operation.phase.is_unfinished())
    {
        // 跨档操作（源档与操作档不同）按源档解析读侧数据根；同档与旧记录视为同档。
        let source_paths = operation
            .source_variant
            .filter(|source_variant| *source_variant != operation.variant)
            .map(&resolve_source_paths);
        match recover_operation(paths, variant, operation, source_paths.as_ref()) {
            RecoverOutcome::Recovered(id) => report.recovered.push(id),
            RecoverOutcome::Abandoned(id) => report.abandoned.push(id),
            RecoverOutcome::NeedsRecovery {
                id,
                reason,
                retryable,
            } => {
                report.needs_recovery.push(RecoveryIssue {
                    operation_id: id,
                    reason,
                    retryable,
                });
            }
        }
    }
    // 恢复之后补清理：回收可安全回收的临时备份残留，保护其余并上报（design §6）。
    report.temporary_files = session_backup::maintain(paths, variant);
    report
}

/// 生产入口：自行获取档位操作锁后恢复（被占用时返回明确错误，不排队）。
pub fn recover_pending_session_operations(variant: WbVariant) -> Result<RecoveryReport, String> {
    let paths = SessionPaths::for_variant(variant);
    let _lock = session_link::try_acquire_variant_ops_lock(&paths, variant)?;
    Ok(recover_pending_session_operations_at_with(
        &paths,
        variant,
        SessionPaths::for_variant,
    ))
}

enum RecoverOutcome {
    Recovered(String),
    Abandoned(String),
    NeedsRecovery {
        id: String,
        reason: String,
        /// 既有宿主契约：true 允许账号切换和 App 启动，不能仅表示故障可重试。
        /// 同步尚未恢复一致时必须为 false，即使稍后重试可能成功。
        retryable: bool,
    },
}

/// 目标正文现状判定（恢复的第一步）。
enum BodyCheck {
    /// 正文与操作记录一致。
    Verified(NormalizedContent),
    /// 尚未写入（操作停在 Prepared 阶段）。
    Absent,
    /// 中间产物被改动/丢失：停止恢复。
    NeedsRecovery(String),
}

fn check_target_body(paths: &SessionPaths, operation: &Operation) -> BodyCheck {
    match find_project_jsonl(paths, &operation.target.session_id)
        .as_deref()
        .map(|path| session_link::read_content_snapshot(path, &operation.target.session_id))
    {
        Some(ContentState::Ready(content)) => {
            if content.normalized.total_digest == operation.expected_content_digest {
                BodyCheck::Verified(content.normalized)
            } else {
                BodyCheck::NeedsRecovery(
                    "目标内容与操作记录不一致（可能被其它程序改动），已停止恢复".to_string(),
                )
            }
        }
        Some(ContentState::Unavailable(reason)) => {
            BodyCheck::NeedsRecovery(format!("目标内容不可验证（{reason}），已停止恢复"))
        }
        Some(ContentState::Missing) | None => {
            if operation.phase >= OpPhase::BodyWritten {
                BodyCheck::NeedsRecovery("目标内容丢失，已停止恢复".to_string())
            } else {
                BodyCheck::Absent
            }
        }
    }
}

/// 恢复单个操作：按「持久化阶段 + 实际状态」逐阶段判断，已经越过的阶段不重放
/// （不重复登记映射、不重写关联存储与基线）；校验不因跳过重放而放松。
fn recover_operation(
    paths: &SessionPaths,
    variant: WbVariant,
    operation: Operation,
    source_paths: Option<&SessionPaths>,
) -> RecoverOutcome {
    if operation.kind == OPERATION_KIND_SYNC {
        return recover_sync_operation(paths, operation);
    }
    recover_copy_operation(paths, variant, operation, source_paths)
}

/// 恢复一次同步（design §5.6）：先校验现场，再按清单补完，绝不覆盖未知内容。
///
/// 顺序：备份必须完好 → 目标正文只能是「本次写入的」或「覆盖前的」→ 目标行归属与
/// 更新时间必须可安全识别 → 复用同一段阶段代码补完。任一项不满足即停止并上报
/// needsRecovery（阻断启动，宿主必须等待恢复一致后才能启动 App）。
fn recover_sync_operation(paths: &SessionPaths, mut operation: Operation) -> RecoverOutcome {
    let operation_id = operation.operation_id.clone();
    let needs = |reason: String, retryable: bool| RecoverOutcome::NeedsRecovery {
        id: operation_id.clone(),
        reason,
        retryable,
    };

    // 1) 备份必须完好：没有可验证的备份就没有恢复依据，也不得盲目重放。
    // 操作日志里记录的是本次的备份清单路径，备份目录是它的父目录。
    let Some(backup_dir) = operation
        .backup
        .clone()
        .map(PathBuf::from)
        .and_then(|manifest_file| manifest_file.parent().map(Path::to_path_buf))
    else {
        return needs(
            "同步备份位置缺失，已停止恢复，请手动处理".to_string(),
            false,
        );
    };
    let Some(manifest) = load_sync_manifest(&backup_dir) else {
        return needs(
            "同步备份清单缺失或损坏，已停止恢复，请手动处理".to_string(),
            false,
        );
    };
    if let Err(reason) = verify_sync_backup(&backup_dir, &manifest) {
        return needs(reason, false);
    }
    let target_body_path = match validated_target_body_path(paths, &manifest) {
        Ok(path) => path,
        Err(reason) => return needs(reason, false),
    };

    // 2) 目标正文：只接受「本次写入的内容」或「覆盖前的内容」。
    let body = classify_sync_body(
        &target_body_path,
        &operation.target.session_id,
        &manifest.target.body_raw_digest,
        &operation.expected_content_digest,
    );
    match &body {
        // 后续无关修改（用户/官方 App 追加过内容）或内容不可验证：停止恢复，不覆盖。
        SyncBodyState::Unknown(reason) => return needs(reason.clone(), false),
        // 状态同步不写正文，也永远不会推进到 BodyWritten：正文丢失后若按正文口径
        // 拦截，每次启动都会重跑并失败，等于永久阻断启动。已提交的状态予以保留，
        // 只放弃本次归档意图（不写库、不补正文、不重建文件）。
        // 必须排在「已写正文却丢失」的通用拦截之前——DbWritten 晚于 BodyWritten，
        // 否则归档已提交的现场会先被通用分支截走，永远走不到这里。
        SyncBodyState::Gone if manifest.mode == SyncMode::StatusOnly => {
            abandon_operation_with(
                paths,
                &mut operation,
                "目标内容已不存在，本次仅同步归档已放弃，已提交的状态保持不变",
            );
            return RecoverOutcome::Abandoned(operation_id);
        }
        SyncBodyState::Gone if operation.phase >= OpPhase::BodyWritten => {
            return needs("目标内容丢失，已停止恢复".to_string(), false);
        }
        _ => {}
    }

    // 3) 目标行：归属与更新时间必须可安全识别。
    let row = match session_row_snapshot(paths, &operation.target.session_id) {
        Ok(row) => row,
        Err(reason) => return needs(reason, false),
    };
    let row_absent = match &row {
        None => true,
        // 已删除的会话（软删除）同样没有可更新的行：会话在 App 里已经不存在。
        Some(row) => row.deleted_at.is_some(),
    };
    if row_absent {
        // 没有可更新的行就无法补完。若残留的是本次写入的正文，按清单回滚成覆盖前
        // 内容，不留无行的半成品；这不算成功，记为放弃（无需人工处理，不阻断启动）。
        // 仅同步归档不写正文，也就没有可回滚的正文：写回即等于补写业务文件。
        if matches!(body, SyncBodyState::Ours) && manifest.mode != SyncMode::StatusOnly {
            if let Err(error) = restore_sync_backup_body(paths, &backup_dir, &manifest) {
                return needs(
                    format!("目标会话记录已不存在且内容回滚失败（{error}），请手动处理"),
                    false,
                );
            }
        }
        abandon_operation_with(
            paths,
            &mut operation,
            "目标会话记录已不存在（或已删除），本次同步已从备份回滚，未保存会话内容",
        );
        return RecoverOutcome::Abandoned(operation_id);
    }
    let Some(row) = row else {
        // row_absent 已经把 None 分支处理掉，这里只是形式上的兜底。
        return needs("目标会话记录无法读取，已停止恢复".to_string(), false);
    };
    if row.user_id != operation.target.uid {
        return needs("目标会话记录归属异常，已停止恢复".to_string(), false);
    }
    let before_row = manifest.db.target_row.as_ref();
    let before_updated_at = before_row.and_then(|row| row.updated_at);
    let applied = row.updated_at == Some(manifest.db.new_updated_at);
    // 状态相等只在「状态就是全部动作」时才要求（仅同步归档）。正文模式的归档是
    // 副作用：目标被打开变成 active 属于与本次正文同步无关的変化，不能因此把一次
    // 正常的正文恢复升级成人工处理——由下面的状态写入决策决定是否跳过归档。
    let status_matters = manifest.mode == SyncMode::StatusOnly;
    let untouched = before_row.is_some_and(|before| {
        before.updated_at == row.updated_at && (!status_matters || before.status == row.status)
    });
    // 状态到底有没有写上去，不能只看时间戳：用户取消归档或改为活跃态时时间戳可能
    // 仍是本次值。但正文模式里归档只是副作用，允许被合法跳过（见
    // planned_status_change：目标变为活跃态时跳过），跳过与否每次都由现场状态重新
    // 推导、结果一致，因此只要本次时间戳已落库，这一步就算完成。
    // 仅同步归档里状态是全部动作，必须额外确认状态也等于本次新值。
    let db_applied = match (
        manifest.mode == SyncMode::StatusOnly,
        manifest.db.new_status.as_deref(),
    ) {
        (true, Some(expected)) => applied && row.status.as_deref() == Some(expected),
        _ => applied,
    };
    if !(db_applied || untouched) && before_updated_at.is_some() {
        // 既不是本次写入的值、也不是覆盖前的值：被其它程序改动过，不覆盖。
        return needs(
            "目标会话记录的状态或更新时间与本次保存及覆盖前值都不一致（可能被其它程序改动），已停止恢复"
                .to_string(),
            false,
        );
    }

    // 4) 复用同一段阶段代码补完（阶段只前进，已越过的阶段只核验不重放）。
    match run_sync_phases(paths, &mut operation, &backup_dir, &manifest, body) {
        Ok(()) => RecoverOutcome::Recovered(operation_id),
        Err(error) => {
            fail_operation(paths, &mut operation, &error);
            // Retry may succeed later, but the unfinished sync must block startup now.
            needs(error, false)
        }
    }
}

/// 恢复一次复制（第一步的原有逻辑，保持不变）。
fn recover_copy_operation(
    paths: &SessionPaths,
    variant: WbVariant,
    mut operation: Operation,
    source_paths: Option<&SessionPaths>,
) -> RecoverOutcome {
    let operation_id = operation.operation_id.clone();
    let needs = |reason: String, retryable: bool| RecoverOutcome::NeedsRecovery {
        id: operation_id.clone(),
        reason,
        retryable,
    };
    // 读侧数据根：跨档操作由调用方解析出的源档；同档与旧记录用目标档。
    let read_paths = source_paths.unwrap_or(paths);

    // 1) 目标正文：已写成则直接复用；未写成则用当前源内容补写；被改动则停止。
    let normalized = match check_target_body(paths, &operation) {
        BodyCheck::Verified(normalized) => normalized,
        BodyCheck::NeedsRecovery(reason) => return needs(reason, false),
        BodyCheck::Absent => {
            let Some(source_path) = find_project_jsonl(read_paths, &operation.source.session_id)
            else {
                abandon_operation(paths, &mut operation);
                return RecoverOutcome::Abandoned(operation_id);
            };
            let source = match session_link::read_content_snapshot(
                &source_path,
                &operation.source.session_id,
            ) {
                ContentState::Ready(snapshot) => snapshot,
                _ => {
                    abandon_operation(paths, &mut operation);
                    return RecoverOutcome::Abandoned(operation_id);
                }
            };
            // 源内容在本机发生了变化：按当前内容继续（副本是快照复制，不是同步）。
            if source.normalized.total_digest != operation.expected_content_digest {
                operation.expected_content_digest = source.normalized.total_digest.clone();
                operation.expected_record_count = source.normalized.record_count;
            }
            if let Err(error) = write_copy_body(
                &source,
                &source_path,
                read_paths,
                paths,
                &operation.source.session_id,
                &operation.target.session_id,
            ) {
                fail_operation(paths, &mut operation, &error);
                return needs(error, true);
            }
            if let Err(error) = advance_operation(paths, &mut operation, OpPhase::BodyWritten) {
                return needs(error, true);
            }
            source.normalized
        }
    };

    // 2) 数据库行：缺失则补写，归属异常则停止。
    match session_row_owner(paths, &operation.target.session_id) {
        Some(owner) if owner == operation.target.uid => {}
        Some(_) => {
            return needs("目标会话记录归属异常，已停止恢复".to_string(), false);
        }
        None => {
            if operation.phase >= OpPhase::DbWritten {
                return needs("目标会话记录丢失，已停止恢复".to_string(), false);
            }
            match insert_session_copy(
                read_paths,
                paths,
                &operation.target.session_id,
                &operation.source.session_id,
                &operation.source.uid,
                &operation.target.uid,
            ) {
                Ok(DbCopyOutcome::Inserted) => {}
                Ok(outcome) => {
                    let error = format!("会话记录保存失败（{outcome:?}），保留操作待重试");
                    fail_operation(paths, &mut operation, &error);
                    return needs(error, false);
                }
                Err(error) => {
                    fail_operation(paths, &mut operation, &error);
                    return needs(error, true);
                }
            }
            if let Err(error) =
                verify_session_row(paths, &operation.target.session_id, &operation.target.uid)
            {
                return needs(error, false);
            }
        }
    }
    if let Err(error) = advance_operation(paths, &mut operation, OpPhase::DbWritten) {
        return needs(error, true);
    }

    // 3) 云端登记已交接给客户端（不预写 edge_sync_mapping）：这里只推进阶段，
    // 不写入、不核验映射行——该行由 edge-sync 扩展在客户端下次启动时写入，
    // 工具恢复时不能要求它已存在，否则会误报 needsRecovery。
    if operation.phase < OpPhase::MappingWritten {
        if let Err(error) = advance_operation(paths, &mut operation, OpPhase::MappingWritten) {
            return needs(error, true);
        }
    }

    // 4) 关联与基线：已提交过就不再 commit_links，避免重复写关联存储与基线；
    // 同样先核验组与成员仍在，不能把「跳过重放」当成「产物一定还在」。
    if operation.phase < OpPhase::LinksCommitted {
        match commit_links(paths, variant, &operation, &normalized) {
            Ok(group_id) => {
                operation.group_id = group_id;
            }
            Err(error) => {
                fail_operation(paths, &mut operation, &error);
                return needs(error, true);
            }
        }
        if let Err(error) = advance_operation(paths, &mut operation, OpPhase::LinksCommitted) {
            return needs(error, true);
        }
    } else if let Err(error) = committed_links_present(paths, &operation) {
        return needs(error, false);
    }
    if let Err(error) = advance_operation(paths, &mut operation, OpPhase::Completed) {
        return needs(error, true);
    }
    cleanup_finished_operation(paths, variant, &operation);
    RecoverOutcome::Recovered(operation_id)
}

fn abandon_operation(paths: &SessionPaths, operation: &mut Operation) {
    abandon_operation_with(
        paths,
        operation,
        "源会话已不可用，且没有复制出任何会话，已放弃该操作",
    );
}

/// 放弃一个未写入任何会话内容的操作（阶段置 Abandoned，不算成功）。
///
/// `cleanupState = safeTerminated` 是「已验证安全终止」的持久标记：只有带标记的
/// Abandoned 才允许维护入口回收对应临时备份（design §3）。
fn abandon_operation_with(paths: &SessionPaths, operation: &mut Operation, reason: &str) {
    operation.phase = OpPhase::Abandoned;
    operation.cleanup_state = Some(CLEANUP_STATE_SAFE_TERMINATED.to_string());
    operation.last_error = Some(reason.to_string());
    operation.updated_at = now_ms();
    if let Err(save_error) = session_link::save_operation(paths, operation) {
        error_log::record(
            "session-operation",
            "保存放弃状态失败",
            &format!(
                "operation={} reason={reason}: {save_error}",
                operation.operation_id
            ),
        );
    }
}

/// 恢复/放弃完成后立即回收该操作的临时备份（待清理推进失败时留给下次维护补转）。
fn cleanup_finished_operation(paths: &SessionPaths, variant: WbVariant, operation: &Operation) {
    let Ok(Some(mut record)) =
        session_backup::load_lifecycle(paths, variant, &operation.operation_id)
    else {
        // 没有维护记录（旧操作/记录损坏）：维护扫描会按自身口径上报，这里不猜测。
        return;
    };
    if session_backup::mark_cleanup_pending(paths, &mut record, None).is_ok() {
        let _ = session_backup::cleanup_after_success(paths, variant, &record);
    }
}

#[cfg(test)]
mod tests;

// 本 fork 的 E2E 会话生命周期用例基于旧的 copy_session_to_user API；上游 0.1.41 起改为
// 关联组 + SessionPaths 幂等复制架构，该 API 已移除，故此处用例不再适用（改由上游测试套件覆盖）。
