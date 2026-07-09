//! `/api/grok-build-oauth/*` admin handlers — grok build(xAI grok CLI 编码后端)账号登录
//! (OAuth2 **authorization code + PKCE**,MOC-300)登录 / 状态 / 注销 / 取消。
//!
//! 跟 [`super::qoder_oauth`] 并行但**单账号**(非账号池,对齐 antigravity 单账号):
//! 1. **auth-code + PKCE**:`prepare_grok_build_authorization`(OIDC discovery + PKCE)→ 起本地
//!    loopback callback server(固定 `127.0.0.1:56121`)→ 内置 webview 导航打开 authorize URL,
//!    用户在**真实 webview 里授权**(自然过 Cloudflare challenge —— device flow 的裸 POST 被 CF 拦,
//!    见 MOC-300)→ **实证(2026-07-09 真机)**:xAI 显示 code 让用户复制、前端粘回经
//!    `submit-code` 送来(loopback 兜底自动捕获)→ `complete_grok_build_login` 换 token 落盘。
//!    login 用 select 等 loopback 或手动粘 code 先到者胜;用户关窗 = 取消。
//! 2. **单账号落盘**:凭证存 `~/.codex-app-transfer/grok-build-oauth.json`(覆盖式,一账号)。
//! 3. **有 refresh**:access token 过期前 5min 自动 refresh(出站前 `ensure_valid_grok_build_token`)。
//!
//! ## 路由
//! - `GET    /api/grok-build-oauth/status`        当前登录态(email / 过期时刻)
//! - `POST   /api/grok-build-oauth/login`         发起登录(长阻塞至完成/取消)
//! - `DELETE /api/grok-build-oauth/login/cancel`  取消 in-flight 登录
//! - `DELETE /api/grok-build-oauth/logout`        注销(删凭证)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::{
    complete_grok_build_login, grok_build_logout, prepare_grok_build_authorization,
    GrokBuildCredentialStore, GrokBuildError, LOOPBACK_PORT, REDIRECT_URI,
};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

/// 登录总超时(用户在授权页停留上限;超时 loopback 停止等待)。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

use super::super::state::AdminState;
use super::common::err;
use crate::web_session_quota;

/// 内置登录 webview 窗口 label。
const GROK_BUILD_LOGIN_WIN: &str = "grok-build-oauth-login";

// ── 进程级 cancel slot(独立于 qoder / workbuddy / trae / zai / gemini-cli)──────

fn cancel_slot() -> &'static Mutex<Option<(u64, watch::Sender<bool>)>> {
    static SLOT: OnceLock<Mutex<Option<(u64, watch::Sender<bool>)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn lock_cancel_slot() -> std::sync::MutexGuard<'static, Option<(u64, watch::Sender<bool>)>> {
    cancel_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn next_epoch() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn cleanup_slot(my_epoch: u64) {
    let mut slot = lock_cancel_slot();
    if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
        slot.take();
    }
}

// ── 手动粘 code slot（[MOC-300] xAI 授权页显示 code 让用户粘回，而非重定向到 loopback）──────
// grok/xAI 的 authorize 页对本 client 显示「复制此 code 到 app」而非跳 loopback（实证 2026-07-09，
// 与官方 grok CLI / pi-xai-oauth 手动粘贴兜底一致）。前端在登录中态提供输入框，用户粘 code 经
// submit-code 端点送到此 slot 唤醒等待中的 login_handler。与 loopback 二选一先到者胜。

fn manual_code_slot() -> &'static Mutex<Option<(u64, oneshot::Sender<String>)>> {
    static SLOT: OnceLock<Mutex<Option<(u64, oneshot::Sender<String>)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn lock_manual_code_slot() -> std::sync::MutexGuard<'static, Option<(u64, oneshot::Sender<String>)>>
{
    manual_code_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn cleanup_manual_code_slot(my_epoch: u64) {
    let mut slot = lock_manual_code_slot();
    if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
        slot.take();
    }
}

/// 从用户粘贴内容提取 authorization code：支持「裸 code」或「完整 callback URL / query 串」
/// （pi-xai-oauth 手动路径同款：bare `[A-Za-z0-9_-]{20,}` 或带 `code=` 的 URL）。
fn extract_manual_code(raw: &str) -> String {
    let s = raw.trim();
    if s.contains("code=") {
        let parsed = tauri::Url::parse(s)
            .or_else(|_| tauri::Url::parse(&format!("http://127.0.0.1/?{s}")))
            .ok();
        if let Some(u) = parsed {
            if let Some((_, v)) = u.query_pairs().find(|(k, _)| k == "code") {
                if !v.is_empty() {
                    return v.into_owned();
                }
            }
        }
    }
    s.to_string()
}

