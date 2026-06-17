//! `/api/zai-oauth/*` admin handlers — z.ai / bigmodel(GLM Coding Plan 账号登录)
//! OAuth 登录 / 状态 / 注销。
//!
//! 跟 [`super::antigravity_oauth`] **并行**,但 ZCode wire 差异:`run_zai_login`
//! 已内含「token 交换 → 换组织 key → 落盘」整条链,所以这里**不需要** antigravity 那样
//! 的独立 bootstrap_project / project_id sync / models 步骤 —— login_handler 只是
//! cancel-aware 地调一次 `run_zai_login` 然后报结果。
//!
//! **两个 provider**(z.ai / bigmodel):路由用 `?provider=zai|bigmodel` query 区分,
//! 各自独立 token 文件(`{zai,bigmodel}-oauth.json`)。共用一个进程级 cancel slot
//! (新登录抢占任何 in-flight,跟 antigravity 单 slot 同语义;两个 provider 真实场景
//! 顺序登录、不并发)。
//!
//! ## 路由
//! - `POST   /api/zai-oauth/login?provider=zai|bigmodel`
//! - `GET    /api/zai-oauth/status?provider=zai|bigmodel`
//! - `DELETE /api/zai-oauth/login/cancel`
//! - `DELETE /api/zai-oauth/logout?provider=zai|bigmodel`

use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::{
    run_zai_login, OauthFlowConfig, ZaiCredentialStore, ZaiError, ZaiProvider,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use super::super::state::AdminState;
use super::common::err;

// ── provider query 解析 ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProviderQuery {
    #[serde(default)]
    provider: String,
}

/// 把 `?provider=zai|bigmodel` 解成 [`ZaiProvider`];非法值 → `None`(call site 返 400)。
fn parse_provider(q: &ProviderQuery) -> Option<ZaiProvider> {
    match q.provider.trim().to_ascii_lowercase().as_str() {
        "zai" => Some(ZaiProvider::Zai),
        "bigmodel" => Some(ZaiProvider::BigModel),
        _ => None,
    }
}

// ── 进程级 cancel slot(独立于 antigravity / gemini-cli)─────────────────

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
                error_id = "ZAI_CANCEL_SLOT_POISONED",
                "zai cancel slot mutex poisoned by prior panic; recovering"
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

// ── shared HTTP client(独立 pool)──────────────────────────────────

fn shared_zai_http_client() -> Result<&'static reqwest::Client, &'static str> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let cell = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            // 登录含浏览器授权等待 + 多步换 key,整体超时给宽(callback_timeout 在
            // OauthFlowConfig 另控 5min;这个是单次 HTTP 上限)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| {
                tracing::error!(
                    error_id = "ZAI_HTTP_CLIENT_BUILDER_FAILED",
                    error = %e,
                    "zai reqwest::Client::builder failed"
                );
                format!("reqwest::Client::builder failed: {e}")
            })
    });
    match cell {
        Ok(c) => Ok(c),
        Err(_) => Err("zai HTTP client init failed (TLS/resolver issue); check ZAI_HTTP_CLIENT_BUILDER_FAILED log"),
    }
}

// ── routes ─────────────────────────────────────────────────────────

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/zai-oauth/status", get(status_handler))
        .route("/api/zai-oauth/login", post(login_handler))
        .route("/api/zai-oauth/login/cancel", delete(cancel_login_handler))
        .route("/api/zai-oauth/logout", delete(logout_handler))
}

async fn cancel_login_handler() -> impl IntoResponse {
    let outcome = cancel_in_flight_login();
    if outcome.cancelled {
        tracing::info!("zai OAuth login cancelled by user request");
    } else if outcome.slot_recovered {
        tracing::warn!(
            error_id = "ZAI_CANCEL_NOOP_AFTER_POISON",
            "zai cancel requested,no in-flight login but slot had been poison-recovered"
        );
    } else {
        tracing::debug!("zai cancel requested but no in-flight login");
    }
    Json(json!({
        "cancelled": outcome.cancelled,
        "slotRecovered": outcome.slot_recovered,
    }))
    .into_response()
}

