//! 本地 WorkBuddy / WorkBuddy 国际版 / CodeBuddy CLI / CodeBuddy IDE Token 统计。
//!
//! 这个模块是统计数据的唯一归属：日志只在这里解码、去重和按时间聚合，
//! Tauri 与 HTTP 层只负责转发结果。响应只包含聚合数字和脱敏标识，不返回
//! 消息正文、arguments 或认证信息。
//!
//! CodeBuddy IDE 不写 JSONL，用量在 `CodeBuddyExtension/Data/**/history/**/index.json`
//! 的 `requests[].usage` 中；消息正文文件（`messages/`）不会被扫描。
//!
//! 国内版与国际版**分开统计**（`workbuddy` / `workbuddy-ai` 两个 source），
//! 不合并：两档位是不同账号体系，混算会让用量无法归因。

use chrono::{Datelike, Local, Timelike};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};

use crate::modules::session::SessionPaths;
use crate::modules::session_link::{self, LinkStore, MemberState, StoreState};
use crate::modules::variant::WbVariant;

/// 明细中最多返回的请求条数（响应窗口）。
const REQUEST_WINDOW: usize = 500;
/// 扫描期间明细缓存的内存硬上限；达到该值后立即裁剪到 `REQUEST_WINDOW`，
/// 使峰值内存恒定，不随历史总量增长。
const REQUEST_COLLECT_LIMIT: usize = 4_000;

/// 单源统计的内存缓存：以文件元数据签名判失效，避免同一进程内反复全量解析。
struct SourceCacheEntry {
    signature: u64,
    value: Value,
    size: usize,
    at: i64,
}

struct SourceCacheState {
    entries: HashMap<String, SourceCacheEntry>,
    order: VecDeque<String>,
    bytes: usize,
}

impl SourceCacheState {
    fn touch(&mut self, key: &str) {
        if let Some(position) = self.order.iter().position(|item| item == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.to_string());
    }

    fn insert(&mut self, key: String, signature: u64, value: Value, size: usize, at: i64) {
        if size > SOURCE_CACHE_MAX_VALUE_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.size);
        }
        if let Some(position) = self.order.iter().position(|item| item == &key) {
            self.order.remove(position);
        }
        while (self.bytes.saturating_add(size) > SOURCE_CACHE_MAX_BYTES
            || self.entries.len() >= SOURCE_CACHE_MAX_ENTRIES)
            && !self.order.is_empty()
        {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(old) = self.entries.remove(&oldest) {
                    self.bytes = self.bytes.saturating_sub(old.size);
                }
            }
        }
        self.bytes = self.bytes.saturating_add(size);
        self.order.push_back(key.clone());
        self.entries.insert(
            key,
            SourceCacheEntry {
                signature,
                value,
                size,
                at,
            },
        );
    }
}

static SOURCE_CACHE: OnceLock<Mutex<SourceCacheState>> = OnceLock::new();

fn source_cache() -> &'static Mutex<SourceCacheState> {
    SOURCE_CACHE.get_or_init(|| {
        Mutex::new(SourceCacheState {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        })
    })
}
/// 统计缓存的内存边界：条目数、单值与总字节预算。
const SOURCE_CACHE_MAX_ENTRIES: usize = 8;
const SOURCE_CACHE_MAX_VALUE_BYTES: usize = 4 * 1024 * 1024;
const SOURCE_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
/// 带解析错误的结果只缓存 60 秒，避免一个坏行永久关闭整源缓存。
const SOURCE_CACHE_ERROR_TTL_MS: i64 = 60_000;

/// 可选诊断：`WB_TOKEN_PROFILE=1` 时输出各来源耗时与计数，不输出路径/正文。
fn token_profile_enabled() -> bool {
    matches!(
        std::env::var("WB_TOKEN_PROFILE").ok().as_deref(),
        Some("1" | "true" | "yes")
    )
}

#[derive(Clone)]
struct CachedTokenRecord {
    timestamp: Option<i64>,
    usage: Usage,
    model: String,
    project: String,
    thinking: u64,
    fingerprint: Option<(i64, u64, u64, u64, u64, String)>,
}

struct CachedTokenFile {
    signature: (u64, u128),
    records: Vec<CachedTokenRecord>,
    title: Option<String>,
    session_project: Option<String>,
    parse_errors: u64,
    /// 含 usage/providerData 元数据但 JSON 损坏的行；这类错误可能隐藏用量，不能据此跳过整文件。
    unsafe_parse_errors: u64,
    max_timestamp: Option<i64>,
}

const TOKEN_FILE_CACHE_MAX_ENTRIES: usize = 32;
const TOKEN_FILE_CACHE_MAX_RECORDS: usize = 100_000;
const TOKEN_FILE_CACHE_DEFAULT_TOTAL_BYTES: usize = 128 * 1024 * 1024;

/// 低内存机器可用 `WB_TOKEN_CACHE_MB` 调整；默认 128MB，限制在 16..512MB。
fn token_file_cache_total_bytes() -> usize {
    static VALUE: OnceLock<usize> = OnceLock::new();
    *VALUE.get_or_init(|| {
        std::env::var("WB_TOKEN_CACHE_MB")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .map(|mb| mb.clamp(16, 512) * 1024 * 1024)
            .unwrap_or(TOKEN_FILE_CACHE_DEFAULT_TOTAL_BYTES)
    })
}

fn token_file_cache_file_bytes() -> usize {
    (token_file_cache_total_bytes() / 4).clamp(4 * 1024 * 1024, 32 * 1024 * 1024)
}

struct TokenFileCacheState {
    entries: HashMap<PathBuf, Arc<CachedTokenFile>>,
    order: VecDeque<PathBuf>,
    bytes: usize,
}

impl TokenFileCacheState {
    fn touch(&mut self, path: &Path) {
        if let Some(position) = self.order.iter().position(|item| item == path) {
            self.order.remove(position);
        }
        self.order.push_back(path.to_path_buf());
    }

    fn insert(&mut self, path: PathBuf, value: Arc<CachedTokenFile>, size: usize) {
        if size > token_file_cache_file_bytes() {
            return;
        }
        if let Some(old) = self.entries.remove(&path) {
            self.bytes = self.bytes.saturating_sub(cached_file_bytes(&old));
        }
        if let Some(position) = self.order.iter().position(|item| item == &path) {
            self.order.remove(position);
        }
        while (self.bytes.saturating_add(size) > token_file_cache_total_bytes()
            || self.entries.len() >= TOKEN_FILE_CACHE_MAX_ENTRIES)
            && !self.order.is_empty()
        {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(old) = self.entries.remove(&oldest) {
                    self.bytes = self.bytes.saturating_sub(cached_file_bytes(&old));
                }
            }
        }
        self.bytes = self.bytes.saturating_add(size);
        self.order.push_back(path.clone());
        self.entries.insert(path, value);
    }
}

fn cached_file_bytes(file: &CachedTokenFile) -> usize {
    let records = file.records.len();
    let strings: usize = file
        .records
        .iter()
        .map(|record| record.model.len() + record.project.len())
        .sum();
    let header = file.title.as_ref().map_or(0, String::len)
        + file.session_project.as_ref().map_or(0, String::len);
    64usize
        .saturating_add(records.saturating_mul(std::mem::size_of::<CachedTokenRecord>()))
        .saturating_add(records.saturating_mul(32))
        .saturating_add(strings)
        .saturating_add(header)
}

static TOKEN_FILE_CACHE: OnceLock<Mutex<TokenFileCacheState>> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
struct FileIndexEntry {
    size: u64,
    mtime_ns: String,
    max_timestamp: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct TokenFileIndex {
    entries: HashMap<String, FileIndexEntry>,
}

static TOKEN_FILE_INDEX: OnceLock<Mutex<TokenFileIndex>> = OnceLock::new();

#[cfg(not(test))]
fn token_file_index_path() -> PathBuf {
    crate::modules::config::store_dir().join("token-file-index.json")
}

fn token_file_index() -> &'static Mutex<TokenFileIndex> {
    TOKEN_FILE_INDEX.get_or_init(|| {
        // 测试不读取/写入真实用户目录，避免污染开发机与并行测试。
        #[cfg(test)]
        {
            Mutex::new(TokenFileIndex::default())
        }
        #[cfg(not(test))]
        {
            let index = std::fs::read_to_string(token_file_index_path())
                .ok()
                .and_then(|text| serde_json::from_str::<TokenFileIndex>(&text).ok())
                .unwrap_or_default();
            Mutex::new(index)
        }
    })
}

fn file_index_should_skip(path: &Path, cutoff: i64) -> bool {
    let Some(signature) = file_signature(path) else {
        return false;
    };
    let key = path.to_string_lossy().to_string();
    let Ok(guard) = token_file_index().lock() else {
        return false;
    };
    let Some(entry) = guard.entries.get(&key) else {
        return false;
    };
    if entry.size != signature.0 || entry.mtime_ns != signature.1.to_string() {
        return false;
    }
    entry.max_timestamp.is_some_and(|max| max < cutoff)
}

fn file_index_update(path: &Path, signature: (u64, u128), max_timestamp: Option<i64>) {
    let Ok(mut guard) = token_file_index().lock() else {
        return;
    };
    guard.entries.insert(
        path.to_string_lossy().to_string(),
        FileIndexEntry {
            size: signature.0,
            mtime_ns: signature.1.to_string(),
            max_timestamp,
        },
    );
}

fn flush_token_file_index() {
    // 测试只验证内存索引，绝不写真实用户缓存文件。
    #[cfg(test)]
    return;
    #[cfg(not(test))]
    {
        let Ok(guard) = token_file_index().lock() else {
            return;
        };
        let Ok(text) = serde_json::to_string(&*guard) else {
            return;
        };
        let path = token_file_index_path();
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        if std::fs::write(&temp, text).is_ok() {
            // 保留上一份索引为 .bak；rename 失败时旧文件仍在，避免崩溃后索引全丢。
            let backup = path.with_extension("json.bak");
            let _ = std::fs::remove_file(&backup);
            let _ = std::fs::rename(&path, &backup);
            if std::fs::rename(&temp, &path).is_err() {
                let _ = std::fs::rename(&backup, &path);
            }
        }
    }
}

