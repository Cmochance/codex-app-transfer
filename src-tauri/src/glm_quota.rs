//! GLM Coding Plan 额度查询(MOC-211)。
//!
//! `GET https://open.bigmodel.cn/api/monitor/usage/quota/limit`(z.ai 国际版换
//! host `api.z.ai`),鉴权 `Authorization: <apiKey>` —— **不带 `Bearer` 前缀**,用的就是
//! 推理同一把 coding-plan key,无需网页 session token、无需抓 HTML。响应(真机实测
//! 2026-06-14):
//! ```json
//! {"code":200,"success":true,"data":{"level":"pro","limits":[
//!   {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":22,"nextResetTime":1781448954156},
//!   {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":25,"nextResetTime":1781779923998},
//!   {"type":"TIME_LIMIT", ... MCP 工具(1月)额度,本仓不显 ... }]}}
//! ```
//! `unit=3,number=5` → 5 小时窗口;`unit=6,number=1` → 每周窗口。`percentage` 是**已用** %
//! (与 antigravity 的 `remainingFraction` 剩余口径相反),parser 统一换算成剩余
//! (`100 - 已用`)落进 [`ProviderQuota`]。
//!
//! 端口 / 鉴权方式 / 字段语义借鉴上游 opencode-glm-quota(`guyinwonder168/opencode-glm-quota`),
//! 并以真实 GLM Coding key 实测取证。

use crate::provider_quota::{ProviderQuota, QuotaWindow};