async fn status_handler(Query(q): Query<ProviderQuery>) -> impl IntoResponse {
    let Some(provider) = parse_provider(&q) else {
        return err(
            StatusCode::BAD_REQUEST,
            "provider 必须是 zai 或 bigmodel".to_string(),
        )
        .into_response();
    };
    let store = match ZaiCredentialStore::for_provider(provider) {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("home directory unavailable: {e}"),
            )
            .into_response()
        }
    };
    match store.load() {
        Ok(None) => {
            Json(json!({ "loggedIn": false, "provider": provider.wire_id() })).into_response()
        }
        Ok(Some(cred)) => Json(json!({
            "loggedIn": true,
            "provider": provider.wire_id(),
            "email": cred.email,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("zai credential store load: {e}"),
        )
        .into_response(),
    }
}

async fn login_handler(Query(q): Query<ProviderQuery>) -> impl IntoResponse {
    let Some(provider) = parse_provider(&q) else {
        return err(
            StatusCode::BAD_REQUEST,
            "provider 必须是 zai 或 bigmodel".to_string(),
        )
        .into_response();
    };
    let my_epoch = next_epoch();
    let _done_guard = LoginDoneGuard { epoch: my_epoch };

    let http = match shared_zai_http_client() {
        Ok(c) => c,
        Err(msg) => return err(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response(),
    };

    let mut config = OauthFlowConfig::default();
    config.on_auth_url = Some(Arc::new(|url: &str| {
        tracing::info!(
            auth_url = url,
            "zai OAuth auth URL 已生成 — 自动打开浏览器中"
        );
    }));

    // 注册 cancel sender + 抢占语义(新登录抢占任何 in-flight)
    let (cancel_tx, cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight zai OAuth login");
            let _ = prev_sender.send(true);
        }
    }

    // run_zai_login 已内含 cancel-aware(OAuth 后 + 落盘前都查 cancel)+ 换 key + 落盘。
    let result = run_zai_login(http, provider, &config, Some(cancel_rx)).await;
    cleanup_slot(my_epoch);

    match result {
        Ok(cred) => Json(json!({
            "loggedIn": true,
            "provider": provider.wire_id(),
            "email": cred.email,
            "obtainedAt": cred.obtained_at_ms,
        }))
        .into_response(),
        Err(ZaiError::Flow(codex_app_transfer_gemini_oauth::FlowError::Cancelled)) => {
            tracing::info!(
                provider = provider.wire_id(),
                "zai OAuth login cancelled — 不落盘"
            );
            Json(json!({"loggedIn": false, "cancelled": true, "provider": provider.wire_id()}))
                .into_response()
        }
        Err(e) => {
            tracing::warn!(provider = provider.wire_id(), error = %e, "zai OAuth login 失败");
            Json(json!({"loggedIn": false, "provider": provider.wire_id(), "error": e.to_string()}))
                .into_response()
        }
    }
}

async fn logout_handler(Query(q): Query<ProviderQuery>) -> impl IntoResponse {
    let Some(provider) = parse_provider(&q) else {
        return err(
            StatusCode::BAD_REQUEST,
            "provider 必须是 zai 或 bigmodel".to_string(),
        )
        .into_response();
    };
    let store = match ZaiCredentialStore::for_provider(provider) {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("home directory unavailable: {e}"),
            )
            .into_response()
        }
    };
    if let Err(e) = store.delete() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("zai credential delete failed: {e}"),
        )
        .into_response();
    }
    Json(json!({ "loggedIn": false, "provider": provider.wire_id() })).into_response()
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
    fn parse_provider_accepts_both_and_rejects_garbage() {
        assert_eq!(
            parse_provider(&ProviderQuery {
                provider: "zai".into()
            }),
            Some(ZaiProvider::Zai)
        );
        assert_eq!(
            parse_provider(&ProviderQuery {
                provider: "BigModel".into()
            }),
            Some(ZaiProvider::BigModel)
        );
        assert_eq!(
            parse_provider(&ProviderQuery {
                provider: "".into()
            }),
            None
        );
        assert_eq!(
            parse_provider(&ProviderQuery {
                provider: "openai".into()
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
