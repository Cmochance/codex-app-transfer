//! WorkBuddy(腾讯 CodeBuddy)积分额度查询。
//!
//! `POST https://copilot.tencent.com/v2/billing/meter/get-user-resource`,鉴权
//! `Authorization: Bearer <Keycloak JWT>`(API-key 与账号登录 token 均可,真机实测),body `{}`。
//!
//! 响应(**真机实测** 2026-06-30):`{ "code":0, "data":{ "Response":{ "Data":{
//!   "Accounts":[ { "CapacityType":4, "CycleCapacityUsedPrecise":"253.61999997",
//!     "CycleCapacitySizePrecise":"500", "CycleCapacityRemainPrecise":"246.38000003",
//!     "CycleEndTime":"2026-06-30 23:59:59", ... }, ... ] }}}}`。
//!
//! 网页 `codebuddy.cn/profile/usage` 把账号分两组显示,本模块逐字对齐:
//! - **基础体验包** = `CapacityType==4`(个人体验版,按 cycle 月度刷新)→ monthly 槽 bar;
//! - **活动赠送包** = 其余(`CapacityType==1` 国内运营裂变包等,多子包)→ 聚合成
//!   [`ProviderQuota::aggregate`] 一条 bar(used/size/remain 累加)。
//!
//! bar 的明细文案显**绝对量**(`253.62 / 500 · 246.38 剩余`),与网页一致;百分比按**剩余**
//! (项目额度 bar 统一「剩余」语义,满额=100,与网页「已用填充」方向相反但数字一致)。

use crate::provider_quota::{ProviderQuota, QuotaWindow, RollingWindows};
use serde_json::Value;