fn file_signature(path: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

fn line_has_token_metadata(line: &[u8]) -> bool {
    const NEEDLES: [&[u8]; 4] = [
        b"\"usage\"",
        b"\"providerData\"",
        b"\"aiTitle\"",
        b"\"summary\"",
    ];
    NEEDLES
        .iter()
        .any(|needle| line.windows(needle.len()).any(|window| window == *needle))
}

fn parse_token_file(path: &Path, fallback_project: &str) -> Option<CachedTokenFile> {
    let signature = file_signature(path)?;
    let file = std::fs::File::open(path).ok()?;
    let mut records = Vec::new();
    let mut parse_errors = 0u64;
    let mut unsafe_parse_errors = 0u64;
    let mut ai_title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut session_project: Option<String> = None;
    let mut max_timestamp: Option<i64> = None;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => {
                parse_errors += 1;
                unsafe_parse_errors += 1;
                break;
            }
        }
        // 消息正文/工具输出通常不含 usage 或标题字段；先做字节级过滤，
        // 避免为它们构造完整 JSON DOM。过滤只跳过不可能产出记录的行走。
        if !line_has_token_metadata(&line) {
            // JSON 对象行可以安全跳过；非对象行仍按原语义记为解析错误。
            let first = line.iter().find(|byte| !byte.is_ascii_whitespace());
            if first != Some(&b'{') {
                parse_errors += 1;
            }
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            parse_errors += 1;
            // 该行包含 usage/标题字段但 JSON 损坏，可能隐藏用量记录。
            unsafe_parse_errors += 1;
            continue;
        };
        if let Some(title) = non_empty_text(value.get("aiTitle")) {
            ai_title = Some(title);
        }
        if let Some(value) = non_empty_text(value.get("summary")) {
            summary = Some(value);
        }
        let Some(usage) = usage(&value) else {
            continue;
        };
        let ts = timestamp(&value);
        if let Some(ts) = ts {
            max_timestamp = Some(max_timestamp.map_or(ts, |current| current.max(ts)));
        }
        let model_name = model(&value);
        let project = record_project(&value, fallback_project);
        if session_project.is_none() {
            session_project = Some(project.clone());
        }
        records.push(CachedTokenRecord {
            timestamp: ts,
            usage,
            model: model_name.clone(),
            project,
            thinking: thinking(&value),
            fingerprint: ts.map(|ts| {
                (
                    ts,
                    usage.input,
                    usage.output,
                    usage.read,
                    usage.write,
                    model_name,
                )
            }),
        });
    }
    Some(CachedTokenFile {
        signature,
        records,
        title: ai_title.or(summary),
        session_project,
        parse_errors,
        unsafe_parse_errors,
        max_timestamp,
    })
}

fn load_token_file(path: &Path, fallback_project: &str) -> Option<Arc<CachedTokenFile>> {
    let signature = file_signature(path)?;
    if let Some(cache) = TOKEN_FILE_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            let hit = guard
                .entries
                .get(path)
                .filter(|cached| cached.signature == signature)
                .cloned();
            if let Some(cached) = hit {
                guard.touch(path);
                return Some(cached);
            }
        }
    }
    // 大文件也可能只有少量 usage 记录；不能仅按原始字节数拒绝缓存。
    // 缓存体积由 records 数 + 估算内存控制，而不是原始文件大小。
    let parsed = Arc::new(parse_token_file(path, fallback_project)?);
    if parsed.records.len() <= TOKEN_FILE_CACHE_MAX_RECORDS {
        let size = cached_file_bytes(&parsed);
        if let Some(cache) = TOKEN_FILE_CACHE.get() {
            if let Ok(mut guard) = cache.lock() {
                guard.insert(path.to_path_buf(), parsed.clone(), size);
            }
        }
    }
    Some(parsed)
}

fn paths_signature(paths: &[(PathBuf, PathBuf)]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (root, path) in paths {
        path.hash(&mut hasher);
        root.hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(path) {
            meta.len().hash(&mut hasher);
            if let Ok(modified) = meta.modified() {
                modified.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
}

#[derive(Clone, Debug, Default)]
struct Totals {
    usage: Usage,
    records: u64,
}

#[derive(Clone, Debug)]
struct SessionTotals {
    key: String,
    title: Option<String>,
    project: String,
    session_id: String,
    totals: Totals,
}

/// 展示总量：`input` 已包含 cache reads，因此只追加 output 与 cache write，
/// 避免把缓存命中重复计入。聚合与请求明细共用这一口径。
fn usage_total(usage: Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.output)
        .saturating_add(usage.write)
}

impl Totals {
    fn add(&mut self, usage: Usage) {
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.read = self.usage.read.saturating_add(usage.read);
        self.usage.write = self.usage.write.saturating_add(usage.write);
        self.records = self.records.saturating_add(1);
    }

    fn value(&self) -> Value {
        let cache_hit_rate =
            (self.usage.input > 0).then(|| self.usage.read as f64 / self.usage.input as f64);
        // `input` already includes cache reads; expose the same headline total
        // used by the dashboard without double-counting the cached portion.
        let total = usage_total(self.usage);
        json!({
            "total": total,
            "input": self.usage.input,
            "output": self.usage.output,
            "cacheRead": self.usage.read,
            "cacheWrite": self.usage.write,
            "uncachedInput": self.usage.input.saturating_sub(self.usage.read),
            "records": self.records,
            "cacheHitRate": cache_hit_rate,
        })
    }
}

/// 一次模型调用的明细行。只携带脱敏标识与 token 数字：不含消息正文、
/// 工具参数、绝对路径或认证信息。
#[derive(Clone, Debug)]
struct RequestRow {
    timestamp: i64,
    model: String,
    project: String,
    session_id: String,
    title: Option<String>,
    usage: Usage,
    /// 思考过程 token 数（`output` 的子集），仅用于拆分「思考 / 回复」。
    thinking: u64,
}

impl RequestRow {
    fn value(&self) -> Value {
        json!({
            "timestamp": self.timestamp,
            "model": self.model,
            "project": self.project,
            "sessionId": self.session_id,
            "title": self.title,
            "input": self.usage.input,
            "output": self.usage.output,
            "cacheRead": self.usage.read,
            "cacheWrite": self.usage.write,
            // 与 `Totals::value()` 同一口径：input 已含 cacheRead，
            // 未命中部分即两者之差，下溢时饱和为 0。
            "uncachedInput": self.usage.input.saturating_sub(self.usage.read),
            // 回复内容 = max(0, output - thinking) 由前端做，这里只给原始计数；
            // 实测存在 thinking > output 的异常记录，故前端必须饱和减。
            "thinking": self.thinking,
            "total": usage_total(self.usage),
        })
    }
}

/// Read a non-negative integer from a JSON number or string.
fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n as u64)
        })
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(number))
}

fn positive_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(number).filter(|value| *value > 0))
}

fn cached_input_field(object: &Map<String, Value>) -> u64 {
    // Providers have emitted both flat aliases and OpenAI-compatible nested
    // details. Prefer a positive flat alias so a stale `cache_read...: 0`
    // field cannot hide a populated `prompt_cache_hit_tokens` value.
    positive_field(
        object,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
        ],
    )
    .or_else(|| {
        object
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| positive_field(details, &["cached_tokens"]))
    })
    .or_else(|| {
        object
            .get("inputTokensDetails")
            .and_then(Value::as_array)
            .and_then(|details| {
                details.iter().find_map(|detail| {
                    detail
                        .as_object()
                        .and_then(|detail| positive_field(detail, &["cached_tokens"]))
                })
            })
    })
    .unwrap_or(0)
}

const CACHE_WRITE_KEYS: &[&str] = &[
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cache_creation_input_tokens",
    "prompt_cache_write_tokens",
];

fn usage_fields(object: &Map<String, Value>) -> Usage {
    Usage {
        input: field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0),
        output: field(
            object,
            &["output_tokens", "outputTokens", "completion_tokens"],
        )
        .unwrap_or(0),
        read: cached_input_field(object),
        write: positive_field(object, CACHE_WRITE_KEYS).unwrap_or(0),
    }
}

fn usage_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object().filter(|object| {
        // Input is the required anchor for a usage record. It may legitimately
        // be zero (for example a provider reports output-only retries), so do
        // not use `input > 0` as the validity check.
        field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).is_some()
    })
}

/// Decode one record. Usage precedence is message.usage > providerData.usage >
/// top-level usage. Cache-write metadata may only exist on a non-selected
/// usage object or rawUsage, so those objects are consulted without counting
/// their input/output again.
fn usage(value: &Value) -> Option<Usage> {
    let provider = value.get("providerData");
    let candidates = [
        value
            .get("message")
            .and_then(|message| message.get("usage")),
        provider.and_then(|data| data.get("usage")),
        value.get("usage"),
    ];
    let selected = candidates.iter().copied().find_map(usage_object)?;
    let mut result = usage_fields(selected);

    if result.write == 0 {
        result.write = candidates
            .iter()
            .copied()
            .filter_map(|candidate| candidate.and_then(Value::as_object))
            .chain(
                provider
                    .and_then(|data| data.get("rawUsage"))
                    .and_then(Value::as_object),
            )
            .find_map(|object| positive_field(object, CACHE_WRITE_KEYS))
            .unwrap_or(0);
    }

    // prompt_cache_miss_tokens is deliberately not a write alias: current
    // WorkBuddy/CodeBuddy logs use it for newly computed (uncached) input,
    // while their explicit cache-write fields may legitimately remain zero.

    Some(result)
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .or_else(|| value.get("ts"))
        .and_then(|timestamp| {
            timestamp
                .as_i64()
                .or_else(|| timestamp.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| timestamp.as_str()?.trim().parse::<i64>().ok())
        })
}

fn date(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp)
        .map(|date| date.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

fn hour(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp).map(|date| {
        let local = date.with_timezone(&Local);
        format!(
            "{}-{}",
            local.weekday().num_days_from_monday(),
            local.hour()
        )
    })
}

fn model(value: &Value) -> String {
    value
        .get("providerData")
        .and_then(|data| data.get("model"))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string()
}

/// 思考过程 token 数：优先 `providerData.rawUsage.completion_thinking_tokens`，
/// 回退到 OpenAI 形状的 `completion_tokens_details.reasoning_tokens`，都没有则为 0。
///
/// 只读取计数字段：`providerData.reasoning` 是思考**正文**，属隐私红线，
/// 任何情况下都不得读取或返回。
fn thinking(value: &Value) -> u64 {
    let Some(raw_usage) = value
        .get("providerData")
        .and_then(|data| data.get("rawUsage"))
        .and_then(Value::as_object)
    else {
        return 0;
    };
    field(raw_usage, &["completion_thinking_tokens"])
        .or_else(|| {
            raw_usage
                .get("completion_tokens_details")
                .and_then(Value::as_object)
                .and_then(|details| field(details, &["reasoning_tokens"]))
        })
        .unwrap_or(0)
}

fn files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Subagent logs duplicate parent-session context and are not part
            // of either product's primary usage accounting.
            if path.file_name().and_then(|name| name.to_str()) != Some("subagents") {
                files(&path, output);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            output.push(path);
        }
    }
}

fn project_name(root: &Path, file: &Path) -> String {
    let name = file
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .filter(|name| !name.is_empty() && !name.ends_with(".jsonl"));
    match name {
        // Product directories commonly encode the complete absolute path.
        // Windows uses `c-Users-...`; checking only `Users-` misses that form.
        // A drive-prefixed name is never a safe project label: fall back rather
        // than exposing user names, parent directories, or workspace timestamps.
        Some(name) if !looks_like_encoded_absolute_path(name) => name.to_string(),
        _ => "未知项目".to_string(),
    }
}

fn looks_like_encoded_absolute_path(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let windows_drive = bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'-';
    windows_drive
        || lower.starts_with("users-")
        || lower.starts_with("home-")
        || lower.contains("-users-")
        || lower.contains("-home-")
}

fn record_project(value: &Value, fallback: &str) -> String {
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 120)
        .unwrap_or(fallback)
        .to_string()
}

fn non_empty_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn groups(groups: HashMap<String, Totals>) -> Vec<Value> {
    let mut values: Vec<_> = groups
        .into_iter()
        .map(|(key, totals)| {
            let mut value = totals.value();
            value["key"] = json!(key);
            value
        })
        .collect();
    values.sort_by_key(|right| std::cmp::Reverse(total_value(right)));
    values
}

fn session_groups(sessions: Vec<SessionTotals>) -> Vec<Value> {
    let mut values: Vec<_> = sessions
        .into_iter()
        .map(|session| {
            let mut value = session.totals.value();
            value["key"] = json!(session.key);
            value["title"] = json!(session.title);
            value["project"] = json!(session.project);
            value["sessionId"] = json!(session.session_id);
            value
        })
        .collect();
    values.sort_by_key(|right| std::cmp::Reverse(total_value(right)));
    values
}

