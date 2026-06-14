//! 小米 MiMo Token Plan 套餐用量查询(MOC-211)。
//!
//! 用量在 `platform.xiaomimimo.com` 控制台 API(**固定 host**,非 `token-plan-*` 推理 host),
//! 认证靠小米账号网页 session cookie(见 [`crate::mimo_session`];httpOnly serviceToken,tp-
//! 推理 key 查不到额度,实测带 key 仍 401)。MiMo 是**月度套餐**单窗口(自然月重置),与
//! antigravity/GLM 的 5h/周双窗口是两套独立显示体系。
//!
//! 端点(浏览器抓 SPA bundle + 真机实证 2026-06-14):
//! - `GET /api/v1/tokenPlan/usage` → `data.usage.items[]`,取 `plan_total_token`(套餐本体,
//!   即控制台「当前套餐用量」那条;`compensation_total_token` 补偿积分 / `monthUsage` 月总均忽略)。
//!   `percent` 是 **fraction**(0.44 = 已用 44%,与 GLM 的 0-100 percentage 口径不同)。
//! - `GET /api/v1/tokenPlan/detail` → `currentPeriodEnd`(套餐重置时刻,UTC)。

use serde_json::Value;

use crate::provider_quota::{ProviderQuota, QuotaWindow};

/// 用量 / 套餐详情都在控制台域名(与推理 host 无关)。
const PLATFORM_HOST: &str = "platform.xiaomimimo.com";

/// fetch 失败分类:Auth = session 失效(需重新登录,caller 清 cookie)/ Transient = 瞬时。
#[derive(Debug)]
pub enum QuotaError {
    Auth,
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth => write!(f, "MiMo session 失效(需重新登录小米账号)"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 从 `/tokenPlan/usage` 提取「套餐用量」(plan_total_token)**剩余**百分比。纯函数,可测。
/// `percent` 是 fraction → 剩余 = `(1 - percent) * 100`;无 percent 时回退 `1 - used/limit`。
/// 找不到该 item → None(不显额度行,而非显错值)。
pub fn parse_plan_remaining(json: &Value) -> Option<f64> {
    let items = json
        .get("data")
        .and_then(|d| d.get("usage"))
        .and_then(|u| u.get("items"))
        .and_then(|v| v.as_array())?;
    let item = items
        .iter()
        .find(|i| i.get("name").and_then(|v| v.as_str()) == Some("plan_total_token"))?;
    let used_frac = match item.get("percent").and_then(Value::as_f64) {
        Some(p) => p,
        None => {
            let used = item.get("used").and_then(Value::as_f64)?;
            let limit = item
                .get("limit")
                .and_then(Value::as_f64)
                .filter(|l| *l > 0.0)?;
            used / limit
        }
    };
    Some(((1.0 - used_frac) * 100.0).clamp(0.0, 100.0))
}

/// `currentPeriodEnd`(如 `2026-06-27 23:59:59`,UTC)→ RFC3339,供 injector 的
/// `fmt_reset_local` 统一转本地。解析失败 → None(不显刷新时间)。
fn period_end_to_rfc3339(s: &str) -> Option<String> {
    let naive = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    Some(naive.and_utc().to_rfc3339())
}

/// 带 session cookie 查 MiMo 套餐用量,产出单窗口「套餐用量」。best-effort:detail(重置时刻)
/// 失败不影响主额度展示。
pub async fn fetch_mimo_quota_summary(
    http: &reqwest::Client,
    cookie: &str,
) -> Result<ProviderQuota, QuotaError> {
    let usage = get_json(
        http,
        &format!("https://{PLATFORM_HOST}/api/v1/tokenPlan/usage"),
        cookie,
    )
    .await?;
    let Some(remaining) = parse_plan_remaining(&usage) else {
        return Ok(ProviderQuota::default());
    };
    // 套餐重置时刻(best-effort,失败就不显刷新时间)。
    let reset = get_json(
        http,
        &format!("https://{PLATFORM_HOST}/api/v1/tokenPlan/detail"),
        cookie,
    )
    .await
    .ok()
    .and_then(|d| {
        d.get("data")
            .and_then(|x| x.get("currentPeriodEnd"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    })
    .and_then(|s| period_end_to_rfc3339(&s));
    Ok(ProviderQuota {
        windows: vec![QuotaWindow {
            label: "套餐用量".into(),
            remaining_percent: remaining,
            reset_rfc3339: reset,
        }],
    })
}

/// GET + 带 Cookie 头 + 鉴权/瞬时错分类。HTTP 401/403 或 body `code==401` → Auth。
async fn get_json(http: &reqwest::Client, url: &str, cookie: &str) -> Result<Value, QuotaError> {
    let resp = http
        .get(url)
        .header("Cookie", cookie)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("MiMo 用量请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth);
        }
        return Err(QuotaError::Transient(format!("MiMo 用量非 2xx: {status}")));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("MiMo 用量解析失败: {e}")))?;
    // 控制台 API 偶尔 200 + body code=401(session 失效)→ 也判 Auth。
    if json.get("code").and_then(Value::as_i64) == Some(401) {
        return Err(QuotaError::Auth);
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 真机响应骨架(2026-06-14):data.usage.items = 套餐本体 + 补偿积分。
    fn real_usage() -> Value {
        json!({
            "code": 0, "message": "",
            "data": {
                "monthUsage": {"percent": 0.7291, "items": [
                    {"name":"month_total_token","used":8020433896i64,"limit":11000000000i64,"percent":0.7291}
                ]},
                "usage": {"percent": 0.44, "items": [
                    {"name":"plan_total_token","used":4841862467i64,"limit":11000000000i64,"percent":0.44},
                    {"name":"compensation_total_token","used":3178571429i64,"limit":3178571429i64,"percent":1.0}
                ]}
            }
        })
    }

    #[test]
    fn takes_plan_total_token_remaining() {
        // 套餐本体已用 44% → 剩 56%(忽略补偿积分 100% 与月总 72.91%)
        let r = parse_plan_remaining(&real_usage()).expect("plan remaining");
        assert!((r - 56.0).abs() < 1e-6, "已用 0.44 → 剩 56,实得 {r}");
    }

    #[test]
    fn falls_back_to_used_over_limit_when_no_percent() {
        let j = json!({"data":{"usage":{"items":[
            {"name":"plan_total_token","used":3000i64,"limit":4000i64}
        ]}}});
        let r = parse_plan_remaining(&j).expect("remaining");
        assert!((r - 25.0).abs() < 1e-6, "3000/4000 已用 → 剩 25");
    }

    #[test]
    fn missing_plan_item_yields_none() {
        assert!(parse_plan_remaining(&json!({})).is_none());
        // 只有补偿积分、无套餐本体 → None(不显)
        let j = json!({"data":{"usage":{"items":[{"name":"compensation_total_token","percent":1.0}]}}});
        assert!(parse_plan_remaining(&j).is_none());
    }

    #[test]
    fn period_end_parses_to_rfc3339() {
        let s = period_end_to_rfc3339("2026-06-27 23:59:59").expect("rfc3339");
        assert!(chrono::DateTime::parse_from_rfc3339(&s).is_ok());
        assert!(s.starts_with("2026-06-27T23:59:59"), "应按 UTC 解析,实得 {s}");
    }
}
