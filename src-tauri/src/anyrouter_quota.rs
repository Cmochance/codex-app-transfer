//! anyrouter 已用额度查询(MOC-211)。
//!
//! anyrouter.top 是 new-api/one-api 网关(Claude 中转)。账户真实**剩余**余额在
//! `/api/user/self`,但该路径被阿里 ESA 反爬挑战挡住(curl/key 取不到)。能用推理同一把
//! key(Bearer)干净拿到的是 OpenAI 兼容 billing 端点的**已用额度**:
//! `GET /v1/dashboard/billing/usage?start_date&end_date` → `{"total_usage": <美分>}`。
//! (token 多为无限额度,`/billing/subscription` 的 hard_limit 是 sentinel,算不出 remaining;
//! 故展示「已用额度 $X」而非剩余。)用户确认走此路(MOC-211 调研 B)。
//!
//! 余额是钱、无百分比 → 数值条目 [`QuotaStat`],不画进度条。

use serde_json::Value;

use crate::provider_quota::{ProviderQuota, QuotaStat};

#[derive(Debug)]
pub enum QuotaError {
    Auth(reqwest::StatusCode),
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "anyrouter usage 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 从 `/v1/dashboard/billing/usage` 提取已用额度条目「已用额度 $120.49」。纯函数,可测。
/// `total_usage` 是**美分**(OpenAI billing 口径)→ /100 得美元。缺失 → None。
pub fn parse_anyrouter_usage(json: &Value) -> Option<QuotaStat> {
    let cents = json.get("total_usage").and_then(Value::as_f64)?;
    let usd = cents / 100.0;
    Some(QuotaStat {
        label: "已用额度".into(),
        value: format!("${usd:.2}"),
    })
}

/// 调 billing usage 取已用额度。`base_host` = provider.baseUrl 的 host(`anyrouter.top`);
/// `today` = 本地当天 `YYYY-MM-DD`(end_date,由 caller 传入以便单测 parse 纯函数)。
pub async fn fetch_anyrouter_usage(
    http: &reqwest::Client,
    base_host: &str,
    api_key: &str,
    today: &str,
) -> Result<ProviderQuota, QuotaError> {
    // 宽区间(早于任何账户创建)→ 拿累计已用;new-api 按 [start,end] 统计。
    let url = format!(
        "https://{base_host}/v1/dashboard/billing/usage?start_date=2020-01-01&end_date={today}"
    );
    let resp = http
        .get(&url)
        .bearer_auth(api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("anyrouter usage 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "anyrouter usage 非 2xx: {status}"
        )));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("anyrouter usage 解析失败: {e}")))?;
    let mut q = ProviderQuota::default();
    if let Some(stat) = parse_anyrouter_usage(&json) {
        q.stats.push(stat);
    }
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_total_usage_cents_to_usd() {
        // 真机响应(2026-06-14):total_usage 美分 12049.345 → $120.49
        let j = json!({"object":"list","total_usage":12049.345});
        let s = parse_anyrouter_usage(&j).expect("usage");
        assert_eq!(s.label, "已用额度");
        assert_eq!(s.value, "$120.49");
    }

    #[test]
    fn zero_usage() {
        assert_eq!(
            parse_anyrouter_usage(&json!({"total_usage":0})).unwrap().value,
            "$0.00"
        );
    }

    #[test]
    fn missing_usage_yields_none() {
        assert!(parse_anyrouter_usage(&json!({})).is_none());
    }
}
