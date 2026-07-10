//! grok build(xAI grok CLI 编码后端)周额度查询(MOC-306)。
//!
//! grok CLI `/usage show` 命令打 `GET {base}/billing?format=credits`(base = provider.baseUrl =
//! `https://cli-chat-proxy.grok.com/v1`),鉴权 `Authorization: Bearer <access_token>` + 最小 grok-shell
//! 客户端头(`x-xai-token-auth: xai-grok-cli` / `x-userid` / `x-grok-client-version` / UA)。**抓包实证
//! 2026-07-09**(mitmproxy 观测真实 grok CLI)。响应:
//! ```json
//! {"config":{
//!   "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"…","end":"2026-07-11T02:57:36Z"},
//!   "creditUsagePercent":3.0,
//!   "productUsage":[{"product":"GrokBuild","usagePercent":2.0},{"product":"GrokChat","usagePercent":1.0}],
//!   "onDemandCap":{"val":0},"onDemandUsed":{"val":0},"prepaidBalance":{"val":0}, …}}
//! ```
//! grok **只有周额度**(`USAGE_PERIOD_TYPE_WEEKLY`):`creditUsagePercent` = 本周**总**已用 credit %,
//! 剩余 = `100 - used`;`currentPeriod.end` = reset 时刻。归一成单个**每周**窗口(`RollingWindows.weekly`,
//! 渲染器显「剩余 X% · <reset> 刷新」)。`productUsage`(GrokBuild/GrokChat 分产品用量)**不展示**——
//! 用户只要总额度(2026-07-09 反馈)。端点/字段为 grok CLI 官方客户端行为实证。

use serde_json::Value;

use crate::provider_quota::{ProviderQuota, RollingWindows};

/// fetch 失败分类:Auth = token 失效(401/403)/ Transient = 瞬时(网络/5xx/解析)。
#[derive(Debug)]
pub enum QuotaError {
    Auth(reqwest::StatusCode),
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "grok billing 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 从 `/billing?format=credits` 响应解析成周额度窗口。纯函数,可测。
/// `config.creditUsagePercent`(本周**总**已用%)→ 剩余 `100-used`;reset=`config.currentPeriod.end`。
/// 只显总额度(不拆 productUsage 分产品)。缺 `creditUsagePercent` → None(不显额度行)。
pub fn parse_grok_credits(json: &Value) -> Option<ProviderQuota> {
    let config = json.get("config")?;
    let used = config.get("creditUsagePercent").and_then(Value::as_f64)?;
    let remaining = (100.0 - used).clamp(0.0, 100.0);
    let reset = config
        .get("currentPeriod")
        .and_then(|p| p.get("end"))
        .and_then(Value::as_str)
        .map(String::from);
    // 普通周窗口:渲染器显「剩余 X% · <reset> 刷新」。
    let rolling = RollingWindows::default().weekly(remaining, reset);
    Some(ProviderQuota {
        rolling,
        ..Default::default()
    })
}

/// 调 `{PINNED_BASE_URL}/billing?format=credits` 取周额度。**base 钉死** grok 官方上游(不用
/// provider.baseUrl —— 用户可改/漂移会把 Bearer token 外泄到非官方 host,review GYJ)。`access_token`
/// 为有效 Bearer;`user_id` = 账号 uid(JWT sub,填 `x-userid`;空则不带该头)。
pub async fn fetch_grok_credits(
    http: &reqwest::Client,
    access_token: &str,
    user_id: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!(
        "{}/billing?format=credits",
        codex_app_transfer_gemini_oauth::PINNED_BASE_URL
    );
    let mut req = http
        .get(&url)
        // 最小 grok-shell 指纹(实证 billing 请求头集,比 /responses 精简,无会话/请求标识)。
        .bearer_auth(access_token)
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-grok-client-version", "0.2.93")
        .header("accept", "*/*")
        .header("user-agent", "grok-shell/0.2.93 (macos; aarch64)");
    if !user_id.is_empty() {
        req = req.header("x-userid", user_id);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("grok billing 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "grok billing 非 2xx: {status}"
        )));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("grok billing 解析失败: {e}")))?;
    parse_grok_credits(&json)
        .ok_or_else(|| QuotaError::Transient("grok billing 响应缺 creditUsagePercent".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({"config":{
            "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",
                "start":"2026-07-04T02:57:36.331252+00:00","end":"2026-07-11T02:57:36.331252+00:00"},
            "creditUsagePercent":3.0,
            "productUsage":[{"product":"GrokBuild","usagePercent":2.0},{"product":"GrokChat","usagePercent":1.0}],
            "onDemandCap":{"val":0},"onDemandUsed":{"val":0},"prepaidBalance":{"val":0}}})
    }

    #[test]
    fn parses_weekly_remaining_and_reset() {
        let q = parse_grok_credits(&sample()).expect("quota");
        let w = q.rolling.weekly.as_ref().expect("weekly window");
        assert_eq!(w.label, "每周额度");
        assert_eq!(w.remaining_percent, 97.0, "used 3% → 剩 97%");
        assert_eq!(
            w.reset_rfc3339.as_deref(),
            Some("2026-07-11T02:57:36.331252+00:00")
        );
        // 只显总额度:不拆分产品,无自定义 detail(渲染器显默认「剩余 X%」)。
        assert!(w.detail.is_none(), "只显总额度,不附分产品 detail");
        // grok 只有周额度:5h / 月槽为空。
        assert!(q.rolling.five_hour.is_none() && q.rolling.monthly.is_none());
    }

    #[test]
    fn parses_without_product_usage() {
        let j = json!({"config":{"creditUsagePercent":10.0,
            "currentPeriod":{"end":"2026-07-11T00:00:00Z"}}});
        let w = parse_grok_credits(&j).unwrap().rolling.weekly.unwrap();
        assert_eq!(w.remaining_percent, 90.0);
        assert!(w.detail.is_none());
    }

    #[test]
    fn missing_credit_percent_yields_none() {
        assert!(parse_grok_credits(&json!({"config":{}})).is_none());
        assert!(parse_grok_credits(&json!({})).is_none());
    }
}
