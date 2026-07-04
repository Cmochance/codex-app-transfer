//! QoderWork(阿里 Qoder)额度显示。
//!
//! `GET openapi.qoder.com.cn/api/v2/quota/usage` + `Bearer <device_token>`(SDK `account.getQuotaUsage`,
//! 与 `/userinfo` 同 host/auth,实测 200,MOC-297)。响应:
//! - `userQuota{total, used, remaining, percentage, unit:"credits"}` —— 基础套餐
//! - `addOnQuota{total, used, remaining, ..., detailUrl}` —— 加油包
//! - `isQuotaExceeded` / `totalUsagePercentage` / `expiresAt`(unix ms)/ `upgradeUrl`
//!
//! 各聚合成一条 credit bar 挂 Codex Usage 面板(仿 [`crate::workbuddy_quota`]):`unit=credits`,
//! remaining% 进度 + 「used / total」明细;基础套餐带 `expiresAt` 刷新时间。

use serde_json::Value;

use crate::provider_quota::{ProviderQuota, QuotaWindow, RollingWindows};
use crate::workbuddy_quota::QuotaError;
use codex_app_transfer_adapters::core::language::{current_language, Language};

/// 取 quota usage 原始 json。`device_token` = 账号 personal_token。best-effort,失败按 [`QuotaError`] 分类。
pub async fn fetch_qoder_quota_usage(
    http: &reqwest::Client,
    device_token: &str,
) -> Result<Value, QuotaError> {
    let url = format!(
        "https://{}/api/v2/quota/usage",
        codex_app_transfer_gemini_oauth::qoder::QODER_OPENAPI_HOST
    );
    let resp = http
        .get(&url)
        .header("Authorization", format!("Bearer {device_token}"))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("qoder quota 请求失败: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Auth(status));
    }
    if !status.is_success() {
        return Err(QuotaError::Transient(format!("qoder quota HTTP {status}")));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| QuotaError::Transient(format!("qoder quota 解析失败: {e}")))
}

/// 解析 quota usage → 显示用 [`ProviderQuota`](纯函数,可测)。
pub fn parse_qoder_quota(json: &Value) -> ProviderQuota {
    parse_qoder_quota_in(json, current_language())
}

/// 把 `{total, used, remaining}` credits 对象转一条 credit bar(`total<=0` / 缺失 → None,不显)。
fn credit_bar_from(obj: Option<&Value>, label: &str, reset: Option<String>) -> Option<QuotaWindow> {
    let obj = obj?;
    let total = obj.get("total").and_then(Value::as_f64)?;
    if total <= 0.0 {
        return None;
    }
    let used = obj.get("used").and_then(Value::as_f64).unwrap_or(0.0);
    // remaining 缺失按 total-used 兜底(防误判 0% 剩余标红)。
    let remaining = obj
        .get("remaining")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| (total - used).max(0.0));
    let pct = remaining / total * 100.0;
    let detail = format!("{} / {}", fmt_credits(used), fmt_credits(total));
    Some(QuotaWindow::credit_bar(label, pct, detail, reset))
}

fn parse_qoder_quota_in(json: &Value, lang: Language) -> ProviderQuota {
    let zh = lang == Language::Chinese;
    // 基础套餐 expiresAt(unix ms)→ 下次刷新时间。
    let reset = json
        .get("expiresAt")
        .and_then(Value::as_i64)
        .and_then(unix_ms_to_rfc3339);
    let rolling = RollingWindows {
        monthly: credit_bar_from(
            json.get("userQuota"),
            if zh { "基础包" } else { "Base" },
            reset,
        ),
        ..RollingWindows::default()
    };
    let aggregate = credit_bar_from(
        json.get("addOnQuota"),
        if zh { "加油包" } else { "Bonus" },
        None,
    );
    ProviderQuota {
        rolling,
        aggregate,
        stats: Vec::new(),
    }
}

/// credits 数字格式化:整数不带小数,否则保 2 位。
fn fmt_credits(v: f64) -> String {
    if v.fract().abs() < f64::EPSILON {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// unix 毫秒 → RFC3339(渲染器统一格式)。
fn unix_ms_to_rfc3339(ms: i64) -> Option<String> {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        // 实测响应结构(MOC-297):基础 300 用尽 + 加油包 1900 用 16 剩 1884。
        json!({
            "usageType": "credits",
            "isQuotaExceeded": false,
            "totalUsagePercentage": 0.15,
            "expiresAt": 1785487944981i64,
            "userQuota": {"total": 300.0, "used": 300.0, "remaining": 0.0, "unit": "credits"},
            "addOnQuota": {"total": 1900.0, "used": 16.0, "remaining": 1884.0, "unit": "credits"}
        })
    }

    #[test]
    fn parse_base_and_addon_bars() {
        let q = parse_qoder_quota_in(&sample(), Language::Chinese);
        let base = q.rolling.monthly.as_ref().expect("基础包 bar");
        assert_eq!(base.label, "基础包");
        assert_eq!(base.remaining_percent, 0.0); // 300/300 用尽
        assert_eq!(base.detail.as_deref(), Some("300 / 300"));
        assert!(base.reset_rfc3339.is_some(), "expiresAt → 刷新时间");
        let addon = q.aggregate.as_ref().expect("加油包 bar");
        assert_eq!(addon.label, "加油包");
        assert!((addon.remaining_percent - 99.157_894).abs() < 0.01); // 1884/1900
        assert_eq!(addon.detail.as_deref(), Some("16 / 1900"));
    }

    #[test]
    fn missing_quota_yields_empty() {
        let q = parse_qoder_quota_in(&json!({}), Language::English);
        assert!(q.rolling.monthly.is_none() && q.aggregate.is_none());
    }

    #[test]
    fn zero_total_bar_hidden() {
        let q = parse_qoder_quota_in(
            &json!({"userQuota": {"total": 0.0, "used": 0.0, "remaining": 0.0}}),
            Language::English,
        );
        assert!(q.rolling.monthly.is_none());
    }
}
