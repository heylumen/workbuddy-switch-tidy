//! 签到：状态查询 / 执行签到 / 自动签到调度。
//!
//! 对照 server.py `get_checkin_status` / `perform_checkin` /
//! `checkin_account` / `run_checkin_cycle` / `_checkin_request` /
//! `_is_unauthorized`。
//!
//! 档位策略：签到仅国内版可用（`WbVariant::supports_checkin`）。国际版没有签到
//! 接口，自动周期、一键签到与单账号签到都在发请求前统一跳过，绝不发起任何请求；
//! 下游的 inactive / statusUnsupported 判定保留为防御，不依赖它们拦截国际版。

use chrono::Local;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::modules::account::{
    account_display_name, build_auth_headers, load_accounts, variant_of,
};
use crate::modules::config::{
    add_checkin_log, http_request, is_route_missing, load_checkin_config, load_checkin_logs,
    now_ms, RunFlagGuard, CHECKIN_API_PREFIX,
};
use crate::modules::refresh::{ensure_fresh_token, refresh_account_token};
use crate::modules::variant::WbVariant;

static CHECKIN_RUNNING: AtomicBool = AtomicBool::new(false);
static CHECKIN_ACCOUNTS_RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Automatic recovery cadence shared by every host.
pub const CHECKIN_RECOVERY_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckinCycleMode {
    /// Always verify every account against the server after a host starts.
    StartupVerify,
    /// Re-verify every account against the server during background recovery.
    PeriodicRecovery,
}

#[derive(Debug, Eq, PartialEq)]
enum StatusDecision {
    Already,
    Submit,
    Error(String),
}

struct AccountRunGuard {
    key: String,
}

impl AccountRunGuard {
    fn try_acquire(account: &Value) -> Option<Self> {
        let key = account
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .unwrap_or_else(|| account_display_name(account));
        let mut running = CHECKIN_ACCOUNTS_RUNNING
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap();
        if !running.insert(key.clone()) {
            return None;
        }
        Some(Self { key })
    }
}

impl Drop for AccountRunGuard {
    fn drop(&mut self) {
        if let Some(running) = CHECKIN_ACCOUNTS_RUNNING.get() {
            running.lock().unwrap().remove(&self.key);
        }
    }
}

