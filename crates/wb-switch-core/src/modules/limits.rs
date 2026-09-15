//! 限额台账：扫描 `~/.workbuddy/logs/` 解析模型频率限制（429）记录与官方重置时间。
//!
//! 数据完全来自本机日志，不调用任何接口（对照 Issue #36 的实测格式）：
//!   - 业务日志 `<date>/<workspace>__<hash>.log`：`[2026/9/11 13:00:10.804] [Info] ...
//!     429 您的使用量已超出频率限制，将在 2026-09-11 17:18:36 UTC+8 重置 ... (reqId/sessionId)`
//!     以及同事件的 `[Warning] [ACP Agent] refusal classified: sessionId=<uuid>,
//!     rpcCode=-32003, httpStatus=429, bizCode=6004, category=quota`
//!   - SDK 日志 `<date>/sdk/conversations/<sessionId>.log`：JSON 行内
//!     `errorMessageMetaPreview` 含同一条 429 文案
//!
//! 账号：workbuddy.db `sessions.user_id` 反查（会话归属账号）。
//! 模型：会话 jsonl 最近一条 `providerData.model` 反查（日志无模型名，仅供参考）。
//! 去重两步：① (sessionId, resetAt) ② 同 resetAt 且 occurredAt 相差 ≤15s 聚合，
//! 兼容同一事件在业务日志写多行、又在 SDK 日志写一份的重复。

use chrono::NaiveDateTime;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::modules::config::{home_dir, now_ms};

/// 429 限额文案的稳定锚点：出现在业务日志与 SDK 日志的同一事件中。
const QUOTA_MARKER: &str = "您的使用量已超出频率限制，将在 ";
/// `将在 ` 之后紧跟 `YYYY-MM-DD HH:MM:SS`，长度固定 19。
const RESET_TIME_LEN: usize = 19;
/// 第二步去重的发生时刻窗口：同一事件在业务日志与 SDK 日志的写入相差不超过此值。
const DEDUP_WINDOW_MS: i64 = 15_000;
/// UTC+8 偏移（issue 实测业务日志为本地时间、SDK 为 UTC，统一按 +8 解析）。
const UTC_PLUS8: i64 = 8 * 3600;

#[derive(Clone, Debug)]
struct RawEvent {
    occurred_at: i64,
    reset_at: i64,
    session_id: Option<String>,
}

