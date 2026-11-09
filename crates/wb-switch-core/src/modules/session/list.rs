use serde_json::{json, Value};

use super::*;

/// 清理某账号下「同一会话被复制多次」产生的重复行（路径 B 副本）。
///
/// 判重键 = (归一化 cwd, 归一化 jsonl 正文)；仅正文一致才视为重复，
/// 不依赖 (cwd + title) 这种过宽键，避免误删同工作区下「无标题 / 会议纪要」
/// 等正常同名会话。每组保留 `updated_at` 最新一条，其余软删除
/// （`deleted_at` 置值），并清理其孤儿 jsonl 与云端映射，避免残留。
///
/// 注意：直接调用会写入 `~/.workbuddy/workbuddy.db`，请在 WorkBuddy 关闭后执行。
pub fn dedup_sessions_for_user(variant: WbVariant, uid: &str) -> Value {
    dedup_sessions_for_user_at(&SessionPaths::for_variant(variant), variant, uid)
}

/// 可注入路径的实现；测试与生产共用同一逻辑。
pub(crate) fn dedup_sessions_for_user_at(
    paths: &SessionPaths,
    variant: WbVariant,
    uid: &str,
) -> Value {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return json!({ "ok": false, "reason": "workbuddy.db 不存在", "removed": 0 });
    }
    let Some(conn) = open_db(&db, false) else {
        return json!({ "ok": false, "reason": "无法打开 workbuddy.db", "removed": 0 });
    };
    if !table_exists(&conn, "sessions") {
        return json!({ "ok": false, "reason": "sessions 表不存在", "removed": 0 });
    }

    let Ok(mut stmt) = conn.prepare(
        "SELECT id, cwd, updated_at FROM sessions WHERE user_id = ?1 AND deleted_at IS NULL",
    ) else {
        return json!({ "ok": false, "reason": "查询会话失败", "removed": 0 });
    };
    let Ok(rows) = stmt.query_map([uid], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    }) else {
        return json!({ "ok": false, "reason": "查询会话失败", "removed": 0 });
    };

    let index = index_project_jsonls_in(&paths.projects_dir());
    // key = (归一化 cwd, 归一化正文) -> 成员列表 (cid, updated_at, jsonl 路径)
    let mut groups: std::collections::HashMap<(String, String), Vec<SessionGroupMember>> =
        std::collections::HashMap::new();
    for r in rows.flatten() {
        let (cid, cwd, updated_at) = r;
        let cid = cid.unwrap_or_default();
        let cwd = cwd.unwrap_or_default();
        if is_claw_workspace(&cwd) {
            continue;
        }
        let norm_cwd = normalize_cwd(&cwd);
        let jsonl_path = index.get(&cid).cloned();
        // 无正文会话使用唯一键（含 cid），不参与内容去重，避免误删。
        let norm_jsonl = match &jsonl_path {
            Some(p) => std::fs::read_to_string(p)
                .ok()
                .map(|t| normalize_jsonl(&t, &cid))
                .unwrap_or_else(|| format!("__nojsonl__{cid}")),
            None => format!("__nojsonl__{cid}"),
        };
        groups.entry((norm_cwd, norm_jsonl)).or_default().push((
            cid,
            updated_at.unwrap_or(0),
            jsonl_path,
        ));
    }

    let mut removed = 0usize;
    let mut protected = 0usize;
    let mut removed_ids: Vec<String> = Vec::new();
    // 仍被关联组引用的会话不能删：删掉后关联立即失效，且 dedup 会连正文一起删除、
    // 无法恢复。这里跨档位取并集，避免只保护当前档位而误伤另一档的关联。
    let linked = match session_link::protected_session_ids(paths) {
        Ok(ids) => ids,
        Err(reason) => return json!({ "ok": false, "reason": reason, "removed": 0 }),
    };
    // 写库失败立即中止并回报原因：WorkBuddy 占用锁时不再静默丢结果、不虚报成功数，
    // 也避免 UPDATE 未生效却把 jsonl 正文删掉造成数据丢失。
    let mut failure: Option<String> = None;
    for ((_, _), mut members) in groups {
        if members.len() <= 1 {
            continue;
        }
        // 保留 updated_at 最大的一条（并列时取遇到的第一条）
        members.sort_by_key(|member| std::cmp::Reverse(member.1));
        let _keep = members.remove(0);
        for (cid, _ua, _jsonl_path) in members {
            if linked.contains(&cid) {
                protected += 1;
                continue;
            }
            let changed = match conn.execute(
                "UPDATE sessions SET deleted_at = ?1 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
                rusqlite::params![now_ms(), &cid, uid],
            ) {
                Ok(changed) => changed,
                Err(e) => {
                    failure = Some(format!(
                        "写入 workbuddy.db 失败（请关闭 WorkBuddy 后重试）：{e}"
                    ));
                    break;
                }
            };
            if changed == 0 {
                failure = Some(format!(
                    "会话 {cid} 未被软删除（可能已被并发修改），已停止清理"
                ));
                break;
            }
            // 只软删，保留 JSONL 正文；与折叠口径一致，避免不可恢复的数据删除。
            removed += 1;
            removed_ids.push(cid);
        }
        if failure.is_some() {
            break;
        }
    }

    if !removed_ids.is_empty() {
        delete_edge_sync_mappings(
            &edge_sync_db_path(&variant.data_root(), variant),
            &removed_ids,
        );
    }

    if let Some(reason) = failure {
        return json!({ "ok": false, "reason": reason, "removed": removed });
    }

    json!({
        "ok": true,
        "removed": removed,
        "protected": protected,
        "removedIds": removed_ids,
    })
}

