//! 本地工作记忆复制（#209）。
//!
//! WorkBuddy 把每个账号的本地记忆放在
//! `<data_root>/user-<uid>-personal/MEMORY.md`。切号只替换登录态，不会复制这份
//! 文件；本模块提供一次显式、可回滚的复制，不做云端同步（云端记忆仍由官方客户端
//! 自己维护）。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::modules::account::{find_account, get_str, variant_of};
use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::process;
use crate::modules::variant::WbVariant;

const MEMORY_FILE: &str = "MEMORY.md";

/// 指定档位/uid 的本地工作记忆文件。
pub fn memory_file(variant: WbVariant, uid: &str) -> PathBuf {
    variant
        .data_root()
        .join(format!("user-{uid}-personal"))
        .join(MEMORY_FILE)
}

fn copy_memory_at(
    source: &Path,
    target: &Path,
    backup_root: &Path,
    target_uid: &str,
) -> Result<(usize, Option<String>, bool), String> {
    if source == target {
        return Err("源账号和目标账号相同，无需复制".to_string());
    }
    if !source.is_file() {
        return Err("源账号没有本地工作记忆文件（MEMORY.md）".to_string());
    }
    let content =
        std::fs::read_to_string(source).map_err(|error| format!("读取源工作记忆失败：{error}"))?;

    if target.is_file() {
        if let Ok(existing) = std::fs::read_to_string(target) {
            if existing == content {
                return Ok((content.len(), None, false));
            }
        }
    }

    let mut backup_path = None;
    if target.is_file() {
        let dir = backup_root.join("memory");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("创建工作记忆备份目录失败：{error}"))?;
        let backup = dir.join(format!(
            "{}-{}-{}-{}",
            now_ms(),
            target_uid,
            MEMORY_FILE,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::copy(target, &backup).map_err(|error| format!("备份目标工作记忆失败：{error}"))?;
        backup_path = Some(backup.to_string_lossy().to_string());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建工作记忆目录失败：{error}"))?;
    }
    atomic_write(target, &content).map_err(|error| format!("写入目标工作记忆失败：{error}"))?;
    Ok((content.len(), backup_path, true))
}

/// 把一个账号的本地工作记忆复制到另一个账号；目标已有内容时先备份。
///
/// 只复制 `MEMORY.md`，不复制 IDENTITY/SOUL/USER 等身份文件，也不参与云端同步。
pub fn copy_memory(source_account_id: &str, target_account_id: &str) -> Result<Value, String> {
    if source_account_id == target_account_id {
        return Err("源账号和目标账号相同，无需复制".to_string());
    }
    let source = find_account(source_account_id).ok_or_else(|| "源账号不存在".to_string())?;
    let target = find_account(target_account_id).ok_or_else(|| "目标账号不存在".to_string())?;
    let source_uid = get_str(&source, "uid").ok_or_else(|| "源账号缺少 uid".to_string())?;
    let target_uid = get_str(&target, "uid").ok_or_else(|| "目标账号缺少 uid".to_string())?;
    if source_uid == target_uid {
        return Err("源账号和目标账号是同一个 UID".to_string());
    }

    let source_variant = variant_of(&source);
    let target_variant = variant_of(&target);
    if [source_variant, target_variant]
        .into_iter()
        .any(process::is_workbuddy_running)
    {
        return Err("请先退出 WorkBuddy 再复制工作记忆，避免运行时缓存覆盖文件".to_string());
    }
    let source_path = memory_file(source_variant, &source_uid);
    let target_path = memory_file(target_variant, &target_uid);
    let (bytes, backup, changed) = copy_memory_at(
        &source_path,
        &target_path,
        &store_dir().join("backups"),
        &target_uid,
    )?;

    Ok(json!({
        "ok": true,
        "changed": changed,
        "bytes": bytes,
        "source": {
            "id": get_str(&source, "id"),
            "uid": source_uid,
            "name": crate::modules::account::account_display_name(&source),
        },
        "target": {
            "id": get_str(&target, "id"),
            "uid": target_uid,
            "name": crate::modules::account::account_display_name(&target),
        },
        "backup": backup,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-switch-memory-{name}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_memory_and_creates_backup() {
        let root = temp_dir("copy");
        let source = root.join("source.md");
        let target = root.join("target.md");
        std::fs::write(&source, "新记忆").unwrap();
        std::fs::write(&target, "旧记忆").unwrap();

        let (bytes, backup, changed) =
            copy_memory_at(&source, &target, &root.join("backups"), "uid-1").unwrap();
        assert!(changed);
        assert_eq!(bytes, "新记忆".len());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "新记忆");
        let backup = backup.expect("应生成备份");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "旧记忆");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn second_copy_is_noop() {
        let root = temp_dir("same");
        let source = root.join("source.md");
        let target = root.join("target.md");
        std::fs::write(&source, "相同").unwrap();
        std::fs::write(&target, "相同").unwrap();

        let (_, backup, changed) =
            copy_memory_at(&source, &target, &root.join("backups"), "uid-1").unwrap();
        assert!(!changed);
        assert!(backup.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_source_is_reported() {
        let root = temp_dir("missing");
        let error = copy_memory_at(
            &root.join("missing.md"),
            &root.join("target.md"),
            &root.join("backups"),
            "uid-1",
        )
        .unwrap_err();
        assert!(error.contains("没有本地工作记忆"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
