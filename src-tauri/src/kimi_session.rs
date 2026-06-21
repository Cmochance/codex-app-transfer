//! Kimi Code(Kimi for Coding 订阅)网页 session 抓取(CAT-256 后续)—— 走共用框架
//! [`crate::web_session_quota`]。
//!
//! Kimi Code 的用量(5 小时滚动 + 7 天周期 + 与主账户共享月度配额)只在网页控制台,经 connect-RPC
//! `kimi.gateway.membership.v2.MembershipService/GetSubscriptionStat` 查(扒 bundle + 解 protobuf
//! descriptor 实证);**inference API key 查不到**(实测 api.kimi.com/coding/* 全 404;web 端点用 key
//! 当 Bearer 返 unauthenticated —— 推理 key ≠ web 鉴权)。
//!
//! **鉴权要的是 `Authorization: Bearer <access_token>`,而 `access_token` 存在 localStorage、不是
//! cookie**(`kimi-auth` cookie 是另一个 web-session JWT,API 直接拒)。Tauri webview `eval` 不能直接
//! 回传值、外部页又拿不到 Tauri IPC,故用**cookie 桥**:每轮注入 JS 把 `localStorage.access_token`
//! 复制进 `cas_kimi_token` cookie(JWT 是 cookie-safe 的),再被 cookie 抓取读到。登录前
//! localStorage 无 access_token → 不会误判(顺带解决了之前「任意 cookie 提前命中」的问题)。

use crate::web_session_quota::{CaptureSignal, SessionLoginSpec};

/// 注入 JS:把 localStorage 的 access_token 复制进 `cas_kimi_token` cookie(供 cookie 抓取读)。
/// access_token 是 JWT(base64url + 点,无 `; , 空格`),cookie-safe,无需编码;登录前不存在则跳过。
const COPY_TOKEN_JS: &str = "try{var t=localStorage.getItem('access_token');if(t){document.cookie='cas_kimi_token='+t+';path=/'}}catch(e){}";

/// Kimi Code 登录抓取规格:控制台 SPA 登录;每轮注入 JS 复制 access_token → `cas_kimi_token` cookie;
/// 抓到该 cookie 即捕获(它的值就是 API 要的 Bearer access_token)。
const LOGIN_SPEC: SessionLoginSpec = SessionLoginSpec {
    login_url: "https://www.kimi.com/code/console",
    win_label: "kimi-login",
    win_title: "登录 Kimi 账号 · 获取 Kimi Code 套餐用量",
    inner_size: (520.0, 780.0),
    cookie_domain: "kimi.com",
    signal: CaptureSignal::CookiePresent("cas_kimi_token"),
    want_cookies: &["cas_kimi_token"],
    ignore_cookie_prefixes: &[],
    pre_capture_eval: Some(COPY_TOKEN_JS),
    extract_workspace_from_url: false,
};

/// 开内嵌登录窗抓 Kimi 的 API access_token(localStorage,经 cookie 桥)。返回**裸 token**
/// (从 `cas_kimi_token=<token>` 抽出),供 fetcher 当 `Authorization: Bearer` 用。
/// `Ok(None)` = 用户关窗 / 超时未完成(前端显「未登录」不弹错)。
pub async fn login_and_capture() -> Result<Option<String>, String> {
    Ok(crate::web_session_quota::login_and_capture(&LOGIN_SPEC)
        .await?
        .and_then(|s| {
            // 框架返回的是拼好的 `cas_kimi_token=<token>` 头,抽出 `=` 后的裸 token。
            s.cookie
                .split_once('=')
                .map(|(_, token)| token.to_string())
                .filter(|t| !t.is_empty())
        }))
}