/// 解析 `YYYY-MM-DD HH:MM:SS` 为 UTC+8 毫秒时间戳。
fn parse_reset_at(line: &str, marker_start: usize) -> Option<i64> {
    let start = marker_start + QUOTA_MARKER.len();
    let slice = line.get(start..start.checked_add(RESET_TIME_LEN)?)?;
    let ndt = NaiveDateTime::parse_from_str(slice, "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = chrono::FixedOffset::east_opt(UTC_PLUS8 as i32)?;
    Some(ndt.and_local_timezone(offset).single()?.timestamp_millis())
}

/// 解析业务日志行首 `[Y/M/D H:M:S.ms]` 为 UTC+8 毫秒时间戳。
/// 年月日允许单数字（如 `2026/9/11`），与 WorkBuddy 业务日志一致。
fn parse_business_line_time(line: &str) -> Option<i64> {
    let close = line.find(']')?;
    if !line.starts_with('[') || close < 2 {
        return None;
    }
    let body = &line[1..close];
    let mut parts = body.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let mut d = date.split('/');
    let y: i32 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let s_part = t.next()?;
    let (sec, milli): (u32, u32) = match s_part.split_once('.') {
        Some((s, ms)) => (s.parse().ok()?, ms.get(..3).and_then(|m| m.parse().ok()).unwrap_or(0)),
        None => (s_part.parse().ok()?, 0),
    };
    let nd = chrono::NaiveDate::from_ymd_opt(y, mo, da)?.and_hms_milli_opt(h, mi, sec, milli)?;
    let offset = chrono::FixedOffset::east_opt(UTC_PLUS8 as i32)?;
    Some(nd.and_local_timezone(offset).single()?.timestamp_millis())
}

/// 提取 `sessionId=<value>` 中的 value（到下一个非 UUID 字符）。
fn extract_session_id_after_marker(line: &str, marker: &str) -> Option<String> {
    let idx = line.find(marker).map(|i| i + marker.len())?;
    let raw = line.get(idx..)?;
    let end = raw
        .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .unwrap_or(raw.len());
    let value = raw.get(..end)?.trim_end();
    let value = value.trim_end_matches(')').trim_end();
    if value.is_empty() || value.eq_ignore_ascii_case("reqId")
        || value.eq_ignore_ascii_case("sessionId")
    {
        return None;
    }
    Some(value.to_string())
}

/// 解析业务 429 行尾的 `(reqId/sessionId)`，取斜杠后的部分；失败回退取整体。
fn extract_session_id_from_parens(line: &str) -> Option<String> {
    let open = line.rfind('(')?;
    let close = line.rfind(')')?;
    if close <= open {
        return None;
    }
    let inner = line.get(open + 1..close)?.trim();
    if inner.is_empty() {
        return None;
    }
    let value = inner.rsplit('/').next().unwrap_or(inner);
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("reqId")
        || value.eq_ignore_ascii_case("sessionId")
    {
        return None;
    }
    Some(value.to_string())
}

/// 判断字符串是否像一个合法会话 id（UUID 或长十六进制），避免把占位符当成真实 id。
fn looks_like_session_id(value: &str) -> bool {
    if value.len() < 8 {
        return false;
    }
    value.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// 收集一个业务日志文件中的 429 事件；同文件内 `sessionId=` Warning 行就近回填。
fn collect_business_log(path: &Path, out: &mut Vec<RawEvent>, errors: &mut u64) {
    let Ok(file) = std::fs::File::open(path) else {
        *errors += 1;
        return;
    };
    // 同文件内 `sessionId=` Warning 行的 (occurredAt, sessionId)，用于就近回填。
    let mut warnings: Vec<(i64, String)> = Vec::new();
    let mut pending: Vec<RawEvent> = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            *errors += 1;
            continue;
        };
        let Some(marker_start) = line.find(QUOTA_MARKER) else {
            // 非 429 行：若含 sessionId= 记录下来供就近回填。
            if let Some(sid) = extract_session_id_after_marker(&line, "sessionId=") {
                if looks_like_session_id(&sid) {
                    if let Some(ts) = parse_business_line_time(&line) {
                        warnings.push((ts, sid));
                    }
                }
            }
            continue;
        };
        let reset_at = match parse_reset_at(&line, marker_start) {
            Some(ts) => ts,
            None => {
                *errors += 1;
                continue;
            }
        };
        let occurred_at = parse_business_line_time(&line).unwrap_or(reset_at);
        // 行内优先 sessionId=，其次行尾 (reqId/sessionId)。
        let session_id = extract_session_id_after_marker(&line, "sessionId=")
            .or_else(|| extract_session_id_from_parens(&line))
            .filter(|s| looks_like_session_id(s));
        pending.push(RawEvent {
            occurred_at,
            reset_at,
            session_id,
        });
    }
    // 就近回填：为缺少 session_id 的 429 行匹配 ±窗口内最近的 Warning sessionId。
    for event in pending.iter_mut() {
        if event.session_id.is_some() {
            continue;
        }
        if let Some((_, sid)) = warnings
            .iter()
            .filter(|(ts, _)| (ts - event.occurred_at).abs() <= DEDUP_WINDOW_MS)
            .min_by_key(|(ts, _)| (ts - event.occurred_at).abs())
        {
            event.session_id = Some(sid.clone());
        }
    }
    out.extend(pending);
}

