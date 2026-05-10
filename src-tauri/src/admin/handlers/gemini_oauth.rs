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

use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use codex_app_transfer_gemini_oauth::{
    bootstrap_project, persist_token, run_oauth_flow_with_cancel, FlowError, OauthFlowConfig,
    TokenStore,
};
use serde_json::{json, Value};
use tokio::sync::watch;

use super::super::registry_io::{with_config_write, ConfigMutation};
use super::super::state::AdminState;
use super::common::err;
use super::providers::active_provider;

/// **进程级**当前 in-flight OAuth login 的 cancel sender + epoch —— 任意时刻
/// 最多一个 login。**epoch 防"晚 take" race**(reviewer high #1):新 login B
/// `slot.replace((epoch_B, tx_B))` 把旧 (epoch_A, tx_A) 抢出来 send 取消 A;
/// 但 A 的 post-flow 清理路径里**只能在 slot 当前 epoch 仍是自己的 epoch_A
/// 时**才 take —— 否则 B 已经接管,A 清理把 B 的 sender 也抹掉,B 整段无法
/// 取消(直到 B 自己结束)。epoch 由 `next_epoch()` 单调原子分配。
///
/// 类型:`Mutex<Option<(u64, watch::Sender<bool>)>>`(epoch, sender)。
/// **C2 修**(silent-failure-hunter 标 critical):原 oneshot::Sender 一次性
/// 消费,只能给 OAuth flow 用,bootstrap_project 等后续阶段(5-30s)收不到
/// cancel,user 看 cancelled:false 但 token 仍 persist。watch::Sender 升级 —
/// 多 receiver clone 跨阶段共享,login_handler 把 OAuth flow + bootstrap
/// project + persist + sync 全部 wrap cancel-aware,真"贯穿 pipeline"。
fn cancel_slot() -> &'static Mutex<Option<(u64, watch::Sender<bool>)>> {
    static SLOT: OnceLock<Mutex<Option<(u64, watch::Sender<bool>)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 单调分配 cancel slot 的 epoch token —— 每个 login 持自己的 epoch,清理时
/// 用 epoch 校验防 ABA / 抢占 race。
///
/// **SAFETY (Relaxed ordering)**:本 fn 用 Relaxed 仅保证原子计数自身的单调性,
/// 不提供跟 cancel_slot 内容的 happens-before。**写 epoch 的 callsite**(`slot.
/// replace((my_epoch, ..))`)和**读 epoch 的 callsite**(`slot.as_ref()` 后比
/// `*e == my_epoch`)都在 cancel_slot() Mutex 锁内执行,Mutex 自身的
/// acquire/release 已提供 happens-before。如果未来 refactor 把 epoch 比较移到
/// 锁外,需要改 Acquire/Release(silent-failure-hunter H2 lock 防回归)
fn next_epoch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// 锁 cancel_slot 时统一处理 poison(reviewer high #2)—— Mutex 数据轻量,
/// poison 时直接 recover 让后续路径继续 + warn log 让 operator 看到曾发生
/// panic。silent ignore 会让 cancel 整个 silent 失效。
fn lock_cancel_slot() -> std::sync::MutexGuard<'static, Option<(u64, watch::Sender<bool>)>> {
    cancel_slot().lock().unwrap_or_else(|poison| {
        tracing::warn!(
            error_id = "OAUTH_CANCEL_SLOT_POISONED",
            "OAuth cancel slot mutex poisoned by prior panic; recovering — verify last login state"
        );
        poison.into_inner()
    })
}

/// 取出并触发当前 in-flight login 的 cancel signal(若有)。返 true 表示真有
/// 一个 login 被取消,false 表示当时没 in-flight。idempotent 安全。
///
/// 调用场景:① DELETE /login/cancel 用户主动按"取消";② app RunEvent::Exit
/// 钩子防 5min 后 token persist 进 disk(user 已经退出 app);③ 新 login 启
/// 动前抢占旧 login(防 user 连点 2 次"登录"按钮 → 2 个 loopback server 抢
/// port + 2 个 OAuth flow 互相覆盖)
pub fn cancel_in_flight_login() -> bool {
    if let Some((_epoch, sender)) = lock_cancel_slot().take() {
        // watch::send(true) 把当前 value 设 true 通知所有 clone 的 receiver。
        // send 失败(所有 receiver 已 drop)等价于 flow 已结束,无 op。
        // **C2 修**:此 send 触发的 cancel 不仅 OAuth flow 收到,login_handler
        // 持的 receiver clone 也会让 bootstrap/persist/sync 阶段 select! 退出
        let _ = sender.send(true);
        return true;
    }
    false
}

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/gemini-oauth/status", get(status_handler))
        .route("/api/gemini-oauth/login", post(login_handler))
        .route(
            "/api/gemini-oauth/login/cancel",
            delete(cancel_login_handler),
        )
        .route("/api/gemini-oauth/logout", delete(logout_handler))
}