/// 折叠某账号的「同名/同目录」会话：按 (归一化 cwd, 展示标题) 分组，
/// 每组保留 `updated_at` 最新一条，其余仅置 `deleted_at` 软隐藏（**不删 jsonl 正文**，
/// 记忆全部留盘、以后可恢复），并清理其孤儿云端映射。
///
/// 清理重复会话的分组成员：(会话 id, updated_at, jsonl 路径)。
type SessionGroupMember = (String, i64, Option<PathBuf>);

/// 与 `dedup_sessions_for_user` 的区别（两者风险等级不同，故判定口径不同）：
/// - `dedup_sessions_for_user`（清理重复会话）：按「正文逐字一致」合并，**只软删数据库行，
///   保留 jsonl 正文**，因此仍用严格口径，杜绝误删。
/// - 本函数（折叠同名会话）：仅软隐藏，正文留盘可恢复 → 放宽为「同工作区 + 同标题」
///   即可收起，用于消除切换账号反复复制产生的同名冗余。
///
/// 请在 WorkBuddy 关闭后执行。
pub fn collapse_sessions_for_user(variant: WbVariant, uid: &str) -> Value {
    let db = workbuddy_db_path(variant);
    if !db.is_file() {
        return json!({ "ok": false, "reason": "workbuddy.db 不存在", "removed": 0 });
    }
    let Some(conn) = open_db(&db, false) else {
        return json!({ "ok": false, "reason": "无法打开 workbuddy.db", "removed": 0 });
    };
    if !table_exists(&conn, "sessions") {
        return json!({ "ok": false, "reason": "sessions 表不存在", "removed": 0 });
    }

    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let sql = if has_custom {
        "SELECT id, cwd, title, custom_title, updated_at FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL"
    } else {
        "SELECT id, cwd, title, NULL, updated_at FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL"
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return json!({ "ok": false, "reason": "查询会话失败", "removed": 0 });
    };
    let Ok(rows) = stmt.query_map([uid], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    }) else {
        return json!({ "ok": false, "reason": "查询会话失败", "removed": 0 });
    };

    // 折叠口径：仅按「归一化 cwd + 展示标题」分组，不再要求正文内容一致。
    // 依据：折叠只置 deleted_at 软隐藏，jsonl 正文原样留盘、随时可恢复；
    // 而 dedup 会 remove_file 删除正文，故 dedup 保持「正文逐字一致」的严格口径。
    // 放宽后能收起切换账号反复复制产生的同名冗余，且不会造成数据丢失。
    // 附带收益：不再逐个读取 jsonl 正文，大仓库下折叠明显更快。
    let mut groups: std::collections::HashMap<(String, String), Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for r in rows.flatten() {
        let (cid, cwd, title, custom_title, ua) = r;
        let cid = cid.unwrap_or_default();
        let cwd = cwd.unwrap_or_default();
        if is_claw_workspace(&cwd) {
            continue;
        }
        let key = (
            normalize_cwd(&cwd),
            session_display_title(title, custom_title),
        );
        groups.entry(key).or_default().push((cid, ua.unwrap_or(0)));
    }

    let mut removed = 0usize;
    let mut protected = 0usize;
    let mut removed_ids: Vec<String> = Vec::new();
    // 关联中的会话即使同名也不折叠：软隐藏后关联成员会立即失效，用户再打开
    // 「关联会话」只能看到「会话记录缺失」。跨档位取并集保护。
    let linked = match session_link::protected_session_ids(&SessionPaths::for_variant(variant)) {
        Ok(ids) => ids,
        Err(reason) => return json!({ "ok": false, "reason": reason, "removed": 0 }),
    };
    // 写库失败立即中止并回报原因：WorkBuddy 占用锁时不再静默丢结果、不虚报成功数。
    let mut failure: Option<String> = None;
    for (_, mut members) in groups {
        if members.len() <= 1 {
            continue;
        }
        // 保留 updated_at 最大的一条（并列时取遇到的第一条），其余软隐藏。
        members.sort_by_key(|member| std::cmp::Reverse(member.1));
        let _keep = members.remove(0);
        for (cid, _ua) in members {
            if linked.contains(&cid) {
                protected += 1;
                continue;
            }
            if let Err(e) = conn.execute(
                "UPDATE sessions SET deleted_at = ?1 WHERE id = ?2 AND user_id = ?3",
                rusqlite::params![now_ms(), &cid, uid],
            ) {
                failure = Some(format!(
                    "写入 workbuddy.db 失败（请关闭 WorkBuddy 后重试）：{e}"
                ));
                break;
            }
            // 注意：不删除 jsonl 正文，记忆留盘、可恢复。
            removed += 1;
            removed_ids.push(cid);
        }
        if failure.is_some() {
            break;
        }
    }

    if !removed_ids.is_empty() {
        delete_edge_sync_mappings(
            &edge_sync_db_path(&variant.data_root(), variant),
            &removed_ids,
        );
    }

    if let Some(reason) = failure {
        return json!({ "ok": false, "reason": reason, "removed": removed });
    }

    json!({
        "ok": true,
        "removed": removed,
        "protected": protected,
        "removedIds": removed_ids,
    })
}