/// fetch 失败分类(对称 trae/glm):区别「鉴权失效(清缓存)」与「瞬时错(留旧)」。
#[derive(Debug)]
pub enum QuotaError {
    /// HTTP 401/403:token 失效。caller 清额度缓存。
    Auth(reqwest::StatusCode),
    /// 网络 / 5xx / 429 / code!=0 / 解析失败 —— 瞬时,caller 留旧缓存重试。
    Transient(String),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Auth(s) => write!(f, "WorkBuddy quota 鉴权失败: {s}"),
            QuotaError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// 一组同类积分包的累加。
#[derive(Default)]
struct Agg {
    used: f64,
    size: f64,
    remain: f64,
}

/// 数字格式化:整数去小数(`500` 不是 `500.00`),非整数保留 2 位去尾零(`253.62`/`246.38`)。
/// 对齐网页显示。
fn fmt_num(v: f64) -> String {
    if v.fract().abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 优先读 `*Precise`(字符串高精度,如 `"253.61999997"`),兜底整数字段。缺失返 `None`
/// ——区分「字段缺失」与「真值 0」,供 remain 缺失时按 `size - used` 兜底(防误判 0% 剩余)。
fn read_amount(acc: &Value, precise_key: &str, int_key: &str) -> Option<f64> {
    acc.get(precise_key)
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| acc.get(int_key).and_then(Value::as_f64))
}

/// `CycleEndTime`(北京时间字符串 `"2026-06-30 23:59:59"`)+ 1 秒 = 下次刷新时刻 → RFC3339。
/// 无法解析 → None(不显刷新时间)。
fn next_refresh_rfc3339(cycle_end: &str) -> Option<String> {
    let naive = chrono::NaiveDateTime::parse_from_str(cycle_end, "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = chrono::FixedOffset::east_opt(8 * 3600)?; // 网关 = 北京时间 +08:00
    let dt = naive.and_local_timezone(offset).single()?;
    Some((dt + chrono::Duration::seconds(1)).to_rfc3339())
}

fn credit_bar(label: &str, agg: &Agg, reset: Option<String>) -> QuotaWindow {
    let pct = if agg.size > 0.0 {
        agg.remain / agg.size * 100.0
    } else {
        0.0
    };
    let detail = format!(
        "{} / {} · {} 剩余",
        fmt_num(agg.used),
        fmt_num(agg.size),
        fmt_num(agg.remain)
    );
    QuotaWindow::credit_bar(label, pct, detail, reset)
}

/// 从 `get-user-resource` 响应提取额度。纯函数,可测。
///
/// `CapacityType==4` → 基础体验包(monthly 槽,带下次刷新);其余聚合成活动赠送包
/// ([`ProviderQuota::aggregate`])。两组都拿不到 → 空 `ProviderQuota`(不显额度行)。
pub fn parse_workbuddy_quota(json: &Value) -> ProviderQuota {
    let Some(accounts) = json
        .pointer("/data/Response/Data/Accounts")
        .and_then(Value::as_array)
    else {
        return ProviderQuota::default();
    };

    let mut base = Agg::default(); // CapacityType == 4(基础体验包)
    let mut gift = Agg::default(); // 其余(活动赠送包,多子包聚合)
    let mut base_reset: Option<String> = None;

    for acc in accounts {
        let used = read_amount(acc, "CycleCapacityUsedPrecise", "CycleCapacityUsed").unwrap_or(0.0);
        let size = read_amount(acc, "CycleCapacitySizePrecise", "CycleCapacitySize").unwrap_or(0.0);
        // remain 缺失时按 size - used 兜底,防个别包只回 size/used → 误判「0% 剩余」标红预警。
        let remain = read_amount(acc, "CycleCapacityRemainPrecise", "CycleCapacityRemain")
            .unwrap_or_else(|| (size - used).max(0.0));
        let is_base = acc.get("CapacityType").and_then(Value::as_i64) == Some(4);
        if is_base {
            base.used += used;
            base.size += size;
            base.remain += remain;
            if base_reset.is_none() {
                base_reset = acc
                    .get("CycleEndTime")
                    .and_then(Value::as_str)
                    .and_then(next_refresh_rfc3339);
            }
        } else {
            gift.used += used;
            gift.size += size;
            gift.remain += remain;
        }
    }

    let mut rolling = RollingWindows::default();
    if base.size > 0.0 {
        rolling.monthly = Some(credit_bar("基础体验包", &base, base_reset));
    }
    let aggregate = (gift.size > 0.0).then(|| credit_bar("活动赠送包", &gift, None));

    ProviderQuota {
        rolling,
        aggregate,
        stats: Vec::new(),
    }
}

/// 调 `get-user-resource` 取额度。`token` = Bearer(API-key 或账号登录 access token)。
/// best-effort:失败按 [`QuotaError`] 分类。
pub async fn fetch_workbuddy_quota_summary(
    http: &reqwest::Client,
    token: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!(
        "https://{}/v2/billing/meter/get-user-resource",
        codex_app_transfer_gemini_oauth::workbuddy::WORKBUDDY_HOST
    );
    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| QuotaError::Transient(format!("WorkBuddy quota 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(QuotaError::Auth(status));
        }
        return Err(QuotaError::Transient(format!(
            "WorkBuddy quota 非 2xx: {status}"
        )));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| QuotaError::Transient(format!("WorkBuddy quota 解析失败: {e}")))?;
    // 网关 {code,msg,data}:code != 0 视作瞬时(留旧缓存重试,不清登录态)。
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = json.get("msg").and_then(Value::as_str).unwrap_or("");
        return Err(QuotaError::Transient(format!(
            "WorkBuddy quota code != 0: {msg}"
        )));
    }
    Ok(parse_workbuddy_quota(&json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 真机实测形态(2026-06-30):type=4 基础体验包 + 多个 type=1 裂变包(聚合赠送包)。
    fn sample() -> Value {
        json!({"code":0,"msg":"OK","data":{"Response":{"Data":{"Accounts":[
            {"CapacityType":4,"CycleEndTime":"2026-06-30 23:59:59",
             "CycleCapacityUsedPrecise":"253.61999997","CycleCapacitySizePrecise":"500",
             "CycleCapacityRemainPrecise":"246.38000003"},
            {"CapacityType":1,"CycleCapacityUsedPrecise":"0","CycleCapacitySizePrecise":"2000",
             "CycleCapacityRemainPrecise":"2000"},
            {"CapacityType":1,"CycleCapacityUsedPrecise":"0","CycleCapacitySizePrecise":"1350",
             "CycleCapacityRemainPrecise":"1350"}
        ]}}}})
    }

    #[test]
    fn base_pkg_maps_to_monthly_bar_with_exact_numbers() {
        let q = parse_workbuddy_quota(&sample());
        let base = q.rolling.monthly.as_ref().expect("基础体验包进 monthly 槽");
        assert_eq!(base.label, "基础体验包");
        // 剩余 246.38 / 500 = 49.276%
        assert!((base.remaining_percent - 49.276).abs() < 0.01);
        assert_eq!(base.detail.as_deref(), Some("253.62 / 500 · 246.38 剩余"));
        // 下次刷新 = CycleEndTime + 1s = 2026-07-01 00:00:00 (+08:00)
        assert_eq!(
            base.reset_rfc3339.as_deref(),
            Some("2026-07-01T00:00:00+08:00")
        );
    }

    #[test]
    fn gift_pkgs_aggregate_into_aggregate_bar() {
        let q = parse_workbuddy_quota(&sample());
        let gift = q.aggregate.as_ref().expect("赠送包进 aggregate");
        assert_eq!(gift.label, "活动赠送包");
        // 2000 + 1350 = 3350,全剩
        assert_eq!(gift.remaining_percent, 100.0);
        assert_eq!(gift.detail.as_deref(), Some("0 / 3350 · 3350 剩余"));
        assert!(gift.reset_rfc3339.is_none(), "赠送包不显刷新时间");
    }

    #[test]
    fn has_any_true_for_workbuddy() {
        assert!(parse_workbuddy_quota(&sample()).has_any());
    }

    #[test]
    fn empty_yields_default() {
        assert!(!parse_workbuddy_quota(&json!({})).has_any());
        assert!(
            !parse_workbuddy_quota(&json!({"data":{"Response":{"Data":{"Accounts":[]}}}}))
                .has_any()
        );
    }

    #[test]
    fn only_gift_no_base_still_shows_aggregate() {
        let j = json!({"data":{"Response":{"Data":{"Accounts":[
            {"CapacityType":1,"CycleCapacityUsedPrecise":"10","CycleCapacitySizePrecise":"100",
             "CycleCapacityRemainPrecise":"90"}
        ]}}}});
        let q = parse_workbuddy_quota(&j);
        assert!(q.rolling.monthly.is_none(), "无 type=4 → 无基础体验包 bar");
        let gift = q.aggregate.as_ref().expect("仍显赠送包");
        assert_eq!(gift.detail.as_deref(), Some("10 / 100 · 90 剩余"));
    }

    #[test]
    fn missing_remain_falls_back_to_size_minus_used() {
        // 个别包只回 size/used 不回 remain → 按 size-used 兜底,不误判 0% 剩余标红。
        let j = json!({"data":{"Response":{"Data":{"Accounts":[
            {"CapacityType":4,"CycleCapacityUsedPrecise":"100","CycleCapacitySizePrecise":"500"}
        ]}}}});
        let q = parse_workbuddy_quota(&j);
        let base = q.rolling.monthly.as_ref().expect("基础体验包");
        assert!(
            (base.remaining_percent - 80.0).abs() < 0.01,
            "500-100=400 → 剩 80%"
        );
        assert_eq!(base.detail.as_deref(), Some("100 / 500 · 400 剩余"));
    }

    #[test]
    fn fmt_num_trims_zeros() {
        assert_eq!(fmt_num(500.0), "500");
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(253.61999997), "253.62");
        assert_eq!(fmt_num(246.38000003), "246.38");
        assert_eq!(fmt_num(3350.0), "3350");
    }
}
