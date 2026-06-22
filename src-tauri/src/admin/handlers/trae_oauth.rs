//! `/api/trae-oauth/*` admin handlers — Trae(字节 TRAE SOLO CN / Work CN 账号登录)
//! OAuth 登录 / 状态 / 注销。
//!
//! 跟 [`super::zai_oauth`] **并行**,但几点不同:
//! 1. **按 provider id keying**(多账号):`?providerId=<id>` 区分;每条目一套独立凭证 +
//!    设备指纹(同设备多账号隔离)。
//! 2. **login-first**:`providerId` 可空 —— 未保存 provider 上登录写 pending,保存后由
//!    `claim` 绑定到新 id(对齐 GLM 的「先登录后保存」)。
//! 3. **内置 webview 登录**:复用 [`crate::web_session_quota`] 的 `WebviewWindowBuilder`
//!    开内置登录窗加载 authorize URL(不开外部浏览器,对齐 MiMo/OpenCode),loopback 收
//!    callback;用户关窗 = 取消。
//!
//! 当前只支持 CN edition([`TraeEdition::Cn`]);国际版留 fast-follow。
//!
//! ## 路由(providerId 可空 = 未保存 provider 走 pending)
//! - `POST   /api/trae-oauth/login?providerId=<id>`
//! - `GET    /api/trae-oauth/status?providerId=<id>`
//! - `DELETE /api/trae-oauth/login/cancel`
//! - `DELETE /api/trae-oauth/logout?providerId=<id>`
//! - `POST   /api/trae-oauth/claim?providerId=<id>`(保存后绑定 pending → provider)

use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use std::sync::atomic::{AtomicBool, Ordering};

use codex_app_transfer_gemini_oauth::{
    claim_pending_for_provider, run_trae_login, OauthFlowConfig, TraeCredentialStore, TraeEdition,
    TraeError, TraePendingStore,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use super::super::state::AdminState;
use super::common::err;
use crate::web_session_quota;

/// 内置登录 webview 窗口 label。
const TRAE_LOGIN_WIN: &str = "trae-oauth-login";

// ── providerId query 解析 ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProviderIdQuery {
    #[serde(default, rename = "providerId")]
    provider_id: String,
}

/// 取 `?providerId=<id>`:有值返 `Some(trim)`;空 → `None`(= 未保存 provider,走 pending,
/// **不是错误**)。
fn parse_provider_id(q: &ProviderIdQuery) -> Option<String> {
    let id = q.provider_id.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

// ── 进程级 cancel slot(独立于 zai / antigravity / gemini-cli)──────────

fn cancel_slot() -> &'static Mutex<Option<(u64, watch::Sender<bool>)>> {
    static SLOT: OnceLock<Mutex<Option<(u64, watch::Sender<bool>)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn next_epoch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn lock_cancel_slot_with_poison_flag() -> (
    std::sync::MutexGuard<'static, Option<(u64, watch::Sender<bool>)>>,
    bool,
) {
    match cancel_slot().lock() {
        Ok(g) => (g, false),
        Err(poison) => {
            tracing::warn!(
                error_id = "TRAE_CANCEL_SLOT_POISONED",
                "trae cancel slot mutex poisoned by prior panic; recovering"
            );
            (poison.into_inner(), true)
        }
    }
}

fn lock_cancel_slot() -> std::sync::MutexGuard<'static, Option<(u64, watch::Sender<bool>)>> {
    lock_cancel_slot_with_poison_flag().0
}

#[derive(Debug, Clone, Copy)]
pub struct CancelOutcome {
    pub cancelled: bool,
    pub slot_recovered: bool,
    pub cancelled_epoch: Option<u64>,
}

/// 取消 in-flight 登录(UI 关窗 / app 退出 / 新登录抢占)。
pub fn cancel_in_flight_login() -> CancelOutcome {
    let (mut guard, slot_recovered) = lock_cancel_slot_with_poison_flag();
    let (cancelled, cancelled_epoch) = if let Some((epoch, sender)) = guard.take() {
        let _ = sender.send(true);
        (true, Some(epoch))
    } else {
        (false, None)
    };
    CancelOutcome {
        cancelled,
        slot_recovered,
        cancelled_epoch,
    }
}

fn login_done_channel() -> &'static (watch::Sender<u64>, watch::Receiver<u64>) {
    static C: OnceLock<(watch::Sender<u64>, watch::Receiver<u64>)> = OnceLock::new();
    C.get_or_init(|| watch::channel(0))
}

