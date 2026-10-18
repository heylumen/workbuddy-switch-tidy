//! Token 刷新与保活。
//!
//! 对照 server.py `refresh_account_token` / `ensure_fresh_token` /
//! `run_keepalive_cycle`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::modules::account::{build_auth_headers, upsert_account};
use crate::modules::config::{http_request, load_checkin_config, norm_ts, now_ms, RunFlagGuard};
use crate::modules::error_log;

static KEEPALIVE_RUNNING: AtomicBool = AtomicBool::new(false);

/// 「残留标记复核」的冷却时间：同一账号最多每 60 秒复查一次（对应上游 #190/#191）。
const RELOGIN_RECHECK_COOLDOWN_MS: i64 = 60_000;

/// 是否该对「带 needs_relogin 标记、但 access token 仍有效」的账号做一次续期复核。
///
/// 场景（上游 #190）：账号曾刷新失败被标记，之后认证恢复、资源请求已正常，但旧标记
/// 一直残留，界面持续显示「需重新登录」。这里只在**凭据仍可用**且**冷却已过**时复查，
/// 复核成功才会清标记（由 refresh_account_token 成功后统一清理）。
fn should_recheck_relogin(account: &Value, now: i64) -> bool {
    if account.get("needs_relogin").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }
    let has_rt = !account.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").is_empty();
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
    let last = account.get("relogin_recheck_at").and_then(|v| v.as_i64()).unwrap_or(0);
    now.saturating_sub(last) >= RELOGIN_RECHECK_COOLDOWN_MS
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
        Some(t) if t > 0 => now.saturating_sub(t) < min_hours * 3600 * 1000,
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
        let _ = upsert_account(&account);
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
        let _ = upsert_account(&account);
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
        let _ = upsert_account(&account);
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
    let _ = upsert_account(&account);
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
        Some(e) => now_ms() >= e || e - now_ms() < lazy_h * 3600 * 1000,
        None => true,
    };
    let has_rt = !account
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty();
    if stale && has_rt {
        account = refresh_account_token(account).await;
    } else if should_recheck_relogin(&account, now_ms()) {
        // 凭据仍可用但残留了「需重新登录」标记：做一次冷却受控的续期复核，成功即自愈。
        if let Some(map) = account.as_object_mut() {
            map.insert("relogin_recheck_at".to_string(), json!(now_ms()));
        }
        let _ = upsert_account(&account);
        account = refresh_account_token(account).await;
    }
    account
}

/// 保活检查：每天由后台循环调用一次，默认（keepalive_days <= 0）无条件刷新
/// 全部带 refresh token 的账号；keepalive_days > 0 时仅刷新剩余不足该天数的账号。
///
/// 高频保活是为了避免官方服务端清理闲置的 refresh 会话——曾出现闲置数天后
/// 刷新返回 12153 invalid_grant（Session doesn't have required client）导致
/// 账号被迫重新登录。
pub async fn run_keepalive_cycle() -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&KEEPALIVE_RUNNING) else {
        return json!({"skipped": "already_running"});
    };
    let cfg = load_checkin_config();
    let keep_days = cfg
        .get("keepalive_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
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
        let exp = acc.get("expiresAt").and_then(|v| v.as_i64());
        let stale = keep_days <= 0
            || match exp {
                Some(e) => now_ms() >= e || e - now_ms() < keep_days * 24 * 3600 * 1000,
                None => true,
            };
        if !stale {
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
            let _ = upsert_account(&acc);
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
    let _ = crate::modules::config::save_checkin_config(&saved);
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
        assert_eq!(classify_refresh_failure(&json!({"code": -1})), RefreshFailure::Transient);
        assert_eq!(classify_refresh_failure(&json!({"code": 502})), RefreshFailure::Transient);
        assert_eq!(classify_refresh_failure(&json!({"code": 10085})), RefreshFailure::Transient);
    }

    #[test]
    fn classify_refresh_failure_detects_revoked_token() {
        assert_eq!(classify_refresh_failure(&json!({"code": 401})), RefreshFailure::Auth);
        assert_eq!(classify_refresh_failure(&json!({"code": 403})), RefreshFailure::Auth);
        assert_eq!(classify_refresh_failure(&json!({"code": 12153})), RefreshFailure::Auth);
        assert_eq!(
            classify_refresh_failure(&json!({"code": 400, "message": "invalid_grant: Invalid refresh token"})),
            RefreshFailure::Auth
        );
    }

    #[test]
    fn keepalive_skips_when_too_soon() {
        let now = 1_700_000_000_000i64;
        assert!(should_skip_keepalive(Some(now - 3600 * 1000), now, 12));
        assert!(!should_skip_keepalive(Some(now - 13 * 3600 * 1000), now, 12));
        assert!(!should_skip_keepalive(None, now, 12));
        assert!(!should_skip_keepalive(Some(now), now, 0));
    }

    #[test]
    fn recheck_only_when_marked_and_token_still_valid() {
        let now = 1_700_000_000_000i64;
        let mut acc = json!({"needs_relogin": true, "refresh_token": "rt", "expiresAt": now + 3_600_000});
        assert!(should_recheck_relogin(&acc, now));
        // 冷却未过 → 不重复复核
        acc["relogin_recheck_at"] = json!(now - 30_000);
        assert!(!should_recheck_relogin(&acc, now));
        // 冷却已过 → 允许
        acc["relogin_recheck_at"] = json!(now - 90_000);
        assert!(should_recheck_relogin(&acc, now));
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
}
