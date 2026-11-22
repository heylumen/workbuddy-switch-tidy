//! 复制的收尾与可观测性：备份清理报告、复制结束落库、进度推进与失败上报。
//!
//! 从 `session.rs` 拆出（4.2 拆分第 5 步，纯搬移、行为不变）。

use std::path::Path;

use serde_json::{json, Value};

use super::rows::{insert_session_copy, verify_session_row, DbCopyOutcome};
use super::{commit_links, write_copy_body, SessionPaths};
use crate::modules::config::now_ms;
use crate::modules::error_log;
use crate::modules::session_backup::{self, BackupLifecycle, CleanupOutcome};
use crate::modules::session_link::{self, ContentSnapshot, OpPhase, Operation};
use crate::modules::variant::WbVariant;

/// 业务完成后的收尾：可靠转 cleanupPending 再回收；失败只报告，不改业务结果。
pub(crate) fn finish_backup_cleanup(
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
pub(crate) fn reconcile_reported_cleanup(items: &mut [Value]) {
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
pub(crate) fn report_cleanup(
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
pub(crate) fn finish_copy_from_body(
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
pub(crate) fn advance_operation(
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

pub(crate) fn fail_operation(paths: &SessionPaths, operation: &mut Operation, error: &str) {
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