/// 取消结果:是否取消 + 被取消的 epoch(供 app 退出路径 [`wait_for_login_epoch_complete`] 等该 task 真退出)。
#[derive(Debug, Clone, Copy)]
pub struct CancelOutcome {
    pub cancelled: bool,
    pub cancelled_epoch: Option<u64>,
}

/// 取消 in-flight 登录(UI 关窗 / 新登录抢占 / 显式 cancel / app 退出)。返回是否取消 + 被取消的 epoch。
pub fn cancel_in_flight_login() -> CancelOutcome {
    let mut guard = lock_cancel_slot();
    if let Some((epoch, sender)) = guard.take() {
        let _ = sender.send(true);
        CancelOutcome {
            cancelled: true,
            cancelled_epoch: Some(epoch),
        }
    } else {
        CancelOutcome {
            cancelled: false,
            cancelled_epoch: None,
        }
    }
}

/// [AI review P2] 每个 login_handler 完成(成功/失败/取消)时经此 channel 广播自己的 epoch;
/// app 退出路径 [`wait_for_login_epoch_complete`] 据此等 in-flight 登录真跑完(避免退出后 grok
/// device flow 仍写 grok-build-oauth.json ghost 凭证)。grok 独立 static,不复用别的 provider。
fn login_done_channel() -> &'static (watch::Sender<u64>, watch::Receiver<u64>) {
    static C: OnceLock<(watch::Sender<u64>, watch::Receiver<u64>)> = OnceLock::new();
    C.get_or_init(|| watch::channel(0))
}

/// app 退出时等当前 in-flight grok 登录跑完(对齐 workbuddy/qoder/zai/trae/gemini 的退出清理)。
pub async fn wait_for_login_epoch_complete(target_epoch: u64) {
    let mut rx = login_done_channel().1.clone();
    loop {
        if *rx.borrow() >= target_epoch {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// login_handler 持有它;返回(含 early return / panic)时 Drop 广播本次 epoch 已完成。
struct LoginDoneGuard {
    epoch: u64,
}
impl Drop for LoginDoneGuard {
    fn drop(&mut self) {
        let (tx, _) = login_done_channel();
        let my = self.epoch;
        let _ = tx.send_if_modified(|cur| {
            if my > *cur {
                *cur = my;
                true
            } else {
                false
            }
        });
    }
}

// ── shared HTTP client(独立 pool)──────────────────────────────────

fn shared_grok_http_client() -> Result<&'static reqwest::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let cell = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| {
                tracing::error!(
                    error_id = "GROK_BUILD_HTTP_CLIENT_BUILDER_FAILED",
                    error = %e,
                    "grok build reqwest::Client::builder failed"
                );
                format!("reqwest::Client::builder failed: {e}")
            })
    });
    match cell {
        Ok(c) => Ok(c),
        Err(_) => Err("grok build HTTP client init failed (TLS/resolver); 见 GROK_BUILD_HTTP_CLIENT_BUILDER_FAILED 日志"),
    }
}

// ── routes ─────────────────────────────────────────────────────────

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/grok-build-oauth/status", get(status_handler))
        .route("/api/grok-build-oauth/login", post(login_handler))
        .route(
            "/api/grok-build-oauth/submit-code",
            post(submit_code_handler),
        )
        .route(
            "/api/grok-build-oauth/login/cancel",
            delete(cancel_login_handler),
        )
        .route("/api/grok-build-oauth/logout", delete(logout_handler))
}

#[derive(Deserialize)]
struct SubmitCodeBody {
    code: String,
}

/// 用户把授权页显示的 code 粘回：送到等待中的 login_handler（[MOC-300] 手动路径）。
async fn submit_code_handler(Json(body): Json<SubmitCodeBody>) -> impl IntoResponse {
    match lock_manual_code_slot().take() {
        Some((_, tx)) => {
            let accepted = tx.send(body.code).is_ok();
            Json(json!({ "accepted": accepted })).into_response()
        }
        None => Json(json!({ "accepted": false, "error": "无进行中的登录" })).into_response(),
    }
}

async fn status_handler() -> impl IntoResponse {
    let store = match GrokBuildCredentialStore::single() {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("grok build status: {e}"),
            )
            .into_response()
        }
    };
    match store.load() {
        Ok(Some(cred)) => Json(json!({
            "loggedIn": true,
            "email": cred.email,
            "userId": cred.user_id,
            "expiryDate": cred.expiry_date,
        }))
        .into_response(),
        Ok(None) => Json(json!({ "loggedIn": false })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("grok build status load: {e}"),
        )
        .into_response(),
    }
}

