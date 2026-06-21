//! OpenCode 控制台网页 session 抓取(CAT-256)—— 走共用框架 [`crate::web_session_quota`]。
//!
//! OpenCode Go 的 5h/周/月用量只在 `opencode.ai` 控制台后面(走 OpenCode 账号登录,数据 SSR
//! 内嵌在 `/workspace/<id>/go` 页 HTML 里),**inference API key 查不到**(实测 balance/usage 端点
//! 全 404)。故内嵌 webview 登一次 OpenCode 账号、抓 `opencode.ai` 域全部 cookie + 从控制台 authed
//! URL 抓 workspace id(`wrk_...`)→ 落库供 [`crate::opencode_go_quota`] 查用量。通用逻辑在
//! [`crate::web_session_quota`],本模块只声明 OpenCode 自己的 [`SessionLoginSpec`]。

use crate::web_session_quota::{CaptureSignal, SessionLoginSpec};

/// OpenCode 登录抓取规格:控制台登录入口(登录后跳 `/workspace/<id>`);URL 进 `/workspace`
/// 即视为已登录;抓 `opencode.ai` 域**全部** cookie(`want_cookies` 空,session cookie 名 = auth /
/// provider);并从 URL 抓 workspace id(查 Go 用量端点 `/workspace/<id>/go` 必需)。
const LOGIN_SPEC: SessionLoginSpec = SessionLoginSpec {
    login_url: "https://opencode.ai/auth",
    win_label: "opencode-login",
    win_title: "登录 OpenCode 账号 · 获取 Go 套餐用量",
    inner_size: (520.0, 780.0),
    cookie_domain: "opencode.ai",
    signal: CaptureSignal::UrlContains("/workspace"),
    want_cookies: &[],
    ignore_cookie_prefixes: &[],
    pre_capture_eval: None,
    extract_workspace_from_url: true,
};

/// 开内嵌登录窗抓 OpenCode 网页 session,返回 `(Cookie 头, workspace id)`。
/// `Ok(None)` = 用户关窗 / 超时未完成(前端显「未登录」不弹错)。
pub async fn login_and_capture() -> Result<Option<(String, Option<String>)>, String> {
    Ok(crate::web_session_quota::login_and_capture(&LOGIN_SPEC)
        .await?
        .map(|s| (s.cookie, s.workspace_id)))
}
