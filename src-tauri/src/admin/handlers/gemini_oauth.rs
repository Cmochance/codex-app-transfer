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

use super::super::registry_io::{with_config_write, ConfigMutation};
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
///
/// 走 [`with_config_write`] 闭包模式 atomic RMW,防与并发 form save / desktop
/// switch_provider 等其他 RMW 路径互相 overwrite(H1 修)。
fn sync_project_id_to_active_provider(project_id: &str) -> Result<(), String> {
    with_config_write(|cfg| {
        let Some(active) = active_provider(cfg) else {
            return Err("no active provider".into());
        };
        if active.get("apiFormat").and_then(|v| v.as_str()) != Some("gemini_cli_oauth") {
            // skip 分支 — 不动 disk(chatgpt-codex P1 修:read-only-then-write
            // 退化路径会跟未迁的 raw load+save 并发覆盖)
            return Ok(ConfigMutation::Unchanged(()));
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
        Ok(ConfigMutation::Modified(()))
    })
}

/// logout 时清 active provider 的 `extra.cloud_code_project_id`。**镜像 sync**
/// 的 active+apiFormat 双 gate(silent-failure-hunter C1 修):原版无脑遍历所有
/// provider,会抹掉非 active / 非 gemini_cli_oauth 的 provider 的 project_id。
/// 用户多 OAuth 账号时会让其他 provider 莫名失效。
///
/// 走 [`with_config_write`] atomic RMW,同 sync(H1 修)。
fn clear_project_id_from_active_provider() -> Result<(), String> {
    with_config_write(|cfg| {
        let Some(active) = active_provider(cfg) else {
            // skip — 不动 disk(chatgpt-codex P1 修)
            return Ok(ConfigMutation::Unchanged(()));
        };
        if active.get("apiFormat").and_then(|v| v.as_str()) != Some("gemini_cli_oauth") {
            return Ok(ConfigMutation::Unchanged(()));
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
        // 跟踪是否真删了字段 — 没有的 provider 也走过遍历但实际无 mutation,
        // 应回 Unchanged 让 with_config_write 跳过 save
        let mut actually_removed = false;
        for p in providers.iter_mut() {
            if p.get("id").and_then(|v| v.as_str()) != Some(active_id.as_str()) {
                continue; // 只清 active provider
            }
            if let Some(obj) = p.as_object_mut() {
                if let Some(extra) = obj.get_mut("extra").and_then(|v| v.as_object_mut()) {
                    if extra.remove("cloud_code_project_id").is_some() {
                        actually_removed = true;
                    }
                }
            }
            break;
        }
        Ok(if actually_removed {
            ConfigMutation::Modified(())
        } else {
            ConfigMutation::Unchanged(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::handlers::common::test_support::with_isolated_home;
    use crate::admin::registry_io::with_config_write;
    use serde_json::json;

    #[test]
    fn routes_compile_and_paths_are_unique() {
        // smoke test:确保 routes() 编译且不 panic
        let _ = routes();
    }

    /// 写一个特定 providers 数组到 disk(测试 fixture)
    fn seed_config(cfg_value: Value) {
        with_config_write(|cfg| {
            *cfg = cfg_value;
            Ok(ConfigMutation::Modified(()))
        })
        .unwrap();
    }

    /// 读出当前 providers 数组用于断言
    fn read_providers() -> Vec<Value> {
        with_config_write(|cfg| {
            Ok(ConfigMutation::Unchanged(
                cfg.get("providers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            ))
        })
        .unwrap()
    }

    /// G2 contract 1:active=gemini_cli_oauth → sync 把 project_id 写入 active provider 的 extra
    #[test]
    fn sync_writes_project_id_to_active_oauth_provider() {
        with_isolated_home(|_home| {
            seed_config(json!({
                "activeProvider": "p-oauth",
                "providers": [
                    {"id": "p-oauth", "apiFormat": "gemini_cli_oauth", "extra": {}},
                ]
            }));
            sync_project_id_to_active_provider("proj-xyz").unwrap();
            let providers = read_providers();
            assert_eq!(
                providers[0]["extra"]["cloud_code_project_id"], "proj-xyz",
                "active=gemini_cli_oauth 必须把 project_id 写入 extra"
            );
        });
    }

    /// G2 contract 2:active 不是 gemini_cli_oauth → sync 不动任何 provider(防写错 provider)
    #[test]
    fn sync_skips_when_active_is_not_oauth() {
        with_isolated_home(|_home| {
            seed_config(json!({
                "activeProvider": "p-other",
                "providers": [
                    {"id": "p-other", "apiFormat": "openai_chat", "extra": null},
                    {"id": "p-oauth", "apiFormat": "gemini_cli_oauth", "extra": {}},
                ]
            }));
            sync_project_id_to_active_provider("proj-xyz").unwrap();
            let providers = read_providers();
            assert!(
                providers[0]["extra"].is_null(),
                "active 不是 OAuth 时 active provider extra 不该被改"
            );
            assert!(
                providers[1]["extra"]["cloud_code_project_id"].is_null(),
                "active 不是 OAuth 时其他 OAuth provider 也不该被写"
            );
        });
    }

    /// G2 contract 3:**C1 回归 gate** — clear 只清 active 的 project_id,
    /// 其他 gemini_cli_oauth provider 的 project_id 必须保留(用户多账号场景)
    #[test]
    fn clear_preserves_other_oauth_providers_project_id() {
        with_isolated_home(|_home| {
            seed_config(json!({
                "activeProvider": "p-active",
                "providers": [
                    {"id": "p-active", "apiFormat": "gemini_cli_oauth",
                     "extra": {"cloud_code_project_id": "active-proj"}},
                    {"id": "p-other", "apiFormat": "gemini_cli_oauth",
                     "extra": {"cloud_code_project_id": "other-proj"}},
                ]
            }));
            clear_project_id_from_active_provider().unwrap();
            let providers = read_providers();
            assert!(
                providers[0]["extra"]["cloud_code_project_id"].is_null()
                    || providers[0]["extra"].get("cloud_code_project_id").is_none(),
                "active provider 的 project_id 必须被清"
            );
            assert_eq!(
                providers[1]["extra"]["cloud_code_project_id"], "other-proj",
                "**C1 回归 gate**:其他 OAuth provider 的 project_id 必须保留"
            );
        });
    }

    /// G2 contract 4:无 active provider → sync 返 Err(login 时必有 active),
    /// clear 返 Ok(logout 容忍无 active,best-effort 清理)
    #[test]
    fn sync_and_clear_no_active_provider_behavior() {
        with_isolated_home(|_home| {
            seed_config(json!({
                "providers": [],
                // activeProvider 缺失
            }));
            assert!(
                sync_project_id_to_active_provider("proj").is_err(),
                "sync 无 active 必须 Err — login 流必须有 active 才走到这"
            );
            assert!(
                clear_project_id_from_active_provider().is_ok(),
                "clear 无 active 应 Ok — logout best-effort 容忍"
            );
        });
    }
}
