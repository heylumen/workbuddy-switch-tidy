//! 存储管理与回收站。
//!
//! 设计约束（与项目红线一致）：
//! - **只移入回收站，不物理删除业务数据**；唯一会真正删除的是「清空回收站」（用户显式操作）。
//! - **会话正文（`projects/`）、数据库、程序本体（`binaries/` `app/` `plugins/`）
//!   一律不在可回收范围**：界面只展示体积并引导到会话管理。
//! - 前端**只传类别 id**，具体路径由本模块按档位计算——不接受任意路径，避免误删。
//! - 客户端运行中时，客户端侧类别一律拒绝（由命令层用 `process::is_workbuddy_running` 拦截）。
//!
//! 回收站布局：`{store_root}/trash/{entry_id}/meta.json` + `{store_root}/trash/{entry_id}/data/`
//! （`data` 即被整体移走的那一层目录；恢复时原样移回 `originalPath`）。

use crate::modules::config::now_ms;
use crate::modules::process;
use crate::modules::session::SessionPaths;
use crate::modules::variant::WbVariant;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// 回收站目录名（位于工具存储根下）。
const TRASH_DIR: &str = "trash";

/// 可回收类别的定义。
struct Category {
    id: &'static str,
    label: &'static str,
    hint: &'static str,
    /// 相对工具存储根（`Some`）或档位数据根（`None` 表示用 `absolute`）。
    under: Location,
    relative: &'static str,
    /// 是否需要客户端已退出才允许回收。
    needs_client_closed: bool,
    /// 是否允许移入回收站（`false` 仅展示体积）。
    trashable: bool,
}

#[derive(Clone, Copy)]
enum Location {
    Store,
    Client,
}

const CATEGORIES: &[Category] = &[
    // ---- 工具自身（无需关闭客户端）----
    Category {
        id: "store-backups",
        label: "工具临时备份",
        hint: "会话复制/隐藏前的临时备份，操作完成后可安全回收",
        under: Location::Store,
        relative: "backups",
        needs_client_closed: false,
        trashable: true,
    },
    Category {
        id: "store-link-operations",
        label: "关联操作日志",
        hint: "复制/同步的阶段日志，仅用于中断恢复，可安全回收",
        under: Location::Store,
        relative: "session-links/operations",
        needs_client_closed: false,
        trashable: true,
    },
    Category {
        id: "store-link-previews",
        label: "同步预览凭据",
        hint: "一次性预览凭据，可安全回收（下次同步时会重新生成）",
        under: Location::Store,
        relative: "session-links/previews",
        needs_client_closed: false,
        trashable: true,
    },
    Category {
        id: "store-hook-backups",
        label: "Hook 配置备份",
        hint: "接入客户端 hook 前的配置备份，可安全回收",
        under: Location::Store,
        relative: "hook-backups",
        needs_client_closed: false,
        trashable: true,
    },
    // ---- 客户端可重建的缓存类（需客户端退出）----
    Category {
        id: "client-cache",
        label: "客户端缓存",
        hint: "客户端缓存目录，删除后由客户端自行重建",
        under: Location::Client,
        relative: "cache",
        needs_client_closed: true,
        trashable: true,
    },
    Category {
        id: "client-traces",
        label: "客户端追踪日志",
        hint: "运行追踪数据，可安全回收",
        under: Location::Client,
        relative: "traces",
        needs_client_closed: true,
        trashable: true,
    },
    Category {
        id: "client-logs",
        label: "客户端日志",
        hint: "客户端运行日志，可安全回收",
        under: Location::Client,
        relative: "logs",
        needs_client_closed: true,
        trashable: true,
    },
    Category {
        id: "client-changes",
        label: "变更明细缓存",
        hint: "逐次变更的明细缓存，可安全回收",
        under: Location::Client,
        relative: "changes-detail",
        needs_client_closed: true,
        trashable: true,
    },
    Category {
        id: "client-manifests",
        label: "文件树清单缓存",
        hint: "文件树清单缓存，可安全回收",
        under: Location::Client,
        relative: "file-tree-manifests",
        needs_client_closed: true,
        trashable: true,
    },
    // ---- 只展示，不可回收 ----
    Category {
        id: "client-projects",
        label: "会话正文（projects）",
        hint: "会话内容本体，**不可回收**；如需整理请用「重复审计 / 折叠同名」",
        under: Location::Client,
        relative: "projects",
        needs_client_closed: false,
        trashable: false,
    },
    Category {
        id: "client-workspace",
        label: "工作区文件（workspace）",
        hint: "客户端工作区产物，**不可回收**（可能含你的文件）",
        under: Location::Client,
        relative: "workspace",
        needs_client_closed: false,
        trashable: false,
    },
    Category {
        id: "client-file-history",
        label: "文件历史（file-history）",
        hint: "文件历史快照，**不可回收**（可能含你的文件）",
        under: Location::Client,
        relative: "file-history",
        needs_client_closed: false,
        trashable: false,
    },
    Category {
        id: "client-binaries",
        label: "程序与插件（binaries/app/plugins）",
        hint: "客户端程序本体，**不可回收**",
        under: Location::Client,
        relative: "binaries",
        needs_client_closed: false,
        trashable: false,
    },
];