async fn cancel_login_handler() -> impl IntoResponse {
    let outcome = cancel_in_flight_login();
    web_session_quota::close_external_login_window(GROK_BUILD_LOGIN_WIN);
    if outcome.cancelled {
        tracing::info!("grok build OAuth login cancelled by user request");
    }
    Json(json!({ "cancelled": outcome.cancelled })).into_response()
}

async fn logout_handler() -> impl IntoResponse {
    match grok_build_logout() {
        Ok(()) => Json(json!({ "loggedIn": false })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("grok build logout failed: {e}"),
        )
        .into_response(),
    }
}

/// loopback 捕获成功结果。
struct CallbackParams {
    code: String,
    state: String,
}

/// loopback 捕获失败原因(用户视角三类:取消 / 超时 / 授权被拒)。
enum CaptureError {
    Cancelled,
    Timeout,
    Denied(String),
}

/// 从 loopback 连接读 HTTP 请求行,取 target(`/callback?code=…&state=…`)。只读一个 buffer 足够
/// (请求行必在首个包);读不到 / 非法返 `None`。
async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).await.ok()?;
    if n == 0 {
        return None;
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    // "GET /callback?code=..&state=.. HTTP/1.1"
    head.lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
}

/// 给浏览器/webview 回极简 HTML(`Connection: close`,别挂住连接)。
async fn write_html_response(
    stream: &mut tokio::net::TcpStream,
    status_line: &str,
    body_html: &str,
) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Grok 登录</title></head>\
         <body style=\"font-family:system-ui;text-align:center;padding-top:15vh;color:#222\">{body_html}</body></html>"
    );
    let resp = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

/// 单连接处理结果。
enum ConnOutcome {
    /// 拿到匹配 `state` 的合法 `code` → 完成捕获。
    Success(CallbackParams),
    /// 授权服务器回 `error`(access_denied 等)→ 终止捕获。
    Denied(String),
    /// 噪声 / 非 callback / state 不匹配 / 畸形 → 忽略,继续等真回调。
    Ignore,
}

/// 处理单个 loopback 连接:读请求行 → 解析 → 回极简 HTML → 判定 [`ConnOutcome`]。`expected_state`
/// 用于**就地校验 state**(不匹配即忽略、不终止捕获),防别处浏览器发来的伪造/杂散 `/callback`
/// 令登录失败(review: 本地 DoS)。
async fn handle_conn(stream: &mut tokio::net::TcpStream, expected_state: &str) -> ConnOutcome {
    let Some(target) = read_request_target(stream).await else {
        write_html_response(stream, "400 Bad Request", "<h2>Bad Request</h2>").await;
        return ConnOutcome::Ignore;
    };
    if !target.starts_with("/callback") {
        write_html_response(stream, "404 Not Found", "").await;
        return ConnOutcome::Ignore;
    }
    // target 是相对路径,配 dummy base 解析 query(tauri 已 re-export url crate)。
    let pairs: std::collections::HashMap<String, String> =
        tauri::Url::parse(&format!("http://127.0.0.1{target}"))
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default();
    if let Some(err) = pairs.get("error") {
        // 同 success 路径:先校验 state(RFC 6749 §4.1.2.1 要求 error 回调也带 state)。别处/杂散的
        // `?error=` 不该中止本次登录(review PlCEe:stale tab 重载 / 本地页面探测)。
        if pairs.get("state").map(String::as_str) != Some(expected_state) {
            write_html_response(
                stream,
                "400 Bad Request",
                "<h2>回调 state 不匹配,已忽略</h2>",
            )
            .await;
            return ConnOutcome::Ignore;
        }
        let desc = pairs
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| err.clone());
        write_html_response(
            stream,
            "200 OK",
            "<h2>授权未完成</h2><p>可关闭此页面返回应用。</p>",
        )
        .await;
        return ConnOutcome::Denied(desc);
    }
    match (pairs.get("code"), pairs.get("state")) {
        (Some(code), Some(state)) if !code.is_empty() && state == expected_state => {
            write_html_response(
                stream,
                "200 OK",
                "<h2>✅ 登录成功</h2><p>可以关闭此页面返回应用。</p>",
            )
            .await;
            ConnOutcome::Success(CallbackParams {
                code: code.clone(),
                state: state.clone(),
            })
        }
        (Some(_), Some(_)) => {
            // state 不匹配 = 很可能别处来的杂散/伪造回调 → 忽略,继续等真回调(不终止登录)。
            write_html_response(
                stream,
                "400 Bad Request",
                "<h2>回调 state 不匹配,已忽略</h2>",
            )
            .await;
            ConnOutcome::Ignore
        }
        _ => {
            write_html_response(stream, "400 Bad Request", "<h2>回调缺少 code</h2>").await;
            ConnOutcome::Ignore
        }
    }
}