/// 收集一个 SDK 会话日志文件中的 429 事件；sessionId 取自文件名。
fn collect_sdk_log(path: &Path, out: &mut Vec<RawEvent>, errors: &mut u64) {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let session_id = if looks_like_session_id(stem) {
        Some(stem.to_string())
    } else {
        None
    };
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let Ok(file) = std::fs::File::open(path) else {
        *errors += 1;
        return;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            *errors += 1;
            continue;
        };
        let Some(marker_start) = line.find(QUOTA_MARKER) else {
            continue;
        };
        let Some(reset_at) = parse_reset_at(&line, marker_start) else {
            *errors += 1;
            continue;
        };
        out.push(RawEvent {
            occurred_at: mtime.unwrap_or(reset_at),
            reset_at,
            session_id: session_id.clone(),
        });
    }
}

/// 递归收集日期目录下的 `.log` 文件，按路径区分业务/SDK 两类。
fn collect_date_folder(folder: &Path, events: &mut Vec<RawEvent>, errors: &mut u64, files: &mut usize) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_date_folder(&path, events, errors, files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("log") {
            *files += 1;
            let is_sdk = path
                .components()
                .any(|c| c.as_os_str().to_str() == Some("sdk"));
            if is_sdk {
                collect_sdk_log(&path, events, errors);
            } else {
                collect_business_log(&path, events, errors);
            }
        }
    }
}

/// 尝试把日期目录名解析为 UTC+8 当天 00:00 的时间戳（多种命名格式）。
fn folder_date_ms(name: &str) -> Option<i64> {
    let clean = name.trim_end_matches('/');
    let parts: Vec<&str> = clean.split(|c| c == '/' || c == '_' || c == '-').collect();
    let parsed = (|| {
        if parts.len() == 3 {
            let (y, m, d) = (
                parts[0].parse::<i32>().ok()?,
                parts[1].parse::<u32>().ok()?,
                parts[2].parse::<u32>().ok()?,
            );
            return chrono::NaiveDate::from_ymd_opt(y, m, d);
        }
        None
    })()?;
    let offset = chrono::FixedOffset::east_opt(UTC_PLUS8 as i32)?;
    Some(parsed.and_hms_opt(0, 0, 0)?.and_local_timezone(offset).single()?.timestamp_millis())
}

/// 扫描 `~/.workbuddy/logs/` 近 N 天的日志，返回去重前的原始事件。
fn scan_logs(root: &Path, cutoff: Option<i64>) -> (Vec<RawEvent>, usize, u64) {
    let mut events = Vec::new();
    let mut files = 0usize;
    let mut errors = 0u64;
    let Ok(entries) = std::fs::read_dir(root) else {
        return (events, files, errors);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(folder_ms) = folder_date_ms(name) else {
            continue; // 非日期目录跳过，避免扫到无关内容
        };
        if cutoff.is_some_and(|c| folder_ms < c) {
            continue;
        }
        collect_date_folder(&path, &mut events, &mut errors, &mut files);
    }
    (events, files, errors)
}

/// 两步去重：① (sessionId, resetAt) ② 同 resetAt 且 occurredAt 相差 ≤15s 聚合。
fn dedup(raw: Vec<RawEvent>) -> Vec<RawEvent> {
    let mut seen: HashSet<(Option<String>, i64)> = HashSet::new();
    let mut kept: Vec<RawEvent> = Vec::with_capacity(raw.len());
    for ev in raw {
        let key = (ev.session_id.clone(), ev.reset_at);
        if ev.session_id.is_some() && !seen.insert(key) {
            // 已有同 (sid, reset)：保留 occurred_at 更早的那条。
            if let Some(existing) = kept.iter_mut().find(|e| {
                e.session_id == ev.session_id && e.reset_at == ev.reset_at
            }) {
                if ev.occurred_at < existing.occurred_at {
                    existing.occurred_at = ev.occurred_at;
                }
            }
            continue;
        }
        kept.push(ev);
    }

    // 第二步：按 resetAt 分组，组内按 occurredAt 排序后做 15s 窗口聚类。
    // 只合并"至少一方无会话 id（或会话 id 相同）"的记录——两个不同会话的事件
    // 即使重置时间相同、时刻接近也是独立事件（官方重置时间常为整点网格，易碰撞）。
    kept.sort_by(|a, b| a.reset_at.cmp(&b.reset_at).then(a.occurred_at.cmp(&b.occurred_at)));
    let mut merged: Vec<RawEvent> = Vec::with_capacity(kept.len());
    for ev in kept {
        let last = merged.last_mut();
        if let Some(last) = last {
            if last.reset_at == ev.reset_at
                && ev.occurred_at - last.occurred_at <= DEDUP_WINDOW_MS
                && (last.session_id.is_none()
                    || ev.session_id.is_none()
                    || last.session_id == ev.session_id)
            {
                if ev.occurred_at < last.occurred_at {
                    last.occurred_at = ev.occurred_at;
                }
                if last.session_id.is_none() && ev.session_id.is_some() {
                    last.session_id = ev.session_id;
                }
                continue;
            }
        }
        merged.push(ev);
    }
    merged
}

