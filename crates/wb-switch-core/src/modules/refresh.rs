//! Token 刷新与保活。
//!
//! 对照 server.py `refresh_account_token` / `ensure_fresh_token` /
//! `run_keepalive_cycle`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;

use crate::modules::account::{build_auth_headers, upsert_account};
use crate::modules::config::{http_request, load_checkin_config, norm_ts, now_ms, RunFlagGuard};
use crate::modules::error_log;

static KEEPALIVE_RUNNING: AtomicBool = AtomicBool::new(false);

/// 「残留标记复核」的冷却时间：同一账号最多每 10 分钟复查一次（对应上游 #190/#191）。
///
/// 原为 60 秒：账号已被服务端吊销（12153）时会反复打无效刷新，放大风控特征。
const RELOGIN_RECHECK_COOLDOWN_MS: i64 = 600_000;

/// 已确认终态（12153 / refresh token invalid）后的复核间隔：6 小时。
///
/// 终态只能重登恢复；仍保留低频复核，用于覆盖服务端事后放行的极小概率，
/// 但不让它参与每 10 分钟一轮的常规重试。
const RELOGIN_TERMINAL_RECHECK_COOLDOWN_MS: i64 = 6 * 3600 * 1000;

/// 刷新临时失败后的退避窗口：期间不再对同一账号发起刷新，避免「失败即重试」。
const REFRESH_FAILURE_BACKOFF_MS: i64 = 600_000;

/// 账号状态落盘：失败必须留痕，不能静默吞掉。
///
/// 刷新成功后若写库失败，接口会返回新 token 而库里仍是旧值，下一次请求会再次刷新，
/// 表现为「刷新成功但很快又失效」。这里统一记录错误，便于从 error.log 定位。
fn persist_account(account: &Value, stage: &str) {
    if let Err(error) = upsert_account(account) {
        error_log::record("refresh", "账号凭据落盘失败", &format!("{stage}：{error}"));
    }
}

/// 每个账号一把刷新锁：同一账号的刷新必须串行。
///
/// 2026-10-23 事故：`should_recheck_relogin` 的「读-判-写」不是原子的，多个并发请求
/// 同时通过冷却检查，57 秒内对 11 个账号发起 119 次刷新。串行化后同一账号不会有
/// 两次并发刷新，配合锁内重读可跳过已被其他任务刷新成功的账号。
fn refresh_locks() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 账号锁的键：优先本地 id，其次 uid，最后退回 refresh token 前缀（不落盘、不打印）。
fn refresh_lock_key(account: &Value) -> String {
    for key in ["id", "uid"] {
        if let Some(v) = account.get(key).and_then(Value::as_str) {
            if !v.trim().is_empty() {
                return format!("{key}:{v}");
            }
        }
    }
    account
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(|t| format!("rt:{}", t.chars().take(16).collect::<String>()))
        .unwrap_or_else(|| "anonymous".to_string())
}

async fn lock_for_account(account: &Value) -> Arc<Mutex<()>> {
    let key = refresh_lock_key(account);
    let mut locks = refresh_locks().lock().await;
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 读取账号库中该账号的最新快照（用于锁内重读）。
fn latest_account_snapshot(account: &Value) -> Option<Value> {
    let id = account.get("id").and_then(Value::as_str)?;
    crate::modules::account::find_account(id)
}

/// 是否处于刷新临时失败后的退避期。
fn refresh_in_backoff(account: &Value, now: i64) -> bool {
    account
        .get("last_refresh_error_at")
        .and_then(Value::as_i64)
        .map(|t| now.saturating_sub(t) < REFRESH_FAILURE_BACKOFF_MS)
        .unwrap_or(false)
}

/// 是否该对「带 needs_relogin 标记、但 access token 仍有效」的账号做一次续期复核。
///
/// 场景（上游 #190）：账号曾刷新失败被标记，之后认证恢复、资源请求已正常，但旧标记
/// 一直残留，界面持续显示「需重新登录」。这里只在**凭据仍可用**且**冷却已过**时复查，
/// 复核成功才会清标记（由 refresh_account_token 成功后统一清理）。
fn should_recheck_relogin(account: &Value, now: i64) -> bool {
    if account.get("needs_relogin").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }
    let has_rt = !account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty();
    if !has_rt {
        return false;
    }
    let at_valid = account
        .get("expiresAt")
        .and_then(|v| v.as_i64())
        .map(|e| e > now)
        .unwrap_or(false);
    if !at_valid {
        return false;
    }
    let last = account
        .get("relogin_recheck_at")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    // 12153 / refresh token invalid 属终态，用长冷却避免持续无效刷新。
    let terminal = account
        .get("needs_relogin_reason")
        .and_then(|v| v.as_str())
        .map(|reason| {
            let lower = reason.to_lowercase();
            reason.contains("12153") || lower.contains("refresh token is invalid")
        })
        .unwrap_or(false);
    let cooldown = if terminal {
        RELOGIN_TERMINAL_RECHECK_COOLDOWN_MS
    } else {
        RELOGIN_RECHECK_COOLDOWN_MS
    };
    now.saturating_sub(last) >= cooldown
}

