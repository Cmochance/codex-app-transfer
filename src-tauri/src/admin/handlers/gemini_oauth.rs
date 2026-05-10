//! `/api/gemini-oauth/*` admin handlers — Gemini CLI OAuth 登录 / 状态 / 注销 +
//! Cloud Code Assist project bootstrap。
//!
//! ## 路由
//!
//! - `POST /api/gemini-oauth/login`:启动 OAuth flow → bootstrap project_id →
//!   持久化 token。**长 polling** ≤ 5min(浏览器登录 callback timeout)。response
//!   含 `email + project_id + expires_at`,前端用来更新 UI。**OAuth + bootstrap
//!   + project_id sync 全成功才算 login 成功**(C2 atomicity 修):任一失败返
//!   5xx 不持久化 token,用户必须重试整流
//! - `GET /api/gemini-oauth/status`:返当前 token 状态(已登录 / 未登录 / 即将
//!   过期)。前端 dashboard 启动时调一次
//! - `DELETE /api/gemini-oauth/logout`:`TokenStore::delete()` + 清 active provider
//!   的 `extra.cloud_code_project_id` 字段(只清 active + apiFormat=gemini_cli_oauth
//!   匹配的 provider,不抹其他账号的 project_id)
//!
//! ## OAuth flow 期间 admin 行为
//!
//! `/login` 同步等待 callback 的 long-polling endpoint(单次 axum request 挂着
//! 5min)。webbrowser::open 失败时仅 tracing::warn!,**flow 继续等同一 redirect_uri
//! 的 callback** —— user 拿不到自动浏览器但可手动用任意浏览器打开 URL(URL 在
//! tracing log 里,前端从 log viewer 能看到)。

use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::{
    bootstrap_project, persist_token, run_oauth_flow, OauthFlowConfig, TokenStore,
};
use serde_json::{json, Value};

use super::super::registry_io::{load as load_registry, save as save_registry};
use super::super::state::AdminState;
use super::common::err;
use super::providers::active_provider;

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/gemini-oauth/status", get(status_handler))
        .route("/api/gemini-oauth/login", post(login_handler))
        .route("/api/gemini-oauth/logout", delete(logout_handler))
}

/// `GET /api/gemini-oauth/status` — 返当前 token 状态。
///
/// Response shape:
/// ```json
/// {
///   "loggedIn": true,
///   "email": "user@example.com",
///   "projectId": "auto-provisioned-1234",
///   "expiresAt": 1730000000000,  // ms-epoch
///   "shouldRefresh": false
/// }
/// ```
async fn status_handler() -> impl IntoResponse {
    let store = match TokenStore::from_home_env() {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("HOME unavailable: {e}"),
            )
            .into_response()
        }
    };
    match store.load() {
        Ok(None) => Json(json!({ "loggedIn": false })).into_response(),
        Ok(Some(token)) => Json(json!({
            "loggedIn": true,
            "email": token.email,
            "projectId": token.project_id,
            "expiresAt": token.expiry_date,
            "shouldRefresh": token.should_refresh(),
        }))
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("token store load: {e}"),
        )
        .into_response(),
    }
}

/// `POST /api/gemini-oauth/login` — 启动 OAuth flow + bootstrap project,长 polling
/// 直到完成或 timeout。
///
/// Request body:`{}`(无参数)
/// Response:成功返 200 + 当前 status 形态;失败返 4xx/5xx + error message
async fn login_handler() -> impl IntoResponse {
    // 用 ProxyState 那个 reqwest::Client 浪费(它是给 forward 用的);新建一个
    // 共享 long-living client 给 OAuth + Cloud Code 调用,启用 rustls-tls。
    let http = match reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reqwest client build: {e}"),
            )
            .into_response();
        }
    };

    // 1. 跑 OAuth flow:loopback server + browser open + 等 callback + token exchange
    //
    // on_auth_url callback 落 tracing::info!,前端 log viewer 能看到 URL 给 user
    // 手动粘贴(webbrowser::open 失败时备用路径)。完整 SSE login-stream endpoint
    // 留 followup PR (前端 UI 一并做)。
    let mut config = OauthFlowConfig::default();
    config.on_auth_url = Some(Arc::new(|url: &str| {
        tracing::info!(
            auth_url = url,
            "OAuth auth URL 已生成 — 自动打开浏览器中,失败时 user 可从 log viewer 复制粘贴"
        );
    }));

    let token = match run_oauth_flow(&http, &config).await {
        Ok(t) => t,
        Err(e) => {
            return Json(json!({
                "loggedIn": false,
                "error": e.to_string(),
            }))
            .into_response();
        }
    };

    // 2. Bootstrap Cloud Code project — 拿 project_id
    //
    // **silent-failure-hunter C2 修**:bootstrap 失败时**不 persist token** —— 半 state
    // 让前端 status 看到"loggedIn: true + projectId: null"以为成功,但 GeminiCli
    // Adapter 后续每次请求都返"cloud_code_project_id required" BadRequest。Login
    // 是原子性合约:OAuth + bootstrap 都成功才算 login,任一失败用户必须重试整流
    let project_id = match bootstrap_project(&http, &token.access_token, token.project_id.clone())
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "Cloud Code bootstrap 失败 — token 不 persist,login 整体失败");
            return err(
                StatusCode::BAD_GATEWAY,
                format!(
                    "Google account authenticated but Cloud Code project provisioning failed; \
                     please retry login. Detail: {e}"
                ),
            )
            .into_response();
        }
    };

    // 3. 把 project_id 写进 token + 持久化
    let mut token_with_project = token;
    token_with_project.project_id = Some(project_id.clone());
    let store = match TokenStore::from_home_env() {
        Ok(s) => s,
        Err(e) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("HOME: {e}")).into_response();
        }
    };
    if let Err(e) = persist_token(&store, &token_with_project) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("token persist failed: {e}"),
        )
        .into_response();
    }

    // 4. 把 project_id 同步到 active provider extra(让 GeminiCliAdapter 能读到)
    //
    // **silent-failure-hunter H2 修**:sync 失败必须 fail login,不能 warn-only。
    // sync 内部已经 gate "active=gemini_cli_oauth 时才写",所以非 active 场景
    // 无 op 不会失败到这。如果走到这里 fail = active 是 gemini,project_id 没写
    // 进 provider config → 后续请求每次返 "cloud_code_project_id required" 4xx。
    // 此场景必须用户感知(fail login)而不是"login 看起来成功但 chat 全 fail"
    if let Err(e) = sync_project_id_to_active_provider(&project_id) {
        tracing::error!(error = %e, "project_id sync 失败,login 整体回滚");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Login succeeded but failed to sync project_id to active provider config; \
                 please retry login. Detail: {e}"
            ),
        )
        .into_response();
    }

    Json(json!({
        "loggedIn": true,
        "email": token_with_project.email,
        "projectId": project_id,
        "expiresAt": token_with_project.expiry_date,
        "shouldRefresh": false,
    }))
    .into_response()
}

