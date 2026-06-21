//! Kimi Code 套餐用量抓取(CAT-256 后续)。
//!
//! 经 connect-RPC(JSON)`POST https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/
//! GetSubscriptionStat` 查(扒 console JS bundle + 解 protobuf descriptor 定位)。鉴权用
//! `Authorization: Bearer <access_token>`(localStorage 抓的,见 [`crate::kimi_session`])。
//!
//! 真机响应(实证):
//! ```json
//! {"ratelimit5h":{"enabled":true,"resetTime":"...Z"},
//!  "subscriptionBalance":{"feature":"FEATURE_OMNI","amountUsedRatio":0.0132,
//!    "kimiCodeUsedRatio":0,"expireTime":"...Z"}}
//! ```
//!
//! **Kimi 额度模型与 OpenCode 不同**:`ratelimit5h/7d` 是**请求频率限**(只有 enabled + resetTime、
//! 无百分比);`subscriptionBalance.amountUsedRatio` 才是带百分比的额度。故主映射
//! `subscriptionBalance` → 一个「套餐用量」窗口(剩余 = 100·(1-ratio),expireTime 重置,同 MiMo 月槽);
//! Code 套餐订阅者的 `kimiCodeUsedRatio` 非 0 时额外出「Kimi Code 用量」窗口。rate-limit 若带 usage
//! 比例则防御性出 5h/周窗口,否则跳过(无百分比不画条)。

use crate::provider_quota::{ProviderQuota, RollingWindows};

/// 抓取错误:`Auth`=token 失效(需重登,清存储 token);`Transient`=网络/瞬时(留旧缓存重试)。
pub enum QuotaError {
    Auth,
    Transient(String),
}

const ENDPOINT: &str =
    "https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStat";

/// 取 0-1 的「已用比例」→ 剩余百分比(0-100)。
fn remaining_from_used_ratio(v: &serde_json::Value, key: &str) -> Option<f64> {
    let used = v.get(key).and_then(serde_json::Value::as_f64)?;
    Some(((1.0 - used) * 100.0).clamp(0.0, 100.0))
}

/// 把 `GetSubscriptionStat` JSON 解析成三槽。纯函数,可测。
pub fn parse_subscription_stat(json: &serde_json::Value) -> RollingWindows {
    let mut rolling = RollingWindows::default();

    // 主额度:subscriptionBalance.amountUsedRatio(整体套餐用量)→ 月槽「套餐用量」(expireTime 重置)。
    if let Some(bal) = json.get("subscriptionBalance") {
        let reset = bal
            .get("expireTime")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        // Code 套餐订阅者:kimiCodeUsedRatio 非 0 → 优先显「Kimi Code 用量」;否则显整体「套餐用量」。
        let code_ratio = bal
            .get("kimiCodeUsedRatio")
            .and_then(serde_json::Value::as_f64);
        match code_ratio {
            Some(r) if r > 0.0 => {
                if let Some(rem) = remaining_from_used_ratio(bal, "kimiCodeUsedRatio") {
                    rolling = rolling.monthly_labeled("Kimi Code 用量", rem, reset.clone());
                }
            }
            _ => {
                if let Some(rem) = remaining_from_used_ratio(bal, "amountUsedRatio") {
                    rolling = rolling.monthly_labeled("套餐用量", rem, reset.clone());
                }
            }
        }
    }

    // rate-limit 窗口(5h / 7d):仅当带 usage 比例(usedRatio,Code 订阅者才有)才出条;
    // 只有 enabled+resetTime(无百分比)→ 跳过(画不了进度条)。防御性,兼容订阅者更丰富的响应。
    if let Some(rem) = json
        .get("ratelimit5h")
        .and_then(|w| remaining_from_used_ratio(w, "usedRatio"))
    {
        let reset = json
            .get("ratelimit5h")
            .and_then(|w| w.get("resetTime"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        rolling = rolling.five_hour(rem, reset);
    }
    if let Some(rem) = json
        .get("ratelimit7d")
        .and_then(|w| remaining_from_used_ratio(w, "usedRatio"))
    {
        let reset = json
            .get("ratelimit7d")
            .and_then(|w| w.get("resetTime"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        rolling = rolling.weekly(rem, reset);
    }
    rolling
}

/// 带 access_token(Bearer)查 Kimi Code 套餐用量。
pub async fn fetch_kimi_code_quota(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<ProviderQuota, QuotaError> {
    let resp = http
        .post(ENDPOINT)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", "Mozilla/5.0")
        .body("{}")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| QuotaError::Transient(e.to_string()))?;
    if !status.is_success() {
        // connect-RPC 失效:401/403 或 body code=unauthenticated/INVALID_AUTH_TOKEN → Auth(清 token 重登)。
        let lower = body.to_ascii_lowercase();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
            || lower.contains("unauthenticated")
            || lower.contains("invalid_auth_token")
        {
            return Err(QuotaError::Auth);
        }
        return Err(QuotaError::Transient(format!("HTTP {status}")));
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| QuotaError::Transient(e.to_string()))?;
    // 200 但 body 是 connect 错误帧(code:unauthenticated)→ Auth。
    if json.get("code").and_then(|v| v.as_str()) == Some("unauthenticated") {
        return Err(QuotaError::Auth);
    }
    let rolling = parse_subscription_stat(&json);
    Ok(ProviderQuota {
        rolling,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_subscription_balance_to_plan_window() {
        // 无 Code 套餐:用整体 amountUsedRatio → 套餐用量(已用 1.32% → 剩 98.68%)。
        let j = json!({
            "ratelimit5h": {"enabled": true, "resetTime": "2026-06-22T01:57:14Z"},
            "subscriptionBalance": {"amountUsedRatio": 0.0132, "kimiCodeUsedRatio": 0, "expireTime": "2026-06-23T00:00:00Z"}
        });
        let r = parse_subscription_stat(&j);
        let m = r.monthly.as_ref().expect("套餐用量窗口");
        assert_eq!(m.label, "套餐用量");
        assert!((m.remaining_percent - 98.68).abs() < 0.01);
        assert_eq!(m.reset_rfc3339.as_deref(), Some("2026-06-23T00:00:00Z"));
        // ratelimit5h 无 usedRatio → 不出 5h 窗口
        assert!(r.five_hour.is_none());
    }

    #[test]
    fn code_subscriber_uses_code_ratio_and_ratelimit() {
        let j = json!({
            "ratelimit5h": {"usedRatio": 0.4, "resetTime": "2026-06-22T05:00:00Z"},
            "ratelimit7d": {"usedRatio": 0.1, "resetTime": "2026-06-29T00:00:00Z"},
            "subscriptionBalance": {"amountUsedRatio": 0.5, "kimiCodeUsedRatio": 0.25, "expireTime": "2026-07-01T00:00:00Z"}
        });
        let r = parse_subscription_stat(&j);
        assert_eq!(r.monthly.as_ref().unwrap().label, "Kimi Code 用量");
        assert!((r.monthly.as_ref().unwrap().remaining_percent - 75.0).abs() < 0.01);
        assert!((r.five_hour.as_ref().unwrap().remaining_percent - 60.0).abs() < 0.01);
        assert!((r.weekly.as_ref().unwrap().remaining_percent - 90.0).abs() < 0.01);
    }
}
