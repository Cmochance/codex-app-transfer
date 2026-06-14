//! 小米 MiMo Token Plan 网页 session 抓取(MOC-211)。
//!
//! MiMo 套餐用量只在 `platform.xiaomimimo.com` 控制台后面、走小米账号 SSO,认证靠
//! **httpOnly** cookie `api-platform_serviceToken`(tp- 推理 key 不通用,实测带 key 仍 401)。
//! app 读不到外部默认浏览器的 httpOnly cookie,故用 app 内嵌 Tauri `WebviewWindow` 登录:
//! 加载控制台 → 用户登录小米账号 → Rust 侧轮询 `webview.cookies()`(底层
//! `WKHTTPCookieStore.getAllCookies`,**含 httpOnly**)抓到 serviceToken → 拼成
//! `Cookie:` 头返回给 caller 落库。
//!
//! 体积零增量:复用 app 主窗口已在用的系统 WebView(macOS WebKit.framework)+ 已链接的
//! tauri/wry crate,无新依赖、无 feature 门禁、无打包引擎。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// setup 阶段注入,供登录开窗用(AdminState 在建 router 时尚无 AppHandle,故走全局)。
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn init(handle: AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

/// 控制台首页;未登录会自动 302 到小米账号 SSO,登录后跳回。
const LOGIN_URL: &str = "https://platform.xiaomimimo.com/console/plan-manage";
const WIN_LABEL: &str = "mimo-login";
/// 抓取所需 cookie:serviceToken(httpOnly)是认证必需项(实测去掉即 401);其余一并带上
/// 确保鉴权完整。顺序即拼 `Cookie:` 头的顺序。
const WANT: &[&str] = &[
    "api-platform_serviceToken",
    "api-platform_slh",
    "api-platform_ph",
    "userId",
];
const POLL_TIMEOUT: Duration = Duration::from_secs(180);

/// 打开内嵌登录窗,轮询抓 session cookie。
/// - `Ok(Some(header))`:抓到,返回拼好的 `Cookie:` 头(供 caller 落库)。
/// - `Ok(None)`:用户关窗 / 超时未完成登录(非错误,前端显「未登录」不弹错)。
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
            let url: tauri::Url = LOGIN_URL.parse().map_err(|e| format!("URL 解析失败: {e}"))?;
            WebviewWindowBuilder::new(&app_build, WIN_LABEL, WebviewUrl::External(url))
                .title("登录小米账号 · 获取 MiMo 套餐用量")
                .inner_size(480.0, 760.0)
                .build()
                .map_err(|e| format!("创建登录窗口失败: {e}"))?;
            Ok(())
        })();
        let _ = tx.send(res);
    })
    .map_err(|e| format!("主线程派发失败: {e}"))?;
    rx.await.map_err(|e| format!("窗口创建回传失败: {e}"))??;

    // 轮询 cookies 抓 serviceToken(每秒一次,≤3min)。cookies() 内部 dispatch 主线程读
    // WKHTTPCookieStore,从 tokio worker 调只阻塞该 worker 短暂、不堵主线程。
    let started = Instant::now();
    let mut snapshot_logged = false;
    loop {
        if started.elapsed() > POLL_TIMEOUT {
            tracing::info!("[MiMo] 登录超时(未捕获 session),关闭登录窗");
            close_win(&app);
            return Ok(None);
        }
        // 用户手动关窗 → 放弃(干净返回,不当错误)。
        if app.get_webview_window(WIN_LABEL).is_none() {
            tracing::info!("[MiMo] 登录窗口被关闭,放弃捕获");
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(win) = app.get_webview_window(WIN_LABEL) else {
            tracing::info!("[MiMo] 登录窗口被关闭,放弃捕获");
            return Ok(None);
        };
        let cookies = match win.cookies() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "[MiMo] 读取 cookies 失败,下轮重试");
                continue;
            }
        };
        // 诊断(首轮一次):打印看到的 xiaomimimo cookie 名 + 域(**只记名/域,不记值**),
        // 定位"为何没匹配到 serviceToken"。
        if !snapshot_logged {
            snapshot_logged = true;
            let seen: Vec<String> = cookies
                .iter()
                .filter(|c| c.domain().unwrap_or("").contains("xiaomimimo"))
                .map(|c| format!("{}@{}", c.name(), c.domain().unwrap_or("?")))
                .collect();
            tracing::debug!(total = cookies.len(), xiaomi = seen.len(), names = ?seen, "[MiMo] cookies 首轮快照");
        }
        // 按 **name** 匹配(不卡 domain:wry 设的 domain 形态可能带前导点/差异,按名最稳)。
        let mut got: HashMap<&str, String> = HashMap::new();
        for c in &cookies {
            if let Some(name) = WANT.iter().find(|w| **w == c.name()) {
                if !c.value().is_empty() {
                    got.insert(*name, c.value().to_string());
                }
            }
        }
        if got.contains_key("api-platform_serviceToken") {
            let header = WANT
                .iter()
                .filter_map(|n| got.get(n).map(|v| format!("{n}={v}")))
                .collect::<Vec<_>>()
                .join("; ");
            tracing::info!(parts = got.len(), "[MiMo] 已捕获 session cookie,关闭登录窗");
            close_win(&app);
            return Ok(Some(header));
        }
    }
}

/// 关登录窗 —— 用 `destroy()`(强制销毁,绕过 CloseRequested 拦截)而非 `close()`,且在
/// **主线程**执行(macOS 窗口操作须主线程)。`close()` 会触发 app 的 close-to-tray 处理器;
/// 虽已让该处理器只管主窗口,这里仍用 destroy 双保险、确保登录窗一定被销毁不残留。
fn close_win(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(WIN_LABEL) {
            let _ = w.destroy();
        }
    });
}