/// 列出某账号未删除的会话（workbuddy.db sessions 表，db 为准）。
///
/// `title` 为 WorkBuddy 侧栏同款展示名；`isPlayground` 对应侧栏「任务」，
/// 其余按 `cwd` 最后一段归入「空间」。
pub fn list_sessions_for_user(variant: WbVariant, uid: &str) -> Value {
    list_sessions_for_user_at(&SessionPaths::for_variant(variant), uid)
}

/// 指定账号名下的会话列表（会话管理页的源账号视角）。
///
/// 与 `GET /api/sessions` 的返回同形（`sessions / current / variant`），但来源是
/// 显式账号而非登录态；账号缺 uid 时返回空列表 + `current: null`（与现有容错一致）。
pub fn list_sessions_for_account(account: &Value) -> Value {
    let variant = account::variant_of(account);
    let uid = account_uid(account);
    if uid.is_empty() {
        return json!({ "sessions": [], "current": Value::Null, "variant": variant.as_str() });
    }
    json!({
        "sessions": list_sessions_for_user(variant, &uid),
        "current": uid,
        "variant": variant.as_str(),
    })
}

/// `sessions` 表的可选列兼容层：不同客户端版本可能没有 custom_title / is_playground。
#[derive(Clone, Copy)]
struct SessionSchema {
    custom_title: bool,
    playground: bool,
}

impl SessionSchema {
    fn detect(conn: &rusqlite::Connection) -> Self {
        Self {
            custom_title: column_exists(conn, "sessions", "custom_title"),
            playground: column_exists(conn, "sessions", "is_playground"),
        }
    }

    fn title_expr(self) -> &'static str {
        if self.custom_title {
            "custom_title"
        } else {
            "NULL"
        }
    }

    fn playground_expr(self) -> &'static str {
        if self.playground {
            "is_playground"
        } else {
            "0"
        }
    }
}

/// 只查一个会话行（关联会话详情/同步使用），避免列出该账号全部会话。
pub(crate) fn session_row_for_user(
    variant: WbVariant,
    uid: &str,
    session_id: &str,
) -> Option<Value> {
    let paths = SessionPaths::for_variant(variant);
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return None;
    }
    let conn = open_db(&db, true)?;
    if !table_exists(&conn, "sessions") {
        return None;
    }
    let schema = SessionSchema::detect(&conn);
    let sql = format!(
        "SELECT cwd, title, {}, updated_at FROM sessions \n         WHERE id = ?1 AND user_id = ?2 AND deleted_at IS NULL LIMIT 1",
        schema.title_expr()
    );
    let (cwd, title, custom_title, updated_at) = conn
        .query_row(&sql, rusqlite::params![session_id, uid], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .ok()?;
    Some(json!({
        "id": session_id,
        "title": session_display_title(title, custom_title),
        "cwd": cwd.unwrap_or_default(),
        "updatedAt": updated_at.unwrap_or(0),
    }))
}