/// app 退出时等当前 in-flight 登录跑完(避免 OAuth 流程被硬切留半截状态)。
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

// ── shared HTTP client(独立 pool）───────────────────────────────────

fn shared_trae_http_client() -> Result<&'static reqwest::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let cell = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| {
                tracing::error!(
                    error_id = "TRAE_HTTP_CLIENT_BUILDER_FAILED",
                    error = %e,
                    "trae reqwest::Client::builder failed"
                );
                format!("reqwest::Client::builder failed: {e}")
            })
    });
    match cell {
        Ok(c) => Ok(c),
        Err(_) => Err("trae HTTP client init failed (TLS/resolver issue); check TRAE_HTTP_CLIENT_BUILDER_FAILED log"),
    }
}

// ── routes ─────────────────────────────────────────────────────────

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/trae-oauth/status", get(status_handler))
        .route("/api/trae-oauth/login", post(login_handler))
        .route("/api/trae-oauth/login/cancel", delete(cancel_login_handler))
        .route("/api/trae-oauth/logout", delete(logout_handler))
        // login-first:保存 provider 后把 pending 凭证绑定到新 id
        .route("/api/trae-oauth/claim", post(claim_handler))
}

async fn cancel_login_handler() -> impl IntoResponse {
    let outcome = cancel_in_flight_login();
    if outcome.cancelled {
        tracing::info!("trae OAuth login cancelled by user request");
    } else if outcome.slot_recovered {
        tracing::warn!(
            error_id = "TRAE_CANCEL_NOOP_AFTER_POISON",
            "trae cancel requested,no in-flight login but slot had been poison-recovered"
        );
    } else {
        tracing::debug!("trae cancel requested but no in-flight login");
    }
    Json(json!({
        "cancelled": outcome.cancelled,
        "slotRecovered": outcome.slot_recovered,
    }))
    .into_response()
}