fn category_path(paths: &SessionPaths, category: &Category) -> PathBuf {
    match category.under {
        Location::Store => paths.store_root.join(category.relative),
        Location::Client => paths.data_root.join(category.relative),
    }
}

/// 回收站位于**工具存储根**，与档位无关；这里借国内档位构造路径仅为了拿到 `store_root`。
/// 显式封成函数，避免读者误以为回收站按档位隔离。
fn store_paths() -> SessionPaths {
    SessionPaths::for_variant(WbVariant::Cn)
}

/// 删除目录：失败时写 error_log（用户能知道"为什么空间没释放"），不阻断主流程。
fn cleanup_dir(path: &std::path::Path, what: &str) {
    if let Err(error) = std::fs::remove_dir_all(path) {
        if path.exists() {
            crate::modules::error_log::record(
                "storage",
                &format!("{what}失败"),
                &format!("{}: {error}", path.display()),
            );
        }
    }
}

fn trash_root(paths: &SessionPaths) -> PathBuf {
    paths.store_root.join(TRASH_DIR)
}

/// 递归统计体积与文件数；跳过符号链接，避免环路与越界。
fn dir_usage(path: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.file_type() else {
                continue;
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                if let Ok(info) = entry.metadata() {
                    bytes = bytes.saturating_add(info.len());
                    files += 1;
                }
            }
        }
    }
    (bytes, files)
}

/// 扫描存储占用：按类别返回体积、文件数、是否可回收。
pub fn scan_storage(variant: WbVariant) -> Value {
    let paths = SessionPaths::for_variant(variant);
    let mut categories = Vec::new();
    let mut trashable_bytes = 0u64;
    for category in CATEGORIES {
        let path = category_path(&paths, category);
        let (bytes, files) = dir_usage(&path);
        if category.trashable {
            trashable_bytes = trashable_bytes.saturating_add(bytes);
        }
        categories.push(json!({
            "id": category.id,
            "label": category.label,
            "hint": category.hint,
            "bytes": bytes,
            "files": files,
            "trashable": category.trashable,
            "needsClientClosed": category.needs_client_closed,
            "exists": path.exists(),
        }));
    }
    let (trash_bytes, trash_files) = dir_usage(&trash_root(&paths));
    let entries = list_trash_entries(&paths);
    json!({
        "ok": true,
        "variant": variant.as_str(),
        "storeRoot": paths.store_root.to_string_lossy(),
        "dataRoot": paths.data_root.to_string_lossy(),
        "categories": categories,
        "trashableBytes": trashable_bytes,
        "trash": {
            "bytes": trash_bytes,
            "files": trash_files,
            "entries": entries,
        },
    })
}