/// 判断是否因 token 失效被拒（用于触发刷新重试）。
fn is_unauthorized(resp: &Value) -> bool {
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 401 || code == 403 {
        return true;
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    ["unauthorized", "401", "登录", "失效", "过期", "token"]
        .iter()
        .any(|k| msg.contains(k))
}

/// 该响应是否应在刷新/重试判定**之前**原样返回（国际版「未开启 / 未开放 / 已过期」类业务码）。
///
/// 这类提示是业务结果，不是鉴权失败；而 `is_unauthorized` 的弱关键字含「过期」，
/// 若不先短路就会白刷一次 token 并重发一次（刷新失败还会写 `needs_relogin`）。
/// 国内版不做任何短路，语义逐字不变。
fn skips_refresh_before_retry(variant: WbVariant, resp: &Value) -> bool {
    if variant != WbVariant::Ai {
        return false;
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    is_inactive_message(msg)
}

/// 发单次签到请求；遇到未授权且存在 refresh token 时刷新一次并重试。
async fn checkin_request_once(path: &str, account: &Value, variant: WbVariant) -> Value {
    let url = format!("{}{path}", variant.api_endpoint());
    let headers = build_auth_headers(account);
    let mut resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    // 国际版「未开放 / 已过期」类业务码：直接返回，绝不刷新、绝不重试。
    if skips_refresh_before_retry(variant, &resp) {
        return resp;
    }
    if is_unauthorized(&resp)
        && !account
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        let refreshed = refresh_account_token(account.clone()).await;
        let headers = build_auth_headers(&refreshed);
        resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    }
    resp
}

/// 发签到相关请求：路径候选按档位生成，**只有 404 才回落**到下一个候选。
///
/// 401/403（鉴权）、10085（网关指纹拦截）、`code=-1`（传输错误）都必须原样返回：
/// 它们不是路径问题，回落只会掩盖真因并多打一次无意义的请求。
async fn checkin_request(suffix: &str, account: &Value) -> Value {
    let variant = variant_of(account);
    let paths = variant.billing_paths(&format!("{CHECKIN_API_PREFIX}{suffix}"));
    let mut last = json!({"code": -1, "message": "无可用签到路径"});
    for (index, path) in paths.iter().enumerate() {
        let resp = checkin_request_once(path, account, variant).await;
        if index + 1 == paths.len() || !is_route_missing(&resp) {
            return resp;
        }
        last = resp;
    }
    last
}

/// 成功的状态响应 → 结果对象；非成功返回 None。
fn status_from_response(resp: &Value) -> Option<Value> {
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 && code != 200 {
        return None;
    }
    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    Some(json!({
        "ok": true,
        "todayCheckedIn": data.get("today_checked_in")
            .or_else(|| data.get("todayCheckedIn"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "raw": data,
    }))
}

/// 查询签到状态：新接口 checkin-activity-status，失败回退 checkin-status。
///
/// `checkin-activity-status` / `checkin-status` 是**国内版专有**接口；国际版没有
/// 对应实现（也没有签到本身）。因此国际版不发起任何请求，直接返回
/// `statusUnsupported: true`；签到链路的其它入口在 `checkin_account` 处统一跳过。
pub async fn get_checkin_status(account: &Value) -> Value {
    if variant_of(account) == WbVariant::Cn {
        let resp = checkin_request("/checkin-activity-status", account).await;
        if let Some(status) = status_from_response(&resp) {
            return status;
        }
        let resp2 = checkin_request("/checkin-status", account).await;
        if let Some(status) = status_from_response(&resp2) {
            return status;
        }
        let code2 = resp2.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        return json!({
            "ok": false,
            "error": resp2.get("message")
                .or_else(|| resp2.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("code={code2}"))
                .to_string(),
        });
    }
    json!({
        "ok": false,
        "statusUnsupported": true,
        "error": "该档位暂无签到状态接口",
    })
}

/// 国际版「功能不可用」类业务提示：签到活动未开启 / 未开放 / 已过期。
fn is_inactive_message(message: &str) -> bool {
    let raw = message.to_lowercase();
    [
        "未开启",
        "未开放",
        "已过期",
        "inactive",
        "not enabled",
        "not available",
    ]
    .iter()
    .any(|keyword| raw.contains(keyword))
}

/// 执行签到（POST daily-checkin）；服务端返回已签到提示按成功处理。
pub async fn perform_checkin(account: &Value) -> Value {
    let variant = variant_of(account);
    let resp = checkin_request("/daily-checkin", account).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 || code == 200 {
        return json!({"ok": true, "raw": resp.get("data").cloned().unwrap_or_else(|| json!({}))});
    }
    let msg = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .and_then(|v| v.as_str())
        .unwrap_or(&format!("code={code}"))
        .to_string();
    // 幂等业务码（已签到）保持既有 `already` 语义，两档位一致。
    if msg.contains("已签到") || msg.to_lowercase().contains("repeat") {
        return json!({"ok": true, "already": true, "message": msg});
    }
    // 国际版「功能未开启 / 未开放 / 已过期 / inactive」= 该档位未开放签到，
    // 归类为新增结果 `inactive`：绝不伪造成 success。仅对国际版生效，
    // 国内版保持既有 error 归类（零回归）。
    if variant == WbVariant::Ai && is_inactive_message(&msg) {
        return json!({"ok": false, "inactive": true, "message": msg});
    }
    json!({"ok": false, "error": msg})
}

fn decide_from_status(status: &Value) -> StatusDecision {
    // 没有状态查询接口的档位允许直接提交一次 daily-checkin，而不是把账号判成失败
    // 并让调度反复重试。安全性来自 daily-checkin 自身的幂等性——重复提交会返回
    // 「已签到」，因此这里不可能产生重复签到；结果也不会被伪造成 success。
    // 国际版当前在 `checkin_account` 入口即被跳过，此分支保留为防御。
    if status.get("statusUnsupported").and_then(Value::as_bool) == Some(true) {
        return StatusDecision::Submit;
    }
    if status.get("ok").and_then(Value::as_bool) != Some(true) {
        return StatusDecision::Error(
            status
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("查询签到状态失败")
                .to_string(),
        );
    }
    if status.get("todayCheckedIn").and_then(Value::as_bool) == Some(true) {
        StatusDecision::Already
    } else {
        StatusDecision::Submit
    }
}

/// 对单个账号执行完整签到流程：惰性刷新 → 查状态 → 未签到时提交 → 写提交日志。
pub async fn checkin_account(account: &Value) -> Value {
    let Some(_account_guard) = AccountRunGuard::try_acquire(account) else {
        return json!({"result": "error", "error": "该账号正在签到，请稍后再试"});
    };
    // 国际版没有签到接口：自动周期、一键签到与单账号签到的公开入口都在这里汇聚，
    // 统一短路以确保不向任何签到接口发起请求。
    if !variant_of(account).supports_checkin() {
        return json!({"result": "skipped", "reason": "unsupported_variant"});
    }
    let cfg = load_checkin_config();
    let acc = ensure_fresh_token(account.clone(), &cfg).await;
    let variant = variant_of(&acc).as_str();
    let status = get_checkin_status(&acc).await;
    match decide_from_status(&status) {
        StatusDecision::Already => return json!({"result": "already"}),
        StatusDecision::Error(error) => {
            return json!({"result": "error", "error": error});
        }
        StatusDecision::Submit => {}
    }

    // Only this branch submits daily-checkin, so only its outcome is eligible
    // for the sign-in log.
    let entry = json!({
        "ts": now_ms(),
        "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
        "email": account_display_name(&acc),
        "variant": variant,
    });
    let res = perform_checkin(&acc).await;
    if res.get("inactive").and_then(|v| v.as_bool()) == Some(true) {
        // inactive 语义（新增）：既不能记为成功，也不应让统计页计为失败，
        // 因此**不写签到日志**（写了必然被算作 success 或 failed 之一），
        // 也不做任何重试；结果对象显式带 inactive: true 供上层区分。
        return json!({
            "result": "inactive",
            "inactive": true,
            "message": res.get("message").cloned().unwrap_or(Value::Null),
        });
    }
    let result = if res.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        if res.get("already").and_then(|v| v.as_bool()) == Some(true) {
            "already"
        } else {
            "success"
        }
    } else {
        "error"
    };
    let error = if result == "error" {
        res.get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };
    let mut entry_map = json!({
        "result": result,
        "ts": entry["ts"],
        "accountId": entry["accountId"],
        "email": entry["email"],
        "variant": entry["variant"],
    });
    if let Some(e) = error.clone() {
        entry_map["error"] = json!(e);
    }
    add_checkin_log(&entry_map);
    json!({"result": result, "error": error})
}

pub fn date_str(ts_ms: Option<i64>) -> String {
    let dt = Local::now();
    if let Some(ms) = ts_ms {
        let secs = ms / 1000;
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| dt.format("%Y-%m-%d").to_string())
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// 执行一轮自动签到。启动与周期轮次均逐账号查询服务端状态。
///
/// 只遍历支持签到的档位（国际版没有签到接口，绝不发起请求）。并发锁防止与
/// 手动签到/上一轮重复运行。
pub async fn run_checkin_cycle(_mode: CheckinCycleMode) -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&CHECKIN_RUNNING) else {
        return json!({"status": "skipped", "reason": "already_running"});
    };
    let cfg = load_checkin_config();
    if cfg.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
        return json!({"status": "disabled"});
    }
    let accounts: Vec<Value> = load_accounts()
        .into_iter()
        .filter(|acc| variant_of(acc).supports_checkin())
        .collect();
    if accounts.is_empty() {
        return json!({"status": "no_accounts"});
    }
    let mut summary = json!({"status": "ok", "accounts": []});
    for acc in accounts {
        let result = checkin_account(&acc).await;
        let mut row = json!({
            "email": account_display_name(&acc),
            "result": result.get("result").cloned().unwrap_or(Value::Null),
            "error": result.get("error").cloned().unwrap_or(Value::Null),
        });
        // 只在 inactive 时追加标记：国内版结果行结构与改造前逐字一致。
        if result.get("inactive").and_then(Value::as_bool) == Some(true) {
            row["inactive"] = json!(true);
        }
        summary["accounts"].as_array_mut().unwrap().push(row);
    }
    summary
}

