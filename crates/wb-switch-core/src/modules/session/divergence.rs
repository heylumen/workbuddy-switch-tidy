//! 消息级分叉预览：把「各副本从哪一条消息开始不一样」摊开给人看。
//!
//! 组内成员的内容判定（`fastForward` / `divergent`）只到「前缀/非前缀」这个粒度，
//! 用户真正想知道的是**差异出现在哪几条消息**。本模块逐条抽消息
//! （`role` + 抹掉内部标签的正文）计算哈希序列，求所有成员的**共同前缀**，
//! 再把每个成员「超出共同前缀」的消息返回。
//!
//! 只读；单文件超过 `MAX_FILE_BYTES` 直接返回不可用，避免把 900 MB 会话读进内存。

use std::hash::{Hash, Hasher};

use serde_json::{json, Value};

use crate::modules::variant::WbVariant;

use super::export::strip_internal_tags;
use super::SessionPaths;

/// 一份副本的可对比消息序列：`(哈希, 角色, 正文)`。
type MessageSeq = Vec<(u64, String, String)>;

/// 参与对比的单文件上限。
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// 每条消息返回的最大字符数。
const TEXT_LIMIT: usize = 300;

/// 比较用的正文归一：抹掉逐副本不同的易变片段。
///
/// 实测（本机 7 份副本同组）：差异来自正文里的**带连字符 UUID**，
/// 之前的「按连续字母数字段」归一会被 `-` 切断，4 位小段低于阈值被保留 → 仍不一致。
/// 因此改为**按 token 整体判定**：
/// - 含 `-`、全为十六进制字符、长度 ≥16 → `<id>`（UUID / trace id）
/// - 连续 ≥10 位纯数字 → `<ts>`
/// - 连续 ≥8 位、全十六进制且含数字 → `<id>`
/// - 其余原样保留（避免误伤 `deadbeef` 这类纯字母单词）
fn normalize_for_compare(role: &str, text: &str) -> String {
    fn classify(token: &str) -> Option<&'static str> {
        let all_hex = token.chars().all(|c| c.is_ascii_hexdigit());
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        if token.contains('-') {
            let hex_and_dash = token.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if hex_and_dash && token.len() >= 16 && has_digit {
                return Some("<id>");
            }
            return None;
        }
        if all_hex && token.chars().all(|c| c.is_ascii_digit()) && token.len() >= 10 {
            return Some("<ts>");
        }
        if all_hex && has_digit && token.len() >= 8 {
            return Some("<id>");
        }
        None
    }

    let mut out = String::with_capacity(text.len() + role.len() + 1);
    out.push_str(role);
    out.push('\u{1}');
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        match classify(token) {
            Some(placeholder) => out.push_str(placeholder),
            None => out.push_str(token),
        }
        token.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            token.push(ch);
        } else {
            flush(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// 从 jsonl 抽取 `(hash, role, text)` 序列；无文本的记录（工具调用等）跳过。
fn message_sequence(text: &str) -> Vec<(u64, String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let body = record
            .get("content")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .filter(|joined| !joined.is_empty())
            .or_else(|| {
                record
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            });
        let Some(body) = body else { continue };
        let cleaned = strip_internal_tags(&body);
        if cleaned.is_empty() {
            continue;
        }
        let role = record
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| record.get("type").and_then(Value::as_str))
            .unwrap_or("message")
            .to_string();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        normalize_for_compare(&role, &cleaned).hash(&mut hasher);
        out.push((hasher.finish(), role, cleaned));
    }
    out
}

/// 超大副本的采样：只读开头 `MAX_FILE_BYTES` 字节并复用整份解析。
///
/// 900 MB 级副本不能整份读入；退化为"比对开头若干条"，结果里标 `sampled: true`。
const SAMPLE_MESSAGES: usize = 2000;

fn sample_sequence(path: &std::path::Path) -> Option<Vec<(u64, String, String)>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; MAX_FILE_BYTES as usize];
    let read = file.read(&mut buf).ok()?;
    buf.truncate(read);
    let text = String::from_utf8_lossy(&buf).into_owned();
    // 截断可能切断最后一行：去掉不完整的尾行再解析。
    let text = match text.rfind('\n') {
        Some(index) => text[..index].to_string(),
        None => text,
    };
    let seq = message_sequence(&text);
    if seq.is_empty() {
        None
    } else {
        Some(seq.into_iter().take(SAMPLE_MESSAGES).collect())
    }
}

/// 所有序列的共同前缀长度（按哈希比较）。
fn common_prefix(seqs: &[Vec<u64>]) -> usize {
    let Some(first) = seqs.first() else { return 0 };
    let mut len = first.len();
    for seq in seqs.iter().skip(1) {
        let mut i = 0;
        while i < len && i < seq.len() && seq[i] == first[i] {
            i += 1;
        }
        len = i;
    }
    len
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= TEXT_LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(TEXT_LIMIT).collect();
    format!("{head}…")
}