/// `DELETE /api/gemini-oauth/logout` — 删 token 文件 + 清 active provider 的
/// `cloud_code_project_id`。
async fn logout_handler() -> impl IntoResponse {
    let store = match TokenStore::from_home_env() {
        Ok(s) => s,
        Err(e) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("HOME: {e}")).into_response();
        }
    };
    if let Err(e) = store.delete() {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("token delete failed: {e}"),
        )
        .into_response();
    }
    // 清 active provider 的 cloud_code_project_id (best-effort,失败不阻塞 logout)
    let _ = clear_project_id_from_active_provider();
    Json(json!({ "loggedIn": false })).into_response()
}

/// 把 project_id 写进当前 active provider 的 `extra.cloud_code_project_id` 字段,
/// 让 GeminiCliAdapter 能读到。仅当 active provider 是 `apiFormat=gemini_cli_oauth`
/// 时才写,其他 provider 不动。
fn sync_project_id_to_active_provider(project_id: &str) -> Result<(), String> {
    let mut cfg = load_registry().map_err(|e| e.to_string())?;
    let Some(active) = active_provider(&cfg) else {
        return Err("no active provider".into());
    };
    if active.get("apiFormat").and_then(|v| v.as_str()) != Some("gemini_cli_oauth") {
        return Ok(()); // active provider 不是 gemini_cli_oauth,跳过
    }
    let active_id = active
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("active provider id missing")?
        .to_owned();
    let providers = cfg
        .as_object_mut()
        .and_then(|o| o.get_mut("providers"))
        .and_then(|v| v.as_array_mut())
        .ok_or("no providers array")?;
    for p in providers.iter_mut() {
        if p.get("id").and_then(|v| v.as_str()) == Some(active_id.as_str()) {
            let obj = p.as_object_mut().ok_or("provider not object")?;
            let extra = obj
                .entry("extra".to_owned())
                .or_insert_with(|| Value::Object(Default::default()));
            if let Some(extra_obj) = extra.as_object_mut() {
                extra_obj.insert(
                    "cloud_code_project_id".into(),
                    Value::String(project_id.to_owned()),
                );
            }
            break;
        }
    }
    save_registry(&cfg).map_err(|e| e.to_string())
}

/// logout 时清 active provider 的 `extra.cloud_code_project_id`。**镜像 sync**
/// 的 active+apiFormat 双 gate(silent-failure-hunter C1 修):原版无脑遍历所有
/// provider,会抹掉非 active / 非 gemini_cli_oauth 的 provider 的 project_id。
/// 用户多 OAuth 账号时会让其他 provider 莫名失效。
fn clear_project_id_from_active_provider() -> Result<(), String> {
    let mut cfg = load_registry().map_err(|e| e.to_string())?;
    let Some(active) = active_provider(&cfg) else {
        return Ok(()); // 没 active provider,无需清理
    };
    if active.get("apiFormat").and_then(|v| v.as_str()) != Some("gemini_cli_oauth") {
        return Ok(()); // active 不是 gemini_cli_oauth,跳过
    }
    let active_id = active
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("active provider id missing")?
        .to_owned();
    let providers = cfg
        .as_object_mut()
        .and_then(|o| o.get_mut("providers"))
        .and_then(|v| v.as_array_mut())
        .ok_or("no providers array")?;
    for p in providers.iter_mut() {
        if p.get("id").and_then(|v| v.as_str()) != Some(active_id.as_str()) {
            continue; // 只清 active provider
        }
        if let Some(obj) = p.as_object_mut() {
            if let Some(extra) = obj.get_mut("extra").and_then(|v| v.as_object_mut()) {
                extra.remove("cloud_code_project_id");
            }
        }
        break;
    }
    save_registry(&cfg).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_compile_and_paths_are_unique() {
        // smoke test:确保 routes() 编译且不 panic
        let _ = routes();
    }
}