fn total_value(value: &Value) -> u64 {
    value
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            value
                .get("input")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(value.get("output").and_then(Value::as_u64).unwrap_or(0))
                .saturating_add(value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0))
        })
}

#[derive(Default)]
struct AccountTotals {
    total: Totals,
    models: HashMap<String, Totals>,
    daily: HashMap<String, Totals>,
}

#[derive(Default)]
struct SourceCollector {
    total: Totals,
    models: HashMap<String, Totals>,
    projects: HashMap<String, Totals>,
    sessions: Vec<SessionTotals>,
    daily: HashMap<String, Totals>,
    daily_by_model: HashMap<String, HashMap<String, Totals>>,
    hours: HashMap<String, Totals>,
    parse_errors: u64,
    coverage_start_at: Option<i64>,
    coverage_end_at: Option<i64>,
    session_key_counts: HashMap<String, usize>,
    requests: Vec<RequestRow>,
    collect_requests: bool,
    accounts: HashMap<String, AccountTotals>,
}

impl SourceCollector {
    fn note_parse_error(&mut self) {
        self.parse_errors = self.parse_errors.saturating_add(1);
    }

    /// Append one request row. Once the in-memory hard limit is exceeded, keep
    /// only the newest window: the sort is stable, so rows sharing a timestamp
    /// keep their collection order and the oldest rows are dropped.
    fn push_request(&mut self, row: RequestRow) {
        if !self.collect_requests {
            return;
        }
        self.requests.push(row);
        if self.requests.len() > REQUEST_COLLECT_LIMIT {
            self.requests
                .sort_by_key(|row| std::cmp::Reverse(row.timestamp));
            self.requests.truncate(REQUEST_WINDOW);
        }
    }

    fn add(&mut self, usage: Usage, value: &Value, project: &str, account: Option<&str>) {
        self.total.add(usage);
        let model_name = model(value);
        self.models
            .entry(model_name.clone())
            .or_default()
            .add(usage);
        self.projects
            .entry(project.to_string())
            .or_default()
            .add(usage);
        if let Some(day) = date(value) {
            self.daily.entry(day.clone()).or_default().add(usage);
            self.daily_by_model
                .entry(model_name.clone())
                .or_default()
                .entry(day)
                .or_default()
                .add(usage);
        }
        if let Some(hour) = hour(value) {
            self.hours.entry(hour).or_default().add(usage);
        }
        if let Some(account) = account.filter(|key| !key.trim().is_empty()) {
            let entry = self.accounts.entry(account.to_string()).or_default();
            entry.total.add(usage);
            entry
                .models
                .entry(model_name.clone())
                .or_default()
                .add(usage);
            if let Some(day) = date(value) {
                entry.daily.entry(day).or_default().add(usage);
            }
        }
        if let Some(timestamp) = timestamp(value) {
            self.coverage_start_at = Some(
                self.coverage_start_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.coverage_end_at = Some(
                self.coverage_end_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
    }

    fn push_session(
        &mut self,
        session_id: String,
        title: Option<String>,
        project: String,
        totals: Totals,
    ) {
        if totals.records == 0 {
            return;
        }
        let base_key = format!("{project} · {session_id}");
        let count = self.session_key_counts.entry(base_key.clone()).or_default();
        *count += 1;
        let key = if *count == 1 {
            base_key
        } else {
            format!("{base_key} · {}", *count)
        };
        self.sessions.push(SessionTotals {
            key,
            title,
            project,
            session_id,
            totals,
        });
    }

    fn into_value(self, name: &str, files_scanned: usize) -> Value {
        self.into_value_with_counts(name, files_scanned, files_scanned, 0)
    }

    fn into_value_with_counts(
        mut self,
        name: &str,
        files_scanned: usize,
        files_parsed: usize,
        files_skipped: usize,
    ) -> Value {
        let daily_by_model = self
            .daily_by_model
            .into_iter()
            .map(|(model, points)| (model, Value::Array(groups(points))))
            .collect::<Map<String, Value>>();
        let mut value = json!({
            "source": name,
            "summary": self.total.value(),
            "models": groups(self.models),
            "projects": groups(self.projects),
            "sessions": session_groups(self.sessions),
            "daily": groups(self.daily),
            "dailyByModel": daily_by_model,
            "hours": groups(self.hours),
            "filesScanned": files_scanned,
            "filesParsed": files_parsed,
            "filesSkipped": files_skipped,
            "parseErrors": self.parse_errors,
            "coverageStartAt": self.coverage_start_at,
            "coverageEndAt": self.coverage_end_at,
            "accounts": account_groups(self.accounts),
        });
        // Only detail sources take the new key; every other source keeps its
        // previous response shape byte for byte.
        if self.collect_requests {
            self.requests
                .sort_by_key(|row| std::cmp::Reverse(row.timestamp));
            self.requests.truncate(REQUEST_WINDOW);
            value["requests"] = Value::Array(self.requests.iter().map(RequestRow::value).collect());
        }
        value
    }
}

fn account_groups(accounts: HashMap<String, AccountTotals>) -> Value {
    let mut items: Vec<Value> = accounts
        .into_iter()
        .map(|(key, value)| {
            json!({
                "key": key,
                "summary": value.total.value(),
                "models": groups(value.models),
                "daily": groups(value.daily),
            })
        })
        .collect();
    items.sort_by(|left, right| {
        let left_total = left
            .get("summary")
            .and_then(|value| value.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let right_total = right
            .get("summary")
            .and_then(|value| value.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        right_total.cmp(&left_total)
    });
    Value::Array(items)
}

/// 从账号库的 DB 读取 session_id → uid 映射；只用于归因，不读取正文。
fn session_account_map(variant: WbVariant) -> HashMap<String, String> {
    let db = variant.data_root().join("workbuddy.db");
    let Ok(conn) = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return HashMap::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT id, user_id FROM sessions") else {
        return HashMap::new();
    };
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    });
    let Ok(rows) = rows else {
        return HashMap::new();
    };
    rows.flatten()
        .filter_map(|(id, uid)| {
            uid.filter(|value| !value.trim().is_empty())
                .map(|uid| (id, uid))
        })
        .collect()
}

fn account_from_path(path: &Path) -> Option<String> {
    path.components().find_map(|component| {
        let name = component.as_os_str().to_str()?;
        name.strip_prefix("user-")
            .and_then(|value| value.strip_suffix("-personal"))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// 单根统计：转发到多根实现，供 CodeBuddy CLI 这类固定单根的数据源使用。
/// `detail` additionally keeps a bounded window of per-call request rows; both
/// outputs share the same decode, cutoff, dedupe, and `subagents`-exclusion
/// pipeline so a detail row can never disagree with the aggregate totals.
#[cfg(test)]
fn source(root: PathBuf, name: &str, cutoff: Option<i64>, detail: bool) -> Value {
    source_from_roots(std::slice::from_ref(&root), name, cutoff, detail)
}

/// 多根合并统计：同一档位下的多个 jsonl 根合并成**一个** source。
///
/// 国际版数据根与国内版不同构（实测无 `projects/`、只有 `sessions/`），因此按
/// 「存在的根」探测；一个都不存在时返回空集且不报错（前端显示空状态）。
#[cfg(test)]
fn source_from_roots(roots: &[PathBuf], name: &str, cutoff: Option<i64>, detail: bool) -> Value {
    source_from_roots_with_accounts(roots, name, cutoff, detail, &HashMap::new(), false)
}

fn source_with_refresh(
    roots: &[PathBuf],
    name: &str,
    cutoff: Option<i64>,
    detail: bool,
    refresh: bool,
) -> Value {
    source_from_roots_with_accounts(roots, name, cutoff, detail, &HashMap::new(), refresh)
}

fn source_from_roots_with_accounts(
    roots: &[PathBuf],
    name: &str,
    cutoff: Option<i64>,
    detail: bool,
    account_by_session: &HashMap<String, String>,
    refresh: bool,
) -> Value {
    let profile_started = std::time::Instant::now();
    let mut paths: Vec<(PathBuf, PathBuf)> = Vec::new();
    for root in roots {
        let mut found = Vec::new();
        files(root, &mut found);
        paths.extend(found.into_iter().map(|file| (root.clone(), file)));
    }
    let mut collector = SourceCollector {
        collect_requests: detail,
        ..SourceCollector::default()
    };

    // Copied/forked sessions replay the parent history (including usage
    // records with their original timestamps) into their own JSONL, so the
    // same request would otherwise be counted once per copy. Process files
    // oldest first and skip fingerprints already seen in an earlier file so
    // usage stays attributed to the original session.
    paths.sort_by_key(|(_, path)| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(u64::MAX)
    });
    // 账号归属也会随切号/复制变化：必须进入缓存键，否则文件签名不变时会复用旧归属。
    let mut account_hasher = DefaultHasher::new();
    let mut account_entries: Vec<_> = account_by_session.iter().collect();
    account_entries.sort_by_key(|(session, _)| *session);
    for (session, uid) in account_entries {
        session.hash(&mut account_hasher);
        uid.hash(&mut account_hasher);
    }
    let cache_key = format!(
        "{name}|{cutoff:?}|{detail}|{roots:?}|{:x}",
        account_hasher.finish()
    );
    let signature = paths_signature(&paths);
    if !refresh {
        if let Ok(mut guard) = source_cache().lock() {
            let now = crate::modules::config::now_ms();
            let cached = guard
                .entries
                .get(&cache_key)
                .filter(|entry| entry.signature == signature)
                .filter(|entry| {
                    let parse_errors = entry
                        .value
                        .get("parseErrors")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    parse_errors == 0 || now.saturating_sub(entry.at) < SOURCE_CACHE_ERROR_TTL_MS
                })
                .map(|entry| entry.value.clone());
            if let Some(value) = cached {
                guard.touch(&cache_key);
                return value;
            }
        }
    }
    let mut seen: HashSet<(i64, u64, u64, u64, u64, String)> = HashSet::new();
    let mut files_parsed = 0usize;
    let mut files_skipped = 0usize;

    struct ScanJob {
        path: PathBuf,
        session_id: String,
        fallback_project: String,
    }

    let mut jobs: Vec<ScanJob> = Vec::new();
    for (root, path) in &paths {
        let session_id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("未知会话")
            .to_string();
        let fallback_project = project_name(root, path);
        if let Some(cutoff) = cutoff {
            if file_index_should_skip(path, cutoff) {
                files_skipped = files_skipped.saturating_add(1);
                continue;
            }
        }
        jobs.push(ScanJob {
            path: path.clone(),
            session_id,
            fallback_project,
        });
    }

    // 解析只读文件可并行；聚合、去重和索引更新仍按原顺序串行执行，保证结果稳定。
    // 有界通道限制乱序结果的驻留内存，避免第一次扫描把所有文件同时留在内存。
    let mut process_cached = |job: &ScanJob, cached: Option<Arc<CachedTokenFile>>| {
        let Some(cached) = cached else {
            collector.note_parse_error();
            return;
        };
        files_parsed = files_parsed.saturating_add(1);
        // 只有“无 usage 元数据的坏行”才允许建索引；含 usage 元数据但 JSON 损坏
        // 的文件可能漏记录，必须每次重扫。
        if cutoff.is_some() && cached.unsafe_parse_errors == 0 {
            file_index_update(&job.path, cached.signature, cached.max_timestamp);
        }
        collector.parse_errors = collector.parse_errors.saturating_add(cached.parse_errors);
        let mut session_totals = Totals::default();
        let mut file_requests: Vec<RequestRow> = Vec::new();
        for record in &cached.records {
            // 时间范围在合并时应用：同一文件可被多个时间窗口复用。
            if cutoff.is_some_and(|minimum| record.timestamp.is_none_or(|ts| ts < minimum)) {
                continue;
            }
            if let Some(fingerprint) = &record.fingerprint {
                if !seen.insert(fingerprint.clone()) {
                    continue;
                }
            }
            session_totals.add(record.usage);
            collector.add(
                record.usage,
                &json!({
                    "timestamp": record.timestamp,
                    "providerData": { "model": record.model },
                }),
                &record.project,
                account_by_session.get(&job.session_id).map(String::as_str),
            );
            if collector.collect_requests {
                if let Some(ts) = record.timestamp {
                    file_requests.push(RequestRow {
                        timestamp: ts,
                        model: record.model.clone(),
                        project: record.project.clone(),
                        session_id: job.session_id.clone(),
                        title: None,
                        usage: record.usage,
                        thinking: record.thinking,
                    });
                }
            }
        }
        let title = cached.title.clone();
        if !file_requests.is_empty() {
            for row in &mut file_requests {
                row.title = title.clone();
            }
            for row in file_requests {
                collector.push_request(row);
            }
        }
        collector.push_session(
            job.session_id.clone(),
            title,
            cached
                .session_project
                .clone()
                .unwrap_or_else(|| job.fallback_project.clone()),
            session_totals,
        );
    };

    let worker_count = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, 4)
        .min(jobs.len().max(1));
    if jobs.len() <= 1 || worker_count <= 1 {
        for job in &jobs {
            process_cached(job, load_token_file(&job.path, &job.fallback_project));
        }
    } else {
        let next_job = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::sync_channel::<(usize, Option<Arc<CachedTokenFile>>)>(
            worker_count.saturating_mul(2),
        );
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let sender = sender.clone();
                let jobs = &jobs;
                let next_job = &next_job;
                scope.spawn(move || loop {
                    let index = next_job.fetch_add(1, Ordering::Relaxed);
                    if index >= jobs.len() {
                        break;
                    }
                    let job = &jobs[index];
                    let cached = load_token_file(&job.path, &job.fallback_project);
                    if sender.send((index, cached)).is_err() {
                        break;
                    }
                });
            }
            drop(sender);
            let mut ready: HashMap<usize, Option<Arc<CachedTokenFile>>> = HashMap::new();
            let mut next_index = 0usize;
            for (index, cached) in receiver {
                ready.insert(index, cached);
                while let Some(cached) = ready.remove(&next_index) {
                    process_cached(&jobs[next_index], cached);
                    next_index += 1;
                }
            }
        });
    }

