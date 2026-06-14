//! DeepSeek 账户余额查询(MOC-211)。
//!
//! DeepSeek 是**充值余额制**(非订阅额度窗口),有官方文档化接口:
//! `GET https://api.deepseek.com/user/balance`,鉴权 `Authorization: Bearer <apiKey>`(与推理
//! 同一把 key)。响应(官方文档 + 真机实证 2026-06-14):
//! ```json
//! {"is_available":true,"balance_infos":[
//!   {"currency":"CNY","total_balance":"5.37","granted_balance":"0.00","topped_up_balance":"5.37"}]}
//! ```
//! 余额是「钱」、无上限/百分比 → 展示成纯数值条目 [`QuotaStat`]「余额 ¥5.37」,不画进度条。
//!
//! 端点 / 字段为 DeepSeek 官方公开 API,无需借鉴第三方实现。

use serde_json::Value;

use crate::provider_quota::{ProviderQuota, QuotaStat};

/// fetch 失败分类:Auth = key 失效 / Transient = 瞬时。
#[derive(Debug)]
pub enum QuotaError {
    Auth(reqwest::StatusCode),
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "DeepSeek balance 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

fn currency_symbol(code: &str) -> Option<&'static str> {
    match code {
        "CNY" => Some("¥"),
        "USD" => Some("$"),
        _ => None,
    }
}

/// 从 `/user/balance` 响应提取余额条目「余额 ¥5.37」。纯函数,可测。
/// 取 `balance_infos` 首条(通常一条,CNY 或 USD);缺失 → None(不显额度行)。
pub fn parse_deepseek_balance(json: &Value) -> Option<QuotaStat> {
    let info = json.get("balance_infos")?.as_array()?.first()?;
    let currency = info.get("currency").and_then(|v| v.as_str()).unwrap_or("");
    // total_balance 是字符串(如 "5.37")。
    let total = info.get("total_balance").and_then(|v| v.as_str())?;
    let value = match currency_symbol(currency) {
        Some(sym) => format!("{sym}{total}"),
        None if currency.is_empty() => total.to_string(),
        None => format!("{total} {currency}"),
    };
    Some(QuotaStat {
        label: "余额".into(),
        value,
    })
}

/// 调 `/user/balance` 取余额条目。`base_host` = provider.baseUrl 的 host(`api.deepseek.com`)。
pub async fn fetch_deepseek_balance(
    http: &reqwest::Client,
    base_host: &str,
    api_key: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!("https://{base_host}/user/balance");
    let resp = http
        .get(&url)
        .bearer_auth(api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("DeepSeek balance 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "DeepSeek balance 非 2xx: {status}"
        )));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("DeepSeek balance 解析失败: {e}")))?;
    let mut q = ProviderQuota::default();
    if let Some(stat) = parse_deepseek_balance(&json) {
        q.stats.push(stat);
    }
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cny_balance() {
        let j = json!({
            "is_available": true,
            "balance_infos": [
                {"currency":"CNY","total_balance":"5.37","granted_balance":"0.00","topped_up_balance":"5.37"}
            ]
        });
        let s = parse_deepseek_balance(&j).expect("balance");
        assert_eq!(s.label, "余额");
        assert_eq!(s.value, "¥5.37");
    }

    #[test]
    fn parses_usd_balance() {
        let j = json!({"balance_infos":[{"currency":"USD","total_balance":"12.00"}]});
        assert_eq!(parse_deepseek_balance(&j).unwrap().value, "$12.00");
    }

    #[test]
    fn unknown_currency_keeps_code() {
        let j = json!({"balance_infos":[{"currency":"EUR","total_balance":"9.90"}]});
        assert_eq!(parse_deepseek_balance(&j).unwrap().value, "9.90 EUR");
    }

    #[test]
    fn missing_balance_yields_none() {
        assert!(parse_deepseek_balance(&json!({})).is_none());
        assert!(parse_deepseek_balance(&json!({"balance_infos": []})).is_none());
    }
}
