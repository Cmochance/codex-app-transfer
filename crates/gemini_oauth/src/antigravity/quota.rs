//! Antigravity gemini 系列额度查询(MOC-204 Phase 3)。
//!
//! `POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary`(空 body
//! `{}`,Bearer + UA `antigravity/hub/<ver>`)返**按组**的双窗口额度:
//! ```json
//! {"groups":[
//!   {"displayName":"Gemini Models","buckets":[
//!     {"bucketId":"gemini-weekly","window":"weekly","resetTime":"...","remainingFraction":1},
//!     {"bucketId":"gemini-5h","window":"5h","resetTime":"...","remainingFraction":1}]},
//!   {"displayName":"Claude and GPT models", ...}]}
//! ```
//! 解包 Antigravity IDE v2.1.4 的 `bin/language_server`(`RetrieveUserQuotaSummary`
//! RPC)+ 实地验证(2026-06-13)。本项目只支持 antigravity 的 **gemini 系列**,故取
//! displayName 含「Gemini」的组(gemini 全系共用同一 5h + weekly 池)。
//!
//! 注:旧 `retrieveUserQuota`(MOC-201)每模型只返**一个绑定窗口**,拿不到完整双窗口
//! → 改用 `retrieveUserQuotaSummary`。

use super::super::constants::{antigravity_user_agent, CLOUD_CODE_BASE_URL};

/// 单个额度窗口(5h 或 weekly)。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    /// **剩余**百分比 = `remainingFraction × 100`,clamp 0-100(满额=100,消耗后降)。
    pub remaining_percent: f64,
    /// 窗口重置时刻(RFC3339 原样,如 `2026-06-20T12:56:06Z`);caller 转本地时间点显示。
    pub reset_rfc3339: Option<String>,
}

/// gemini 组的双窗口额度。任一窗口缺失(上游没返)→ None。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeminiQuota {
    pub five_hour: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
}

impl GeminiQuota {
    /// 是否有任一窗口(用于 caller 判定要不要显示额度行)。
    pub fn has_any(&self) -> bool {
        self.five_hour.is_some() || self.weekly.is_some()
    }
}