/// fetch 失败分类(对称 antigravity 的 `QuotaError`):让 caller 区别对待「鉴权失效(清缓存)」
/// 与「瞬时错(留旧缓存重试)」。
#[derive(Debug)]
pub enum QuotaError {
    /// HTTP 401/403:key 被服务端拒(失效 / 非 coding-plan key)。caller 清额度缓存。
    Auth(reqwest::StatusCode),
    /// 网络 / 5xx / 429 / 解析失败 —— 瞬时,caller 可留旧缓存重试。
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "GLM quota 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// unix 毫秒时间戳 → RFC3339(供 injector 的 `fmt_reset_local` 统一转本地)。越界/无效 → None。
fn reset_ms_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

/// 从 `monitor/usage/quota/limit` 响应提取 5h + weekly 双窗口,产出带 label 的窗口列表
/// (5 小时额度 / 每周额度,固定顺序)。纯函数,可测。只取 `TOKENS_LIMIT`(按 unit/number
/// 归位);`TIME_LIMIT`(MCP 工具额度)忽略。
pub fn parse_glm_quota(json: &serde_json::Value) -> ProviderQuota {
    let Some(limits) = json
        .get("data")
        .and_then(|d| d.get("limits"))
        .and_then(|v| v.as_array())
    else {
        return ProviderQuota::default();
    };
    // 先归位到 5h / weekly(再按固定顺序 push,避免上游 limits 顺序影响展示顺序)。
    let mut five_hour: Option<(f64, Option<String>)> = None;
    let mut weekly: Option<(f64, Option<String>)> = None;
    for lim in limits {
        if lim.get("type").and_then(|v| v.as_str()) != Some("TOKENS_LIMIT") {
            continue;
        }
        let used = lim
            .get("percentage")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let remaining = (100.0 - used).clamp(0.0, 100.0);
        let reset = lim
            .get("nextResetTime")
            .and_then(serde_json::Value::as_i64)
            .and_then(reset_ms_to_rfc3339);
        match (
            lim.get("unit").and_then(serde_json::Value::as_i64),
            lim.get("number").and_then(serde_json::Value::as_i64),
        ) {
            (Some(3), Some(5)) => five_hour = Some((remaining, reset)),
            (Some(6), Some(1)) => weekly = Some((remaining, reset)),
            _ => {}
        }
    }
    let mut windows = Vec::new();
    if let Some((remaining, reset)) = five_hour {
        windows.push(QuotaWindow {
            label: "5 小时额度".into(),
            remaining_percent: remaining,
            reset_rfc3339: reset,
        });
    }
    if let Some((remaining, reset)) = weekly {
        windows.push(QuotaWindow {
            label: "每周额度".into(),
            remaining_percent: remaining,
            reset_rfc3339: reset,
        });
    }
    ProviderQuota { windows }
}

/// 调 monitor 端口取 GLM coding 双窗口额度。`base_host` = provider.baseUrl 的 host
/// (`open.bigmodel.cn` / `api.z.ai`)。best-effort:失败按 [`QuotaError`] 分类。
pub async fn fetch_glm_quota_summary(
    http: &reqwest::Client,
    base_host: &str,
    api_key: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!("https://{base_host}/api/monitor/usage/quota/limit");
    let resp = http
        .get(&url)
        // 关键:Authorization 直接放 key,**不带 Bearer 前缀**(与上游 monitor 端口约定一致)。
        .header("Authorization", api_key)
        .header("Accept-Language", "en-US,en")
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("GLM quota 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!("GLM quota 非 2xx: {status}")));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("GLM quota 解析失败: {e}")))?;
    Ok(parse_glm_quota(&json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 真机抓的响应骨架(2026-06-14):data.limits = 5h + weekly(TOKENS_LIMIT)+ MCP(TIME_LIMIT)。
    fn real_response() -> serde_json::Value {
        json!({
            "code": 200,
            "msg": "Operation successful",
            "success": true,
            "data": {
                "level": "pro",
                "limits": [
                    {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":22,"nextResetTime":1781448954156i64},
                    {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":25,"nextResetTime":1781779923998i64},
                    {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":0,"remaining":1000,"percentage":0,"nextResetTime":1783767123981i64}
                ]
            }
        })
    }

    fn win<'a>(q: &'a ProviderQuota, label: &str) -> Option<&'a QuotaWindow> {
        q.windows.iter().find(|w| w.label == label)
    }

    #[test]
    fn parses_both_token_windows_as_remaining() {
        let q = parse_glm_quota(&real_response());
        assert_eq!(q.windows.len(), 2);
        // 顺序:5 小时额度 在前、每周额度 在后
        assert_eq!(q.windows[0].label, "5 小时额度");
        assert_eq!(q.windows[1].label, "每周额度");
        // percentage=已用 → 剩余 = 100 - 已用
        let h = win(&q, "5 小时额度").expect("5h");
        assert!((h.remaining_percent - 78.0).abs() < 1e-6, "5h 已用 22 → 剩 78");
        assert!(h.reset_rfc3339.is_some(), "5h 重置时刻应解析自 nextResetTime");
        let w = win(&q, "每周额度").expect("weekly");
        assert!((w.remaining_percent - 75.0).abs() < 1e-6, "weekly 已用 25 → 剩 75");
    }

    #[test]
    fn ignores_time_limit_mcp_bucket() {
        // TIME_LIMIT(MCP 工具)不应被当成 token 窗口;只保留两个 TOKENS_LIMIT。
        let q = parse_glm_quota(&real_response());
        assert_eq!(q.windows.len(), 2);
    }

    #[test]
    fn missing_data_yields_empty() {
        assert_eq!(parse_glm_quota(&json!({})), ProviderQuota::default());
        assert!(!parse_glm_quota(&json!({"data": {}})).has_any());
    }

    #[test]
    fn clamps_overflow_and_handles_partial() {
        // percentage>100(异常)→ 剩余 clamp 到 0;缺 weekly → 只剩 5h 单窗口。
        let j = json!({"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":140}
        ]}});
        let q = parse_glm_quota(&j);
        assert_eq!(q.windows.len(), 1);
        assert_eq!(win(&q, "5 小时额度").unwrap().remaining_percent, 0.0, "已用>100 → 剩 0");
        assert!(win(&q, "每周额度").is_none(), "缺 weekly → 无该窗口");
    }

    #[test]
    fn reset_ms_converts_to_parseable_rfc3339() {
        let s = reset_ms_to_rfc3339(1781448954156).expect("rfc3339");
        // 必须能被 injector 的 fmt_reset_local(parse_from_rfc3339)解析回去
        assert!(chrono::DateTime::parse_from_rfc3339(&s).is_ok());
    }
}
