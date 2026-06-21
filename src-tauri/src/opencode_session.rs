//! OpenCode 控制台网页 session 抓取(CAT-256 / 镜像 MiMo `mimo_session.rs`)。
//!
//! OpenCode Go 的 5h/周/月用量只在 `opencode.ai` 控制台后面(走 OpenCode 账号登录,
//! 数据由 SolidStart server-function 取),**inference API key 查不到**(实测 balance/usage
//! 等端点全 404)。所以跟 MiMo 一样:app 内嵌 Tauri `WebviewWindow` 让用户登一次 OpenCode
//! 账号 → Rust 侧轮询 `webview.cookies()`(底层 `WKHTTPCookieStore.getAllCookies`,**含
//! httpOnly**)抓到 `opencode.ai` 域全部 cookie → 拼成 `Cookie:` 头返回给 caller 落库
//! (provider 的 `opencodeCookie`),后续 quota fetcher 带它查控制台用量。
//!
//! 体积零增量:复用主窗口已在用的系统 WebView + 已链接的 tauri/wry,无新依赖。
//!
//! **注**:OpenCode 控制台是 SolidStart RPC,具体用量端点还要抓包确定(见 CAT-256 讨论);
//! 本模块只负责「登录 + 抓 session cookie 落库」这层基础设施,先把账号记下来不反复登。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// setup 阶段注入(AdminState 建 router 时尚无 AppHandle,走全局),供开登录窗用。
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn init(handle: AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

/// OpenCode 控制台登录入口;未登录会引导到 OpenCode 账号登录,登录后跳 `/workspace/...`。
const LOGIN_URL: &str = "https://opencode.ai/auth";
const WIN_LABEL: &str = "opencode-login";
/// 登录成功信号:URL 进到已认证区(控制台 dashboard 在 `/workspace/<id>` 路由下)。
const AUTHED_PATH_MARKER: &str = "/workspace";
const COOKIE_DOMAIN_MARKER: &str = "opencode.ai";
const POLL_TIMEOUT: Duration = Duration::from_secs(180);

/// 打开内嵌登录窗,轮询抓 `opencode.ai` 域的 session cookie。
/// - `Ok(Some(header))`:登录成功,返回拼好的 `Cookie:` 头(opencode.ai 域全部 cookie,供 caller 落库)。
/// - `Ok(None)`:用户关窗 / 超时未完成(非错误,前端显「未登录」不弹错)。
/// - `Err(_)`:真错误(开窗失败 / AppHandle 未初始化)。
pub async fn login_and_capture() -> Result<Option<String>, String> {
    let app = APP_HANDLE.get().ok_or("AppHandle 未初始化")?.clone();

    // 防连点:已有同名登录窗先关掉重开。
    if app.get_webview_window(WIN_LABEL).is_some() {
        close_win(&app);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // 窗口创建放主线程(macOS 在非主线程建 webview 会 panic);结果经 oneshot 回传。
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let app_build = app.clone();
    app.run_on_main_thread(move || {
        let res = (|| -> Result<(), String> {
            let url: tauri::Url = LOGIN_URL
                .parse()
                .map_err(|e| format!("URL 解析失败: {e}"))?;
            WebviewWindowBuilder::new(&app_build, WIN_LABEL, WebviewUrl::External(url))
                .title("登录 OpenCode 账号 · 获取 Go 套餐用量")
                .inner_size(520.0, 780.0)
                .build()
                .map_err(|e| format!("创建登录窗口失败: {e}"))?;
            Ok(())
        })();
        let _ = tx.send(res);
    })
    .map_err(|e| format!("主线程派发失败: {e}"))?;
    rx.await.map_err(|e| format!("窗口创建回传失败: {e}"))??;

    // 轮询 URL + cookies(每秒一次,≤3min)。URL 进 `/workspace` 即视为已登录 → 抓全部
    // opencode.ai 域 cookie 拼 `Cookie:` 头返回。
    let started = Instant::now();
    let mut last_url = String::new();
    loop {
        if started.elapsed() > POLL_TIMEOUT {
            tracing::info!("[OpenCode] 登录超时(未捕获 session),关闭登录窗");
            close_win(&app);
            return Ok(None);
        }
        if app.get_webview_window(WIN_LABEL).is_none() {
            tracing::info!("[OpenCode] 登录窗口被关闭,放弃捕获");
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(win) = app.get_webview_window(WIN_LABEL) else {
            tracing::info!("[OpenCode] 登录窗口被关闭,放弃捕获");
            return Ok(None);
        };

        let cur_url = win.url().map(|u| u.to_string()).unwrap_or_default();
        let cookies = match win.cookies() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "[OpenCode] 读取 cookies 失败,下轮重试");
                continue;
            }
        };

        // 诊断:URL 变化时记一次当前 URL + 此刻 opencode.ai cookie 名(**只记名/域,不记值**),
        // 完整追踪 auth→workspace 跳转流 + session cookie 何时出现,给后续「抓包确定用量端点」定位。
        if cur_url != last_url {
            last_url = cur_url.clone();
            let seen: Vec<String> = cookies
                .iter()
                .filter(|c| c.domain().unwrap_or("").contains(COOKIE_DOMAIN_MARKER))
                .map(|c| format!("{}@{}", c.name(), c.domain().unwrap_or("?")))
                .collect();
            tracing::info!(opencode_cookies = seen.len(), names = ?seen, url = %cur_url, "[OpenCode] 登录窗 URL/cookie 快照");
        }

        // 登录成功信号:URL 进已认证区。抓 opencode.ai 域全部 cookie(name=value 拼 Cookie 头)。
        if cur_url.contains(AUTHED_PATH_MARKER) {
            let parts: Vec<String> = cookies
                .iter()
                .filter(|c| {
                    c.domain().unwrap_or("").contains(COOKIE_DOMAIN_MARKER) && !c.value().is_empty()
                })
                .map(|c| format!("{}={}", c.name(), c.value()))
                .collect();
            if parts.is_empty() {
                // 进了 authed 区但还没拿到 cookie(可能在跳转中),下轮再试。
                continue;
            }
            let names: Vec<&str> = cookies
                .iter()
                .filter(|c| {
                    c.domain().unwrap_or("").contains(COOKIE_DOMAIN_MARKER) && !c.value().is_empty()
                })
                .map(|c| c.name())
                .collect();
            let header = parts.join("; ");
            tracing::info!(
                parts = parts.len(),
                names = ?names,
                url = %cur_url,
                "[OpenCode] 已捕获 session cookie,关闭登录窗"
            );
            close_win(&app);
            return Ok(Some(header));
        }
    }
}

/// 关登录窗 —— 用 `destroy()`(强制销毁,绕过 CloseRequested 拦截)且在**主线程**执行
/// (macOS 窗口操作须主线程),确保登录窗一定被销毁不残留(同 mimo_session 的处理)。
fn close_win(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(WIN_LABEL) {
            let _ = w.destroy();
        }
    });
}