/// 刷新失败的类别：决定「标记需要重新登录」还是「只记录临时失败」。
///
/// 背景（上游 issue #189）：官方服务端曾出现大范围 token 失效，同时也有大量用户
/// 只是网络/代理抖动。若把后者也标成「需要重新登录」，用户会白跑一次扫码。
#[derive(Debug, PartialEq, Eq)]
enum RefreshFailure {
    /// 鉴权被拒：refresh token 被吊销或已失效 → 需要重新登录
    Auth,
    /// 传输层或服务端临时故障（网络/代理/5xx/风控）→ 保留凭据，稍后重试
    Transient,
}

/// 判定刷新失败类别。口径与 `checkin::is_unauthorized` 对齐，但**不把一切非 0/200
/// 都当成鉴权失败**：只认明确的吊销信号，其余按临时故障处理。
fn classify_refresh_failure(resp: &Value) -> RefreshFailure {
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    // code = -1 是 http_request 包装的传输层失败（连接/代理/超时）
    if code < 0 || (500..=599).contains(&code) {
        return RefreshFailure::Transient;
    }
    // 非 JSON 响应里的 code 只是 HTTP 状态码：网关 HTML 401 不等于业务鉴权结论，
    // 先按临时故障保留凭据，避免把可恢复账号误标成「需重新登录」。
    if resp.get("nonJson").and_then(Value::as_bool) == Some(true) {
        return RefreshFailure::Transient;
    }
    // 12153 = Session doesn't have required client（官方刷新会话被清理）
    if code == 401 || code == 403 || code == 12153 {
        return RefreshFailure::Auth;
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    const AUTH_HINTS: [&str; 8] = [
        "invalid_grant",
        "invalid_token",
        "unauthorized",
        "token has expired",
        "refresh token",
        "会话已失效",
        "登录已过期",
        "请重新登录",
    ];
    if AUTH_HINTS.iter().any(|hint| msg.contains(hint)) {
        RefreshFailure::Auth
    } else {
        RefreshFailure::Transient
    }
}

/// 保活是否应因「距上次太近」而跳过（按持久化的 last_keepalive_at 判断）。
///
/// 只存在内存里的日切标记会在应用频繁重启时被反复重置，导致每启动一次就全量刷新
/// 所有账号——这种「同一台机器短时间内多轮刷新」对风控最不友好，因此改为持久化节流。
fn should_skip_keepalive(last_at_ms: Option<i64>, now: i64, min_hours: i64) -> bool {
    if min_hours <= 0 {
        return false;
    }
    match last_at_ms {
        Some(t) if t > 0 => now.saturating_sub(t) < min_hours.saturating_mul(3600 * 1000),
        _ => false,
    }
}

/// 刷新接口 URL：域名与路径前缀都按账号自身档位选（缺档位字段 → 国内版，零回归）。
fn refresh_url(account: &Value) -> String {
    let variant = crate::modules::account::variant_of(account);
    format!(
        "{}{}/auth/token/refresh",
        variant.api_endpoint(),
        variant.api_prefix()
    )
}

/// 构造刷新请求头：鉴权头 + 刷新会话头。
///
/// `X-Auth-Refresh-Source: plugin` 必须保留：两个档位的官方客户端刷新时都发送该头，
/// 缺失时网关会把这次刷新判定成另一个 client 来源，国际版实测返回
/// `invalid_grant: Invalid refresh token`（code 12153）。抽成纯函数是为了让单测
/// 能把这个头固定住，避免将来被顺手删掉。
fn refresh_headers(account: &Value, refresh_token: &str) -> HashMap<String, String> {
    let mut headers = build_auth_headers(account);
    headers.insert("X-Refresh-Token".to_string(), refresh_token.to_string());
    headers.insert("X-Auth-Refresh-Source".to_string(), "plugin".to_string());
    headers
}

/// 刷新单账号 token（POST /v2/plugin/auth/token/refresh），成功则落盘并返回新账号。
///
/// 刷新失败（refresh token 失效等）时给账号标记 needs_relogin，避免无限重试。
pub async fn refresh_account_token(mut account: Value) -> Value {
    // 同一账号串行刷新：并发刷新会重复消费 refresh token，也会放大风控特征。
    let lock = lock_for_account(&account).await;
    let _guard = lock.lock().await;

    // 锁内重读：等待期间若已被其他任务刷新成功，直接复用新凭据，不再重复刷新。
    let before = account
        .get("refreshedAt")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if let Some(latest) = latest_account_snapshot(&account) {
        let after = latest
            .get("refreshedAt")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if before > 0 && after > before {
            return latest;
        }
        account = latest;
    }

    let previous_access_token = account
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let rt = account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    if rt.is_empty() {
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!("缺少 refresh token，无法刷新，需重新登录");
        persist_account(&account, "刷新状态");
        return account;
    }

    let headers = refresh_headers(&account, &rt);
    // 刷新必须走账号自身档位的域名：把国际版 token 打到国内网关等于登录失效。
    let url = refresh_url(&account);
    let resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 && code != 200 {
        let reason = format!(
            "刷新失败(code={code}): {}",
            resp.get("message")
                .or_else(|| resp.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误")
        );
        match classify_refresh_failure(&resp) {
            RefreshFailure::Auth => {
                account["needs_relogin"] = json!(true);
                account["needs_relogin_reason"] = json!(reason);
                error_log::record("refresh", "刷新鉴权失败（标记需重新登录）", &reason);
            }
            RefreshFailure::Transient => {
                // 临时故障不动 needs_relogin：用户不必为此重新扫码，下次保活/惰性刷新会重试。
                account["last_refresh_error"] = json!(reason);
                account["last_refresh_error_at"] = json!(now_ms());
                error_log::record("refresh", "刷新临时失败（未标记需重新登录）", &reason);
            }
        }
        persist_account(&account, "刷新状态");
        return account;
    }

    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let new_at = data
        .get("accessToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("access_token").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let Some(new_at) = new_at else {
        account["needs_relogin"] = json!(true);
        account["needs_relogin_reason"] = json!("刷新响应缺少 accessToken");
        persist_account(&account, "刷新状态");
        return account;
    };

    account["access_token"] = json!(new_at);
    if let Some(new_rt) = data
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("refresh_token").and_then(|v| v.as_str()))
    {
        account["refresh_token"] = json!(new_rt);
    }
    // 官方接口只返回相对 expiresIn（秒），需换算为绝对时间戳
    let new_exp = norm_ts(data.get("expiresAt").or_else(|| data.get("expires_at")));
    let new_exp = match new_exp {
        Some(v) => Some(v),
        None => data
            .get("expiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    if let Some(v) = new_exp {
        account["expiresAt"] = json!(v);
    }
    let fallback_rt_exp = norm_ts(
        account
            .get("auth_raw")
            .and_then(|a| a.get("refreshExpiresAt")),
    );
    let mut new_rt_exp = norm_ts(
        data.get("refreshExpiresAt")
            .or_else(|| data.get("refresh_expires_at")),
    );
    if new_rt_exp.is_none() {
        new_rt_exp = fallback_rt_exp;
    }
    let new_rt_exp = match new_rt_exp {
        Some(v) => Some(v),
        None => data
            .get("refreshExpiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    if let Some(v) = new_rt_exp {
        account["refreshExpiresAt"] = json!(v);
    }
    account["refreshedAt"] = json!(now_ms());
    let map = account.as_object_mut().unwrap();
    map.remove("needs_relogin");
    map.remove("needs_relogin_reason");
    map.remove("last_refresh_error");
    map.remove("last_refresh_error_at");
    map.remove("relogin_recheck_at");
    persist_account(&account, "刷新状态");
    // Windows 不执行 apiKeyHelper；当前 CLI 账号刷新后同步 settings env。
    // 同步失败不阻断 WorkBuddy 保活；状态接口会根据 settings 与账号库是否
    // 一致显示“待同步”，避免把认证配置错误混入账号数据。
    if cfg!(windows) {
        let _ = crate::modules::codebuddy_cli::sync_windows_env_for_account(
            &account,
            previous_access_token.as_deref(),
        );
    }
    account
}

/// 惰性刷新：expiresAt 缺失或剩余 < lazy_refresh_hours 则刷新。返回最新账号。
pub async fn ensure_fresh_token(mut account: Value, cfg: &Value) -> Value {
    let lazy_h = cfg
        .get("lazy_refresh_hours")
        .and_then(|v| v.as_i64())
        .unwrap_or(24);
    let exp = account.get("expiresAt").and_then(|v| v.as_i64());
    let stale = match exp {
        Some(e) => now_ms() >= e || e - now_ms() < lazy_h.saturating_mul(3600 * 1000),
        None => true,
    };
    let has_rt = !account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty();
    if stale && has_rt {
        // 自动惰性刷新遵守失败退避；用户手动刷新不受此限制。
        if !refresh_in_backoff(&account, now_ms()) {
            account = refresh_account_token(account).await;
        }
    } else if should_recheck_relogin(&account, now_ms()) {
        // 凭据仍可用但残留了「需重新登录」标记：做一次冷却受控的续期复核，成功即自愈。
        if let Some(map) = account.as_object_mut() {
            map.insert("relogin_recheck_at".to_string(), json!(now_ms()));
        }
        persist_account(&account, "刷新状态");
        account = refresh_account_token(account).await;
    }
    account
}

/// 保活是否到期：`keepalive_days <= 0` 保留旧版全量语义。
///
/// 大于 0 时采用**双条件**，两者任一成立即刷新：
/// 1. 距上次成功刷新已达 `keepalive_days`（防服务端清理闲置 refresh 会话，
///    本项目实测闲置数天后会返回 12153）；
/// 2. access token 剩余有效期不足 `keepalive_days`（到期前提前续期）。
///
/// 只做第 2 条会让 30 天有效期的账号约 23 天不刷新，正好落进闲置清理窗口。
fn should_refresh_for_keepalive(
    expires_at: Option<i64>,
    last_refreshed_at: Option<i64>,
    now: i64,
    keepalive_days: i64,
) -> bool {
    if keepalive_days <= 0 {
        return true;
    }
    let window = keepalive_days.saturating_mul(24 * 3600 * 1000);
    let idle_due = match last_refreshed_at.filter(|t| *t > 0) {
        Some(last) => now.saturating_sub(last) >= window,
        None => true,
    };
    let expiring = match expires_at {
        Some(e) => now >= e || e - now < window,
        None => true,
    };
    idle_due || expiring
}

/// 保活检查：默认每 7 天至少刷新一次，且覆盖剩余有效期不足 7 天的账号
/// （对齐上游 PR #244 的 168h 窗口，同时保留防闲置清理的下限）。
/// 不再每天无条件轮换全部账号的 refresh token，减少无效请求与风控暴露。
/// `keepalive_days <= 0` 仍保留旧版全量行为，供手动兼容配置使用。
pub async fn run_keepalive_cycle() -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&KEEPALIVE_RUNNING) else {
        return json!({"skipped": "already_running"});
    };
    let cfg = load_checkin_config();
    let keep_days = cfg
        .get("keepalive_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(7);
    let min_interval_h = cfg
        .get("keepalive_min_interval_hours")
        .and_then(|v| v.as_i64())
        .unwrap_or(12);
    let last_at = cfg.get("last_keepalive_at").and_then(|v| v.as_i64());
    if should_skip_keepalive(last_at, now_ms(), min_interval_h) {
        return json!({"skipped": "too_soon", "min_interval_hours": min_interval_h});
    }
    let accounts = crate::modules::account::load_accounts();
    let total = accounts.len();
    let mut results: Vec<Value> = Vec::new();
    let mut transient_failures = 0usize;
    for (idx, mut acc) in accounts.into_iter().enumerate() {
        // 账号之间插入确定性抖动（300–800ms），避免「同一秒内 N 个刷新」的风控特征。
        if idx > 0 {
            tokio::time::sleep(Duration::from_millis(300 + ((idx as u64 * 137) % 500))).await;
        }
        // 已明确判定需重登的账号不再由自动保活反复打 12153；若后续恢复，
        // 由实际资源请求路径的残留标记复核负责自愈。
        if acc.get("needs_relogin").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let exp = acc.get("expiresAt").and_then(|v| v.as_i64());
        let last_refreshed = acc.get("refreshedAt").and_then(|v| v.as_i64());
        if !should_refresh_for_keepalive(exp, last_refreshed, now_ms(), keep_days) {
            continue;
        }
        if refresh_in_backoff(&acc, now_ms()) {
            continue;
        }
        if acc
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            acc["needs_relogin"] = json!(true);
            acc["needs_relogin_reason"] = json!("缺少 refresh token，无法保活，需重新登录");
            persist_account(&acc, "保活状态");
            results.push(json!({
                "email": crate::modules::account::account_display_name(&acc),
                "status": "missing_rt",
            }));
            continue;
        }
        let fresh = refresh_account_token(acc).await;
        let failed = fresh.get("needs_relogin").and_then(|v| v.as_bool()) == Some(true);
        if !failed && fresh.get("last_refresh_error").is_some() {
            transient_failures += 1;
        }
        results.push(json!({
            "email": crate::modules::account::account_display_name(&fresh),
            "status": if failed { "failed" } else { "ok" },
            "error": if failed {
                fresh.get("needs_relogin_reason").and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            },
        }));
    }
    let mut saved = load_checkin_config();
    if let Some(map) = saved.as_object_mut() {
        map.insert("last_keepalive_at".to_string(), json!(now_ms()));
    }
    if let Err(error) = crate::modules::config::save_checkin_config(&saved) {
        error_log::record(
            "refresh",
            "保活时间落盘失败",
            &format!("last_keepalive_at：{error}"),
        );
    }
    json!({
        "checked": total,
        "refreshed": results,
        "transientFailures": transient_failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_url_follows_account_variant() {
        // 旧账号（无档位字段）→ 国内版，与改造前逐字一致。
        assert_eq!(
            refresh_url(&json!({"uid": "u-1"})),
            format!(
                "{}/v2/plugin/auth/token/refresh",
                crate::modules::variant::WbVariant::Cn.api_endpoint()
            )
        );
        assert_eq!(
            refresh_url(&json!({"uid": "u-1", "access_token": "t"})),
            "https://www.codebuddy.cn/v2/plugin/auth/token/refresh"
        );
        // 国际版账号 → 国际版域名。
        assert_eq!(
            refresh_url(&json!({"uid": "u-2", "variant": "ai"})),
            format!(
                "{}/v2/plugin/auth/token/refresh",
                crate::modules::variant::WbVariant::Ai.api_endpoint()
            )
        );
        assert_ne!(
            refresh_url(&json!({"variant": "ai"})),
            refresh_url(&json!({"variant": "cn"}))
        );
    }

    /// AC11：刷新请求头必须带 `X-Auth-Refresh-Source: plugin`，两个档位都覆盖。
    ///
    /// 断言为 `Some("plugin")`：头被删掉时为 `None`，用例立即失败，不会被写成恒真。
    #[test]
    fn refresh_headers_always_declare_plugin_refresh_source() {
        let cases = [
            // 国内版：旧账号（无档位字段）与显式档位各一例。
            json!({"uid": "u-cn", "access_token": "at", "refresh_token": "rt"}),
            json!({"uid": "u-cn", "access_token": "at", "variant": "cn"}),
            // 国际版：D1 的失败现场。
            json!({"uid": "u-ai", "access_token": "at", "variant": "ai"}),
        ];
        for account in cases {
            let headers = refresh_headers(&account, "rt-value");
            assert_eq!(
                headers.get("X-Auth-Refresh-Source").map(String::as_str),
                Some("plugin"),
                "刷新请求头缺少 X-Auth-Refresh-Source: plugin（{account}）"
            );
            // 刷新会话头不能被这次改动挤掉。
            assert_eq!(
                headers.get("X-Refresh-Token").map(String::as_str),
                Some("rt-value")
            );
            // 刷新不带 platform 查询参数/头：官方刷新同样不带（research §2 已推翻旧假设）。
            assert!(!headers.contains_key("platform"));
        }
    }

    #[test]
    fn classify_refresh_failure_treats_transient_as_retryable() {
        // 传输层失败 / 5xx / 未知业务码 → 临时故障，不应要求重新登录
        assert_eq!(
            classify_refresh_failure(&json!({"code": -1})),
            RefreshFailure::Transient
        );
        assert_eq!(
            classify_refresh_failure(&json!({"code": 502})),
            RefreshFailure::Transient
        );
        assert_eq!(
            classify_refresh_failure(&json!({"code": 10085})),
            RefreshFailure::Transient
        );
    }

    #[test]
    fn classify_refresh_failure_detects_revoked_token() {
        assert_eq!(
            classify_refresh_failure(&json!({"code": 401})),
            RefreshFailure::Auth
        );
        assert_eq!(
            classify_refresh_failure(&json!({"code": 403})),
            RefreshFailure::Auth
        );
        assert_eq!(
            classify_refresh_failure(&json!({"code": 12153})),
            RefreshFailure::Auth
        );
        assert_eq!(
            classify_refresh_failure(
                &json!({"code": 400, "message": "invalid_grant: Invalid refresh token"})
            ),
            RefreshFailure::Auth
        );
    }

    #[test]
    fn keepalive_skips_when_too_soon() {
        let now = 1_700_000_000_000i64;
        assert!(should_skip_keepalive(Some(now - 3600 * 1000), now, 12));
        assert!(!should_skip_keepalive(
            Some(now - 13 * 3600 * 1000),
            now,
            12
        ));
        assert!(!should_skip_keepalive(None, now, 12));
        assert!(!should_skip_keepalive(Some(now), now, 0));
    }

    #[test]
    fn recheck_only_when_marked_and_token_still_valid() {
        let now = 1_700_000_000_000i64;
        let mut acc =
            json!({"needs_relogin": true, "refresh_token": "rt", "expiresAt": now + 3_600_000});
        assert!(should_recheck_relogin(&acc, now));
        // 冷却未过 → 不重复复核
        acc["relogin_recheck_at"] = json!(now - 30_000);
        assert!(!should_recheck_relogin(&acc, now));
        // 10 分钟冷却已过 → 允许
        acc["relogin_recheck_at"] = json!(now - RELOGIN_RECHECK_COOLDOWN_MS - 1);
        assert!(should_recheck_relogin(&acc, now));
        // 终态 12153 → 6 小时冷却，10 分钟刚过仍不再复核。
        acc["needs_relogin_reason"] = json!("刷新失败(code=12153): 12153:refresh token is invalid");
        acc["relogin_recheck_at"] = json!(now - RELOGIN_RECHECK_COOLDOWN_MS - 1);
        assert!(!should_recheck_relogin(&acc, now));
        acc["relogin_recheck_at"] = json!(now - RELOGIN_TERMINAL_RECHECK_COOLDOWN_MS - 1);
        assert!(should_recheck_relogin(&acc, now));
        acc.as_object_mut().unwrap().remove("needs_relogin_reason");
        acc["relogin_recheck_at"] = json!(now - RELOGIN_RECHECK_COOLDOWN_MS - 1);
        // access token 已过期 → 交给普通刷新路径
        acc["expiresAt"] = json!(now - 1000);
        assert!(!should_recheck_relogin(&acc, now));
        // 未标记 / 无 RT → 不复核
        acc["expiresAt"] = json!(now + 3_600_000);
        acc["needs_relogin"] = json!(false);
        assert!(!should_recheck_relogin(&acc, now));
        acc["needs_relogin"] = json!(true);
        acc["refresh_token"] = json!("");
        assert!(!should_recheck_relogin(&acc, now));
    }

    #[test]
    fn non_json_gateway_401_is_not_misclassified_as_auth() {
        // 网关 HTML 错误页不是业务鉴权结论，必须保留凭据供后续重试。
        assert_eq!(
            classify_refresh_failure(&json!({
                "code": 401,
                "message": "服务端返回 HTML 错误页：401 Authorization Required",
                "nonJson": true
            })),
            RefreshFailure::Transient
        );
        // 真正的业务 401/12153 仍然必须判为鉴权失效。
        assert_eq!(
            classify_refresh_failure(&json!({"code": 12153, "msg": "refresh token is invalid"})),
            RefreshFailure::Auth
        );
    }

    #[test]
    fn transient_refresh_failure_has_bounded_backoff() {
        let now = 1_700_000_000_000i64;
        let recent = json!({"last_refresh_error_at": now - 1000});
        let old = json!({"last_refresh_error_at": now - REFRESH_FAILURE_BACKOFF_MS - 1});
        assert!(refresh_in_backoff(&recent, now));
        assert!(!refresh_in_backoff(&old, now));
        assert!(!refresh_in_backoff(&json!({}), now));
    }

    #[test]
    fn keepalive_refreshes_by_idle_or_expiry_window() {
        let now = 1_700_000_000_000i64;
        let day = 24 * 3600 * 1000;
        // 刚刷新过且不临期 → 跳过。
        assert!(!should_refresh_for_keepalive(
            Some(now + 8 * day),
            Some(now - day),
            now,
            7
        ));
        // 临期（不足 7 天）→ 刷新，即使刚刷新过。
        assert!(should_refresh_for_keepalive(
            Some(now + 6 * day),
            Some(now - day),
            now,
            7
        ));
        // 虽未临期，但已 7 天未刷新 → 刷新（防服务端清理闲置会话）。
        assert!(should_refresh_for_keepalive(
            Some(now + 20 * day),
            Some(now - 8 * day),
            now,
            7
        ));
        // 已过期 / 到期未知 / 刷新时间未知 → 刷新。
        assert!(should_refresh_for_keepalive(
            Some(now - 1),
            Some(now - day),
            now,
            7
        ));
        assert!(should_refresh_for_keepalive(None, Some(now - day), now, 7));
        assert!(should_refresh_for_keepalive(
            Some(now + 8 * day),
            None,
            now,
            7
        ));
        // 兼容旧配置：窗口为 0 时仍是全量。
        assert!(should_refresh_for_keepalive(
            Some(now + 30 * day),
            Some(now),
            now,
            0
        ));
    }

    #[test]
    fn refresh_lock_key_prefers_stable_identity_and_never_exposes_token() {
        assert_eq!(
            refresh_lock_key(&json!({"id": "local", "uid": "u"})),
            "id:local"
        );
        assert_eq!(refresh_lock_key(&json!({"uid": "u"})), "uid:u");
        let fallback = refresh_lock_key(&json!({"refresh_token": "secret-token-value"}));
        assert!(fallback.starts_with("rt:"));
        assert!(!fallback.contains("secret-token-value"));
    }
}
