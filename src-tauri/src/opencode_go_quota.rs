//! OpenCode Go 套餐用量抓取(CAT-256)。
//!
//! OpenCode Go 的 5 小时 / 每周 / 每月 三档用量只在 opencode.ai 控制台,且**无干净 API**
//! (实测 balance/usage 端点全 404);控制台是 SolidStart,数据 **SSR 内嵌在 Go 页 HTML** 里。
//! 故用 OpenCode 账号网页 session cookie(见 [`crate::opencode_session`])+ workspace id 取
//! `GET /workspace/<id>/go`,正则解析 SSR hydration 里的三窗口:
//!
//! ```text
//! {mine:!0,useBalance:!1,
//!  rollingUsage:$R[34]={status:"ok",resetInSec:12200,usagePercent:4},
//!  weeklyUsage:$R[35]={status:"ok",resetInSec:17553,usagePercent:1},
//!  monthlyUsage:$R[36]={status:"ok",resetInSec:2539966,usagePercent:0}}
//! ```
//!
//! 每窗口 `usagePercent`=**已用%**(剩余 = 100-已用),`resetInSec`=重置倒计时(→ 绝对 RFC3339)。
//! 产出 [`ProviderQuota`] 三个 [`QuotaWindow`](5 小时额度 / 每周额度 / 每月额度),交由
//! [`crate::codex_quota_injector`] 跟 GLM/antigravity 的滚动窗口同款渲染(各 provider 各显各的;
//! 无月窗口的 provider 不产出该窗口即自动不显,不需另建模块)。
//!
//! **健壮性**:session 失效时控制台会把 `/workspace/<id>/go` 跳登录页 → 解析不到任何窗口 →
//! 返 [`QuotaError::Auth`],caller 清存储 cookie 让前端转「未登录」提示重登(session 无 refresh)。

use crate::provider_quota::{ProviderQuota, QuotaWindow};

/// 抓取错误:`Auth`=session 失效(需重登,清 cookie);`Transient`=网络/瞬时(留旧缓存重试)。
pub enum QuotaError {
    Auth,
    Transient(String),
}

/// 从块里取某数值字段(`field:<number>`),支持整数 / 小数 / 负号。块内无该字段 → None。
fn extract_num(block: &str, field: &str) -> Option<f64> {
    let key = format!("{field}:");
    let start = block.find(&key)? + key.len();
    let rest = &block[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

/// 解析单个窗口块,返回 `(usagePercent 已用%, resetInSec 重置倒计时秒)`。
/// 块形如 `rollingUsage:$R[34]={status:"ok",resetInSec:12200,usagePercent:4}` —— 定位
/// `<name>:` 后第一个 `{...}`(`$R[n]=` 引用前缀不影响),在该 `{}` 范围内取字段。
/// 找不到该窗口 / 无 usagePercent → None(该窗口跳过,不产出 → 该档不显示)。
fn parse_window(html: &str, name: &str) -> Option<(f64, Option<i64>)> {
    let key = format!("{name}:");
    let name_at = html.find(&key)?;
    let brace_off = html[name_at..].find('{')?;
    let block_start = name_at + brace_off;
    let block_end = html[block_start..].find('}')? + block_start;
    let block = &html[block_start..block_end];
    let used = extract_num(block, "usagePercent")?;
    let reset_sec = extract_num(block, "resetInSec").map(|n| n as i64);
    Some((used, reset_sec))
}

/// 取 OpenCode Go 套餐三窗口用量。`workspace_id` 形如 `wrk_...`(登录时从控制台 URL 抓),
/// `cookie` 为 opencode.ai 域网页 session(`auth=...; provider=...`)。
pub async fn fetch_opencode_go_quota(
    http: &reqwest::Client,
    workspace_id: &str,
    cookie: &str,
) -> Result<ProviderQuota, QuotaError> {
    let url = format!("https://opencode.ai/workspace/{workspace_id}/go");
    let resp = http
        .get(&url)
        .header("Cookie", cookie)
        // 控制台对默认 UA 可能区别对待,带常规浏览器 UA 拿 SSR HTML(同抓包验证时一致)。
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| QuotaError::Transient(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(QuotaError::Auth);
    }
    if !status.is_success() {
        return Err(QuotaError::Transient(format!("HTTP {status}")));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| QuotaError::Transient(e.to_string()))?;

    let now = chrono::Utc::now();
    let mut windows = Vec::new();
    // 固定顺序:5 小时 → 每周 → 每月(与 GLM 5h/周 同序;月窗口 OpenCode Go 独有,无则自动跳过)。
    for (name, label) in [
        ("rollingUsage", "5 小时额度"),
        ("weeklyUsage", "每周额度"),
        ("monthlyUsage", "每月额度"),
    ] {
        if let Some((used, reset_sec)) = parse_window(&html, name) {
            let reset = reset_sec
                .filter(|s| *s > 0)
                .map(|s| (now + chrono::Duration::seconds(s)).to_rfc3339());
            windows.push(QuotaWindow {
                label: label.into(),
                remaining_percent: (100.0 - used).clamp(0.0, 100.0),
                reset_rfc3339: reset,
            });
        }
    }
    if windows.is_empty() {
        // 一个窗口都没解析到:多半 session 失效跳了登录页(或页面结构变)。按 Auth 让前端提示重登。
        return Err(QuotaError::Auth);
    }
    Ok(ProviderQuota {
        windows,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"...{mine:!0,useBalance:!1,rollingUsage:$R[34]={status:"ok",resetInSec:12200,usagePercent:4},weeklyUsage:$R[35]={status:"ok",resetInSec:17553,usagePercent:1},monthlyUsage:$R[36]={status:"ok",resetInSec:2539966,usagePercent:0}})..."#;

    #[test]
    fn parses_three_windows() {
        let rolling = parse_window(SAMPLE, "rollingUsage").unwrap();
        assert_eq!(rolling.0, 4.0);
        assert_eq!(rolling.1, Some(12200));
        let weekly = parse_window(SAMPLE, "weeklyUsage").unwrap();
        assert_eq!(weekly.0, 1.0);
        let monthly = parse_window(SAMPLE, "monthlyUsage").unwrap();
        assert_eq!(monthly.0, 0.0);
        assert_eq!(monthly.1, Some(2539966));
    }

    #[test]
    fn missing_window_returns_none() {
        assert!(parse_window("no usage here", "rollingUsage").is_none());
        // 月窗口缺失(其他计划)→ None,不产出该档。
        assert!(parse_window(r#"rollingUsage:$R[1]={usagePercent:5}"#, "monthlyUsage").is_none());
    }
}