/// True when every stored account has a today's log of `success` or `already`.
///
/// Empty account list is false so the tray keeps offering 一键签到.
pub fn all_accounts_checked_in_today() -> bool {
    accounts_checked_in_today(&load_accounts(), &load_checkin_logs(), &date_str(None))
}

/// 判定「今天是否所有应签到的账号都已签到」。
///
/// 只有支持签到的档位（见 `WbVariant::supports_checkin`）参与判定：国际版账号不会
/// 产生签到日志，若把它们算进来，托盘会永远显示「可签到」。
/// 有账号但没有任何档位需要签到（例如只装了国际版）时视为无需签到，返回 true；
/// 账号库为空仍返回 false，保留「一键签到」入口。
pub fn accounts_checked_in_today(accounts: &[Value], logs: &[Value], today: &str) -> bool {
    if accounts.is_empty() {
        return false;
    }
    let pending: Vec<&Value> = accounts
        .iter()
        .filter(|account| variant_of(account).supports_checkin())
        .collect();
    if pending.is_empty() {
        return true;
    }
    pending.iter().all(|account| {
        let Some(id) = account.get("id").and_then(Value::as_str) else {
            return false;
        };
        latest_today_result(logs, id, today)
            .map(|result| result == "success" || result == "already")
            .unwrap_or(false)
    })
}