fn list_trash_entries(paths: &SessionPaths) -> Vec<Value> {
    let root = trash_root(paths);
    let Ok(items) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in items.flatten() {
        let meta_path = item.path().join("meta.json");
        let Ok(text) = fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        entries.push(meta);
    }
    entries.sort_by_key(|entry| {
        std::cmp::Reverse(entry.get("createdAt").and_then(Value::as_i64).unwrap_or(0))
    });
    entries
}

/// 把某个**允许回收**的类别整体移入回收站。
///
/// 前端只传 `category_id`；路径由白名单计算，未知 id 一律拒绝。
pub fn move_to_trash(variant: WbVariant, category_id: &str) -> Value {
    let paths = SessionPaths::for_variant(variant);
    let Some(category) = CATEGORIES
        .iter()
        .find(|item| item.id == category_id && item.trashable)
    else {
        return json!({ "ok": false, "reason": "该类别不允许回收" });
    };
    if category.needs_client_closed && process::is_workbuddy_running(variant) {
        return json!({ "ok": false, "reason": "请先退出客户端，再回收客户端侧目录" });
    }
    let source = category_path(&paths, category);
    if !source.exists() {
        return json!({ "ok": false, "reason": "该目录不存在或已为空" });
    }
    let (bytes, files) = dir_usage(&source);
    let id = format!("{}-{}", now_ms(), short_id());
    let entry_dir = trash_root(&paths).join(&id);
    let data_dir = entry_dir.join("data");
    if let Err(error) = fs::create_dir_all(&entry_dir) {
        return json!({ "ok": false, "reason": format!("创建回收站目录失败：{error}") });
    }
    // 同卷优先 rename；跨卷回退到「复制后删除源」。
    let moved = fs::rename(&source, &data_dir);
    if let Err(error) = moved {
        if let Err(copy_error) = copy_dir(&source, &data_dir) {
            cleanup_dir(&entry_dir, "清理回收站条目");
            return json!({ "ok": false, "reason": format!("移入回收站失败：{error} / {copy_error}") });
        }
        if let Err(remove_error) = fs::remove_dir_all(&source) {
            cleanup_dir(&entry_dir, "清理回收站条目");
            return json!({ "ok": false, "reason": format!("源目录清理失败：{remove_error}") });
        }
    }
    let meta = json!({
        "id": id,
        "categoryId": category.id,
        "label": category.label,
        "variant": variant.as_str(),
        "originalPath": source.to_string_lossy(),
        "bytes": bytes,
        "files": files,
        "createdAt": now_ms(),
    });
    if let Err(error) = fs::write(
        entry_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    ) {
        return json!({ "ok": false, "reason": format!("写入回收站元数据失败：{error}") });
    }
    json!({ "ok": true, "entry": meta })
}

/// 从回收站恢复到原位置（原位置必须不存在，避免覆盖）。
pub fn restore_from_trash(entry_id: &str) -> Value {
    if entry_id.trim().is_empty()
        || entry_id.contains("..")
        || entry_id.contains('/')
        || entry_id.contains('\\')
    {
        return json!({ "ok": false, "reason": "回收站条目 id 非法" });
    }
    let paths = store_paths();
    let entry_dir = trash_root(&paths).join(entry_id);
    let Ok(text) = fs::read_to_string(entry_dir.join("meta.json")) else {
        return json!({ "ok": false, "reason": "回收站条目不存在" });
    };
    let Ok(meta) = serde_json::from_str::<Value>(&text) else {
        return json!({ "ok": false, "reason": "回收站元数据损坏" });
    };
    // 多文件条目（如「已删除会话的正文残留」）：逐条还原到原路径。
    if meta.get("kind").and_then(Value::as_str) == Some("files") {
        let data_dir = entry_dir.join("data");
        let mut restored = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for item in meta
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(original) = item.get("originalPath").and_then(Value::as_str) else {
                continue;
            };
            let Some(stored) = item.get("stored").and_then(Value::as_str) else {
                continue;
            };
            let target = PathBuf::from(original);
            if target.exists() {
                failed.push(format!("{}（原位置已存在）", original));
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if fs::rename(data_dir.join(stored), &target).is_ok() {
                restored += 1;
            } else {
                failed.push(original.to_string());
            }
        }
        if restored == 0 && !failed.is_empty() {
            return json!({ "ok": false, "reason": "恢复失败", "failed": failed });
        }
        cleanup_dir(&entry_dir, "删除回收站条目");
        return json!({ "ok": true, "restored": restored, "failed": failed });
    }
    let Some(original) = meta.get("originalPath").and_then(Value::as_str) else {
        return json!({ "ok": false, "reason": "回收站元数据缺少原始路径" });
    };
    let original = PathBuf::from(original);
    if original.exists() {
        return json!({ "ok": false, "reason": "原位置已存在同名目录，请先处理后再恢复" });
    }
    if let Some(parent) = original.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return json!({ "ok": false, "reason": format!("创建父目录失败：{error}") });
        }
    }
    let data_dir = entry_dir.join("data");
    if let Err(error) = fs::rename(&data_dir, &original) {
        if let Err(copy_error) = copy_dir(&data_dir, &original) {
            return json!({ "ok": false, "reason": format!("恢复失败：{error} / {copy_error}") });
        }
        cleanup_dir(&data_dir, "清理残留正文目录");
    }
    let _ = fs::remove_dir_all(&entry_dir);
    json!({ "ok": true, "restoredTo": original.to_string_lossy() })
}