/// workbuddy.db 反查 session_id → user_id（账号 uid）。`home` 用于定位 db，避免依赖全局 env。
fn session_account_map(home: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let path = home.join(".workbuddy").join("workbuddy.db");
    let Ok(conn) = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return map;
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let Ok(mut stmt) = conn.prepare("SELECT id, user_id FROM sessions WHERE deleted_at IS NULL")
    else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        if let (Some(id), Some(uid)) = row {
            if !id.is_empty() && !uid.is_empty() {
                map.insert(id, uid);
            }
        }
    }
    map
}

/// 在 `home/.workbuddy/projects` 下按 cid 索引 jsonl 路径。
fn index_project_jsonls(home: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let projects = home.join(".workbuddy").join("projects");
    if !projects.is_dir() {
        return map;
    }
    let mut stack = vec![projects];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    map.entry(stem.to_string()).or_insert(path);
                }
            }
        }
    }
    map
}

/// 取一个会话 jsonl 最近一条 `providerData.model`（日志无模型名，仅供参考）。
fn session_model(jsonl: &Path) -> Option<String> {
    let Ok(file) = std::fs::File::open(jsonl) else {
        return None;
    };
    let mut last_model: Option<String> = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(model) = value
            .get("providerData")
            .and_then(|d| d.get("model"))
            .or_else(|| value.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            last_model = Some(model.to_string());
        }
    }
    last_model
}

/// 生成限额台账。`days` 为扫描天数（7/30/90 等），None 表示全量。
pub fn get_limits(days: Option<i64>) -> Value {
    get_limits_with_home(&home_dir(), days)
}

