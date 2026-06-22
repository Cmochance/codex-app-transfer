//! Trae(字节 TRAE SOLO CN / Work CN)额度查询(CAT-257)。
//!
//! `POST https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage`,鉴权
//! `Authorization: Cloud-IDE-JWT <token>`,body `{"require_usage": true}`。
//!
//! 响应结构(逆向自 `out/...solo-lite.js` 的 `ICubeUsageService`,字段名按 JS 取):
//! ```json
//! { "Result": { "user_entitlement_pack_list": [
//!     { "entitlement_base_info": { "product_type": "Pro",
//!         "quota": { "premium_model_fast_request_limit": 1000 }, "expire_time": ... },
//!       "usage": { "premium_model_fast_amount": 137, "pay_go_amount": 0 } } ] } }
//! ```
//! `premium_model_fast_request_limit == -1` 表无限;剩余 = `max(limit - used, 0)`。
//! 国际 SaaS 变体走 `quota_snapshots.{chat,completions,premium_interactions}`,也兜一层。
//!
//! ⚠️ 额度响应正文实测抓包时被 ttnet 绕过未取到,parser 按 JS 字段逆向 + 做容错;
//! 真机登录后以实际响应校准(见 CAT-257 verify)。

use crate::provider_quota::{ProviderQuota, QuotaStat, RollingWindows};

/// fetch 失败分类(对称 glm/antigravity):区别「鉴权失效(清缓存)」与「瞬时错(留旧)」。
#[derive(Debug)]
pub enum QuotaError {
    /// HTTP 401/403:token 失效。caller 清额度缓存。
    Auth(reqwest::StatusCode),
    /// 网络 / 5xx / 429 / 解析失败 —— 瞬时,caller 留旧缓存重试。
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "Trae quota 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 容错时间戳(秒 / 毫秒 / RFC3339 字符串)→ RFC3339。无效 → None。
fn to_rfc3339(v: &serde_json::Value) -> Option<String> {
    if let Some(n) = v.as_i64() {
        // < 1e12 视为秒
        let ms = if n > 0 && n < 1_000_000_000_000 {
            n * 1000
        } else {
            n
        };
        return chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339());
    }
    if let Some(s) = v.as_str() {
        if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
            return Some(s.to_string());
        }
        if let Ok(n) = s.parse::<i64>() {
            let ms = if n > 0 && n < 1_000_000_000_000 {
                n * 1000
            } else {
                n
            };
            return chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339());
        }
    }
    None
}

/// 从 `ide_user_ent_usage` 响应提取额度。纯函数,可测。
///
/// 主路径:`user_entitlement_pack_list` —— 聚合各 pack 的 premium fast 配额(任一
/// limit=-1 即无限);产出月槽剩余% bar + 「X / Y」stat + 套餐名 stat。
/// 兜底:国际 SaaS `quota_snapshots`。
pub fn parse_trae_quota(json: &serde_json::Value) -> ProviderQuota {
    // Result 可能在顶层或裹一层
    let root = json.get("Result").unwrap_or(json);

    if let Some(q) = parse_pack_list(root) {
        return q;
    }
    if let Some(q) = parse_quota_snapshots(root) {
        return q;
    }
    ProviderQuota::default()
}