    if cutoff.is_some() {
        flush_token_file_index();
    }
    let parse_errors = collector.parse_errors;
    let value = collector.into_value_with_counts(name, paths.len(), files_parsed, files_skipped);
    if token_profile_enabled() {
        eprintln!(
            "[token-profile] source={name} files={} parsed={files_parsed} skipped={files_skipped} errors={parse_errors} elapsed_ms={}",
            paths.len(),
            profile_started.elapsed().as_millis()
        );
    }
    // 零错误结果按普通 TTL 长期命中；带解析错误的结果也缓存，但查找时只保留 60 秒，
    // 避免瞬时 IO 失败被固化，同时避免一个坏行让整源每次进入都全扫。
    let size = serde_json::to_vec(&value)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if size > 0 && size <= SOURCE_CACHE_MAX_VALUE_BYTES {
        if let Ok(mut guard) = source_cache().lock() {
            guard.insert(
                cache_key,
                signature,
                value.clone(),
                size,
                crate::modules::config::now_ms(),
            );
        }
    }
    value
}

/// 某档位的候选 jsonl 根（顺序固定；纯函数，便于单测）。
///
/// 国内版保持既有单一 `projects/` 根；国际版两个根都探测（实测没有
/// `projects/`，只有 `sessions/`）。
fn jsonl_root_candidates(variant: WbVariant, root: &Path) -> Vec<PathBuf> {
    match variant {
        WbVariant::Cn => vec![root.join("projects")],
        WbVariant::Ai => vec![root.join("projects"), root.join("sessions")],
    }
}

/// 某档位下实际存在的 jsonl 扫描根；都不存在则返回空列表（空源，不报错）。
fn variant_source_roots(variant: WbVariant) -> Vec<PathBuf> {
    jsonl_root_candidates(variant, &variant.data_root())
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

fn codebuddy_extension_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("CodeBuddyExtension")
        .join("Data")
}

fn is_ide_conversation_index(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("index.json")
        && path
            .parent()
            .and_then(|parent| parent.parent())
            .and_then(|parent| parent.parent())
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            == Some("history")
}

fn ide_index_files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|name| name.to_str());
            // Message bodies contain chat content and must not be scanned.
            // Checkpoints and the shared Public bucket are unrelated to usage.
            if !matches!(
                name,
                Some("messages" | "check-point" | "backups" | "Public")
            ) {
                ide_index_files(&path, output);
            }
        } else if is_ide_conversation_index(&path) {
            output.push(path);
        }
    }
}

fn decode_genie_workspace(name: &str) -> Option<String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let try_decode = |value: &str| -> Option<String> {
        let padded = match value.len() % 4 {
            0 => value.to_string(),
            remainder => format!("{value}{}", "=".repeat(4 - remainder)),
        };
        let bytes = engine.decode(padded).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.contains('\0') {
            return None;
        }
        Some(trimmed.to_string())
    };
    try_decode(name)
        .or_else(|| try_decode(&name.replace('_', "/")))
        .or_else(|| try_decode(&name.replace('_', "+")))
}

fn ide_project_by_session() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(root) = crate::modules::vscode_cn_inject::codebuddy_cn_data_dir().map(|dir| {
        dir.join("User")
            .join("globalStorage")
            .join("tencent-cloud.coding-copilot")
            .join("genie-history")
    }) else {
        return map;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let project = decode_genie_workspace(folder)
            .as_deref()
            .map(Path::new)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.len() <= 120)
            .unwrap_or("未知项目")
            .to_string();
        let Ok(conversations) = std::fs::read_dir(path.join("conversations")) else {
            continue;
        };
        for conversation in conversations.flatten() {
            if let Some(id) = conversation.file_name().to_str() {
                if !id.is_empty() {
                    map.insert(id.to_string(), project.clone());
                }
            }
        }
    }
    map
}

fn ide_workspace_meta(conv_index: &Path, conv_id: &str) -> (Option<String>, String) {
    let Some(ws_index) = conv_index
        .parent()
        .and_then(|parent| parent.parent())
        .map(|parent| parent.join("index.json"))
    else {
        return (None, "未知模型".to_string());
    };
    let Ok(text) = std::fs::read_to_string(ws_index) else {
        return (None, "未知模型".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (None, "未知模型".to_string());
    };
    let Some(conversations) = value.get("conversations").and_then(Value::as_array) else {
        return (None, "未知模型".to_string());
    };
    for conversation in conversations {
        if conversation.get("id").and_then(Value::as_str) != Some(conv_id) {
            continue;
        }
        let title = non_empty_text(conversation.get("name"))
            .or_else(|| non_empty_text(conversation.get("title")));
        let model = non_empty_text(conversation.get("selectedModelId"))
            .or_else(|| non_empty_text(conversation.get("modelId")))
            .or_else(|| non_empty_text(conversation.get("model")))
            .unwrap_or_else(|| "未知模型".to_string());
        return (title, model);
    }
    (None, "未知模型".to_string())
}

fn ide_request_usage(request: &Value) -> Option<Usage> {
    let object = request.get("usage")?.as_object()?;
    field(object, &["inputTokens", "input_tokens", "prompt_tokens"])?;
    Some(Usage {
        input: field(object, &["inputTokens", "input_tokens", "prompt_tokens"]).unwrap_or(0),
        output: field(
            object,
            &["outputTokens", "output_tokens", "completion_tokens"],
        )
        .unwrap_or(0),
        read: field(
            object,
            &[
                "cacheTokens",
                "cacheReadInputTokens",
                "cache_read_input_tokens",
            ],
        )
        .unwrap_or(0),
        write: positive_field(
            object,
            &[
                "cachedWriteTokens",
                "cacheWriteInputTokens",
                "cache_write_input_tokens",
                "cache_creation_input_tokens",
            ],
        )
        .unwrap_or(0),
    })
}

fn ide_request_timestamp(request: &Value) -> Option<i64> {
    timestamp(request).or_else(|| {
        request.get("startedAt").and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        })
    })
}

/// 单条请求的指纹：优先「时间戳 + 用量」，缺时间戳时退回请求 id。
///
/// 副本之间是逐字重放——时间戳与用量都不变——所以这个指纹可以直接当并集去重的键；
/// 两者都没有就返回 `None`（不进并集，按原样计数，实测不存在这种记录）。
fn ide_request_fingerprint(
    request: &Value,
    usage: &Usage,
    timestamp: Option<i64>,
) -> Option<String> {
    Some(match timestamp {
        Some(timestamp) => format!(
            "{timestamp}:{}:{}:{}:{}",
            usage.input, usage.output, usage.read, usage.write
        ),
        None => non_empty_text(request.get("id"))?.to_string(),
    })
}

/// 会话 id：会话目录名。副本会被重随机 id，所以它**不是**会话身份（见 [`ide_file_weight`]）。
fn ide_session_id(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("未知会话")
        .to_string()
}

/// 一份会话 index 的轻量指标：**会话指纹**（首个可计入请求的指纹，同一会话的所有副本
/// 共享它）、可计入的请求条数、token 总量。
///
/// 切号复制会让同一会话在多个账号数据目录下各留一份：副本整份重放父历史，之后各自继续
/// 追加——所以各份长短不一、内容并不完全相同，会话 id 也可能被重随机（插件侧
/// `CopyIdPolicy::AlwaysNew`）。会话身份因此不能按目录名判，只能按「同一个起点请求」判。
/// 指标里的条数/总量用于挑出最完整的那份，作为该会话的标题、模型与项目归属。
fn ide_file_weight(value: &Value) -> Option<(String, usize, u64)> {
    let requests = value.get("requests")?.as_array()?;
    let mut key: Option<String> = None;
    let mut count = 0usize;
    let mut total = 0u64;
    for request in requests {
        let Some(usage) = ide_request_usage(request) else {
            continue;
        };
        if key.is_none() {
            key = ide_request_fingerprint(request, &usage, ide_request_timestamp(request));
        }
        count += 1;
        total = total.saturating_add(usage_total(usage));
    }
    Some((key?, count, total))
}

/// 把一个会话（`requests` 是它的某一份副本）里可计入的请求并入统计。
///
/// `seen` 按请求指纹去重、跨副本共享：副本重放父历史时带的是原始时间戳，所以并集
/// 既不会把历史日期重复抬高，也不会丢掉各副本复制后各自追加的请求。指纹缺失的记录
/// 一律计入。
#[allow(clippy::too_many_arguments)]
fn collect_ide_requests(
    collector: &mut SourceCollector,
    requests: &[Value],
    cutoff: Option<i64>,
    model_name: &str,
    project: &str,
    account: Option<&str>,
    seen: &mut HashSet<String>,
    session_totals: &mut Totals,
) {
    for request in requests {
        let ts = ide_request_timestamp(request);
        if cutoff.is_some_and(|minimum| ts.is_none_or(|value| value < minimum)) {
            continue;
        }
        let Some(usage) = ide_request_usage(request) else {
            continue;
        };
        if let Some(fingerprint) = ide_request_fingerprint(request, &usage, ts) {
            // 同一请求已在另一份副本里计过。
            if !seen.insert(fingerprint) {
                continue;
            }
        }
        session_totals.add(usage);
        collector.add(
            usage,
            &json!({
                "timestamp": ts,
                "providerData": { "model": model_name },
            }),
            project,
            account,
        );
    }
}