pub(crate) fn list_sessions_for_user_at(paths: &SessionPaths, uid: &str) -> Value {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return json!([]);
    }
    let Some(conn) = open_db(&db, true) else {
        return json!([]);
    };
    if !table_exists(&conn, "sessions") {
        return json!([]);
    }
    let schema = SessionSchema::detect(&conn);
    let sql = format!(
        "SELECT id, cwd, title, {}, updated_at, {} FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC",
        schema.title_expr(),
        schema.playground_expr()
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return json!([]),
    };
    let rows = stmt.query_map([uid], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    });

    let mut sessions: Vec<Value> = Vec::new();
    if let Ok(iter) = rows {
        for r in iter.flatten() {
            let (cid, cwd, title, custom_title, updated_at, is_playground) = r;
            let cid = cid.unwrap_or_default();
            let cwd = cwd.unwrap_or_default();
            if is_claw_workspace(&cwd) {
                continue;
            }
            sessions.push(json!({
                "id": cid,
                "title": session_display_title(title, custom_title),
                "cwd": cwd,
                "updatedAt": updated_at.unwrap_or(0),
                "hasHistory": find_project_jsonl(paths, &cid).is_some(),
                "isPlayground": is_playground.unwrap_or(0) != 0,
            }));
        }
    }
    json!(sessions)
}

// ---------------------------------------------------------------------------
// P1a：重复候选只读分析（不写数据库）
// ---------------------------------------------------------------------------

type AuditMember = (String, i64, Option<Vec<String>>, String);

/// 只读分析某账号的同名会话候选组。
///
/// 判定基于归一化消息序列，不修改 `deleted_at`、不删正文、不改关联表。
/// 输出可按 `kind` 直接驱动后续预览页：
/// - `exact`：消息序列完全相同
/// - `fastForward`：所有较短序列都是最长序列的前缀
/// - `divergent`：有公共前缀，但存在互不为前缀的分支
/// - `independent`：没有可用的公共前缀
/// - `unavailable`：有会话正文无法读取/解析，不能安全分类
pub fn audit_session_duplicates_for_user(variant: WbVariant, uid: &str) -> Value {
    audit_session_duplicates_for_user_at(&SessionPaths::for_variant(variant), variant, uid)
}

