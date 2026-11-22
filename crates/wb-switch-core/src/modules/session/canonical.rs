//! 逻辑会话（canonical）发现：把「不同账号下的同一段对话」聚成一族。
//!
//! 关联组（`LinkGroup`）只覆盖「被复制/关联过」的会话；用户从未复制过的同一段对话
//! 在各账号里各有一份（实测本机存在 9 个账号各一份 915 MB 的同内容副本），
//! 这种族在模型里等于不存在。本模块做**只读普查**。
//!
//! 聚类键：`(归一 cwd, 显示标题, 正文指纹)`。正文指纹读取 jsonl 后走
//! `normalize_jsonl`（抹平各副本自己的 id）再哈希；超过 64 MB 的文件不读正文，
//! 退化为「体积分桶」，避免普查把 900 MB 级会话整份读入。

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use serde_json::{json, Value};

use crate::modules::variant::WbVariant;

use super::store::open_db;
use super::{
    index_project_jsonls_in, normalize_cwd, normalize_jsonl, session_display_title, SessionPaths,
};

/// 参与正文指纹的单文件上限。
const MAX_FINGERPRINT_BYTES: u64 = 64 * 1024 * 1024;
/// 单次普查读取正文的总预算：超出后剩余会话退化为体积分桶。
///
/// 实测全量指纹要 68s，限到 192MB 后降到 2.8s；需要精确聚类时可设
///  放大预算（代价是更慢）。
const MAX_FINGERPRINT_TOTAL_BYTES: u64 = 192 * 1024 * 1024;

fn fingerprint_budget() -> u64 {
    std::env::var("WB_CANONICAL_FINGERPRINT_MB")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|mb| *mb > 0)
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(MAX_FINGERPRINT_TOTAL_BYTES)
}

fn body_fingerprint(path: &std::path::Path, cid: &str, size: u64) -> Option<u64> {
    if size > MAX_FINGERPRINT_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let normalized = normalize_jsonl(&text, cid);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    Some(hasher.finish())
}

/// 普查结果缓存：同档位 + 同预算下 5 分钟内复用；`force=true` 可强制重算。
///
/// 192MB 预算实测 2.8s，全量 68s —— 用户来回点不该每次都等。
const CACHE_TTL_SECS: i64 = 300;

fn cached_audit(variant: WbVariant, limit: usize, force: bool) -> Value {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Option<(String, i64, Value)>>> = OnceLock::new();
    let key = format!("{}:{}", variant.as_str(), fingerprint_budget());
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let now = crate::modules::config::now_secs();
    if !force {
        if let Ok(guard) = cache.lock() {
            if let Some((cached_key, at, value)) = guard.as_ref() {
                if cached_key == &key && now.saturating_sub(*at) < CACHE_TTL_SECS {
                    let mut hit = value.clone();
                    if let Some(obj) = hit.as_object_mut() {
                        obj.insert("cached".to_string(), json!(true));
                        obj.insert("cachedAt".to_string(), json!(*at));
                    }
                    return hit;
                }
            }
        }
    }
    let mut value = audit_canonical_uncached(variant, limit);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((key, now, value.clone()));
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert("cached".to_string(), json!(false));
    }
    value
}

