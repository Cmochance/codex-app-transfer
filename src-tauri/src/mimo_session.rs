//! 小米 MiMo Token Plan 网页 session 抓取(MOC-211)—— 走共用框架 [`crate::web_session_quota`]。
//!
//! MiMo 套餐用量在 `platform.xiaomimimo.com` 控制台后面、走小米账号 SSO,认证靠 **httpOnly**
//! cookie `api-platform_serviceToken`(tp- 推理 key 不通用,实测带 key 仍 401)。app 读不到外部
//! 默认浏览器的 httpOnly cookie,故用内嵌 webview 登录抓(底层 `WKHTTPCookieStore.getAllCookies`
//! 含 httpOnly)。webview 生命周期 / 轮询 / 拼头等通用逻辑在 [`crate::web_session_quota`],本模块
//! 只声明 MiMo 自己的 [`SessionLoginSpec`]。

use crate::web_session_quota::{CaptureSignal, SessionLoginSpec};

/// MiMo 登录抓取规格:控制台首页(未登录自动 302 小米 SSO,登录后跳回);抓到 httpOnly
/// `serviceToken` 即捕获;按 WANT 顺序拼 `Cookie:` 头(serviceToken 是认证必需,其余一并带上
/// 确保鉴权完整)。WANT 按**名**匹配(wry 设的 domain 形态可能带前导点/差异,按名最稳)。
const LOGIN_SPEC: SessionLoginSpec = SessionLoginSpec {
    login_url: "https://platform.xiaomimimo.com/console/plan-manage",
    win_label: "mimo-login",
    win_title: "登录小米账号 · 获取 MiMo 套餐用量",
    inner_size: (480.0, 760.0),
    cookie_domain: "xiaomimimo",
    signal: CaptureSignal::CookiePresent("api-platform_serviceToken"),
    want_cookies: &[
        "api-platform_serviceToken",
        "api-platform_slh",
        "api-platform_ph",
        "userId",
    ],
    extract_workspace_from_url: false,
};

/// 开内嵌登录窗抓 MiMo 网页 session,返回拼好的 `Cookie:` 头(MiMo 无 workspace 概念)。
/// `Ok(None)` = 用户关窗 / 超时未完成(前端显「未登录」不弹错)。
pub async fn login_and_capture() -> Result<Option<String>, String> {
    Ok(crate::web_session_quota::login_and_capture(&LOGIN_SPEC)
        .await?
        .map(|s| s.cookie))
}