/// 单连接读+回包整体上限(防不发数据的连接卡住 accept 循环、令 deadline/cancel 长时间不被 poll)。
const CONN_HANDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// 等 xAI 重定向回 loopback,捕获匹配 `expected_state` 的 `code`。可被 cancel 唤醒、有总 `timeout`;
/// 非 `/callback` / state 不匹配 / 畸形回调都忽略后继续等,不让噪声请求提前终止登录。每个连接的
/// 读+回包有 [`CONN_HANDLE_TIMEOUT`] 上限,避免静默/半包连接把 accept 循环挂死(review)。
async fn capture_callback(
    listener: TcpListener,
    expected_state: &str,
    mut cancel_rx: watch::Receiver<bool>,
    timeout: Duration,
) -> Result<CallbackParams, CaptureError> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Err(CaptureError::Timeout),
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    return Err(CaptureError::Cancelled);
                }
            }
            accept = listener.accept() => {
                let mut stream = match accept {
                    Ok((s, _)) => s,
                    Err(_) => continue,
                };
                // 单连接整体设超时:静默/半包连接最多卡 CONN_HANDLE_TIMEOUT 就放弃、回到 select
                // 重新 poll deadline/cancel(否则 300s 总超时与取消会失效)。
                match tokio::time::timeout(
                    CONN_HANDLE_TIMEOUT,
                    handle_conn(&mut stream, expected_state),
                )
                .await
                {
                    Ok(ConnOutcome::Success(params)) => return Ok(params),
                    Ok(ConnOutcome::Denied(desc)) => return Err(CaptureError::Denied(desc)),
                    Ok(ConnOutcome::Ignore) | Err(_) => continue,
                }
            }
        }
    }
}