pub(crate) fn audit_session_duplicates_for_user_at(
    paths: &SessionPaths,
    _variant: WbVariant,
    uid: &str,
) -> Value {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return json!({ "ok": false, "reason": "workbuddy.db 不存在", "groups": [], "summary": {} });
    }
    let Some(conn) = open_db(&db, true) else {
        return json!({ "ok": false, "reason": "无法读取 workbuddy.db", "groups": [], "summary": {} });
    };
    if !table_exists(&conn, "sessions") {
        return json!({ "ok": false, "reason": "sessions 表不存在", "groups": [], "summary": {} });
    }
    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let sql = if has_custom {
        "SELECT id, cwd, title, custom_title, updated_at FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL"
    } else {
        "SELECT id, cwd, title, NULL, updated_at FROM sessions \
         WHERE user_id = ?1 AND deleted_at IS NULL"
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return json!({ "ok": false, "reason": "查询会话失败", "groups": [], "summary": {} });
    };
    let Ok(rows) = stmt.query_map([uid], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    }) else {
        return json!({ "ok": false, "reason": "查询会话失败", "groups": [], "summary": {} });
    };

    // 先按「工作区 + 标题」收窄候选；只有重复组需要读取正文。
    let mut candidates: HashMap<(String, String), Vec<(String, i64)>> = HashMap::new();
    for r in rows.flatten() {
        let (cid, cwd, title, custom_title, updated_at) = r;
        let cid = cid.unwrap_or_default();
        let cwd = cwd.unwrap_or_default();
        if is_claw_workspace(&cwd) {
            continue;
        }
        candidates
            .entry((
                normalize_cwd(&cwd),
                session_display_title(title, custom_title),
            ))
            .or_default()
            .push((cid, updated_at.unwrap_or(0)));
    }

    let index = index_project_jsonls_in(&paths.projects_dir());
    let mut groups = Vec::new();
    let mut counters = HashMap::<String, usize>::new();
    for ((cwd, title), candidates_members) in candidates {
        if candidates_members.len() <= 1 {
            continue;
        }
        let members: Vec<AuditMember> = candidates_members
            .into_iter()
            .map(|(cid, updated_at)| {
                let body =
                    index.get(&cid).and_then(|path| {
                        match session_link::read_content_snapshot(path, &cid) {
                            ContentState::Ready(snapshot) => Some(snapshot.normalized.line_digests),
                            ContentState::Missing | ContentState::Unavailable(_) => None,
                        }
                    });
                let status = if body.is_some() {
                    "ready"
                } else {
                    "unavailable"
                };
                (cid, updated_at, body, status.to_string())
            })
            .collect();
        let kind = classify_duplicate_group(&members);
        *counters.entry(kind.to_string()).or_default() += 1;
        let sessions: Vec<Value> = members
            .iter()
            .map(|(cid, updated_at, body, status)| {
                json!({
                    "sessionId": cid,
                    "updatedAt": updated_at,
                    "records": body.as_ref().map(Vec::len),
                    "status": status,
                })
            })
            .collect();
        groups.push(json!({
            "uid": uid,
            "cwd": cwd,
            "title": title,
            "count": members.len(),
            "kind": kind,
            "suggested": match kind {
                "exact" => "keep-newest",
                "fastForward" => "fast-forward-to-longest",
                "divergent" | "independent" | "unavailable" => "manual",
                _ => "manual",
            },
            "suggestedKeepSessionId": if kind == "fastForward" {
                members
                    .iter()
                    .filter(|(_, _, body, _)| body.is_some())
                    .max_by_key(|(_, _, body, _)| body.as_ref().map(Vec::len).unwrap_or(0))
                    .map(|(cid, _, _, _)| cid.clone())
            } else if kind == "exact" {
                members
                    .iter()
                    .max_by_key(|(_, updated_at, _, _)| *updated_at)
                    .map(|(cid, _, _, _)| cid.clone())
            } else {
                None
            },
            "sessions": sessions,
        }));
    }

    groups.sort_by(|a, b| {
        b["count"].as_u64().cmp(&a["count"].as_u64()).then_with(|| {
            a["title"]
                .as_str()
                .unwrap_or("")
                .cmp(b["title"].as_str().unwrap_or(""))
        })
    });
    json!({
        "ok": true,
        "uid": uid,
        "groups": groups,
        "summary": {
            "groups": counters.values().sum::<usize>(),
            "exact": counters.get("exact").copied().unwrap_or(0),
            "fastForward": counters.get("fastForward").copied().unwrap_or(0),
            "divergent": counters.get("divergent").copied().unwrap_or(0),
            "independent": counters.get("independent").copied().unwrap_or(0),
            "unavailable": counters.get("unavailable").copied().unwrap_or(0),
        }
    })
}

fn classify_duplicate_group(
    members: &[(String, i64, Option<Vec<String>>, String)],
) -> &'static str {
    let mut seqs: Vec<&Vec<String>> = Vec::new();
    for (_, _, body, _) in members {
        match body {
            Some(body) => seqs.push(body),
            None => return "unavailable",
        }
    }
    if seqs.len() <= 1 {
        return "independent";
    }
    if seqs.windows(2).all(|w| w[0] == w[1]) {
        return "exact";
    }
    let longest = seqs.iter().max_by_key(|seq| seq.len()).unwrap();
    if seqs.iter().all(|seq| seq_is_prefix(seq, longest)) {
        return "fastForward";
    }
    if common_prefix_len(&seqs) > 0 {
        "divergent"
    } else {
        "independent"
    }
}

fn seq_is_prefix(short: &[String], long: &[String]) -> bool {
    short.len() <= long.len() && short.iter().zip(long.iter()).all(|(a, b)| a == b)
}

fn common_prefix_len(seqs: &[&Vec<String>]) -> usize {
    let Some(first) = seqs.first() else { return 0 };
    let mut len = 0usize;
    'outer: for (i, digest) in first.iter().enumerate() {
        for seq in seqs.iter().skip(1) {
            if seq.get(i) != Some(digest) {
                break 'outer;
            }
        }
        len += 1;
    }
    len
}