fn ide_source(
    root: PathBuf,
    name: &str,
    cutoff: Option<i64>,
    project_by_session: &HashMap<String, String>,
) -> Value {
    let mut paths = Vec::new();
    ide_index_files(&root, &mut paths);
    paths.sort();

    // 同一会话（按起点请求判身份）因切号复制可能散落在多个账号数据目录里。各份之间按
    // 请求指纹取**并集**：父历史只算一次（副本会把历史日期回溯性抬高），副本复制后各自
    // 追加的请求也都保留（只取「最长一份」会漏掉别的副本独有的请求）。会话标题、模型与
    // 项目归属取最完整的那份。
    // 峰值内存优先：权重计算逐文件读取后立即释放，不把全部 index.json 同时留在内存。
    // 代价是后续并入请求时再读一次；IDE index 通常不大，换取更低的峰值更划算。
    let weights: Vec<Option<(String, usize, u64)>> = paths
        .iter()
        .map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .as_ref()
                .and_then(ide_file_weight)
        })
        .collect();
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut lead: HashMap<&str, (usize, usize, u64)> = HashMap::new();
    for (index, weight) in weights.iter().enumerate() {
        let Some((fingerprint, count, total)) = weight else {
            continue;
        };
        groups.entry(fingerprint.as_str()).or_default().push(index);
        let replace = lead
            .get(fingerprint.as_str())
            .is_none_or(|current| (*count, *total) > (current.1, current.2));
        if replace {
            lead.insert(fingerprint.as_str(), (index, *count, *total));
        }
    }

    let mut collector = SourceCollector::default();

    for (index, _) in paths.iter().enumerate() {
        let fingerprint = weights[index]
            .as_ref()
            .map(|(fingerprint, _, _)| fingerprint.as_str());
        // 同一会话只在它的第一份副本处处理一次，并把其余副本一起并入。读不出会话指纹
        // （读取/解析失败）的文件照旧单独走一遍，让解析错误被记一笔。
        if let Some(fingerprint) = fingerprint {
            if groups.get(fingerprint).and_then(|members| members.first()) != Some(&index) {
                continue;
            }
        }
        // 标题/模型/项目取最完整的那份：副本 id 被重随机过，元信息不一定都齐全。
        let lead_index = fingerprint
            .and_then(|fingerprint| lead.get(fingerprint).map(|current| current.0))
            .unwrap_or(index);
        let lead_session_id = ide_session_id(&paths[lead_index]);
        let (title, model_name) = ide_workspace_meta(&paths[lead_index], &lead_session_id);
        let project = project_by_session
            .get(&lead_session_id)
            .cloned()
            .unwrap_or_else(|| "未知项目".to_string());
        let members: &[usize] = match fingerprint {
            Some(fingerprint) => groups
                .get(fingerprint)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            None => std::slice::from_ref(&index),
        };
        let mut session_totals = Totals::default();
        let mut seen: HashSet<String> = HashSet::new();
        for member in members {
            let Ok(text) = std::fs::read_to_string(&paths[*member]) else {
                collector.note_parse_error();
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                collector.note_parse_error();
                continue;
            };
            let Some(requests) = value.get("requests").and_then(Value::as_array) else {
                continue;
            };
            collect_ide_requests(
                &mut collector,
                requests,
                cutoff,
                &model_name,
                &project,
                account_from_path(&paths[*member]).as_deref(),
                &mut seen,
                &mut session_totals,
            );
        }

        collector.push_session(lead_session_id, title, project, session_totals);
    }

    collector.into_value(name, paths.len())
}

/// Return independent WorkBuddy (CN), WorkBuddy AI, CodeBuddy CLI, and
/// CodeBuddy IDE aggregates.
/// `days` is interpreted in Rust using the same millisecond clock for every source.
/// 关联组内最早已关联的 active 成员视为规范副本；只用于把已关联副本归到同一账号。
/// 未关联副本仍走文件级启发式，不能宣称请求级归因。
fn canonical_accounts_from_store(store: &LinkStore, variant: WbVariant) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for group in &store.groups {
        let canonical = group
            .members
            .iter()
            .filter(|member| member.state == MemberState::Active)
            .min_by_key(|member| (member.linked_at, member.member_id.clone()))
            .or_else(|| {
                group
                    .members
                    .iter()
                    .min_by_key(|member| (member.linked_at, member.member_id.clone()))
            });
        let Some(canonical) = canonical else {
            continue;
        };
        if canonical.uid.trim().is_empty() {
            continue;
        }
        for member in &group.members {
            let member_variant = session_link::member_variant(group, member);
            if member_variant == variant
                && !member.session_id.trim().is_empty()
                && !member.uid.trim().is_empty()
            {
                map.insert(member.session_id.clone(), canonical.uid.clone());
            }
        }
    }
    map
}

fn canonical_accounts_by_session(variant: WbVariant) -> HashMap<String, String> {
    let paths = SessionPaths::for_variant(variant);
    match session_link::load_store(&paths) {
        StoreState::Ready(store) => canonical_accounts_from_store(&store, variant),
        StoreState::Missing | StoreState::Unavailable(_) => HashMap::new(),
    }
}

/// 只返回 CodeBuddy CLI 的请求明细，避免打开明细时重算四个来源。
pub fn get_requests(days: Option<i64>, refresh: bool) -> Value {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let generated_at = crate::modules::config::now_ms();
    let range_days = match days {
        Some(1) => Some(1),
        Some(7) => Some(7),
        Some(30) => Some(30),
        Some(90) => Some(90),
        _ => None,
    };
    let cutoff = range_days.map(|value| generated_at - value * 86_400_000);
    let source = source_with_refresh(
        std::slice::from_ref(&home.join(".codebuddy/projects")),
        "codebuddy-cli",
        cutoff,
        true,
        refresh,
    );
    json!({
        "generatedAt": generated_at,
        "rangeDays": range_days,
        "requests": source.get("requests").cloned().unwrap_or_else(|| json!([])),
        "filesScanned": source.get("filesScanned").cloned().unwrap_or(json!(0)),
        "parseErrors": source.get("parseErrors").cloned().unwrap_or(json!(0)),
    })
}