async fn login_handler() -> impl IntoResponse {
    let my_epoch = next_epoch();
    // [AI review P2] 任何返回路径(成功/失败/取消/panic)Drop 都广播本 epoch 完成,供 app 退出等待。
    let _login_done_guard = LoginDoneGuard { epoch: my_epoch };
    let http = match shared_grok_http_client() {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response(),
    };

    // 注册 cancel sender + 抢占语义(新登录抢占任何 in-flight)。
    let (cancel_tx, cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight grok build OAuth login");
            let _ = prev_sender.send(true);
        }
    }

    // 1. 发起授权(OIDC discovery + PKCE + authorize URL);不触网授权,CF 拦不到。
    let auth_req = match prepare_grok_build_authorization(http, REDIRECT_URI).await {
        Ok(r) => r,
        Err(e) => {
            cleanup_slot(my_epoch);
            tracing::warn!(error = %e, "grok build 发起授权失败");
            return Json(json!({ "loggedIn": false, "error": e.to_string() })).into_response();
        }
    };

    // 2. 先 bind loopback(开浏览器**之前**,确保重定向回来时 server 已监听)。端口固定 56121 =
    //    xAI client 注册的 redirect_uri,被占则无法完成登录(不能换随机端口),给明确错误。
    let listener = match TcpListener::bind(("127.0.0.1", LOOPBACK_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            cleanup_slot(my_epoch);
            tracing::warn!(error = %e, port = LOOPBACK_PORT, "grok build loopback 端口 bind 失败");
            return Json(json!({
                "loggedIn": false,
                "error": format!("本地回调端口 {LOOPBACK_PORT} 被占用,请关闭占用它的程序后重试({e})"),
            }))
            .into_response();
        }
    };

    // 3. 内置 webview 导航打开 authorize URL —— 真实 webview 过 CF challenge(弃 device flow 的原因)。
    {
        let url = auth_req.authorize_url.clone();
        tracing::info!(authorize_url = %url, "grok build 授权已发起 — 内置 webview 打开 authorize 页");
        tauri::async_runtime::spawn(async move {
            if let Err(e) = web_session_quota::open_external_login_window(
                GROK_BUILD_LOGIN_WIN,
                "Grok Build 登录",
                &url,
                (520.0, 720.0),
            )
            .await
            {
                tracing::warn!(error = %e, "[GrokBuild] 打开内置登录窗失败");
            }
        });
    }

    // 4. 用户手动关登录窗 = 取消(否则会傻等到超时)。
    let login_done = Arc::new(AtomicBool::new(false));
    {
        let done = login_done.clone();
        tauri::async_runtime::spawn(async move {
            let mut seen_open = false;
            loop {
                tokio::time::sleep(Duration::from_millis(800)).await;
                if done.load(Ordering::Relaxed) {
                    break;
                }
                if web_session_quota::external_login_window_open(GROK_BUILD_LOGIN_WIN) {
                    seen_open = true;
                } else if seen_open {
                    tracing::info!("[GrokBuild] 登录窗被用户关闭 → 取消登录");
                    cancel_in_flight_login();
                    break;
                }
            }
        });
    }

    // 5. 等 code:loopback 自动捕获 **或** 用户手动粘回(先到者胜)。xAI 授权页对本 client 显示
    //    code 让用户复制粘回(不跳 loopback,实证 2026-07-09),故手动路径是主路径;loopback 保留
    //    兜底(万一某些情况真跳回来)。
    let (code_tx, code_rx) = oneshot::channel::<String>();
    {
        let mut slot = lock_manual_code_slot();
        // 抢占旧的 pending(与 cancel slot 一致);旧 sender 直接丢弃。
        *slot = Some((my_epoch, code_tx));
    }
    let capture = tokio::select! {
        r = capture_callback(listener, &auth_req.state, cancel_rx, LOGIN_TIMEOUT) => r,
        manual = code_rx => match manual {
            Ok(raw) => {
                let code = extract_manual_code(&raw);
                if code.is_empty() {
                    Err(CaptureError::Denied("粘贴的内容里没有可识别的 code".into()))
                } else {
                    // 手动粘贴 = 用户可信来源(非杂散本地回调),state 用本次登录 expected,
                    // complete 内的 state 校验直接通过(CSRF 由「用户亲手粘」这一动作保证)。
                    Ok(CallbackParams { code, state: auth_req.state.clone() })
                }
            }
            Err(_) => Err(CaptureError::Cancelled), // sender 被抢占/丢弃
        },
    };
    login_done.store(true, Ordering::Relaxed);
    cleanup_manual_code_slot(my_epoch);
    web_session_quota::close_external_login_window(GROK_BUILD_LOGIN_WIN);

    // 6. 换 token 落盘(complete 内校验 state + PKCE code_verifier)。
    let result = match capture {
        Ok(cb) => complete_grok_build_login(http, &auth_req, &cb.code, &cb.state).await,
        Err(CaptureError::Cancelled) => Err(GrokBuildError::Cancelled),
        Err(CaptureError::Timeout) => Err(GrokBuildError::DeviceCodeExpired),
        Err(CaptureError::Denied(desc)) => Err(GrokBuildError::OAuth {
            error: "access_denied".into(),
            description: desc,
        }),
    };
    cleanup_slot(my_epoch);

    match result {
        Ok(cred) => Json(json!({
            "loggedIn": true,
            "email": cred.email,
            "userId": cred.user_id,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(GrokBuildError::Cancelled) => {
            tracing::info!("grok build OAuth login cancelled — 不落盘");
            Json(json!({ "loggedIn": false, "cancelled": true })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "grok build OAuth login 失败");
            Json(json!({ "loggedIn": false, "error": e.to_string() })).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_compile() {
        let _ = routes();
    }

    #[test]
    fn cancel_with_no_in_flight_returns_false() {
        let _ = lock_cancel_slot().take();
        let outcome = cancel_in_flight_login();
        assert!(!outcome.cancelled);
        assert!(outcome.cancelled_epoch.is_none());
    }

    #[test]
    fn extract_manual_code_handles_bare_url_and_query() {
        // 裸 code(xAI 授权页显示的形态)原样。
        assert_eq!(
            extract_manual_code("  TpKQG9jySp_uFU6ahZcpAIWEBxhEO1Uzd7szAn  "),
            "TpKQG9jySp_uFU6ahZcpAIWEBxhEO1Uzd7szAn"
        );
        // 完整 callback URL → 取 code。
        assert_eq!(
            extract_manual_code("http://127.0.0.1:56121/callback?code=ABC123&state=xy"),
            "ABC123"
        );
        // 裸 query 串(无 scheme)→ 取 code。
        assert_eq!(extract_manual_code("code=ZZ9&state=q"), "ZZ9");
        // 不含 code= 的裸串原样(不误判)。
        assert_eq!(extract_manual_code("plaincode123"), "plaincode123");
    }
}
