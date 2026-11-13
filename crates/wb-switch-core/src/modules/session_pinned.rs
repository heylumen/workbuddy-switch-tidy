//! 跨账号复制/同步会话时同步置顶状态（#207 / #205）。
//!
//! WorkBuddy 的置顶不在 `sessions` 表，而在
//! `<data_root>/storage/user-<uid>-personal/global/conversations.json` 的 `pinned`
//! 数组。本模块只做**单向追加**：源账号置顶的会话映射到目标账号的新 id 后追加；
//! 目标已有的条目和顺序保持不变，不做镜像删除。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::modules::config::{atomic_write, now_ms};

const CONVERSATIONS_FILE: &str = "conversations.json";

pub fn pinned_file_at(data_root: &Path, uid: &str) -> PathBuf {
    data_root
        .join("storage")
        .join(format!("user-{uid}-personal"))
        .join("global")
        .join(CONVERSATIONS_FILE)
}

fn skipped(reason: &str) -> Value {
    json!({ "added": 0, "skippedReason": reason })
}

/// 同步置顶；`pairs` 为 (源 session_id, 目标 session_id)。
///
/// 任何异常都只返回 `skippedReason`，绝不阻塞复制/同步的关键路径。
pub fn sync_pairs(
    source_root: &Path,
    target_root: &Path,
    source_uid: &str,
    target_uid: &str,
    pairs: &[(String, String)],
    backup_root: &Path,
) -> Value {
    if pairs.is_empty() {
        return skipped("没有可映射的会话");
    }
    let source_path = pinned_file_at(source_root, source_uid);
    let target_path = pinned_file_at(target_root, target_uid);
    let source: Value = match std::fs::read_to_string(&source_path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => return skipped("源置顶文件 JSON 损坏"),
        },
        Err(_) => return skipped("源账号没有置顶文件"),
    };
    let Some(source_pinned) = source.get("pinned").and_then(Value::as_array) else {
        return skipped("源账号没有 pinned 数组");
    };

    let original = std::fs::read_to_string(&target_path).ok();
    let mut target: Value = match original.as_deref() {
        Some(text) => match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return skipped("目标置顶文件 JSON 损坏，未写入"),
        },
        None => json!({ "pinned": [] }),
    };
    let Some(target_obj) = target.as_object_mut() else {
        return skipped("目标置顶文件不是对象");
    };
    if !target_obj.contains_key("pinned") {
        target_obj.insert("pinned".to_string(), json!([]));
    }
    let Some(target_pinned) = target_obj.get_mut("pinned").and_then(Value::as_array_mut) else {
        return skipped("目标 pinned 不是数组");
    };

    let mut added = 0usize;
    for source_id in source_pinned {
        let Some(id) = source_id.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some((_, target_id)) = pairs.iter().find(|(from, _)| from == id) else {
            continue;
        };
        if target_pinned
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(target_id.as_str()))
        {
            continue;
        }
        let mut cloned = source_id.clone();
        cloned["id"] = json!(target_id);
        target_pinned.push(cloned);
        added += 1;
    }

    if added == 0 {
        return json!({ "added": 0 });
    }
    if let Some(parent) = target_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return skipped(&format!("创建目标置顶目录失败：{error}"));
        }
    }
    if let Some(original) = original {
        let dir = backup_root.join("pinned");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return skipped(&format!("创建置顶备份目录失败：{error}"));
        }
        let backup = dir.join(format!(
            "{}-{}-{}-{}",
            now_ms(),
            target_uid,
            CONVERSATIONS_FILE,
            uuid::Uuid::new_v4().simple()
        ));
        if let Err(error) = std::fs::write(&backup, original) {
            return skipped(&format!("备份目标置顶文件失败：{error}"));
        }
    }
    let content = match serde_json::to_string_pretty(&target) {
        Ok(content) => content,
        Err(error) => return skipped(&format!("序列化置顶文件失败：{error}")),
    };
    if let Err(error) = atomic_write(&target_path, &content) {
        return skipped(&format!("写入目标置顶文件失败：{error}"));
    }
    json!({ "added": added })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("wb-switch-pinned-{name}-{}", now_ms()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_pinned(root: &Path, uid: &str, pinned: Value) {
        let path = pinned_file_at(root, uid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::to_string_pretty(&json!({ "pinned": pinned })).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn appends_mapped_pins_and_backs_up() {
        let source = temp_root("source");
        let target = temp_root("target");
        write_pinned(
            &source,
            "uid-a",
            json!([{ "id": "s1", "groupKey": "/work" }, { "id": "s2", "groupKey": "/other" }]),
        );
        write_pinned(&target, "uid-b", json!([{ "id": "keep", "groupKey": "" }]));
        let result = sync_pairs(
            &source,
            &target,
            "uid-a",
            "uid-b",
            &[("s1".to_string(), "t1".to_string())],
            &target.join("store"),
        );
        assert_eq!(result["added"], 1);
        let stored: Value = serde_json::from_str(
            &std::fs::read_to_string(pinned_file_at(&target, "uid-b")).unwrap(),
        )
        .unwrap();
        let pinned = stored["pinned"].as_array().unwrap();
        assert_eq!(pinned.len(), 2);
        assert_eq!(pinned[0]["id"], "keep");
        assert_eq!(pinned[1]["id"], "t1");
        assert_eq!(pinned[1]["groupKey"], "/work");
        let backups = std::fs::read_dir(target.join("store/pinned"))
            .expect("应生成置顶备份")
            .flatten()
            .count();
        assert_eq!(backups, 1);
        std::fs::remove_dir_all(source).unwrap();
        std::fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn idempotent_and_missing_source_are_safe() {
        let source = temp_root("source2");
        let target = temp_root("target2");
        write_pinned(&source, "uid-a", json!([{ "id": "s1" }]));
        write_pinned(&target, "uid-b", json!([]));
        let pairs = vec![("s1".to_string(), "t1".to_string())];
        assert_eq!(
            sync_pairs(&source, &target, "uid-a", "uid-b", &pairs, &target)["added"],
            1
        );
        assert_eq!(
            sync_pairs(&source, &target, "uid-a", "uid-b", &pairs, &target)["added"],
            0
        );
        std::fs::remove_file(pinned_file_at(&source, "uid-a")).unwrap();
        let missing = sync_pairs(&source, &target, "uid-a", "uid-b", &pairs, &target);
        assert!(missing.get("skippedReason").is_some());
        std::fs::remove_dir_all(source).unwrap();
        std::fs::remove_dir_all(target).unwrap();
    }
}