/// 从 `retrieveUserQuotaSummary` JSON 提取 gemini 组的 5h + weekly。纯函数,可测。
/// 取 displayName 含「gemini」的组;bucket 按 `window`(`5h` / `weekly`)归位。
pub fn parse_gemini_quota_summary(json: &serde_json::Value) -> GeminiQuota {
    let mut out = GeminiQuota::default();
    let Some(groups) = json.get("groups").and_then(|v| v.as_array()) else {
        return out;
    };
    let group = groups.iter().find(|g| {
        g.get("displayName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase().contains("gemini"))
            .unwrap_or(false)
    });
    let Some(buckets) = group
        .and_then(|g| g.get("buckets"))
        .and_then(|v| v.as_array())
    else {
        return out;
    };
    for b in buckets {
        let frac = b
            .get("remainingFraction")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(1.0);
        let win = QuotaWindow {
            remaining_percent: (frac * 100.0).clamp(0.0, 100.0),
            reset_rfc3339: b
                .get("resetTime")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        };
        match b.get("window").and_then(|v| v.as_str()) {
            Some("5h") => out.five_hour = Some(win),
            Some("weekly") => out.weekly = Some(win),
            _ => {}
        }
    }
    out
}

/// quota fetch 失败分类:让 caller 区别对待「鉴权失效(清缓存)」和「瞬时错(留旧缓存重试)」。
#[derive(Debug)]
pub enum QuotaError {
    /// HTTP 401/403:access_token 被**服务端**撤销 / 失效(本地 token 文件可能还看着有效)。
    /// caller 应清额度缓存、不再展示上个账号/状态的额度。
    Auth(reqwest::StatusCode),
    /// 网络 / 非鉴权非 2xx(5xx、429 等)/ 解析失败 —— 瞬时,同账号,caller 可留旧缓存重试。
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "retrieveUserQuotaSummary 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 调 `retrieveUserQuotaSummary` 取 gemini 双窗口额度。best-effort:失败按 [`QuotaError`]
/// 分类(Auth=服务端鉴权失效 / Transient=瞬时),caller 据此决定清缓存还是留旧值。
pub async fn fetch_gemini_quota_summary(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<GeminiQuota, QuotaError> {
    fetch_gemini_quota_summary_at(http, CLOUD_CODE_BASE_URL, access_token).await
}

pub(crate) async fn fetch_gemini_quota_summary_at(
    http: &reqwest::Client,
    base_url: &str,
    access_token: &str,
) -> Result<GeminiQuota, QuotaError> {
    let url = format!("{base_url}/v1internal:retrieveUserQuotaSummary");
    let resp = http
        .post(&url)
        .bearer_auth(access_token)
        .header("User-Agent", antigravity_user_agent())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("retrieveUserQuotaSummary 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        // 401/403 = token 服务端失效 → Auth(caller 清缓存);其余非 2xx(5xx/429 等)当瞬时。
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "retrieveUserQuotaSummary 非 2xx: {status}"
        )));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("retrieveUserQuotaSummary 解析失败: {e}")))?;
    Ok(parse_gemini_quota_summary(&json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 实地抓的真实响应(2026-06-13):gemini 组双窗口 + claude/gpt 组。
    fn real_summary() -> serde_json::Value {
        json!({
            "groups": [
                {"displayName": "Gemini Models", "description": "Gemini Flash, Gemini Pro", "buckets": [
                    {"bucketId":"gemini-weekly","window":"weekly","resetTime":"2026-06-20T12:56:06Z","remainingFraction":1.0},
                    {"bucketId":"gemini-5h","window":"5h","resetTime":"2026-06-13T17:56:06Z","remainingFraction":0.98}
                ]},
                {"displayName": "Claude and GPT models", "buckets": [
                    {"bucketId":"3p-weekly","window":"weekly","resetTime":"2026-06-20T12:56:06Z","remainingFraction":0.0},
                    {"bucketId":"3p-5h","window":"5h","resetTime":"2026-06-13T17:56:06Z","remainingFraction":1.0}
                ]}
            ]
        })
    }

    #[test]
    fn extracts_gemini_group_both_windows() {
        let q = parse_gemini_quota_summary(&real_summary());
        let h = q.five_hour.expect("5h");
        assert!((h.remaining_percent - 98.0).abs() < 1e-6, "5h: 0.98→剩 98%"); // gemini 组的 0.98
        assert_eq!(h.reset_rfc3339.as_deref(), Some("2026-06-13T17:56:06Z"));
        let w = q.weekly.expect("weekly");
        assert!(
            (w.remaining_percent - 100.0).abs() < 1e-6,
            "weekly: 1.0→剩 100%"
        );
        assert_eq!(w.reset_rfc3339.as_deref(), Some("2026-06-20T12:56:06Z"));
    }

    #[test]
    fn ignores_claude_group() {
        // claude 组 weekly remainingFraction=0(已耗尽,剩 0),不能误取到 gemini 行(剩 100)
        let q = parse_gemini_quota_summary(&real_summary());
        assert_eq!(
            q.weekly.unwrap().remaining_percent,
            100.0,
            "应取 gemini(剩 100)非 claude(剩 0)"
        );
    }

    #[test]
    fn missing_groups_yields_empty() {
        assert_eq!(
            parse_gemini_quota_summary(&json!({})),
            GeminiQuota::default()
        );
        assert!(!parse_gemini_quota_summary(&json!({})).has_any());
    }

    #[test]
    fn clamps_and_handles_partial() {
        let j = json!({"groups":[{"displayName":"Gemini Models","buckets":[
            {"window":"5h","remainingFraction":1.2}
        ]}]});
        let q = parse_gemini_quota_summary(&j);
        assert_eq!(
            q.five_hour.unwrap().remaining_percent,
            100.0,
            "frac>1 → clamp 100%"
        );
        assert!(q.weekly.is_none(), "缺 weekly bucket → None");
    }
}