/// 给签到日志行补齐档位（宿主按档位过滤用）。
///
/// 新写入的日志自带 `variant`；历史行按当前账号库回填，账号已删除或缺失时按
/// 国内版解释（缺省即 cn，见 design D2）。纯函数，便于单测。
pub fn checkin_logs_with_variant(logs: &[Value], accounts: &[Value]) -> Vec<Value> {
    let mut known: HashMap<String, &'static str> = HashMap::new();
    for account in accounts {
        if let Some(id) = account.get("id").and_then(Value::as_str) {
            known.insert(id.to_string(), variant_of(account).as_str());
        }
    }
    logs.iter()
        .map(|entry| {
            let mut row = entry.clone();
            if row.get("variant").and_then(Value::as_str).is_none() {
                let fallback = row
                    .get("accountId")
                    .and_then(Value::as_str)
                    .and_then(|id| known.get(id).copied())
                    .unwrap_or_else(|| WbVariant::parse(None).as_str());
                row["variant"] = json!(fallback);
            }
            row
        })
        .collect()
}

/// 读取签到日志并补齐档位字段。
pub fn load_checkin_logs_with_variant() -> Vec<Value> {
    checkin_logs_with_variant(&load_checkin_logs(), &load_accounts())
}

fn latest_today_result<'a>(logs: &'a [Value], account_id: &str, today: &str) -> Option<&'a str> {
    logs.iter()
        .rev()
        .find(|entry| {
            entry.get("accountId").and_then(Value::as_str) == Some(account_id)
                && date_str(entry.get("ts").and_then(Value::as_i64)) == today
        })
        .and_then(|entry| entry.get("result").and_then(Value::as_str))
}