/// 与 `get_limits` 同构，但显式传入 home 目录，便于测试隔离（不污染全局 env）。
fn get_limits_with_home(home: &Path, days: Option<i64>) -> Value {
    let generated_at = now_ms();
    let range_days = match days {
        Some(7) => Some(7),
        Some(30) => Some(30),
        Some(90) => Some(90),
        Some(n) if n > 0 => Some(n),
        _ => None,
    };
    let cutoff = range_days.map(|d| generated_at - d * 86_400_000);
    let logs_root = home.join(".workbuddy").join("logs");
    let (raw, files_scanned, parse_errors) = scan_logs(&logs_root, cutoff);
    let events = dedup(raw);

    let account_map = session_account_map(home);
    let jsonl_map = index_project_jsonls(home);
    let mut coverage_start: Option<i64> = None;
    let mut coverage_end: Option<i64> = None;
    let now = generated_at;

    let mut out_events: Vec<Value> = Vec::with_capacity(events.len());
    for ev in events {
        coverage_start = Some(coverage_start.map_or(ev.occurred_at, |c| c.min(ev.occurred_at)));
        coverage_end = Some(coverage_end.map_or(ev.occurred_at, |c| c.max(ev.occurred_at)));
        let session_id = ev.session_id.clone();
        let account_uid = session_id.as_ref().and_then(|s| account_map.get(s)).cloned();
        let model = session_id
            .as_ref()
            .and_then(|s| jsonl_map.get(s))
            .and_then(|p| session_model(p));
        out_events.push(json!({
            "occurredAt": ev.occurred_at,
            "resetAt": ev.reset_at,
            "sessionId": session_id,
            "accountUid": account_uid,
            "model": model,
            "active": ev.reset_at > now,
        }));
    }

    let active: Vec<&Value> = out_events.iter().filter(|e| e["active"].as_bool() == Some(true)).collect();

    json!({
        "generatedAt": generated_at,
        "rangeDays": range_days,
        "events": out_events,
        "activeCount": active.len(),
        "filesScanned": files_scanned,
        "parseErrors": parse_errors,
        "coverageStartAt": coverage_start,
        "coverageEndAt": coverage_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wb-switch-limits-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn parse_reset_at_accepts_zero_padded_local_time() {
        let line = "[2026/9/11 13:00:10.804] [Info] error: 429 您的使用量已超出频率限制，将在 2026-09-11 17:18:36 UTC+8 重置，您也可以切换其他模型继续使用。 (req/sess-1234abcd)";
        let idx = line.find(QUOTA_MARKER).unwrap();
        let ts = parse_reset_at(line, idx).expect("reset timestamp");
        // 2026-09-11 17:18:36 UTC+8 == 2026-09-11 09:18:36 UTC
        let expected = chrono::DateTime::parse_from_rfc3339("2026-09-11T09:18:36Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(ts, expected);
    }

    #[test]
    fn parse_business_line_time_handles_single_digit_date() {
        let ts = parse_business_line_time("[2026/9/11 13:00:10.804]").unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-09-11T05:00:10.804Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(ts, expected);
    }

    #[test]
    fn extract_session_id_from_warning_line() {
        let line = "[2026/9/11 13:00:10.804] [Warning] [ACP Agent] refusal classified: sessionId=ab12cd34-1234-5678-9abc-def012345678, rpcCode=-32003, httpStatus=429, bizCode=6004, category=quota";
        let sid = extract_session_id_after_marker(line, "sessionId=").unwrap();
        assert_eq!(sid, "ab12cd34-1234-5678-9abc-def012345678");
    }

    #[test]
    fn extract_session_id_from_parens_rejects_placeholder() {
        // 占位符 (reqId/sessionId) 不应被当成真实 id。
        let line = "... 重置 ... (reqId/sessionId)";
        assert!(extract_session_id_from_parens(line).is_none());
        let real = "... 重置 ... (req-1/sess-12345678abcdef)";
        assert_eq!(
            extract_session_id_from_parens(real).unwrap(),
            "sess-12345678abcdef"
        );
    }

    #[test]
    fn dedup_merges_business_and_sdk_duplicates_for_same_event() {
        // 同一事件：业务日志一条 + SDK 日志一条（同 sessionId、同 resetAt），应合并为 1 条。
        let reset = now_ms() + 3600_000;
        let raw = vec![
            RawEvent { occurred_at: now_ms() - 10_000, reset_at: reset, session_id: Some("sess-aaaabbbbcccc".into()) },
            RawEvent { occurred_at: now_ms() - 8_000, reset_at: reset, session_id: Some("sess-aaaabbbbcccc".into()) },
        ];
        assert_eq!(dedup(raw).len(), 1);
    }

    #[test]
    fn dedup_second_step_collapses_no_session_records_by_reset_time_window() {
        // 同 resetAt、occurredAt 相差 ≤15s、无 sessionId：第二步应聚成 1 条。
        let reset = now_ms() + 3600_000;
        let base = now_ms();
        let raw = vec![
            RawEvent { occurred_at: base, reset_at: reset, session_id: None },
            RawEvent { occurred_at: base + 5_000, reset_at: reset, session_id: None },
            RawEvent { occurred_at: base + 20_000, reset_at: reset, session_id: None }, // 超出窗口，独立一条
        ];
        let merged = dedup(raw);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn dedup_keeps_distinct_reset_times_separate() {
        let now = now_ms();
        let raw = vec![
            RawEvent { occurred_at: now, reset_at: now + 3600_000, session_id: Some("sess-one".into()) },
            RawEvent { occurred_at: now, reset_at: now + 7200_000, session_id: Some("sess-one".into()) },
        ];
        assert_eq!(dedup(raw).len(), 2);
    }

    #[test]
    fn dedup_does_not_merge_distinct_sessions_sharing_reset_time() {
        // 两个不同会话、重置时间相同（整点网格）、时刻相差 ≤15s：必须保持 2 条独立事件。
        let base = now_ms();
        let reset = base + 3600_000;
        let raw = vec![
            RawEvent { occurred_at: base, reset_at: reset, session_id: Some("sess-aaaaaaaa1111".into()) },
            RawEvent { occurred_at: base + 5_000, reset_at: reset, session_id: Some("sess-bbbbbbbb2222".into()) },
        ];
        let merged = dedup(raw);
        assert_eq!(merged.len(), 2);
        // 无会话 id 的重复行仍可与任一方合并（回填归属）。
        let raw2 = vec![
            RawEvent { occurred_at: base, reset_at: reset, session_id: Some("sess-aaaaaaaa1111".into()) },
            RawEvent { occurred_at: base + 5_000, reset_at: reset, session_id: None },
        ];
        let merged2 = dedup(raw2);
        assert_eq!(merged2.len(), 1);
        assert_eq!(merged2[0].session_id.as_deref(), Some("sess-aaaaaaaa1111"));
    }

    #[test]
    fn get_limits_scans_fixture_logs_and_dedups() {
        let root = temp_root("fixture");
        let date = root.join(".workbuddy/logs/2026-09-11");
        fs::create_dir_all(&date).unwrap();
        // 业务日志：429 行 + 同事件 Warning 行（提供 sessionId）。
        let business = date.join("workspace__hash.log");
        fs::write(
            &business,
            "[2026/9/11 13:00:10.804] [Warning] [ACP Agent] refusal classified: sessionId=ab12cd34-1234-5678-9abc-def012345678, rpcCode=-32003, httpStatus=429, bizCode=6004, category=quota\n\
             [2026/9/11 13:00:10.900] [Info] [Interruption] Catch block entered, error: 429 您的使用量已超出频率限制，将在 2026-09-11 17:18:36 UTC+8 重置，您也可以切换其他模型继续使用。 (req/ab12cd34-1234-5678-9abc-def012345678)\n",
        ).unwrap();
        // SDK 日志：同事件再写一份，文件名即 sessionId，去重后应只剩 1 条。
        let sdk_dir = date.join("sdk/conversations");
        fs::create_dir_all(&sdk_dir).unwrap();
        fs::write(
            sdk_dir.join("ab12cd34-1234-5678-9abc-def012345678.log"),
            "runtime.applyStopReason {\"errorMessageMetaPreview\":\"429 您的使用量已超出频率限制，将在 2026-09-11 17:18:36 UTC+8 重置\"}\n",
        ).unwrap();

        let value = get_limits_with_home(&root, Some(30));

        assert_eq!(value["filesScanned"], 2); // 业务日志 + SDK 日志各 1 个 .log 文件
        let events = value["events"].as_array().unwrap();
        // 业务行 + SDK 行 + Warning 行经两步去重后应合并为 1 条事件。
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["sessionId"], "ab12cd34-1234-5678-9abc-def012345678");
        assert_eq!(events[0]["resetAt"].as_i64().unwrap() > 0, true);
        assert_eq!(events[0]["accountUid"].is_null(), true); // 无 workbuddy.db 时为 null
        assert_eq!(events[0]["model"].is_null(), true); // 无 jsonl 时为 null
        fs::remove_dir_all(root).unwrap();
    }
}
