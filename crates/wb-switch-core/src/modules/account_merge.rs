//! 账号资料迁移：把「人格 / 记忆 / 偏好 / 专家」从来源账号复制到目标账号。
//!
//! 安全边界（2026-11-22 实测确认）：
//! - 只处理**纯文本或纯配置**的账号级文件；**连接器状态不迁移**——
//!   `connectors/<uid>/` 内含 `.master.key`、`connector-states.json` 的 `encryption` 字段与
//!   `accountIdentityKey`，是按账号加密并绑定身份的，跨账号复制既解不开也会泄露密钥。
//! - `credentials/`、`plugin-account-state/` 一律不碰。
//! - 覆盖前把目标原文件备份到 `{store_root}/backups/account-merge-{ms}/`，可人工回滚。
//! - 前端只传条目 id + 账号 id；路径全部由本模块按 uid 计算，拒绝任意路径。

use crate::modules::config::now_ms;
use crate::modules::session::SessionPaths;
use crate::modules::variant::WbVariant;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

struct Artifact {
    id: &'static str,
    label: &'static str,
    hint: &'static str,
    /// 路径模板：`{uid}` 会被替换。
    template: &'static str,
}

const ARTIFACTS: &[Artifact] = &[
    Artifact {
        id: "identity-identity",
        label: "身份（IDENTITY.md）",
        hint: "账号的人设/角色定位",
        template: "user-{uid}-personal/IDENTITY.md",
    },
    Artifact {
        id: "identity-soul",
        label: "人格（SOUL.md）",
        hint: "回答风格与行为约束",
        template: "user-{uid}-personal/SOUL.md",
    },
    Artifact {
        id: "identity-user",
        label: "用户画像（USER.md）",
        hint: "关于你的长期偏好记录",
        template: "user-{uid}-personal/USER.md",
    },
    Artifact {
        id: "identity-bootstrap",
        label: "启动记忆（BOOTSTRAP.md）",
        hint: "每次会话开始时加载的引导内容",
        template: "user-{uid}-personal/BOOTSTRAP.md",
    },
    Artifact {
        id: "memory-working",
        label: "本地工作记忆（旧版 MEMORY.md，若存在）",
        hint: "仅旧版客户端会在本地保留该文件；当前版本的记忆在云端，本地不落文件，因此通常显示为「来源无此项」",
        template: "user-{uid}-personal/MEMORY.md",
    },
    Artifact {
        id: "preferences",
        label: "偏好设置（preferences.json）",
        hint: "主题、权限模式等账号级偏好",
        template: "storage/user-{uid}-personal/global/preferences.json",
    },
    Artifact {
        id: "experts",
        label: "最近使用专家（expert.json）",
        hint: "该账号的最近专家记录",
        template: "storage/user-{uid}-personal/global/expert.json",
    },
];

fn valid_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() >= 8
        && uid
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '_')
}

fn artifact_path(data_root: &Path, uid: &str, artifact: &Artifact) -> PathBuf {
    data_root.join(artifact.template.replace("{uid}", uid))
}

/// 扫描：列出可迁移条目在来源/目标两侧的存在情况与体积。
pub fn scan_account_artifacts(variant: WbVariant, source_uid: &str, target_uid: &str) -> Value {
    if !valid_uid(source_uid) || !valid_uid(target_uid) {
        return json!({ "ok": false, "reason": "账号 uid 非法" });
    }
    if source_uid == target_uid {
        return json!({ "ok": false, "reason": "来源与目标是同一个账号" });
    }
    let paths = SessionPaths::for_variant(variant);
    let items: Vec<Value> = ARTIFACTS
        .iter()
        .map(|artifact| {
            let source = artifact_path(&paths.data_root, source_uid, artifact);
            let target = artifact_path(&paths.data_root, target_uid, artifact);
            let source_bytes = fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
            let target_bytes = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
            json!({
                "id": artifact.id,
                "label": artifact.label,
                "hint": artifact.hint,
                "sourceExists": source.exists(),
                "sourceBytes": source_bytes,
                "targetExists": target.exists(),
                "targetBytes": target_bytes,
            })
        })
        .collect();
    json!({
        "ok": true,
        "sourceUid": source_uid,
        "targetUid": target_uid,
        "items": items,
        "notMigratable": [
            "连接器与插件账号态：按账号加密并绑定身份（含 .master.key / identityKey），跨账号复制无意义且有泄露风险",
            "凭据目录（credentials/）：永不迁移",
            "云端记忆缓存（memory/<uid>_memory.md）：是服务端档案的本地缓存，复制到别的账号会被误认为该账号的记忆，不迁移"
        ],
    })
}