/// 只读普查：跨账号的逻辑会话族（未分组族尤其值得关注）。
fn audit_canonical_uncached(variant: WbVariant, limit: usize) -> Value {
    let limit = limit.clamp(1, 200);
    let paths = SessionPaths::for_variant(variant);
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return json!({ "ok": false, "reason": "workbuddy.db 不存在", "clusters": [] });
    }
    let Some(conn) = open_db(&db, true) else {
        return json!({ "ok": false, "reason": "无法打开 workbuddy.db", "clusters": [] });
    };
    let sql = "SELECT id, user_id, cwd, COALESCE(NULLIF(custom_title, ''), title), \
               COALESCE(last_activity_at, updated_at) \
               FROM sessions WHERE deleted_at IS NULL";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return json!({ "ok": false, "reason": "查询失败", "clusters": [] });
    };
    let rows: Vec<(String, String, String, String, i64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })
        .map(|items| items.flatten().collect())
        .unwrap_or_default();

    let covered: HashSet<String> = crate::modules::session_link::active_session_ids(&paths);
    let index = index_project_jsonls_in(&paths.projects_dir());

    struct Slot {
        title: String,
        cwd: String,
        sessions: Vec<(String, String, i64, u64)>,
    }
    let mut clusters: HashMap<(String, String, Option<u64>, u64), Slot> = HashMap::new();
    let mut total = 0usize;
    let mut oversized = 0usize;
    let mut budget = fingerprint_budget();
    let mut budget_exhausted = false;
    for (cid, uid, cwd, title, updated_at) in rows {
        total += 1;
        let Some(path) = index.get(&cid) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let size = meta.len();
        let fingerprint = if size <= budget {
            let value = body_fingerprint(path, &cid, size);
            if value.is_some() {
                budget = budget.saturating_sub(size);
            }
            value
        } else {
            budget_exhausted = true;
            None
        };
        if fingerprint.is_none() {
            oversized += 1;
        }
        let bucket = if fingerprint.is_none() {
            size / (16 * 1024 * 1024)
        } else {
            0
        };
        let key = (
            normalize_cwd(&cwd),
            session_display_title(Some(title.clone()), None),
            fingerprint,
            bucket,
        );
        clusters
            .entry(key)
            .or_insert_with(|| Slot {
                title: title.clone(),
                cwd: cwd.clone(),
                sessions: Vec::new(),
            })
            .sessions
            .push((cid, uid, updated_at, size));
    }

    let mut out: Vec<Value> = Vec::new();
    let mut uncovered_count = 0usize;
    let mut covered_count = 0usize;
    for (_, slot) in clusters {
        let uids: HashSet<&str> = slot
            .sessions
            .iter()
            .map(|(_, uid, _, _)| uid.as_str())
            .collect();
        if uids.len() < 2 {
            continue;
        }
        let is_covered = slot
            .sessions
            .iter()
            .any(|(cid, _, _, _)| covered.contains(cid));
        if is_covered {
            covered_count += 1;
        } else {
            uncovered_count += 1;
        }
        let bytes: u64 = slot.sessions.iter().map(|(_, _, _, size)| *size).sum();
        let members: Vec<Value> = slot
            .sessions
            .iter()
            .take(12)
            .map(|(cid, uid, updated_at, size)| {
                json!({
                    "sessionId": cid,
                    "uid": uid,
                    "updatedAt": updated_at,
                    "bytes": size,
                    "covered": covered.contains(cid),
                })
            })
            .collect();
        out.push(json!({
            "title": slot.title,
            "cwd": slot.cwd,
            "accountCount": uids.len(),
            "copyCount": slot.sessions.len(),
            "bytes": bytes,
            "covered": is_covered,
            "members": members,
        }));
    }
    out.sort_by_key(|item| -(item.get("bytes").and_then(Value::as_u64).unwrap_or(0) as i64));
    let total_clusters = out.len();
    out.truncate(limit);
    json!({
        "ok": true,
        "totalSessions": total,
        "oversized": oversized,
        "budgetExhausted": budget_exhausted,
        "clusters": out,
        "stats": {
            "logicalClusters": total_clusters,
            "coveredClusters": covered_count,
            "uncoveredClusters": uncovered_count,
            "returned": out.len(),
        },
    })
}

/// 逻辑会话普查（带缓存）。`force` 为真时强制重算。
pub fn audit_canonical(variant: WbVariant, limit: usize, force: bool) -> Value {
    cached_audit(variant, limit, force)
}

#[cfg(test)]
mod tests {
    use super::body_fingerprint;

    #[test]
    fn missing_file_has_no_fingerprint() {
        let path = std::env::temp_dir().join("wb-canonical-missing.jsonl");
        assert!(body_fingerprint(&path, "cid", 10).is_none());
    }

    #[test]
    fn oversized_files_are_skipped() {
        let dir = std::env::temp_dir().join("wb-canonical-oversize");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jsonl");
        std::fs::write(&path, b"{}").unwrap();
        assert!(body_fingerprint(&path, "cid", u64::MAX).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
