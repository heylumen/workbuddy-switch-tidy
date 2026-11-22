//! 会话导出（Markdown）。
//!
//! 从 `session.rs` 拆出（4.2 拆分第 1 步，纯搬移、行为不变）。
//! 读取一律流式（`BufRead`）：本机存在 900 MB 级会话，整份读入会把内存翻倍。

use serde_json::{json, Value};

use crate::modules::variant::WbVariant;

use super::{find_project_jsonl, SessionPaths};

// ---------------------------------------------------------------------------
// 会话导出 Markdown（只读）
// ---------------------------------------------------------------------------

/// 去掉客户端注入的内部标签块（如 system-reminder），导出面向人读。
pub(crate) fn strip_internal_tags(text: &str) -> String {
    const OPEN: &str = "<system-reminder";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    // 用户消息信封标签只保留正文。
    out.replace("<user_query>", "")
        .replace("</user_query>", "")
        .trim()
        .to_string()
}

/// 毫秒时间戳 → 本地可读时间（秒级时间戳自动归一为毫秒）。
fn format_ms(value: i64) -> String {
    use chrono::{Local, TimeZone};
    let ms = if value > 0 && value < 10_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    };
    match Local.timestamp_millis_opt(ms).single() {
        Some(time) => time.format("%Y-%m-%d %H:%M").to_string(),
        None => "-".to_string(),
    }
}

/// 单条记录里可读的文本片段（按 content 数组的 text 字段拼接）。
fn message_text(record: &serde_json::Value) -> String {
    if let Some(items) = record.get("content").and_then(|value| value.as_array()) {
        let mut parts: Vec<String> = Vec::new();
        for item in items {
            if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        return strip_internal_tags(&parts.join("\n\n"));
    }
    record
        .get("content")
        .and_then(|value| value.as_str())
        .map(strip_internal_tags)
        .unwrap_or_default()
}

/// 由 jsonl 文本生成 Markdown。`title` 为空时用会话 id 兜底。
///
/// 只保留带可读文本的记录；工具调用、进度等无文本记录会被跳过。输出上限 4 MB，
/// 超出时截断并标注，避免把界面拖死。
#[cfg(test)]
pub(crate) fn markdown_from_jsonl(
    text: &str,
    session_id: &str,
    title: &str,
    uid: &str,
    variant: &str,
) -> MarkdownExport {
    markdown_from_reader(
        std::io::Cursor::new(text.as_bytes()),
        session_id,
        title,
        uid,
        variant,
    )
}

/// 会话导出结果（含读取错误，避免坏行被静默吞掉）。
pub(crate) struct MarkdownExport {
    pub markdown: String,
    pub messages: usize,
    pub records: usize,
    /// 第几条记录开始读不下去（坏行/非 UTF-8）；`None` 表示完整读完。
    pub read_error: Option<String>,
}

/// 流式生成 Markdown：逐行读取，**不把整份正文读进内存**。
///
/// 实测本机存在 900 MB 级会话；若整份 `read_to_string` 再逐行解析，峰值可达 1.5 GB 以上，
/// 而输出上限只有 4 MB —— 完全没必要。这里按行消费，内存只与单行大小相关。
pub(crate) fn markdown_from_reader<R: std::io::BufRead>(
    reader: R,
    session_id: &str,
    title: &str,
    uid: &str,
    variant: &str,
) -> MarkdownExport {
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    let mut out = String::new();
    out.push_str(&format!(
        "# {}\n\n",
        if title.trim().is_empty() {
            session_id
        } else {
            title.trim()
        }
    ));
    out.push_str(&format!(
        "- 会话 ID：{session_id}\n- 账号：{uid}\n- 档位：{variant}\n- 导出时间：{}\n\n---\n\n",
        format_ms(crate::modules::config::now_ms())
    ));
    let mut kept = 0usize;
    let mut total = 0usize;
    let mut truncated = false;
    let mut read_error: Option<String> = None;
    for item in reader.lines() {
        let line = match item {
            Ok(line) => line,
            Err(error) => {
                read_error = Some(format!("第 {} 条记录读取失败：{error}", total + 1));
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total += 1;
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let body = message_text(&record);
        if body.is_empty() {
            continue;
        }
        let role = record
            .get("role")
            .and_then(|value| value.as_str())
            .or_else(|| record.get("type").and_then(|value| value.as_str()))
            .unwrap_or("message");
        let ts = record
            .get("timestamp")
            .and_then(|value| value.as_i64())
            .unwrap_or(0);
        let stamp = if ts > 0 {
            format!(" · {}", format_ms(ts))
        } else {
            String::new()
        };
        let section = format!("### {role}{stamp}\n\n{body}\n\n");
        if out.len() + section.len() > MAX_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&section);
        kept += 1;
    }
    if truncated {
        out.push_str("\n> 内容过大，已截断；完整内容请在客户端查看。\n");
    }
    if let Some(reason) = &read_error {
        out.push_str(&format!(
            "\n> 部分内容读取失败（{reason}），导出可能不完整。\n"
        ));
    }
    MarkdownExport {
        markdown: out,
        messages: kept,
        records: total,
        read_error,
    }
}

/// 文件名安全化：Windows 非法字符/路径分隔符替换为下划线并截断。
///
/// 会话标题常含 `/` `:` 等字符（例如"如何做 A/B 测试"），直接当文件名会让下载失败
/// 或写进子目录，因此统一在 core 侧清洗。
fn safe_file_name(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if (ch as u32) < 0x20 => '_',
            ch => ch,
        })
        .collect();
    let trimmed = cleaned.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    let limited: String = trimmed.chars().take(80).collect();
    if limited.is_empty() {
        "session".to_string()
    } else {
        limited
    }
}

/// 导出指定会话为 Markdown（只读，不改动任何数据）。
pub fn export_session_markdown(
    variant: WbVariant,
    uid: &str,
    session_id: &str,
    title: &str,
) -> Result<Value, String> {
    if session_id.trim().is_empty() {
        return Err("缺少会话 ID".to_string());
    }
    let paths = SessionPaths::for_variant(variant);
    let path = find_project_jsonl(&paths, session_id)
        .ok_or_else(|| "找不到该会话的正文文件".to_string())?;
    let file = std::fs::File::open(&path).map_err(|error| format!("读取会话失败：{error}"))?;
    let export = markdown_from_reader(
        std::io::BufReader::new(file),
        session_id,
        title,
        uid,
        variant.as_str(),
    );
    let read_error = export.read_error.clone();
    let markdown = export.markdown;
    let kept = export.messages;
    let total = export.records;
    Ok(json!({
        "ok": true,
        "markdown": markdown,
        "messages": kept,
        "records": total,
        "bytes": markdown.len(),
        "fileName": format!(
            "{}.md",
            if title.trim().is_empty() {
                safe_file_name(session_id)
            } else {
                safe_file_name(title)
            }
        ),
        "readError": read_error,
    }))
}

#[cfg(test)]
mod file_name_tests {
    #[test]
    fn unsafe_characters_are_replaced() {
        assert_eq!(super::safe_file_name("A/B 测试: v2?"), "A_B 测试_ v2_");
        assert_eq!(super::safe_file_name("..."), "session");
        assert_eq!(super::safe_file_name(""), "session");
        let long = "长".repeat(200);
        assert_eq!(super::safe_file_name(&long).chars().count(), 80);
    }
}