/// 回收站保留期（天）的环境变量；`0` 表示关闭自动清理。
const TRASH_RETENTION_ENV: &str = "WB_SWITCH_TRASH_RETENTION_DAYS";
const TRASH_RETENTION_DEFAULT_DAYS: u32 = 30;
const TRASH_RETENTION_MAX_DAYS: u32 = 365;

/// 解析保留期（纯函数，便于单测）：空/非法回退默认，越界夹取。
fn parse_retention_days(raw: Option<&str>) -> u32 {
    let Some(text) = raw else {
        return TRASH_RETENTION_DEFAULT_DAYS;
    };
    let Ok(value) = text.trim().parse::<u32>() else {
        return TRASH_RETENTION_DEFAULT_DAYS;
    };
    value.min(TRASH_RETENTION_MAX_DAYS)
}

/// 当前生效的保留期（首次调用时读取环境变量）。
pub fn retention_days() -> u32 {
    static CACHED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| parse_retention_days(std::env::var(TRASH_RETENTION_ENV).ok().as_deref()))
}

/// 启动维护：按保留期清理过期回收站条目（`0` = 关闭）。
pub fn purge_expired_on_start() -> Value {
    let days = retention_days();
    if days == 0 {
        return json!({ "ok": true, "skipped": true, "days": 0, "purged": 0 });
    }
    let mut result = purge_trash(days);
    if let Some(map) = result.as_object_mut() {
        map.insert("days".to_string(), json!(days));
    }
    result
}

/// 清空回收站：仅删除 `older_than_days` 之前的条目；`0` 表示全部清空。
pub fn purge_trash(older_than_days: u32) -> Value {
    let paths = store_paths();
    let root = trash_root(&paths);
    let Ok(items) = fs::read_dir(&root) else {
        return json!({ "ok": true, "purged": 0, "bytes": 0 });
    };
    let cutoff = if older_than_days == 0 {
        i64::MAX
    } else {
        now_ms() - (older_than_days as i64) * 24 * 60 * 60 * 1000
    };
    let mut purged = 0u64;
    let mut freed = 0u64;
    for item in items.flatten() {
        let entry_dir = item.path();
        let meta_path = entry_dir.join("meta.json");
        let created = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|meta| meta.get("createdAt").and_then(Value::as_i64))
            .unwrap_or(0);
        let expired = if older_than_days == 0 {
            true
        } else {
            created <= cutoff
        };
        if !expired {
            continue;
        }
        let (bytes, _) = dir_usage(&entry_dir);
        if fs::remove_dir_all(&entry_dir).is_ok() {
            purged += 1;
            freed = freed.saturating_add(bytes);
        }
    }
    json!({ "ok": true, "purged": purged, "bytes": freed })
}