/// 生成某逻辑会话的消息级分叉预览。
pub fn divergence_preview(
    paths: &SessionPaths,
    variant: WbVariant,
    group_id: &str,
    max_messages: usize,
) -> Value {
    let max_messages = max_messages.clamp(1, 50);
    let Some(members) = crate::modules::session_link::group_members_of(paths, group_id) else {
        return json!({ "ok": false, "reason": "找不到该逻辑会话", "members": [] });
    };
    let active: Vec<_> = members
        .into_iter()
        .filter(|(_, _, state, _)| *state == crate::modules::session_link::MemberState::Active)
        .collect();
    if active.is_empty() {
        return json!({ "ok": false, "reason": "该逻辑会话没有可用副本", "members": [] });
    }

    let mut sequences: Vec<(String, String, MessageSeq)> = Vec::new();
    let mut unavailable: Vec<Value> = Vec::new();
    let mut sampled = false;
    for (uid, session_id, _, member_variant) in active {
        let member_paths = SessionPaths::for_variant(member_variant);
        let Some(file) = super::find_project_jsonl(&member_paths, &session_id) else {
            unavailable.push(json!({ "sessionId": session_id, "uid": uid, "reason": "正文缺失", "variant": member_variant.as_str() }));
            continue;
        };
        let Ok(meta) = std::fs::metadata(&file) else {
            unavailable.push(json!({ "sessionId": session_id, "uid": uid, "reason": "正文不可读", "variant": member_variant.as_str() }));
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            match sample_sequence(&file) {
                Some(seq) => {
                    sampled = true;
                    sequences.push((uid.clone(), session_id.clone(), seq));
                }
                None => unavailable.push(json!({
                    "sessionId": session_id,
                    "uid": uid,
                    "reason": format!("正文 {} MB 且开头无可读消息", meta.len() / 1024 / 1024),
                    "variant": member_variant.as_str(),
                })),
            }
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            unavailable.push(json!({ "sessionId": session_id, "uid": uid, "reason": "正文读取失败", "variant": member_variant.as_str() }));
            continue;
        };
        sequences.push((uid, session_id, message_sequence(&text)));
    }

    if sequences.len() < 2 {
        return json!({
            "ok": true,
            "group_id": group_id,
            "comparable": 0,
            "commonPrefix": 0,
            "members": [],
            "unavailable": unavailable,
            "reason": "可对比的副本不足两个",
        });
    }

    let hashes: Vec<Vec<u64>> = sequences
        .iter()
        .map(|(_, _, seq)| seq.iter().map(|(h, _, _)| *h).collect())
        .collect();
    let prefix = common_prefix(&hashes);

    let members_json: Vec<Value> = sequences
        .iter()
        .map(|(uid, session_id, seq)| {
            let beyond: Vec<Value> = seq
                .iter()
                .skip(prefix)
                .take(max_messages)
                .map(|(_, role, text)| json!({ "role": role, "text": truncate(text) }))
                .collect();
            json!({
                "uid": uid,
                "sessionId": session_id,
                "variant": variant.as_str(),
                "messages": seq.len(),
                "beyondCommon": seq.len().saturating_sub(prefix),
                "items": beyond,
            })
        })
        .collect();

    json!({
        "ok": true,
        "group_id": group_id,
        "comparable": sequences.len(),
        "sampled": sampled,
        "commonPrefix": prefix,
        "members": members_json,
        "unavailable": unavailable,
    })
}

#[cfg(test)]
mod tests {
    use super::{common_prefix, message_sequence};

    #[test]
    fn common_prefix_stops_at_first_difference() {
        let seqs = vec![vec![1u64, 2, 3, 4], vec![1, 2, 9, 4], vec![1, 2, 3]];
        assert_eq!(common_prefix(&seqs), 2);
        assert_eq!(common_prefix(&[]), 0);
        assert_eq!(common_prefix(&[vec![1, 2]]), 2);
    }

    #[test]
    fn message_sequence_skips_tool_calls_and_internal_tags() {
        let jsonl = [
            r#"{"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>你好"}]}"#,
            r#"{"type":"tool_call","content":[]}"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"收到"}]}"#,
        ]
        .join("\n");
        let seq = message_sequence(&jsonl);
        assert_eq!(seq.len(), 2);
        assert_eq!(seq[0].1, "user");
        assert_eq!(seq[0].2, "你好", "内部标签应被剥离");
        assert_eq!(seq[1].1, "assistant");
    }
}

#[cfg(test)]
mod precision_tests {
    use super::normalize_for_compare;

    #[test]
    fn volatile_ids_do_not_break_equality() {
        let a = normalize_for_compare("assistant", "429 请稍后重试 (f74d2c31a9b8)");
        let b = normalize_for_compare("assistant", "429 请稍后重试 (0a1b2c3d4e5f)");
        assert_eq!(a, b, "不同副本的 id 不影响一致性");
        let c = normalize_for_compare("assistant", "429 请稍后重试");
        assert_ne!(a, c, "有无 id 仍是不同文本");
    }

    #[test]
    fn timestamps_are_normalized_but_words_are_kept() {
        assert_eq!(
            normalize_for_compare("user", "重置于 1760000000000"),
            normalize_for_compare("user", "重置于 1799999999999"),
        );
        // 普通英文单词（无数字）不应被当成 id
        let word = normalize_for_compare("user", "deadbeef");
        assert!(word.contains("deadbeef"), "全字母串应原样保留：{word}");
    }
}

#[cfg(test)]
mod uuid_precision_tests {
    use super::normalize_for_compare;

    #[test]
    fn dashed_uuid_is_normalized_as_whole_token() {
        let a = normalize_for_compare("assistant", "完成 8f3e7c1d-d97a-4c80-9b3c-59f704077841");
        let b = normalize_for_compare("assistant", "完成 0d2b3a4c-93c1-4f62-b5b2-72cf36f5d0e1");
        assert_eq!(a, b, "带连字符的 UUID 应整体归一");
        assert!(a.contains("<id>"), "{a}");
    }
}
