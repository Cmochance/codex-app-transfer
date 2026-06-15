//! Kimi (月之暗面 / Moonshot) PAYG 账户余额查询。
//!
//! **仅对 `kimi (月之暗面)` PAYG provider 生效**(baseUrl host = `api.moonshot.cn` /
//! `api.moonshot.ai`)。与订阅制的 `kimi-code`(`api.kimi.com/coding`,5h 滚动额度、**无**余额
//! 接口)是**两个不同 provider**;后者 host 是 `api.kimi.com`,不会被本模块的 host gate 匹配
//! (gate 见 [`crate::codex_quota_injector`] 的 `active_moonshot_provider`)。
//!
//! Moonshot 是充值余额制,官方文档化接口:
//! `GET https://api.moonshot.{cn,ai}/v1/users/me/balance`,鉴权 `Authorization: Bearer <apiKey>`
//! (与推理同一把 key)。响应:
//! ```json
//! {"code":0,"data":{"available_balance":49.58,"voucher_balance":46.58,"cash_balance":3.0},"status":true}
//! ```
//! `available_balance` = 可用余额(= cash + voucher)。响应**无币种字段** → 按 host 推断
//! (`.cn`→¥/CNY,`.ai`→$/USD)。余额是「钱」、无上限/百分比 → 展示成数值条目 [`QuotaStat`]
//! 「余额 ¥49.58」,不画进度条(对齐 DeepSeek,见 [`crate::deepseek_quota`])。
//!
//! 端点 / 字段为 Moonshot 官方公开 API,无需借鉴第三方实现。

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
            QuotaError::Auth(s) => write!(f, "Moonshot balance 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 按 host 推断币种符号(响应无币种字段)。`.ai` 国际站记 USD,其余(`.cn`)记 CNY。
fn currency_symbol(base_host: &str) -> &'static str {
    if base_host.ends_with("moonshot.ai") {
        "$"
    } else {
        "¥"
    }
}

/// 取 `data.available_balance`(数字或字符串均容忍)。
fn available_balance(json: &Value) -> Option<f64> {
    match json.get("data")?.get("available_balance")? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 从 `/v1/users/me/balance` 响应提取余额条目「余额 ¥49.58」。纯函数,可测。
/// `symbol` 为币种符号(由 [`currency_symbol`] 按 host 定)。缺 `available_balance` → None。
pub fn parse_moonshot_balance(json: &Value, symbol: &str) -> Option<QuotaStat> {
    let avail = available_balance(json)?;
    Some(QuotaStat {
        label: "余额".into(),
        value: format!("{symbol}{avail:.2}"),
    })
}

/// 调 `/v1/users/me/balance` 取余额条目。`base_host` = provider.baseUrl 的 host
/// (`api.moonshot.cn` / `api.moonshot.ai`)。
pub async fn fetch_moonshot_balance(
    http: &reqwest::Client,
    base_host: &str,
    api_key: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!("https://{base_host}/v1/users/me/balance");
    let resp = http
        .get(&url)
        .bearer_auth(api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("Moonshot balance 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "Moonshot balance 非 2xx: {status}"
        )));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("Moonshot balance 解析失败: {e}")))?;
    let mut q = ProviderQuota::default();
    if let Some(stat) = parse_moonshot_balance(&json, currency_symbol(base_host)) {
        q.stats.push(stat);
    }
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cny_available_balance() {
        let j = json!({
            "code": 0,
            "data": {"available_balance": 49.58894, "voucher_balance": 46.58, "cash_balance": 3.0},
            "status": true
        });
        let s = parse_moonshot_balance(&j, "¥").expect("balance");
        assert_eq!(s.label, "余额");
        assert_eq!(s.value, "¥49.59");
    }

    #[test]
    fn tolerates_string_balance() {
        let j = json!({"data": {"available_balance": "12.5"}});
        assert_eq!(parse_moonshot_balance(&j, "$").unwrap().value, "$12.50");
    }

    #[test]
    fn host_picks_currency_symbol() {
        assert_eq!(currency_symbol("api.moonshot.cn"), "¥");
        assert_eq!(currency_symbol("api.moonshot.ai"), "$");
    }

    #[test]
    fn missing_balance_yields_none() {
        assert!(parse_moonshot_balance(&json!({}), "¥").is_none());
        assert!(parse_moonshot_balance(&json!({"data": {}}), "¥").is_none());
    }
}