/// 主路径:`user_entitlement_pack_list`。
fn parse_pack_list(root: &serde_json::Value) -> Option<ProviderQuota> {
    let packs = root
        .get("user_entitlement_pack_list")
        .and_then(|v| v.as_array())?;
    if packs.is_empty() {
        return None;
    }

    let mut unlimited = false;
    let mut total_limit: i64 = 0;
    let mut total_used: i64 = 0;
    let mut product_type: Option<String> = None;
    let mut expire: Option<String> = None;
    let mut saw_any = false;

    for pack in packs {
        let base = pack.get("entitlement_base_info");
        let limit = base
            .and_then(|b| b.get("quota"))
            .and_then(|q| q.get("premium_model_fast_request_limit"))
            .and_then(serde_json::Value::as_i64);
        let used = pack
            .get("usage")
            .and_then(|u| u.get("premium_model_fast_amount"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if let Some(limit) = limit {
            saw_any = true;
            if limit < 0 {
                unlimited = true;
            } else {
                total_limit += limit;
            }
            total_used += used;
        }
        if product_type.is_none() {
            product_type = base
                .and_then(|b| b.get("product_type"))
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
        }
        if expire.is_none() {
            expire = base.and_then(|b| b.get("expire_time")).and_then(to_rfc3339);
        }
    }

    if !saw_any {
        return None;
    }

    let mut stats: Vec<QuotaStat> = Vec::new();
    if let Some(pt) = product_type {
        stats.push(QuotaStat {
            label: "套餐".to_string(),
            value: pt,
        });
    }

    let mut rolling = RollingWindows::default();
    if unlimited {
        stats.push(QuotaStat {
            label: "Premium 快速请求".to_string(),
            value: "无限".to_string(),
        });
    } else if total_limit > 0 {
        let remaining = (total_limit - total_used).max(0);
        let remaining_pct = (remaining as f64 / total_limit as f64) * 100.0;
        rolling = rolling.monthly_labeled("Premium 快速请求", remaining_pct, expire);
        stats.push(QuotaStat {
            label: "Premium 快速请求".to_string(),
            value: format!("{remaining} / {total_limit}"),
        });
    }

    Some(ProviderQuota { rolling, stats })
}

/// 兜底:国际 SaaS `quota_snapshots.{chat,completions,premium_interactions}`。
fn parse_quota_snapshots(root: &serde_json::Value) -> Option<ProviderQuota> {
    let snaps = root.get("quota_snapshots")?.as_object()?;
    let mut stats: Vec<QuotaStat> = Vec::new();
    let mut rolling = RollingWindows::default();
    for (key, snap) in snaps {
        let label = match key.as_str() {
            "chat" => "Chat",
            "completions" => "补全",
            "premium_interactions" => "Premium 交互",
            other => other,
        }
        .to_string();
        if snap
            .get("unlimited")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            stats.push(QuotaStat {
                label,
                value: "无限".to_string(),
            });
            continue;
        }
        let remaining = snap.get("remaining").and_then(serde_json::Value::as_i64);
        let entitlement = snap.get("entitlement").and_then(serde_json::Value::as_i64);
        if let (Some(rem), Some(ent)) = (remaining, entitlement) {
            if ent > 0 {
                let pct = (rem.max(0) as f64 / ent as f64) * 100.0;
                // 只把 premium_interactions 提进月槽 bar(最有代表性),其余走 stat 行
                if key == "premium_interactions" && rolling.monthly.is_none() {
                    rolling = rolling.monthly_labeled(label.clone(), pct, None);
                }
                stats.push(QuotaStat {
                    label,
                    value: format!("{} / {}", rem.max(0), ent),
                });
            }
        }
    }
    if stats.is_empty() && rolling.is_empty() {
        return None;
    }
    Some(ProviderQuota { rolling, stats })
}

/// 调 `ide_user_ent_usage` 取额度。`api_host` = `https://api.trae.cn`(含 scheme)。
/// `token` = Cloud-IDE-JWT。best-effort:失败按 [`QuotaError`] 分类。
pub async fn fetch_trae_quota_summary(
    http: &reqwest::Client,
    api_host: &str,
    token: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!(
        "{}/trae/api/v2/pay/ide_user_ent_usage",
        api_host.trim_end_matches('/')
    );
    let resp = http
        .post(&url)
        .header("Authorization", format!("Cloud-IDE-JWT {token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "require_usage": true }))
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("Trae quota 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "Trae quota 非 2xx: {status}"
        )));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("Trae quota 解析失败: {e}")))?;
    Ok(parse_trae_quota(&json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_pack_list_limited() {
        let j = json!({"Result":{"user_entitlement_pack_list":[
            {"entitlement_base_info":{"product_type":"Pro",
                "quota":{"premium_model_fast_request_limit":1000},
                "expire_time":1781448954156i64},
             "usage":{"premium_model_fast_amount":137}}
        ]}});
        let q = parse_trae_quota(&j);
        assert!(q.has_any());
        // 月槽 bar:剩余 863/1000 = 86.3%
        let bar = q.rolling.monthly.as_ref().expect("月槽 bar");
        assert_eq!(bar.label, "Premium 快速请求");
        assert!((bar.remaining_percent - 86.3).abs() < 1e-6, "剩 863/1000");
        // stat:套餐 + X/Y
        assert!(q
            .stats
            .iter()
            .any(|s| s.label == "套餐" && s.value == "Pro"));
        assert!(q
            .stats
            .iter()
            .any(|s| s.label == "Premium 快速请求" && s.value == "863 / 1000"));
    }

    #[test]
    fn unlimited_shows_stat_no_bar() {
        let j = json!({"Result":{"user_entitlement_pack_list":[
            {"entitlement_base_info":{"product_type":"Ultra",
                "quota":{"premium_model_fast_request_limit":-1}},
             "usage":{"premium_model_fast_amount":5000}}
        ]}});
        let q = parse_trae_quota(&j);
        assert!(q.rolling.is_empty(), "无限不画 bar");
        assert!(q
            .stats
            .iter()
            .any(|s| s.label == "Premium 快速请求" && s.value == "无限"));
    }

    #[test]
    fn aggregates_multiple_packs() {
        let j = json!({"Result":{"user_entitlement_pack_list":[
            {"entitlement_base_info":{"quota":{"premium_model_fast_request_limit":500}},
             "usage":{"premium_model_fast_amount":100}},
            {"entitlement_base_info":{"quota":{"premium_model_fast_request_limit":500}},
             "usage":{"premium_model_fast_amount":50}}
        ]}});
        let q = parse_trae_quota(&j);
        // limit 1000, used 150 → 剩 850
        assert!(q.stats.iter().any(|s| s.value == "850 / 1000"));
    }

    #[test]
    fn falls_back_to_quota_snapshots() {
        let j = json!({"Result":{"quota_snapshots":{
            "premium_interactions":{"entitlement":200,"remaining":150,"unlimited":false},
            "chat":{"unlimited":true}
        }}});
        let q = parse_trae_quota(&j);
        assert!(q.has_any());
        assert!(q.rolling.monthly.is_some(), "premium_interactions 进月槽");
        assert!(q.stats.iter().any(|s| s.value == "无限"), "chat 无限 stat");
    }

    #[test]
    fn empty_yields_default() {
        assert!(!parse_trae_quota(&json!({})).has_any());
        assert!(!parse_trae_quota(&json!({"Result":{"user_entitlement_pack_list":[]}})).has_any());
    }

    #[test]
    fn handles_root_without_result_wrapper() {
        // 有些响应可能不裹 Result
        let j = json!({"user_entitlement_pack_list":[
            {"entitlement_base_info":{"quota":{"premium_model_fast_request_limit":100}},
             "usage":{"premium_model_fast_amount":40}}
        ]});
        let q = parse_trae_quota(&j);
        assert!(q.stats.iter().any(|s| s.value == "60 / 100"));
    }
}
