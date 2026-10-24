//! HTTP 业务响应分类的共享口径。
//!
//! 签到、积分、旅行都曾各自实现一份「是否鉴权失败」，关键字与网关码处理逐次分叉。
//! 这里收敛为单一实现：新增网关码或文案时只改一处。

use serde_json::Value;

/// 是否属于「鉴权被拒」（用于触发一次刷新重试）。
///
/// 口径：
/// - 网关 WAF `10085` 是客户端指纹拦截，刷新无效 → 不算鉴权失败；
/// - 业务 `401` / `403` → 鉴权失败；
/// - 文案命中未授权/过期/登录失效类关键字 → 鉴权失败（兼容旧网关只回文案）。
///
/// 文案同时兼容 `message` / `msg` 与 `data.message` / `data.msg` 两种信封，
/// 与积分模块此前的私有实现保持一致。
pub fn is_unauthorized(resp: &Value) -> bool {
    let code = resp
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| {
            resp.get("data")
                .and_then(|data| data.get("code"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(-1);
    // 网关 WAF：客户端指纹拦截，不是 token 过期；刷新无效。
    if code == 10085 {
        return false;
    }
    if code == 401 || code == 403 {
        return true;
    }
    let message = resp
        .get("message")
        .or_else(|| resp.get("msg"))
        .or_else(|| resp.get("data").and_then(|value| value.get("message")))
        .or_else(|| resp.get("data").and_then(|value| value.get("msg")))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    ["unauthorized", "401", "登录", "失效", "过期", "token"]
        .iter()
        .any(|keyword| message.contains(keyword))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn business_auth_codes_are_unauthorized() {
        assert!(is_unauthorized(&json!({"code": 401})));
        assert!(is_unauthorized(&json!({"code": 403})));
        assert!(is_unauthorized(
            &json!({"code": 400, "message": "invalid token"})
        ));
        assert!(is_unauthorized(&json!({"code": 0, "msg": "登录已过期"})));
        assert!(is_unauthorized(&json!({"data": {"code": 401}})));
        assert!(is_unauthorized(
            &json!({"data": {"message": "refresh token expired"}})
        ));
    }

    #[test]
    fn gateway_fingerprint_and_business_errors_are_not_unauthorized() {
        // 10085 是网关指纹拦截，刷新无效。
        assert!(!is_unauthorized(
            &json!({"code": 10085, "message": "token 拦截"})
        ));
        assert!(!is_unauthorized(
            &json!({"code": 429, "message": "too many requests"})
        ));
        assert!(!is_unauthorized(
            &json!({"code": 404, "message": "route not found"})
        ));
        assert!(!is_unauthorized(&json!({})));
        // 10085 优先于文案关键字：文案里即使有 token 也不能当鉴权失败。
        assert!(!is_unauthorized(
            &json!({"code": 10085, "message": "token expired"})
        ));
    }
}