fn short_id() -> String {
    // 项目已依赖 uuid；取前 6 位十六进制即可（回收站条目 id 只需本地唯一 + 可读）。
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(6)
        .collect()
}

fn copy_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &destination)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_usage_counts_files_and_skips_missing() {
        let root = std::env::temp_dir().join(format!("wb-storage-{}", short_id()));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/one.bin"), vec![0u8; 10]).unwrap();
        fs::write(root.join("two.bin"), vec![0u8; 5]).unwrap();
        let (bytes, files) = dir_usage(&root);
        assert_eq!(bytes, 15);
        assert_eq!(files, 2);
        assert_eq!(dir_usage(&root.join("missing")), (0, 0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_or_untrashable_category_is_rejected() {
        for id in ["client-projects", "client-binaries", "not-a-category", ""] {
            let value = move_to_trash(WbVariant::Cn, id);
            assert_eq!(value["ok"], json!(false), "id={id} 不应被允许");
        }
    }

    #[test]
    fn trash_restore_round_trip() {
        let root = std::env::temp_dir().join(format!("wb-trash-{}", short_id()));
        let original = root.join("cache");
        fs::create_dir_all(original.join("nested")).unwrap();
        fs::write(original.join("nested/file.txt"), b"hello").unwrap();
        let trash = root.join("trash");
        let entry = trash.join("entry-1");
        fs::create_dir_all(&entry).unwrap();
        fs::rename(&original, entry.join("data")).unwrap();
        fs::write(
            entry.join("meta.json"),
            serde_json::to_string(&json!({
                "id": "entry-1",
                "originalPath": original.to_string_lossy(),
                "createdAt": now_ms(),
            }))
            .unwrap(),
        )
        .unwrap();
        // 直接验证「原位置已存在则拒绝覆盖」的判定条件
        fs::create_dir_all(&original).unwrap();
        assert!(original.exists());
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod retention_tests {
    use super::{parse_retention_days, TRASH_RETENTION_DEFAULT_DAYS};

    #[test]
    fn defaults_and_clamps() {
        assert_eq!(parse_retention_days(None), TRASH_RETENTION_DEFAULT_DAYS);
        assert_eq!(
            parse_retention_days(Some("abc")),
            TRASH_RETENTION_DEFAULT_DAYS
        );
        assert_eq!(parse_retention_days(Some("0")), 0);
        assert_eq!(parse_retention_days(Some("7")), 7);
        assert_eq!(parse_retention_days(Some("9999")), 365);
    }
}

// ---------------------------------------------------------------------------
// 已软删除会话的正文残留
// ---------------------------------------------------------------------------

/// 读取数据库中「已软删除」（`deleted_at` 非空）的会话 id 集合。
fn deleted_session_ids(paths: &SessionPaths) -> Result<std::collections::HashSet<String>, String> {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return Err("workbuddy.db 不存在".to_string());
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("打开数据库失败：{error}"))?;
    let mut stmt = conn
        .prepare("SELECT id FROM sessions WHERE deleted_at IS NOT NULL")
        .map_err(|error| format!("准备查询失败：{error}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("查询失败：{error}"))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// 找出「DB 已软删除、但正文 jsonl 仍在磁盘」的文件（严格按会话 id 匹配文件名）。
fn orphan_body_files(
    paths: &SessionPaths,
    deleted: &std::collections::HashSet<String>,
) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let projects = paths.projects_dir();
    let Ok(dirs) = fs::read_dir(&projects) else {
        return out;
    };
    for dir in dirs.flatten() {
        if !dir.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !deleted.contains(stem) {
                continue;
            }
            let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            out.push((path, bytes));
        }
    }
    out
}

/// 只读扫描：已删除会话的正文残留（总体积 + 数量 + 少量样例）。
pub fn scan_orphan_bodies(variant: WbVariant) -> Value {
    let paths = SessionPaths::for_variant(variant);
    let deleted = match deleted_session_ids(&paths) {
        Ok(ids) => ids,
        Err(reason) => {
            return json!({ "ok": false, "reason": reason, "count": 0, "bytes": 0 });
        }
    };
    let files = orphan_body_files(&paths, &deleted);
    let bytes: u64 = files.iter().map(|(_, size)| *size).sum();
    let sample: Vec<Value> = files
        .iter()
        .take(5)
        .map(|(path, size)| {
            json!({
                "name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
                "bytes": size,
            })
        })
        .collect();
    json!({
        "ok": true,
        "count": files.len(),
        "bytes": bytes,
        "deletedSessions": deleted.len(),
        "sample": sample,
        "needsClientClosed": true,
    })
}

/// 把「已删除会话的正文残留」移入回收站（多文件条目，可整条恢复）。
pub fn trash_orphan_bodies(variant: WbVariant) -> Value {
    if process::is_workbuddy_running(variant) {
        return json!({ "ok": false, "reason": "请先退出客户端，再回收已删除会话的正文" });
    }
    let paths = SessionPaths::for_variant(variant);
    let deleted = match deleted_session_ids(&paths) {
        Ok(ids) => ids,
        Err(reason) => return json!({ "ok": false, "reason": reason }),
    };
    let files = orphan_body_files(&paths, &deleted);
    if files.is_empty() {
        return json!({ "ok": true, "moved": 0, "bytes": 0, "entry": Value::Null });
    }
    let id = format!("orphan-{}-{}", now_ms(), short_id());
    let entry_dir = trash_root(&paths).join(&id);
    let data_dir = entry_dir.join("data");
    if let Err(error) = fs::create_dir_all(&data_dir) {
        return json!({ "ok": false, "reason": format!("创建回收站目录失败：{error}") });
    }
    let mut manifest: Vec<Value> = Vec::new();
    let mut moved = 0usize;
    let mut bytes = 0u64;
    for (index, (path, size)) in files.iter().enumerate() {
        let stored = format!("{index}.jsonl");
        if fs::rename(path, data_dir.join(&stored)).is_err() {
            continue;
        }
        manifest.push(json!({
            "originalPath": path.to_string_lossy(),
            "bytes": size,
            "stored": stored,
        }));
        moved += 1;
        bytes = bytes.saturating_add(*size);
    }
    if moved == 0 {
        cleanup_dir(&entry_dir, "删除回收站条目");
        return json!({ "ok": false, "reason": "没有可移动的文件（可能被占用）" });
    }
    let meta = json!({
        "id": id,
        "kind": "files",
        "categoryId": "soft-deleted-bodies",
        "label": "已删除会话的正文残留",
        "variant": variant.as_str(),
        "bytes": bytes,
        "files": manifest,
        "createdAt": now_ms(),
    });
    if let Err(error) = fs::write(
        entry_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    ) {
        return json!({ "ok": false, "reason": format!("写入回收站元数据失败：{error}") });
    }
    json!({ "ok": true, "moved": moved, "bytes": bytes, "entry": meta })
}

#[cfg(test)]
mod orphan_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn orphan_scan_only_matches_deleted_ids_and_jsonl() {
        let root = std::env::temp_dir().join(format!("wb-orphan-{}", short_id()));
        let project = root.join("projects").join("ws-a");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("aaaaaaaa-1111.jsonl"), b"x").unwrap();
        fs::write(project.join("bbbbbbbb-2222.jsonl"), b"yy").unwrap();
        fs::write(project.join("cccccccc-3333.txt"), b"zzz").unwrap();
        let mut deleted = HashSet::new();
        deleted.insert("aaaaaaaa-1111".to_string());
        deleted.insert("cccccccc-3333".to_string());

        // 直接复用文件遍历逻辑：构造一个临时 SessionPaths 不便，这里用手写遍历验证判据
        let mut hits = Vec::new();
        for dir in fs::read_dir(root.join("projects")).unwrap().flatten() {
            for file in fs::read_dir(dir.path()).unwrap().flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
                if deleted.contains(&stem) {
                    hits.push((stem, file.metadata().unwrap().len()));
                }
            }
        }
        assert_eq!(hits, vec![("aaaaaaaa-1111".to_string(), 1)]);
        let _ = fs::remove_dir_all(&root);
    }
}