async fn status_handler(Query(q): Query<ProviderIdQuery>) -> impl IntoResponse {
    let provider_id = parse_provider_id(&q);
    // 有 id → 查 trae/<id>.json;无 id(未保存 provider)→ 查 pending(login-first 后已落 pending)。
    let loaded = match &provider_id {
        Some(id) => TraeCredentialStore::for_provider_id(id).and_then(|s| s.load()),
        None => TraePendingStore::for_pending().and_then(|s| s.load()),
    };
    match loaded {
        Ok(None) => {
            Json(json!({ "loggedIn": false, "providerId": provider_id, "pending": provider_id.is_none() }))
                .into_response()
        }
        Ok(Some(cred)) => Json(json!({
            "loggedIn": true,
            "providerId": provider_id,
            "pending": provider_id.is_none(),
            "email": cred.email,
            "userId": cred.user_id,
            "aiRegion": cred.ai_region,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("trae credential store load: {e}"),
        )
        .into_response(),
    }
}

async fn login_handler(Query(q): Query<ProviderIdQuery>) -> impl IntoResponse {
    // provider_id 可空:有 id = 已保存 provider(直接写 trae/<id>.json);无 id = 未保存
    // provider 上 login-first(写 pending,保存时再 claim)。
    let provider_id = parse_provider_id(&q);
    let id_label = provider_id
        .clone()
        .unwrap_or_else(|| "(pending)".to_string());
    let my_epoch = next_epoch();
    let _done_guard = LoginDoneGuard { epoch: my_epoch };

    let http = match shared_trae_http_client() {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response(),
    };

    let mut config = OauthFlowConfig::default();
    // 复用 transfer 内置 webview(对齐 MiMo/OpenCode),不开外部浏览器:auto_open_browser=false,
    // 在 on_auth_url 回调里开内置登录窗加载 authorize URL,loopback 照常收 callback。
    config.auto_open_browser = false;
    config.on_auth_url = Some(Arc::new(|url: &str| {
        tracing::info!(
            auth_url = url,
            "trae OAuth auth URL 已生成 — 内置 webview 打开中"
        );
        let url = url.to_string();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = web_session_quota::open_external_login_window(
                TRAE_LOGIN_WIN,
                "Trae 登录",
                &url,
                (520.0, 720.0),
            )
            .await
            {
                tracing::warn!(error = %e, "[Trae] 打开内置登录窗失败");
            }
        });
    }));

    // 注册 cancel sender + 抢占语义(新登录抢占任何 in-flight)
    let (cancel_tx, cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight trae OAuth login");
            let _ = prev_sender.send(true);
        }
    }

    // 用户手动关登录窗 = 取消:登录结束前轮询窗口,曾开过又关掉则触发 cancel。
    let login_done = Arc::new(AtomicBool::new(false));
    {
        let done = login_done.clone();
        tauri::async_runtime::spawn(async move {
            let mut seen_open = false;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                if done.load(Ordering::Relaxed) {
                    break; // 登录已结束(成功/失败/超时),停止监视
                }
                if web_session_quota::external_login_window_open(TRAE_LOGIN_WIN) {
                    seen_open = true;
                } else if seen_open {
                    tracing::info!("[Trae] 登录窗被用户关闭 → 取消登录");
                    cancel_in_flight_login();
                    break;
                }
            }
        });
    }

    // 当前只支持 CN edition
    let result = run_trae_login(
        http,
        TraeEdition::Cn,
        provider_id.as_deref(),
        &config,
        Some(cancel_rx),
    )
    .await;
    login_done.store(true, Ordering::Relaxed);
    web_session_quota::close_external_login_window(TRAE_LOGIN_WIN);
    cleanup_slot(my_epoch);

    let pending = provider_id.is_none();
    match result {
        Ok(cred) => Json(json!({
            "loggedIn": true,
            "providerId": provider_id,
            "pending": pending,
            "email": cred.email,
            "userId": cred.user_id,
            "aiRegion": cred.ai_region,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(TraeError::Flow(codex_app_transfer_gemini_oauth::FlowError::Cancelled)) => {
            tracing::info!(
                provider_id = id_label,
                "trae OAuth login cancelled — 不落盘"
            );
            Json(json!({"loggedIn": false, "cancelled": true, "providerId": provider_id}))
                .into_response()
        }
        Err(e) => {
            tracing::warn!(provider_id = id_label, error = %e, "trae OAuth login 失败");
            Json(json!({"loggedIn": false, "providerId": provider_id, "error": e.to_string()}))
                .into_response()
        }
    }
}

async fn logout_handler(Query(q): Query<ProviderIdQuery>) -> impl IntoResponse {
    let provider_id = parse_provider_id(&q);
    // 有 id → 删 trae/<id>.json;无 id → 删 pending。
    let deleted = match &provider_id {
        Some(id) => TraeCredentialStore::for_provider_id(id).and_then(|s| s.delete()),
        None => TraePendingStore::for_pending().and_then(|s| s.delete()),
    };
    if let Err(e) = deleted {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("trae credential delete failed: {e}"),
        )
        .into_response();
    }
    Json(json!({ "loggedIn": false, "providerId": provider_id })).into_response()
}

/// login-first 收尾:保存 provider 拿到 id 后,把 pending 凭证绑定到该 id。
async fn claim_handler(Query(q): Query<ProviderIdQuery>) -> impl IntoResponse {
    let Some(provider_id) = parse_provider_id(&q) else {
        return err(StatusCode::BAD_REQUEST, "claim 需要 providerId".to_string()).into_response();
    };
    match claim_pending_for_provider(&provider_id) {
        Ok(claimed) => {
            if claimed {
                tracing::info!(provider_id, "trae pending 凭证已绑定到 provider");
            }
            Json(json!({ "claimed": claimed, "providerId": provider_id })).into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("trae claim pending failed: {e}"),
        )
        .into_response(),
    }
}

fn cleanup_slot(my_epoch: u64) {
    let mut slot = lock_cancel_slot();
    if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
        slot.take();
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
    fn parse_provider_id_trims_and_rejects_empty() {
        assert_eq!(
            parse_provider_id(&ProviderIdQuery {
                provider_id: "  trae-cn-1  ".into()
            }),
            Some("trae-cn-1".to_string())
        );
        assert_eq!(
            parse_provider_id(&ProviderIdQuery {
                provider_id: "   ".into()
            }),
            None
        );
        assert_eq!(
            parse_provider_id(&ProviderIdQuery {
                provider_id: "".into()
            }),
            None
        );
    }

    #[test]
    fn cancel_with_no_in_flight_returns_false() {
        let _ = lock_cancel_slot().take();
        let outcome = cancel_in_flight_login();
        assert!(!outcome.cancelled);
    }
}