/// 软隐藏一个已审计重复组中除 `keep_session_id` 外的别名。
///
/// 只写 `sessions.deleted_at`；不删除 JSONL、不改 edge mapping、不改关联表。
/// - exact：保留任意一条；其余软隐藏。
/// - fastForward：必须保留最长副本。
/// - divergent：允许用户显式指定保留项。
/// - unavailable / independent：拒绝执行。
/// - 关联中的 active 成员跳过。
pub fn hide_duplicate_group_for_user(
    variant: WbVariant,
    uid: &str,
    cwd: &str,
    title: &str,
    keep_session_id: &str,
) -> Value {
    hide_duplicate_group_for_user_at(
        &SessionPaths::for_variant(variant),
        variant,
        uid,
        cwd,
        title,
        keep_session_id,
    )
}

pub(crate) fn hide_duplicate_group_for_user_at(
    paths: &SessionPaths,
    variant: WbVariant,
    uid: &str,
    cwd: &str,
    title: &str,
    keep_session_id: &str,
) -> Value {
    let report = audit_session_duplicates_for_user_at(paths, variant, uid);
    if report["ok"] != true {
        return json!({ "ok": false, "reason": report["reason"].clone() });
    }
    let group = report["groups"].as_array().and_then(|groups| {
        groups
            .iter()
            .find(|group| group["uid"] == uid && group["cwd"] == cwd && group["title"] == title)
    });
    let Some(group) = group else {
        return json!({ "ok": false, "reason": "审计中未找到该重复组" });
    };
    let kind = group["kind"].as_str().unwrap_or("unavailable");
    if !matches!(kind, "exact" | "fastForward" | "divergent") {
        return json!({ "ok": false, "reason": "该组无法安全自动处理，请人工检查" });
    }
    let Some(sessions) = group["sessions"].as_array() else {
        return json!({ "ok": false, "reason": "审计组缺少会话列表" });
    };
    let ids: Vec<String> = sessions
        .iter()
        .filter_map(|session| session["sessionId"].as_str().map(str::to_string))
        .collect();
    if !ids.iter().any(|id| id == keep_session_id) {
        return json!({ "ok": false, "reason": "保留项不在该重复组中" });
    }
    if kind == "fastForward" && group["suggestedKeepSessionId"] != keep_session_id {
        return json!({ "ok": false, "reason": "快进副本必须保留最长副本" });
    }
    let linked = match session_link::protected_session_ids(paths) {
        Ok(ids) => ids,
        Err(reason) => return json!({ "ok": false, "reason": reason }),
    };
    let hide_ids: Vec<String> = ids
        .into_iter()
        .filter(|id| id != keep_session_id && !linked.contains(id))
        .collect();
    let skipped_linked = sessions.len().saturating_sub(1 + hide_ids.len());
    if hide_ids.is_empty() {
        return json!({
            "ok": true,
            "hidden": 0,
            "skippedLinked": skipped_linked,
            "reason": "没有可隐藏的非关联别名"
        });
    }

    let backup_root = paths
        .store_root
        .join("backups")
        .join(format!("duplicate-hide-{}", now_ms()));
    let backup = match backup_workbuddy_db(paths, &backup_root) {
        Ok(path) => path.to_string_lossy().to_string(),
        Err(error) => return json!({ "ok": false, "reason": error }),
    };
    let db = paths.workbuddy_db();
    let Some(conn) = open_db(&db, false) else {
        return json!({ "ok": false, "reason": "无法打开 workbuddy.db", "backup": backup });
    };
    if let Err(error) = conn.execute_batch("BEGIN IMMEDIATE") {
        return json!({ "ok": false, "reason": format!("事务启动失败：{error}"), "backup": backup });
    }
    for id in &hide_ids {
        if let Err(error) = conn.execute(
            "UPDATE sessions SET deleted_at = ?1 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
            rusqlite::params![now_ms(), id, uid],
        ) {
            let _ = conn.execute_batch("ROLLBACK");
            return json!({ "ok": false, "reason": format!("软隐藏失败：{error}"), "backup": backup });
        }
    }
    if let Err(error) = conn.execute_batch("COMMIT") {
        let _ = conn.execute_batch("ROLLBACK");
        return json!({ "ok": false, "reason": format!("提交失败：{error}"), "backup": backup });
    }
    json!({
        "ok": true,
        "hidden": hide_ids.len(),
        "hiddenIds": hide_ids,
        "skippedLinked": skipped_linked,
        "keepSessionId": keep_session_id,
        "kind": kind,
        "backup": backup,
    })
}