pub fn get_statistics(days: Option<i64>, include_requests: bool, refresh: bool) -> Value {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let generated_at = crate::modules::config::now_ms();
    let range_days = match days {
        Some(1) => Some(1),
        Some(7) => Some(7),
        Some(30) => Some(30),
        Some(90) => Some(90),
        _ => None,
    };
    let cutoff = range_days.map(|value| generated_at - value * 86_400_000);
    let ide_projects = ide_project_by_session();
    let mut cn_accounts = session_account_map(WbVariant::Cn);
    cn_accounts.extend(canonical_accounts_by_session(WbVariant::Cn));
    let mut ai_accounts = session_account_map(WbVariant::Ai);
    ai_accounts.extend(canonical_accounts_by_session(WbVariant::Ai));
    json!({
        "generatedAt": generated_at,
        "rangeDays": range_days,
        "sources": [
            source_from_roots_with_accounts(
                &variant_source_roots(WbVariant::Cn),
                "workbuddy",
                cutoff,
                false,
                &cn_accounts,
                refresh,
            ),
            // 国际版独立 source：不与国内版混算（不同账号体系）。
            source_from_roots_with_accounts(
                &variant_source_roots(WbVariant::Ai),
                "workbuddy-ai",
                cutoff,
                false,
                &ai_accounts,
                refresh,
            ),
            // 请求明细目前只对 CodeBuddy CLI 开放（见 spec 的 requests 契约）。
            source_with_refresh(
                std::slice::from_ref(&home.join(".codebuddy/projects")),
                "codebuddy-cli",
                cutoff,
                include_requests,
                refresh,
            ),
            ide_source(
                codebuddy_extension_data_dir(),
                "codebuddy-ide",
                cutoff,
                &ide_projects,
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 固定 fixture 的 mtime，让「原始会话先于副本被处理」的断言不依赖目录遍历顺序。
    ///
    /// Windows 上 `File::set_modified` 需要 `FILE_WRITE_ATTRIBUTES`，而 `File::open`
    /// 只申请 `GENERIC_READ`——只读句柄改时间会得到 `os error 5`，故用可写句柄打开。
    fn pin_mtime(path: std::path::PathBuf, mtime: std::time::SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open fixture for mtime")
            .set_modified(mtime)
            .expect("pin fixture mtime");
    }

    #[test]
    fn usage_priority_aliases_and_raw_cache_write() {
        let value = json!({
            "providerData": {
                "usage": { "inputTokens": 99, "outputTokens": 22 },
                "rawUsage": { "prompt_cache_write_tokens": 2 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(
            usage(&value),
            Some(Usage {
                input: 10,
                output: 3,
                read: 4,
                write: 2
            })
        );
    }

    #[test]
    fn cache_write_uses_explicit_aliases_but_never_cache_miss() {
        let provider_usage_write = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 22,
                    "cache_write_input_tokens": 0,
                    "cache_creation_input_tokens": 7
                },
                "rawUsage": {
                    "prompt_cache_miss_tokens": 91,
                    "prompt_cache_write_tokens": 0
                }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(
            usage(&provider_usage_write),
            Some(Usage {
                input: 10,
                output: 3,
                read: 4,
                write: 7,
            })
        );

        let cache_miss_only = json!({
            "providerData": {
                "rawUsage": { "prompt_cache_miss_tokens": 91 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3
            }}
        });
        assert_eq!(
            usage(&cache_miss_only),
            Some(Usage {
                input: 10,
                output: 3,
                read: 0,
                write: 0,
            })
        );
    }

    #[test]
    fn cache_read_accepts_nested_provider_details() {
        let value = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 3,
                    "inputTokensDetails": [{ "cached_tokens": 7 }]
                }
            }
        });
        assert_eq!(
            usage(&value),
            Some(Usage {
                input: 99,
                output: 3,
                read: 7,
                write: 0,
            })
        );

        let raw = json!({
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 2,
                "cache_read_input_tokens": 0,
                "prompt_cache_hit_tokens": 12
            }
        });
        assert_eq!(
            usage(&raw),
            Some(Usage {
                input: 20,
                output: 2,
                read: 12,
                write: 0,
            })
        );
    }

    #[test]
    fn source_per_file_cache_invalidates_when_file_changes() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-incremental-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |ts, input| {
            json!({
                "timestamp": ts,
                "cwd": "/fixture/example-project",
                "providerData": {
                    "model": "fixture-model",
                    "usage": { "inputTokens": input, "outputTokens": 1 }
                }
            })
        };
        let file = project.join("session.jsonl");
        fs::write(&file, format!("{}\n", record(now, 10))).expect("write first fixture");

        let first = source(root.clone(), "fixture", None, false);
        assert_eq!(first["summary"]["input"], 10);
        assert_eq!(first["summary"]["records"], 1);

        // 文件变化（size 改变）必须使缓存失效；未变化文件的复用由缓存实现保证。
        fs::write(
            &file,
            format!("{}\n{}\n", record(now, 10), record(now + 1, 7)),
        )
        .expect("append fixture");
        let second = source(root.clone(), "fixture", None, false);
        assert_eq!(second["summary"]["input"], 17);
        assert_eq!(second["summary"]["records"], 2);
        assert_eq!(second["parseErrors"], 0);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn source_excludes_subagents_and_counts_each_record_once() {
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        let project = root.join("fixture-project");
        let ignored = project.join("subagents");
        fs::create_dir_all(&ignored).expect("create fixture dirs");
        let record = json!({
            "timestamp": crate::modules::config::now_ms(),
            "providerData": {
                "model": "fixture-model",
                "usage": { "inputTokens": 20, "outputTokens": 5 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        fs::write(
            project.join("session.jsonl"),
            format!("{}\nnot-json\n", record),
        )
        .expect("write fixture");
        fs::write(ignored.join("agent.jsonl"), format!("{}\n", record))
            .expect("write ignored fixture");

        let result = source(root.clone(), "fixture", None, false);
        assert_eq!(result["filesScanned"], 1);
        assert_eq!(result["parseErrors"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["summary"]["output"], 3);
        assert_eq!(result["summary"]["cacheRead"], 4);
        assert_eq!(result["summary"]["total"], 13);
        assert_eq!(result["summary"]["records"], 1);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(result["dailyByModel"]["fixture-model"][0]["key"], today);
        assert_eq!(result["projects"][0]["key"], "fixture-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn source_deduplicates_copied_session_history() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-fork-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = json!({
            "timestamp": now,
            "cwd": "/fixture/example-project",
            "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
        });
        // The copied session replays the same usage record under a new file.
        fs::write(
            project.join("session-original.jsonl"),
            format!("{}\n", record),
        )
        .expect("write original fixture");
        fs::write(
            project.join("session-forked.jsonl"),
            format!(
                "{}\n{}\n",
                record,
                json!({
                    "timestamp": now + 1_000,
                    "message": { "usage": { "input_tokens": 7, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write forked fixture");
        // Back-to-back writes can land in the same millisecond, and the
        // millisecond mtime key leaves ordering to the filesystem's directory
        // iteration, which is not sorted. The copy would then be processed
        // first, own the replayed record, and leave the original with zero
        // records. Pin the mtimes so the original always precedes its copy.
        pin_mtime(
            project.join("session-original.jsonl"),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        pin_mtime(
            project.join("session-forked.jsonl"),
            std::time::SystemTime::now(),
        );

        let result = source(root.clone(), "fixture", None, false);
        // The replayed record counts once; the fork's new record still counts.
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 17);
        assert_eq!(result["summary"]["output"], 5);
        let sessions = result["sessions"].as_array().expect("session groups");
        assert_eq!(sessions.len(), 2);
        let forked = sessions
            .iter()
            .find(|session| session["sessionId"] == "session-forked")
            .expect("forked session kept its new record");
        assert_eq!(forked["input"], 7);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn bounded_source_excludes_records_before_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-range-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp| {
            json!({
                "timestamp": timestamp,
                "cwd": "/fixture/example-project",
                "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } }
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!("{}\n{}\n", record(now - 10_000), record(now - 100_000)),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000), false);
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["projects"][0]["key"], "example-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn session_titles_are_file_scoped_and_independent_from_usage_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-titles-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let usage_record = |input| {
            json!({
                "timestamp": now,
                "cwd": "/private/example-project",
                "message": { "usage": { "input_tokens": input, "output_tokens": 2 } }
            })
        };

        fs::write(
            project.join("session-a.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                json!({ "type": "summary", "summary": "摘要不应覆盖 AI 标题" }),
                json!({ "type": "ai-title", "aiTitle": "旧标题" }),
                usage_record(10),
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
            ),
        )
        .expect("write ai title fixture");
        fs::write(
            project.join("session-b.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "type": "ai-title",
                    "timestamp": now - 100_000,
                    "aiTitle": "范围外保留标题"
                }),
                usage_record(20),
            ),
        )
        .expect("write cutoff title fixture");
        fs::write(
            project.join("session-c.jsonl"),
            format!(
                "{}\n{}\n",
                usage_record(30),
                json!({ "type": "summary", "summary": "摘要回退标题" }),
            ),
        )
        .expect("write summary fixture");
        fs::write(
            project.join("session-d.jsonl"),
            format!("{}\n", usage_record(40)),
        )
        .expect("write untitled fixture");
        fs::write(
            project.join("session-e.jsonl"),
            format!(
                "{}\n{}\n",
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
                usage_record(50),
            ),
        )
        .expect("write duplicate title fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000), false);
        let sessions = result["sessions"].as_array().expect("session groups");
        let by_id = |session_id: &str| {
            sessions
                .iter()
                .find(|session| session["sessionId"] == session_id)
                .expect("session group by id")
        };

        assert_eq!(result["summary"]["input"], 150);
        assert_eq!(result["summary"]["records"], 5);
        assert_eq!(sessions.len(), 5);
        assert_eq!(by_id("session-a")["title"], "最新 AI 标题");
        assert_eq!(by_id("session-b")["title"], "范围外保留标题");
        assert_eq!(by_id("session-c")["title"], "摘要回退标题");
        assert!(by_id("session-d")["title"].is_null());
        assert_eq!(by_id("session-a")["project"], "example-project");
        assert_ne!(by_id("session-a")["key"], by_id("session-e")["key"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_key_only_present_for_detail_sources() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-key-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
                })
            ),
        )
        .expect("write fixture");

        let aggregate_only = source(root.clone(), "workbuddy", None, false);
        assert!(aggregate_only.get("requests").is_none());

        let detailed = source(root.clone(), "codebuddy-cli", None, true);
        let rows = detailed["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        // 明细只是附加键，聚合数值与不采集明细时完全一致。
        assert_eq!(detailed["summary"], aggregate_only["summary"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_rows_share_the_total_formula_and_sort_newest_first() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-rows-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp, input, write| {
            json!({
                "timestamp": timestamp,
                "cwd": "/private/example-project",
                "providerData": { "model": "fixture-model" },
                "message": { "usage": {
                    "input_tokens": input,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 5,
                    "cache_write_input_tokens": write
                }}
            })
        };
        // 文件顺序刻意与时间顺序相反，排序必须来自 timestamp。
        fs::write(
            project.join("session-detail.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                record(now - 1_000, 10, 1),
                record(now - 2_000, 20, 2),
                record(now - 3_000, 30, 3),
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "codebuddy-cli", None, true);
        assert_eq!(result["summary"]["records"], 3);
        // input 60 + output 6 + cacheWrite 6；cacheRead 不计入合计。
        assert_eq!(result["summary"]["total"], 72);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["timestamp"], now - 1_000);
        assert_eq!(rows[2]["timestamp"], now - 3_000);
        assert_eq!(rows[0]["model"], "fixture-model");
        // 只暴露 cwd 的 basename，不返回绝对路径。
        assert_eq!(rows[0]["project"], "example-project");
        assert_eq!(rows[0]["sessionId"], "session-detail");
        assert!(rows[0]["title"].is_null());
        assert_eq!(rows[0]["input"], 10);
        assert_eq!(rows[0]["output"], 2);
        assert_eq!(rows[0]["cacheRead"], 5);
        assert_eq!(rows[0]["cacheWrite"], 1);
        assert_eq!(rows[0]["total"], 13);
        // 缓存未命中 = input - cacheRead，与 `summary` 同口径。
        assert_eq!(rows[0]["uncachedInput"], 5);
        // 无 providerData.rawUsage 时思考过程为 0。
        assert_eq!(rows[0]["thinking"], 0);
        let row_total: u64 = rows
            .iter()
            .map(|row| row["total"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(row_total, result["summary"]["total"].as_u64().unwrap_or(0));
        let row_uncached: u64 = rows
            .iter()
            .map(|row| row["uncachedInput"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(
            row_uncached,
            result["summary"]["uncachedInput"].as_u64().unwrap_or(0)
        );

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_window_keeps_only_the_newest_rows() {
        let row = |timestamp| RequestRow {
            timestamp,
            model: "fixture-model".to_string(),
            project: "fixture-project".to_string(),
            session_id: "fixture-session".to_string(),
            title: None,
            usage: Usage {
                input: 1,
                output: 1,
                read: 0,
                write: 0,
            },
            thinking: 0,
        };

        // 触达内存硬上限后只保留最新窗口，且按时间倒序。
        let mut collector = SourceCollector {
            collect_requests: true,
            ..SourceCollector::default()
        };
        for timestamp in 0..=(REQUEST_COLLECT_LIMIT as i64) {
            collector.push_request(row(timestamp));
        }
        assert_eq!(collector.requests.len(), REQUEST_WINDOW);
        assert_eq!(
            collector.requests[0].timestamp,
            REQUEST_COLLECT_LIMIT as i64
        );

        // 未触达硬上限时，输出窗口同样截断到最新 REQUEST_WINDOW 条。
        let mut bounded = SourceCollector {
            collect_requests: true,
            ..SourceCollector::default()
        };
        for timestamp in 0..(REQUEST_WINDOW as i64 + 100) {
            bounded.push_request(row(timestamp));
        }
        let value = bounded.into_value("fixture", 1);
        let rows = value["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), REQUEST_WINDOW);
        assert_eq!(rows[0]["timestamp"], REQUEST_WINDOW as i64 + 99);
        assert_eq!(rows[REQUEST_WINDOW - 1]["timestamp"], 100);

        // 关闭明细采集时不缓存任何行，也不输出 requests 键。
        let mut off = SourceCollector::default();
        off.push_request(row(1));
        assert!(off.into_value("fixture", 1).get("requests").is_none());
    }

    #[test]
    fn request_detail_window_stays_the_global_newest_after_any_arrival_order() {
        let row = |timestamp| RequestRow {
            timestamp,
            model: "fixture-model".to_string(),
            project: "fixture-project".to_string(),
            session_id: "fixture-session".to_string(),
            title: None,
            usage: Usage {
                input: 1,
                output: 1,
                read: 0,
                write: 0,
            },
            thinking: 0,
        };

        // 文件按 mtime 顺序处理、行内时间戳可以乱序：中途裁剪过之后，后到的
        // 旧行不能占位，后到的新行仍须进入窗口 —— 结果恒等于「全局最新 500」。
        let mut collector = SourceCollector {
            collect_requests: true,
            ..SourceCollector::default()
        };
        for timestamp in 0..=(REQUEST_COLLECT_LIMIT as i64) {
            collector.push_request(row(timestamp));
        }
        collector.push_request(row(-1));
        for timestamp in 10_000..10_100 {
            collector.push_request(row(timestamp));
        }

        let value = collector.into_value("fixture", 1);
        let rows = value["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), REQUEST_WINDOW);
        assert_eq!(rows[0]["timestamp"], 10_099);
        assert_eq!(rows[99]["timestamp"], 10_000);
        // 裁剪后残留的旧行必须让位：第 101 条是最新一轮保留里的最新一行。
        assert_eq!(rows[100]["timestamp"], REQUEST_COLLECT_LIMIT as i64);
        // 全局最新 500 = 100 条新行 + 上一轮保留里最新的 400 条（4000..=3601）。
        assert_eq!(rows[REQUEST_WINDOW - 1]["timestamp"], 3_601);
        assert!(rows.iter().all(|row| row["timestamp"] != -1));
    }

    #[test]
    fn request_detail_deduplicates_copied_session_history() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-fork-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = json!({
            "timestamp": now,
            "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
        });
        fs::write(
            project.join("session-original.jsonl"),
            format!("{}\n", record),
        )
        .expect("write original fixture");
        fs::write(
            project.join("session-forked.jsonl"),
            format!(
                "{}\n{}\n",
                record,
                json!({
                    "timestamp": now + 1_000,
                    "message": { "usage": { "input_tokens": 7, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write forked fixture");
        // 与聚合去重用例相同：固定 mtime，保证原始会话先于副本被处理。
        pin_mtime(
            project.join("session-original.jsonl"),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        pin_mtime(
            project.join("session-forked.jsonl"),
            std::time::SystemTime::now(),
        );

        let result = source(root.clone(), "fixture", None, true);
        assert_eq!(result["summary"]["records"], 2);
        let rows = result["requests"].as_array().expect("request rows");
        // 复制会话重放的记录沿用同一指纹，明细里只出现一次。
        assert_eq!(rows.len(), 2);
        let inputs: Vec<u64> = rows
            .iter()
            .filter_map(|row| row["input"].as_u64())
            .collect();
        assert_eq!(inputs, [7, 10]);
        assert_eq!(rows[0]["sessionId"], "session-forked");
        assert_eq!(rows[1]["sessionId"], "session-original");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_skips_records_without_timestamp() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-undated-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
                }),
                // 没有 timestamp 的记录仍然计入聚合，但无法排序，不进明细。
                json!({ "message": { "usage": { "input_tokens": 90, "output_tokens": 9 } } }),
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 100);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["input"], 10);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_backfills_file_title_after_usage() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-title-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp| {
            json!({
                "timestamp": timestamp,
                "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                json!({ "type": "summary", "summary": "摘要回退标题" }),
                record(now - 1_000),
                // aiTitle 出现在 usage 之后：文件读完后必须回填到所有明细行。
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "最新 AI 标题");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_thinking_prefers_completion_thinking_tokens() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-thinking-priority-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 40 } },
                    // 两个来源同时存在时取扁平的 completion_thinking_tokens。
                    "providerData": { "rawUsage": {
                        "completion_thinking_tokens": 7,
                        "completion_tokens_details": { "reasoning_tokens": 3 }
                    }}
                })
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["thinking"], 7);
        assert_eq!(rows[0]["output"], 40);
        // 思考过程只是 output 的拆分，不改变合计口径。
        assert_eq!(rows[0]["total"], result["summary"]["total"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_thinking_falls_back_to_reasoning_tokens_and_defaults_to_zero() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-thinking-fallback-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 11, "output_tokens": 21 } },
                    // rawUsage 只有思考计数字段时取 OpenAI 形状的 reasoning_tokens。
                    "providerData": { "rawUsage": {
                        "completion_tokens_details": { "reasoning_tokens": 5 }
                    }}
                }),
                json!({
                    "timestamp": now - 1_000,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 20 } },
                    // rawUsage 缺少思考计数 -> 0；`reasoning` 是思考正文，
                    // 只用来验证它不会被当成计数读取（隐私红线）。
                    "providerData": {
                        "reasoning": "思考正文不得进入明细",
                        "rawUsage": { "prompt_cache_write_tokens": 1 }
                    }
                })
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["thinking"], 5);
        assert_eq!(rows[1]["thinking"], 0);
        assert_eq!(rows[1]["cacheWrite"], 1);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_thinking_over_output_saturates_reply_to_zero() {
        // 实测存在 thinking > output 的异常记录：后端如实返回两个原始计数，
        // 前端的 `output.saturating_sub(thinking)` 必须得到 0 而不是负数。
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-thinking-overflow-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } },
                    "providerData": { "rawUsage": { "completion_thinking_tokens": 9 } }
                })
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["output"], 2);
        assert_eq!(rows[0]["thinking"], 9);
        let reply = rows[0]["output"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(rows[0]["thinking"].as_u64().unwrap_or(0));
        assert_eq!(reply, 0);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn ide_source_reads_request_usage_and_skips_message_bodies() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-ide-{}-{now}",
            std::process::id()
        ));
        let history = root
            .join("uid")
            .join("CodeBuddyIDE")
            .join("uid")
            .join("history")
            .join("workspace-hash");
        let conv_a = history.join("conv-a");
        let conv_b = history.join("conv-b");
        let messages = conv_a.join("messages");
        fs::create_dir_all(&messages).expect("create ide fixture dirs");
        fs::create_dir_all(&conv_b).expect("create second conversation");

        fs::write(
            history.join("index.json"),
            json!({
                "conversations": [
                    {
                        "id": "conv-a",
                        "name": "IDE 会话标题",
                        "selectedModelId": "deepseek-v4-flash"
                    },
                    {
                        "id": "conv-b",
                        "name": "范围外会话",
                        "selectedModelId": "hy4-preview"
                    }
                ]
            })
            .to_string(),
        )
        .expect("write workspace index");
        fs::write(
            conv_a.join("index.json"),
            json!({
                "messages": [{ "id": "m1", "role": "assistant", "isComplete": true }],
                "requests": [{
                    "id": "req-1",
                    "state": "complete",
                    "startedAt": now,
                    "usage": {
                        "inputTokens": 100,
                        "outputTokens": 20,
                        "cacheTokens": 40,
                        "cachedWriteTokens": 5
                    }
                }]
            })
            .to_string(),
        )
        .expect("write conversation index");
        fs::write(
            conv_b.join("index.json"),
            json!({
                "requests": [{
                    "id": "req-old",
                    "state": "complete",
                    "startedAt": now - 100_000,
                    "usage": {
                        "inputTokens": 999,
                        "outputTokens": 9,
                        "cacheTokens": 1,
                        "cachedWriteTokens": 0
                    }
                }]
            })
            .to_string(),
        )
        .expect("write out-of-range conversation");
        fs::write(
            messages.join("ignored.json"),
            json!({
                "role": "assistant",
                "usage": { "inputTokens": 10_000, "outputTokens": 10_000 }
            })
            .to_string(),
        )
        .expect("write ignored message body");

        let mut projects = HashMap::new();
        projects.insert("conv-a".to_string(), "example-project".to_string());
        let result = ide_source(root.clone(), "codebuddy-ide", Some(now - 50_000), &projects);

        assert_eq!(result["source"], "codebuddy-ide");
        assert_eq!(result["filesScanned"], 2);
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 100);
        assert_eq!(result["summary"]["output"], 20);
        assert_eq!(result["summary"]["cacheRead"], 40);
        assert_eq!(result["summary"]["cacheWrite"], 5);
        assert_eq!(result["summary"]["total"], 125);
        assert_eq!(result["summary"]["uncachedInput"], 60);
        assert_eq!(result["models"][0]["key"], "deepseek-v4-flash");
        assert_eq!(result["projects"][0]["key"], "example-project");
        assert_eq!(result["sessions"][0]["sessionId"], "conv-a");
        assert_eq!(result["sessions"][0]["title"], "IDE 会话标题");
        assert_eq!(result["sessions"].as_array().map(Vec::len), Some(1));
        // IDE 来源不是明细来源，响应形状保持不变。
        assert!(result.get("requests").is_none());
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(result["dailyByModel"]["deepseek-v4-flash"][0]["key"], today);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// 切号复制会让同一会话散落在多个账号数据目录里：副本整份重放父历史（请求连时间戳
    /// 一起逐字复制）后各自继续追加，会话 id 还会被重随机。统计必须按请求指纹取**并集**：
    /// 父历史不重复累计，各副本复制后各自追加的请求也不丢。
    #[test]
    fn ide_source_merges_copied_session_history_as_a_union() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-ide-copy-{}-{now}",
            std::process::id()
        ));
        let conversation = |uid: &str, conv: &str| {
            root.join(uid)
                .join("VSCode")
                .join(uid)
                .join("history")
                .join("workspace-hash")
                .join(conv)
        };
        let original = conversation("uid-a", "conv-a");
        // 插件侧副本取新 id：目录名与源不同，但起点请求相同。
        let replay = conversation("uid-b", "conv-replay");
        // 另一个账号从同一份历史往下走：两份分叉后各自独有的请求都要保留。
        let fork = conversation("uid-c", "conv-fork");
        for dir in [&original, &replay, &fork] {
            fs::create_dir_all(dir).expect("create conversation dirs");
        }
        let request = |id: &str, input: u64| {
            json!({
                "id": id,
                "state": "complete",
                "startedAt": now,
                "usage": {
                    "inputTokens": input,
                    "outputTokens": 0,
                    "cacheTokens": 0,
                    "cachedWriteTokens": 0
                }
            })
        };
        fs::write(
            original.join("index.json"),
            json!({ "requests": [request("req-1", 100)] }).to_string(),
        )
        .expect("write original conversation");
        fs::write(
            replay.join("index.json"),
            json!({ "requests": [request("req-1", 100), request("req-2", 200)] }).to_string(),
        )
        .expect("write replayed conversation");
        fs::write(
            fork.join("index.json"),
            json!({ "requests": [request("req-1", 100), request("req-3", 400)] }).to_string(),
        )
        .expect("write forked conversation");

        let result = ide_source(root.clone(), "codebuddy-ide", None, &HashMap::new());

        assert_eq!(result["filesScanned"], 3);
        // 并集：100 + 200 + 400；重放的 req-1 只算一次，分叉两边独有的请求都在。
        assert_eq!(result["summary"]["records"], 3);
        assert_eq!(result["summary"]["input"], 700);
        assert_eq!(result["summary"]["total"], 700);
        // 同一个会话只出一行，不出现重复副本。
        assert_eq!(result["sessions"].as_array().map(Vec::len), Some(1));

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn decode_genie_workspace_recovers_unix_project_path() {
        assert_eq!(
            decode_genie_workspace("L1VzZXJzL2FwcGxlL0RvY3VtZW50cy9Qcm9qZWN0L215LWFnZW50")
                .as_deref(),
            Some("/Users/apple/Documents/Project/my-agent")
        );
    }

    #[test]
    #[ignore = "扫描真实用户数据，耗时不可控；用 --ignored 手动运行"]
    fn get_statistics_returns_four_isolated_sources() {
        // 注意：get_statistics 会扫描真实数据根（~/.workbuddy、~/.workbuddy-ai、
        // ~/.codebuddy 与扩展目录）。在数据量大的机器上耗时可达成分钟级，且结果随本机
        // 数据漂移——因此这条用例默认跳过，仅在需要端到端核对时手动运行：
        //   cargo test -p wb-switch-core --lib -- --ignored get_statistics_returns_four_isolated_sources
        let value = get_statistics(None, true, false);
        let sources = value["sources"].as_array().expect("sources");
        let names: Vec<_> = sources
            .iter()
            .map(|source| source["source"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            names,
            [
                "workbuddy",
                "workbuddy-ai",
                "codebuddy-cli",
                "codebuddy-ide"
            ]
        );
        // 国际版与国内版是两个独立 source，不合并。
        assert_ne!(sources[0]["source"], sources[1]["source"]);
    }

    /// 国际版数据根缺少 jsonl 根时返回空集，不报错。
    #[test]
    fn ai_source_is_empty_and_error_free_when_roots_are_missing() {
        let value = source_from_roots(&[], "workbuddy-ai", None, false);
        assert_eq!(value["source"], "workbuddy-ai");
        assert_eq!(value["filesScanned"], 0);
        assert_eq!(value["summary"]["input"], 0);
        assert_eq!(value["parseErrors"], 0);
        assert_eq!(value["sessions"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["projects"].as_array().map(Vec::len), Some(0));
    }

    /// 多根共享同一个指纹集：国际版两个根之间重放的同一请求只计一次，
    /// 同时第二个根自身的新记录仍要计入（证明第二个根确实被扫了）。
    #[test]
    fn multi_root_source_deduplicates_across_roots() {
        let now = crate::modules::config::now_ms();
        let base = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-multiroot-{}-{now}",
            std::process::id()
        ));
        let projects = base.join("projects");
        let sessions = base.join("sessions");
        fs::create_dir_all(&projects).expect("create projects root");
        fs::create_dir_all(&sessions).expect("create sessions root");
        let replayed = json!({
            "timestamp": now,
            "cwd": "/fixture/example-project",
            "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
        });
        fs::write(
            projects.join("session-original.jsonl"),
            format!("{}\n", replayed),
        )
        .expect("write original fixture");
        // 第二个根里的会话重放了同一个请求，并另有一条自己的新请求。
        fs::write(
            sessions.join("session-root-two.jsonl"),
            format!(
                "{}\n{}\n",
                replayed,
                json!({
                    "timestamp": now + 1_000,
                    "message": { "usage": { "input_tokens": 7, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write second-root fixture");
        // 与 source_deduplicates_copied_session_history 同理：毫秒级 mtime 并列时
        // 顺序退化为 readdir，重放记录可能先被第二个根认领。pin 住 mtime 让
        // 原始会话先处理，断言才稳定。
        pin_mtime(
            projects.join("session-original.jsonl"),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        pin_mtime(
            sessions.join("session-root-two.jsonl"),
            std::time::SystemTime::now(),
        );

        let result = source_from_roots(
            &[projects.clone(), sessions.clone()],
            "workbuddy-ai",
            None,
            false,
        );
        // 两个根都被扫到；重放记录计一次，第二个根的新记录另计一次。
        assert_eq!(result["filesScanned"], 2);
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 17);
        assert_eq!(result["summary"]["output"], 5);
        fs::remove_dir_all(base).expect("remove fixture");
    }

    #[test]
    fn account_aggregation_groups_usage_by_uid() {
        let mut collector = SourceCollector::default();
        let usage = Usage {
            input: 10,
            output: 3,
            read: 4,
            write: 1,
        };
        collector.add(
            usage,
            &json!({"timestamp": 1_700_000_000_000i64, "providerData": {"model": "m1"}}),
            "p",
            Some("uid-1"),
        );
        collector.add(
            usage,
            &json!({"timestamp": 1_700_000_100_000i64, "providerData": {"model": "m1"}}),
            "p",
            None,
        );
        let value = collector.into_value("test", 1);
        let accounts = value["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["key"], "uid-1");
        assert_eq!(accounts[0]["summary"]["records"], 1);
        assert_eq!(accounts[0]["summary"]["total"], 14);
        assert_eq!(value["summary"]["records"], 2);
    }

    /// 候选根按档位不同：国内版单根，国际版双根（无 projects/ 时只扫 sessions/）。
    #[test]
    fn jsonl_root_candidates_differ_by_variant() {
        let root = PathBuf::from("/tmp/wb-root");
        assert_eq!(
            jsonl_root_candidates(WbVariant::Cn, &root),
            vec![PathBuf::from("/tmp/wb-root/projects")]
        );
        assert_eq!(
            jsonl_root_candidates(WbVariant::Ai, &root),
            vec![
                PathBuf::from("/tmp/wb-root/projects"),
                PathBuf::from("/tmp/wb-root/sessions")
            ]
        );
    }
}

#[cfg(test)]
mod perf_tests {
    use super::*;

    #[test]
    fn metadata_prefilter_skips_message_only_lines() {
        assert!(line_has_token_metadata(br#"{"providerData":{"usage":{}}}"#));
        assert!(line_has_token_metadata(br#"{"aiTitle":"title"}"#));
        assert!(line_has_token_metadata(br#"{"summary":"summary"}"#));
        assert!(!line_has_token_metadata(
            br#"{"type":"message","text":"hello"}"#
        ));
    }
}

#[cfg(test)]
mod timestamp_index_tests {
    use super::*;
    use std::fs;

    #[test]
    fn recent_mtime_old_records_are_skipped_after_first_index() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-timestamp-index-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("old.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now - 40 * 86_400_000i64,
                    "providerData": {
                        "model": "fixture-model",
                        "usage": { "inputTokens": 10, "outputTokens": 1 }
                    }
                })
            ),
        )
        .expect("write fixture");

        let cutoff = now - 30 * 86_400_000;
        let first = source(root.clone(), "fixture-first", Some(cutoff), false);
        assert_eq!(first["summary"]["records"], 0);
        assert!(file_index_should_skip(&project.join("old.jsonl"), cutoff));
        let second = source(root.clone(), "fixture-second", Some(cutoff), false);
        assert_eq!(second["summary"]["records"], 0);
        assert_eq!(second["filesScanned"], 1);
        fs::remove_dir_all(root).expect("remove fixture");
    }
}

#[cfg(test)]
mod canonical_account_tests {
    use super::*;
    use crate::modules::session_link::{LinkGroup, LinkMember};

    fn member(id: &str, uid: &str, linked_at: i64) -> LinkMember {
        LinkMember {
            member_id: id.to_string(),
            account_id: None,
            uid: uid.to_string(),
            session_id: format!("session-{id}"),
            variant: Some(WbVariant::Cn),
            state: MemberState::Active,
            linked_at,
            last_synced_at: None,
        }
    }

    #[test]
    fn linked_copies_use_earliest_active_member_as_canonical_account() {
        let store = LinkStore {
            version: 1,
            revision: 1,
            groups: vec![LinkGroup {
                id: "group".to_string(),
                variant: WbVariant::Cn,
                created_at: 1,
                members: vec![
                    member("copy", "uid-copy", 2),
                    member("origin", "uid-origin", 1),
                ],
                pair_bases: Vec::new(),
            }],
        };
        let accounts = canonical_accounts_from_store(&store, WbVariant::Cn);
        assert_eq!(
            accounts.get("session-origin").map(String::as_str),
            Some("uid-origin")
        );
        assert_eq!(
            accounts.get("session-copy").map(String::as_str),
            Some("uid-origin")
        );
    }
}

#[cfg(test)]
mod source_refresh_tests {
    use super::*;
    use std::fs;

    #[test]
    fn parallel_scan_preserves_all_files_and_records() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-parallel-{}-{now}",
            std::process::id()
        ));
        let project = root.join("project");
        fs::create_dir_all(&project).expect("create parallel fixture");
        for index in 0..8u64 {
            fs::write(
                project.join(format!("session-{index}.jsonl")),
                format!(
                    "{}
",
                    json!({
                        "timestamp": now,
                        "providerData": {
                            "model": "fixture-model",
                            "usage": { "inputTokens": index + 1, "outputTokens": 1 }
                        }
                    })
                ),
            )
            .expect("write parallel fixture");
        }
        let value = source(root.clone(), "parallel-fixture", None, false);
        assert_eq!(value["summary"]["records"], 8);
        assert_eq!(value["sessions"].as_array().map(Vec::len), Some(8));
        fs::remove_dir_all(root).expect("remove parallel fixture");
    }

    #[test]
    fn explicit_refresh_bypasses_seeded_source_cache() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-refresh-{}-{now}",
            std::process::id()
        ));
        let project = root.join("project");
        fs::create_dir_all(&project).expect("create refresh fixture");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "providerData": {
                        "model": "fixture-model",
                        "usage": { "inputTokens": 7, "outputTokens": 1 }
                    }
                })
            ),
        )
        .expect("write refresh fixture");
        let roots = std::slice::from_ref(&root);
        let first = source_from_roots(roots, "refresh-fixture", None, false);
        assert_eq!(first["summary"]["records"], 1);

        // 直接污染缓存，证明 refresh=false 会命中旧值，refresh=true 不会。
        {
            let mut guard = source_cache().lock().expect("source cache lock");
            let entry = guard
                .entries
                .values_mut()
                .find(|entry| entry.value["source"] == "refresh-fixture")
                .expect("seeded source cache entry");
            entry.value["summary"]["records"] = json!(999);
        }
        let cached = source_from_roots(roots, "refresh-fixture", None, false);
        assert_eq!(cached["summary"]["records"], 999);
        let fresh = source_from_roots_with_accounts(
            roots,
            "refresh-fixture",
            None,
            false,
            &HashMap::new(),
            true,
        );
        assert_eq!(fresh["summary"]["records"], 1);
        fs::remove_dir_all(root).expect("remove refresh fixture");
    }
}

#[cfg(test)]
mod project_name_tests {
    use super::*;
    use std::fs;

    #[test]
    fn encoded_absolute_paths_never_leak_into_token_statistics() {
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-path-privacy-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        let encoded = root.join("c-Users-alice-WorkBuddy-2026-01-01");
        fs::create_dir_all(&encoded).expect("create encoded path fixture");
        fs::write(
            encoded.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": crate::modules::config::now_ms(),
                    "providerData": {
                        "model": "fixture-model",
                        "usage": { "inputTokens": 1, "outputTokens": 1 }
                    }
                })
            ),
        )
        .expect("write fixture");
        let value = source(root.clone(), "workbuddy", None, false);
        let text = value.to_string();
        assert!(
            !text.contains("alice"),
            "response leaked encoded user path: {text}"
        );
        assert_eq!(value["projects"][0]["key"], "未知项目");
        assert_eq!(value["projects"][0]["records"], 1);
        fs::remove_dir_all(root).expect("remove path privacy fixture");
    }

    #[test]
    fn normal_project_directory_is_preserved() {
        let root = Path::new("/fixture/root");
        let file = root.join("wb-switch").join("session.jsonl");
        assert_eq!(project_name(root, &file), "wb-switch");
    }
}

#[cfg(test)]
mod parse_error_index_tests {
    use super::*;
    use std::fs;

    fn fixture_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wb-switch-parse-index-{tag}-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        fs::create_dir_all(&root).expect("create fixture root");
        root
    }

    #[test]
    fn metadata_broken_line_blocks_index_but_plain_bad_line_does_not() {
        let now = crate::modules::config::now_ms();
        let old = now - 40 * 86_400_000;
        let root = fixture_root("metadata");
        let unsafe_file = root.join("unsafe.jsonl");
        fs::write(
            &unsafe_file,
            format!(
                "{}\n{{\"usage\":{{}}\n",
                json!({
                    "timestamp": old,
                    "providerData": { "model": "m", "usage": { "inputTokens": 1 } }
                })
            ),
        )
        .expect("write unsafe fixture");
        source(root.clone(), "unsafe", Some(now - 30 * 86_400_000), false);
        assert!(!file_index_should_skip(&unsafe_file, now - 30 * 86_400_000));

        let root = fixture_root("plain");
        let safe_file = root.join("safe.jsonl");
        fs::write(
            &safe_file,
            format!(
                "{}\nnot-json-no-usage\n",
                json!({
                    "timestamp": old,
                    "providerData": { "model": "m", "usage": { "inputTokens": 1 } }
                })
            ),
        )
        .expect("write safe fixture");
        source(root.clone(), "safe", Some(now - 30 * 86_400_000), false);
        assert!(file_index_should_skip(&safe_file, now - 30 * 86_400_000));
    }
}