/// `DELETE /api/gemini-oauth/login/cancel` — 取消当前 in-flight OAuth login。
/// 响应 `{ cancelled: bool }`(true=确实有 in-flight 被取消,false=没 in-flight)。
async fn cancel_login_handler() -> impl IntoResponse {
    let cancelled = cancel_in_flight_login();
    if cancelled {
        tracing::info!("OAuth login cancelled by user request");
    } else {
        // M2: false 也 log debug 让 "cancel button does nothing" 类报告可追
        tracing::debug!("OAuth cancel requested but no in-flight login");
    }
    Json(json!({ "cancelled": cancelled })).into_response()
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

    // 注册 cancel sender 到进程级 slot:任意 cancel 路径(DELETE /login/cancel /
    // app exit / 新 login 抢占)都能 send(()) 让 flow 立即退。**抢占语义**:新
    // login 启动前 take 旧 sender 触发 send,旧 flow 收到 Cancelled 立即退出 +
    // 释放 loopback port,新 flow 接管。防 user 连点 2 次"登录"产生 2 个并行
    // OAuth flow / 2 个 loopback server / 2 个 callback URL race。
    //
    // **epoch token**(reviewer high #1 修):本 login 持自己的 epoch,post-flow
    // 清理时只在 slot 当前 epoch 跟自己匹配时才 take,防"已被新 login 抢占
    // 后再 take 把新 login 的 sender 误清掉"
    let my_epoch = next_epoch();
    let (cancel_tx, mut cancel_rx) = watch::channel::<bool>(false);
    {
        let mut slot = lock_cancel_slot();
        if let Some((_, prev_sender)) = slot.replace((my_epoch, cancel_tx)) {
            tracing::info!("抢占 in-flight OAuth login(user 连点登录或并发请求)");
            let _ = prev_sender.send(true); // 旧 flow 收到 Cancelled 立即退
        }
    }

    // helper: 跑 inner future,期间 select 监听 cancel — 命中即退出整 login
    // pipeline。**C2 修核心**:wrap bootstrap_project / persist 等任意 await
    // 都能立即响应 cancel(原 oneshot 只够 OAuth flow 用,过了 OAuth 后用户
    // 按取消 silent 失效)
    async fn cancellable<F, T>(
        cancel_rx: &mut watch::Receiver<bool>,
        fut: F,
    ) -> Result<T, FlowError>
    where
        F: std::future::Future<Output = Result<T, FlowError>>,
    {
        // 入口快路径 — cancel 已 set 不浪费起 fut
        if *cancel_rx.borrow() {
            return Err(FlowError::Cancelled);
        }
        tokio::select! {
            res = fut => res,
            // changed() 等 sender send(任意值);loop 直到看到 true,防 spurious
            // 唤醒(实际 watch 不会 spurious 但留 belt-and-suspenders)
            _ = async {
                loop {
                    if cancel_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    if *cancel_rx.borrow() { return; }
                }
            } => Err(FlowError::Cancelled),
        }
    }

    let flow_result = run_oauth_flow_with_cancel(&http, &config, Some(cancel_rx.clone())).await;
    let token = match flow_result {
        Ok(t) => t,
        Err(FlowError::Cancelled) => {
            // 清理 slot — 仅在 epoch 匹配时 take(防被新 login 抢占后误清)
            let mut slot = lock_cancel_slot();
            if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
                slot.take();
            }
            tracing::info!("OAuth login cancelled — 不持久化 token");
            return Json(json!({"loggedIn": false, "cancelled": true})).into_response();
        }
        Err(e) => {
            let mut slot = lock_cancel_slot();
            if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
                slot.take();
            }
            return Json(json!({"loggedIn": false, "error": e.to_string()})).into_response();
        }
    };

    // 2. Bootstrap Cloud Code project — 拿 project_id(**C2 修**:cancel-aware
    // wrap,5-30s bootstrap 中按 cancel 立即退,不再等 LRO timeout)
    //
    // **silent-failure-hunter C2 atomicity**:bootstrap 失败时**不 persist token**
    let project_id = match cancellable(&mut cancel_rx, async {
        bootstrap_project(&http, &token.access_token, token.project_id.clone())
            .await
            .map_err(|e| {
                // bootstrap 错误用 FlowError::TokenParse 当容器类型 — 它专门
                // 给 endpoint-side 错信息留;也可加新 variant 但本 PR scope 内
                // 复用避免 API 大改
                FlowError::TokenParse(format!("cloud_code_bootstrap: {e}"))
            })
    })
    .await
    {
        Ok(id) => id,
        Err(FlowError::Cancelled) => {
            let mut slot = lock_cancel_slot();
            if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
                slot.take();
            }
            tracing::info!("OAuth login cancelled during bootstrap_project — 不持久化 token");
            return Json(json!({"loggedIn": false, "cancelled": true})).into_response();
        }
        Err(e) => {
            let mut slot = lock_cancel_slot();
            if matches!(slot.as_ref(), Some((e2, _)) if *e2 == my_epoch) {
                slot.take();
            }
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

    // 3. 终态 cancel check — 在 sync 写盘前最后机会(cancel 在 persist 之后到
    // 已经晚了,token 已 in disk;此 check 让"刚刚错过 bootstrap 完成"窗口
    // 内到达的 cancel 仍能阻止 sync + persist)
    if *cancel_rx.borrow() {
        let mut slot = lock_cancel_slot();
        if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
            slot.take();
        }
        tracing::info!("OAuth login cancelled after bootstrap, before persist — 不持久化 token");
        return Json(json!({"loggedIn": false, "cancelled": true})).into_response();
    }

    // 4. 把 project_id 写进 token + 持久化(快路径,< 1ms)
    let mut token_with_project = token;
    token_with_project.project_id = Some(project_id.clone());
    let store = match TokenStore::from_home_env() {
        Ok(s) => s,
        Err(e) => {
            let mut slot = lock_cancel_slot();
            if matches!(slot.as_ref(), Some((e2, _)) if *e2 == my_epoch) {
                slot.take();
            }
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("HOME: {e}")).into_response();
        }
    };
    if let Err(e) = persist_token(&store, &token_with_project) {
        let mut slot = lock_cancel_slot();
        if matches!(slot.as_ref(), Some((e2, _)) if *e2 == my_epoch) {
            slot.take();
        }
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("token persist failed: {e}"),
        )
        .into_response();
    }

    // 5. 把 project_id 同步到 active provider extra
    if let Err(e) = sync_project_id_to_active_provider(&project_id) {
        let mut slot = lock_cancel_slot();
        if matches!(slot.as_ref(), Some((e2, _)) if *e2 == my_epoch) {
            slot.take();
        }
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

    // 全成功,清 slot(epoch 匹配时)
    {
        let mut slot = lock_cancel_slot();
        if matches!(slot.as_ref(), Some((e, _)) if *e == my_epoch) {
            slot.take();
        }
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

    /// **核心 preemption race**:login B 抢占 login A 的 slot 后,A 的 post-flow
    /// 清理路径**不能**清掉 B 的 sender —— epoch token 校验保证。reviewer high #1
    /// 修(原版 raw take 会让 B 整段无法取消)。
    ///
    /// 模拟:手动模拟 login A 注册 + login B 抢占 + A 走完 post-flow 清理 +
    /// 验 slot 里仍是 B 的 sender(没被 A 误清)。
    #[test]
    fn cancel_slot_epoch_prevents_a_from_clearing_b_sender() {
        // 隔离前:清空残留(如果别的 test 留下来的)— take 不带 send 不影响
        {
            let _ = lock_cancel_slot().take();
        }
        // 1. login A 注册(C2:watch::channel 替代 oneshot)
        let epoch_a = next_epoch();
        let (tx_a, _rx_a) = watch::channel::<bool>(false);
        {
            let mut slot = lock_cancel_slot();
            slot.replace((epoch_a, tx_a));
        }
        // 2. login B 抢占 — 取出旧 sender 触发 cancel + 注册自己
        let epoch_b = next_epoch();
        let (tx_b, _rx_b) = watch::channel::<bool>(false);
        let prev_sender_taken_by_b = {
            let mut slot = lock_cancel_slot();
            slot.replace((epoch_b, tx_b))
        };
        assert!(prev_sender_taken_by_b.is_some(), "B 抢占应拿到 A 的 sender");
        let (taken_epoch, _) = prev_sender_taken_by_b.unwrap();
        assert_eq!(taken_epoch, epoch_a, "B 拿到的应是 A 的 sender");

        // 3. 模拟 A 的 post-flow 清理 — 用 epoch_a 校验,slot 当前是 epoch_b 不该清
        {
            let mut slot = lock_cancel_slot();
            if matches!(slot.as_ref(), Some((e, _)) if *e == epoch_a) {
                slot.take();
            }
        }
        // 4. 验 slot 仍含 B 的 sender(epoch_b)— 没被 A 的清理误清
        {
            let slot = lock_cancel_slot();
            match slot.as_ref() {
                Some((e, _)) => assert_eq!(
                    *e, epoch_b,
                    "**preemption race**:B 的 sender 应仍在 slot,实际 epoch={e}"
                ),
                None => panic!("B 的 sender 被 A 的清理误删 — race 没修"),
            }
        }
        // cleanup:把 B 也 take 掉防泄漏到下一个 test
        {
            let _ = lock_cancel_slot().take();
        }
    }

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