/// 执行迁移：`overwrite = false` 时跳过目标已存在的条目。
pub fn merge_account_artifacts(
    variant: WbVariant,
    source_uid: &str,
    target_uid: &str,
    item_ids: &[String],
    overwrite: bool,
) -> Value {
    if !valid_uid(source_uid) || !valid_uid(target_uid) {
        return json!({ "ok": false, "reason": "账号 uid 非法" });
    }
    if source_uid == target_uid {
        return json!({ "ok": false, "reason": "来源与目标是同一个账号" });
    }
    let paths = SessionPaths::for_variant(variant);
    let backup_root = paths
        .store_root
        .join("backups")
        .join(format!("account-merge-{}", now_ms()));
    let mut copied: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut backed_up = 0usize;

    for id in item_ids {
        let Some(artifact) = ARTIFACTS.iter().find(|item| item.id == id) else {
            skipped.push(json!({ "id": id, "reason": "未知条目" }));
            continue;
        };
        let source = artifact_path(&paths.data_root, source_uid, artifact);
        let target = artifact_path(&paths.data_root, target_uid, artifact);
        if !source.is_file() {
            skipped.push(
                json!({ "id": artifact.id, "label": artifact.label, "reason": "来源不存在" }),
            );
            continue;
        }
        if target.exists() && !overwrite {
            skipped.push(json!({ "id": artifact.id, "label": artifact.label, "reason": "目标已存在，已跳过" }));
            continue;
        }
        // 目标已有内容先备份（保留相对路径，便于人工回滚）。
        if target.is_file() {
            let relative = target
                .strip_prefix(&paths.data_root)
                .map(|value| value.to_path_buf())
                .unwrap_or_else(|_| PathBuf::from(artifact.id));
            let backup = backup_root.join(relative);
            if let Some(parent) = backup.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    skipped.push(json!({
                        "id": artifact.id,
                        "label": artifact.label,
                        "reason": format!("创建备份目录失败（{}）：{error}", parent.display())
                    }));
                    continue;
                }
            }
            if let Err(error) = fs::copy(&target, &backup) {
                skipped.push(json!({
                    "id": artifact.id,
                    "label": artifact.label,
                    "reason": format!("备份目标文件失败：{error}")
                }));
                continue;
            }
            backed_up += 1;
        }
        if let Some(parent) = target.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                skipped.push(json!({
                    "id": artifact.id,
                    "label": artifact.label,
                    "reason": format!("创建目标目录失败：{error}")
                }));
                continue;
            }
        }
        match fs::copy(&source, &target) {
            Ok(bytes) => copied.push(json!({
                "id": artifact.id,
                "label": artifact.label,
                "bytes": bytes,
            })),
            Err(error) => skipped.push(json!({
                "id": artifact.id,
                "label": artifact.label,
                "reason": format!("复制失败：{error}")
            })),
        }
    }

    let backup = if backed_up > 0 {
        Some(backup_root.to_string_lossy().to_string())
    } else {
        None
    };
    json!({
        "ok": true,
        "copied": copied,
        "skipped": skipped,
        "backedUp": backed_up,
        "backup": backup,
    })
}

#[cfg(test)]
mod tests {
    // 夹具约定：uid / 会话 id 一律用合成值（全 1/全 2 等），
    // 禁止从本机账号库复制真实值——它们会随仓库公开。

    use super::*;

    #[test]
    fn uid_validation_rejects_traversal() {
        assert!(valid_uid("11111111-1111-4111-8111-111111111111"));
        assert!(!valid_uid("../etc/passwd"));
        assert!(!valid_uid("short"));
        assert!(!valid_uid(""));
    }

    #[test]
    fn rejects_same_or_invalid_accounts() {
        let value = scan_account_artifacts(WbVariant::Cn, "1af6e961-b0e1", "1af6e961-b0e1");
        assert_eq!(value["ok"], json!(false));
        let value = merge_account_artifacts(WbVariant::Cn, "nope", "1af6e961-b0e1", &[], true);
        assert_eq!(value["ok"], json!(false));
    }

    #[test]
    fn unknown_item_is_skipped_not_fatal() {
        let value = merge_account_artifacts(
            WbVariant::Cn,
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            &["not-an-artifact".to_string()],
            true,
        );
        assert_eq!(value["ok"], json!(true));
        assert_eq!(
            value["skipped"].as_array().map(|items| items.len()),
            Some(1)
        );
    }
}