/// 对全部账号立即签到（前端一键签到）。
///
/// `variant = None` 覆盖全部档位（自动周期与设置页「立即签到」语义）；
/// 显式传入时只处理该档位（账号页按当前档位触发，避免跨档位误签到）。
/// 无论哪种取值，都只处理支持签到的档位：国际版没有签到接口，绝不发起请求。
pub async fn run_checkin_all(variant: Option<WbVariant>) -> Value {
    let Some(_guard) = RunFlagGuard::try_acquire(&CHECKIN_RUNNING) else {
        return json!({"accounts": [], "status": "skipped", "reason": "already_running"});
    };
    let accounts: Vec<Value> = load_accounts()
        .into_iter()
        .filter(|acc| {
            let acc_variant = variant_of(acc);
            acc_variant.supports_checkin() && variant.is_none_or(|target| acc_variant == target)
        })
        .collect();
    let mut results: Vec<Value> = Vec::new();
    for acc in accounts {
        let r = checkin_account(&acc).await;
        let mut row = json!({
            "accountId": acc.get("id").cloned().unwrap_or(Value::Null),
            "email": account_display_name(&acc),
            "result": r.get("result").cloned().unwrap_or(Value::Null),
            "error": r.get("error").cloned().unwrap_or(Value::Null),
        });
        if r.get("inactive").and_then(Value::as_bool) == Some(true) {
            row["inactive"] = json!(true);
        }
        results.push(row);
    }
    json!({"accounts": results})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_status_returns_already_without_submission() {
        assert_eq!(
            decide_from_status(&json!({"ok": true, "todayCheckedIn": true})),
            StatusDecision::Already
        );
    }

    #[test]
    fn failed_status_returns_error_without_submission() {
        assert_eq!(
            decide_from_status(&json!({"ok": false, "error": "offline"})),
            StatusDecision::Error("offline".to_string())
        );
    }

    #[test]
    fn unchecked_status_submits() {
        assert_eq!(
            decide_from_status(&json!({"ok": true, "todayCheckedIn": false})),
            StatusDecision::Submit
        );
    }

    /// 有意扩展：该档位没有状态接口时允许直接提交一次（daily-checkin 自身幂等）。
    #[test]
    fn status_unsupported_variant_submits_once() {
        assert_eq!(
            decide_from_status(&json!({
                "ok": false,
                "statusUnsupported": true,
                "error": "该档位暂无签到状态接口"
            })),
            StatusDecision::Submit
        );
        // 其它失败仍然不进提交流程。
        assert_eq!(
            decide_from_status(&json!({"ok": false, "statusUnsupported": false, "error": "x"})),
            StatusDecision::Error("x".to_string())
        );
    }

    #[test]
    fn inactive_messages_are_recognized() {
        for message in [
            "功能未开启",
            "签到未开放",
            "活动已过期",
            "inactive",
            "Feature not enabled",
            "not available",
        ] {
            assert!(is_inactive_message(message), "{message}");
        }
        assert!(!is_inactive_message("签到成功"));
        assert!(!is_inactive_message("系统繁忙，请稍后重试"));
        assert!(!is_inactive_message(""));
    }

    /// 国际版「未开放 / 已过期」类业务码不得触发 token 刷新与重试（P1-1）。
    ///
    /// `checkin_request_once` 的刷新/重试是唯一会产生副作用的分支，这里直接对
    /// 该分支的前置判定（纯函数）做断言：命中即原样返回，不会走到
    /// `refresh_account_token` + 重发。
    #[test]
    fn ai_inactive_response_skips_token_refresh_and_retry() {
        for resp in [
            json!({"code": 10011, "message": "签到活动已过期"}),
            json!({"code": 1, "msg": "签到未开放"}),
            json!({"code": 1, "message": "Feature not enabled"}),
        ] {
            assert!(
                skips_refresh_before_retry(WbVariant::Ai, &resp),
                "国际版 inactive 响应必须先短路: {resp}"
            );
            // 同一响应确实会被归类为 inactive（两个判定词表一致）。
            let msg = resp
                .get("message")
                .or_else(|| resp.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap();
            assert!(is_inactive_message(msg), "{msg}");
        }

        // 国内版语义逐字不变：「已过期」仍走既有 is_unauthorized 分支（允许刷新重试）。
        let cn_resp = json!({"code": 10011, "message": "签到活动已过期"});
        assert!(!skips_refresh_before_retry(WbVariant::Cn, &cn_resp));
        assert!(is_unauthorized(&cn_resp));

        // 国际版真正的鉴权失败仍然保留「一次刷新 + 一次重试」。
        assert!(!skips_refresh_before_retry(
            WbVariant::Ai,
            &json!({"code": 401, "message": "token 失效"})
        ));
        // 成功响应不短路（幂等「已签到」照常返回）。
        assert!(!skips_refresh_before_retry(
            WbVariant::Ai,
            &json!({"code": 0, "message": "签到成功"})
        ));
    }

    /// 国际版不调用国内版专有的状态接口；国内版保持两次尝试的顺序。
    #[tokio::test]
    async fn ai_account_does_not_call_cn_status_endpoints() {
        let ai = json!({"id": "ai-1", "uid": "u-1", "variant": "ai"});
        let status = get_checkin_status(&ai).await;
        assert_eq!(status["ok"], false);
        assert_eq!(status["statusUnsupported"], true);
        assert!(status.get("raw").is_none());
        // 没有产生任何状态查询响应（未发请求）。
        assert!(status.get("todayCheckedIn").is_none());
    }

    /// 国际版没有签到接口：入口即跳过，绝不发起任何请求。
    ///
    /// 守卫位于 `load_checkin_config` / `ensure_fresh_token` / 状态查询之前，
    /// 因此这里既不会触发 token 刷新，也不会触碰任何签到接口。
    #[tokio::test]
    async fn ai_account_checkin_is_skipped_without_requests() {
        let ai = json!({"id": "ai-skip-checkin", "uid": "u-ai", "variant": "ai"});
        let result = checkin_account(&ai).await;
        assert_eq!(result["result"], "skipped");
        assert_eq!(result["reason"], "unsupported_variant");
        // 不伪装成成功、失败或 inactive。
        assert!(result.get("error").is_none());
        assert!(result.get("inactive").is_none());
    }

    /// 状态成功响应解析保持原有结构（国内版零回归）。
    #[test]
    fn status_from_response_matches_legacy_shape() {
        assert_eq!(status_from_response(&json!({"code": 500})), None);
        let ok = status_from_response(&json!({
            "code": 0,
            "data": {"today_checked_in": true, "extra": 1}
        }))
        .expect("成功响应应解析");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["todayCheckedIn"], true);
        assert_eq!(ok["raw"]["extra"], 1);

        let snake = status_from_response(&json!({
            "code": 200,
            "data": {"todayCheckedIn": false}
        }))
        .expect("camelCase 也应解析");
        assert_eq!(snake["todayCheckedIn"], false);
    }

    /// 路径候选：国际版先 /billing/meter/... 再回落 /v2/billing/meter/...。
    #[test]
    fn checkin_path_candidates_are_variant_specific() {
        assert_eq!(
            variant_of(&json!({"variant": "ai"}))
                .billing_paths(&format!("{CHECKIN_API_PREFIX}/daily-checkin")),
            vec![
                "/billing/meter/daily-checkin",
                "/v2/billing/meter/daily-checkin"
            ]
        );
        assert_eq!(
            variant_of(&json!({})).billing_paths(&format!("{CHECKIN_API_PREFIX}/daily-checkin")),
            vec!["/v2/billing/meter/daily-checkin"]
        );
    }

    #[test]
    fn same_account_cannot_acquire_two_operation_guards() {
        let account = json!({"id": "checkin-guard-test-account"});
        let first = AccountRunGuard::try_acquire(&account).expect("first operation acquires guard");
        assert!(AccountRunGuard::try_acquire(&account).is_none());
        drop(first);
        assert!(AccountRunGuard::try_acquire(&account).is_some());
    }

    #[tokio::test]
    async fn manual_all_reports_busy_when_cycle_is_running() {
        let _cycle_guard =
            RunFlagGuard::try_acquire(&CHECKIN_RUNNING).expect("test acquires cycle guard");
        let result = run_checkin_all(None).await;

        assert_eq!(result["accounts"], json!([]));
        assert_eq!(result["status"], "skipped");
        assert_eq!(result["reason"], "already_running");
    }

    #[test]
    fn is_unauthorized_detects_code() {
        assert!(is_unauthorized(&json!({"code": 401})));
        assert!(is_unauthorized(&json!({"code": 403})));
        assert!(!is_unauthorized(&json!({"code": 0})));
    }

    #[test]
    fn checked_in_today_requires_every_account() {
        let accounts = vec![json!({"id": "a"}), json!({"id": "b"})];
        let logs = vec![
            json!({"accountId": "a", "result": "success", "ts": 1_700_000_000_000_i64}),
            json!({"accountId": "b", "result": "already", "ts": 1_700_000_100_000_i64}),
        ];
        let today = date_str(Some(1_700_000_000_000));
        assert!(accounts_checked_in_today(&accounts, &logs, &today));
    }

    #[test]
    fn checked_in_today_false_when_one_failed_last() {
        let accounts = vec![json!({"id": "a"})];
        let logs = vec![
            json!({"accountId": "a", "result": "success", "ts": 1_700_000_000_000_i64}),
            json!({"accountId": "a", "result": "error", "ts": 1_700_000_200_000_i64}),
        ];
        let today = date_str(Some(1_700_000_200_000));
        assert!(!accounts_checked_in_today(&accounts, &logs, &today));
    }

    #[test]
    fn checked_in_today_false_when_empty_or_missing() {
        assert!(!accounts_checked_in_today(&[], &[], "2026-08-19"));
        let accounts = vec![json!({"id": "a"})];
        assert!(!accounts_checked_in_today(&accounts, &[], "2026-08-19"));
    }

    /// 国际版账号不会有签到日志，不得让托盘永远显示「可签到」。
    #[test]
    fn checked_in_today_ignores_variants_without_checkin() {
        let today = date_str(Some(1_700_000_000_000));
        let logs = vec![json!({
            "accountId": "cn-1",
            "result": "success",
            "ts": 1_700_000_000_000_i64
        })];

        // 仅国际版账号：没有待签到项，不再提示「可签到」。
        let ai_only = vec![json!({"id": "ai-1", "variant": "ai"})];
        assert!(accounts_checked_in_today(&ai_only, &[], &today));

        // 国内版已签 + 国际版无日志：国际版不拖累判定。
        let mixed = vec![
            json!({"id": "cn-1", "variant": "cn"}),
            json!({"id": "ai-1", "variant": "ai"}),
        ];
        assert!(accounts_checked_in_today(&mixed, &logs, &today));

        // 国内版未签 + 国际版无日志：仍需签到。
        let pending_cn = vec![
            json!({"id": "cn-2", "variant": "cn"}),
            json!({"id": "ai-1", "variant": "ai"}),
        ];
        assert!(!accounts_checked_in_today(&pending_cn, &logs, &today));
    }

    #[test]
    fn logs_are_tagged_with_variant_and_legacy_rows_fall_back_to_cn() {
        let accounts = vec![
            json!({"id": "cn-1"}),
            json!({"id": "ai-1", "variant": "ai"}),
        ];
        let logs = vec![
            // 新日志自带档位。
            json!({"accountId": "ai-1", "result": "success", "variant": "ai"}),
            // 历史日志按账号库回填。
            json!({"accountId": "ai-1", "result": "success"}),
            json!({"accountId": "cn-1", "result": "success"}),
            // 账号已删除：按缺省国内版解释。
            json!({"accountId": "gone", "result": "success"}),
            json!({"email": "legacy@example.com", "result": "success"}),
        ];

        let rows = checkin_logs_with_variant(&logs, &accounts);
        assert_eq!(rows.len(), logs.len());
        assert_eq!(rows[0]["variant"], "ai");
        assert_eq!(rows[1]["variant"], "ai");
        assert_eq!(rows[2]["variant"], "cn");
        assert_eq!(rows[3]["variant"], "cn");
        assert_eq!(rows[4]["variant"], "cn");
    }
}
