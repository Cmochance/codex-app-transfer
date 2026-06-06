//! 透传 forward handler。
//!
//! 行为(Stage 3.1,包含 B1 路由 + B2 鉴权改写 + adapter 协议层):
//! 1. 接收 `Request<Body>`,把 body 完整读出
//! 2. 调 `ProviderResolver` 校验 gateway key,选定上游 provider
//! 3. 按 `provider.api_format` 查 adapter,跑 `prepare_request` 得到上游路径 + 改写后的 body
//! 4. 复制非 hop / 非 Authorization 头到出站
//! 5. 按 `provider.auth_scheme` 注入上游凭据(Bearer 或 X-Api-Key)
//! 6. 注入 `provider.extra_headers`(如 kimi-code 的 User-Agent)和 adapter
//!    默认协议头(如 Anthropic `anthropic-version`)
//! 7. 若 body 中 `model` 是 `"<slug>/<real>"` 形式,把 `<slug>/` 剥掉
//! 8. 用 reqwest 发起出站
//! 9. 用 adapter `transform_response_stream`(默认透传)把响应灌回 axum

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::Response,
};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use codex_app_transfer_adapters::{
    registry::is_local_responses_route, AdapterError, AdapterRegistry,
};
use codex_app_transfer_registry::strip_internal_model_suffix;
use futures_core::Stream;
use futures_util::TryStreamExt;
use thiserror::Error;

use crate::diagnostics::{
    forward_trace_enabled, write_chatgpt_backend_trace, write_forward_trace_jsonl,
    write_upstream_error_bundle, ForwardTraceInput, UpstreamErrorBundleInput,
};
use crate::resolver::{AuthScheme, ResolveError, ResolvedProvider, SharedResolver};
use crate::telemetry::proxy_telemetry;

#[derive(Clone)]
pub struct ProxyState {
    pub http: reqwest::Client,
    pub resolver: SharedResolver,
    pub adapters: AdapterRegistry,
    /// [MOC-124 H-2] chatgpt backend 透传的**鉴权结果**回灌 src-tauri 账号状态机的通道。
    /// relay 下 transfer 不主动刷新 token,`detect()` 用本地 JWT `exp` 判有效 —— 服务端撤销 /
    /// refresh_token 失效本地 exp 看不到 → 前端永显账号正常、用户不知要重登。上游 401 是唯一
    /// 能感知 token 被服务端撤销的信号。proxy crate 不依赖 src-tauri,故用依赖倒置:此处只持
    /// `Arc<dyn Fn>`,由 src-tauri 注入 `report_chatgpt_backend_auth`。`None` = 未注入(测试 /
    /// proxy 独立运行),回灌静默跳过。
    ///
    /// 回调参数 `(token_fp, unauthorized)`:`token_fp` = 该请求 Authorization Bearer token 的
    /// FNV-1a 指纹;`unauthorized` = 上游是否返 401。src-tauri 据此:
    /// - **401**(`unauthorized=true`)→ 记下被撤销 token 的指纹 + 标记需重登。`detect()` 的
    ///   self-heal 只在 active token **变了**(app 外 login / 重新导入 → 指纹不同)时才清,
    ///   否则(还是这个被撤销、本地 exp 没过的旧 token)**保持** relogin —— 不然 detect 用
    ///   local-exp 判有效会立刻抹掉本回灌(H-2 形同无效,本 PR 的 BLOCKER)。
    /// - **2xx**(`unauthorized=false`)→ 若该 token 正是之前被 401 标记撤销的、说明它又有效了
    ///   (瞬时 401 恢复)→ 清。**真撤销持续 401、永不 2xx → 保持;瞬时 401 后续 2xx → 自愈**,
    ///   据此精确区分,避免瞬时 401 把 relogin 卡死。
    on_chatgpt_unauthorized: Option<std::sync::Arc<dyn Fn(u64, bool) + Send + Sync>>,
}

/// 出站 reqwest 默认 User-Agent — 在 provider.extra_headers 没配 UA、客户端
/// UA 又被 `is_strip_on_forward` 剔除后兜底用,**绝不能含 codex/openai/codex_cli
/// 等关键字**(否则等于把 strip 的 UA 又自己写回来)。
const DEFAULT_OUTBOUND_USER_AGENT: &str = concat!("Codex-App-Transfer/", env!("CARGO_PKG_VERSION"));

impl ProxyState {
    pub fn new(resolver: SharedResolver) -> Self {
        Self {
            http: reqwest::Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                // fix(#210): 添加连接超时 + 读超时,防止上游 provider 卡住时
                // proxy 无限等待导致客户端"失联"。LLM reasoning 可能长达数分钟,
                // 设 15 分钟作为绝对上限;connect_timeout 10 秒足够建连。
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(900))
                // 显式设 default UA:client header 复制循环已 strip 客户端
                // user-agent;若 provider.extra_headers 也没配 UA,reqwest
                // 默认会用 `reqwest/<ver>` 作为 default UA,部分 provider
                // 反爬可能 ban "reqwest" 字串。改用中性的 Codex-App-Transfer/<v>
                // 兜底,既不命中 codex 反爬规则,也不在 reqwest 黑名单。
                .user_agent(DEFAULT_OUTBOUND_USER_AGENT)
                // SSRF(AP-001):对每一跳重定向目标复检 host,拒绝跳向私有/内部地址。
                // reqwest 默认跟随最多 10 跳,只校验初始 URL 会被 `302 → 169.254.169.254`
                // 之类绕过整个 SSRF 防护(MOC-68 review 复盘)。这里限制跳数 + 逐跳复检。
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= 5 {
                        return attempt.error("too many redirects".to_string());
                    }
                    let host = attempt.url().host_str().unwrap_or("").to_string();
                    match redirect_host_is_safe(&host) {
                        Ok(()) => attempt.follow(),
                        Err(reason) => {
                            proxy_telemetry()
                                .logs
                                .add("WARN", format!("SSRF blocked redirect → {host}: {reason}"));
                            attempt.error(reason)
                        }
                    }
                }))
                .build()
                .expect("reqwest client"),
            resolver,
            adapters: AdapterRegistry::with_builtins(),
            on_chatgpt_unauthorized: None,
        }
    }

    pub fn from_arc(http: reqwest::Client, resolver: SharedResolver) -> Self {
        Self {
            http,
            resolver,
            adapters: AdapterRegistry::with_builtins(),
            on_chatgpt_unauthorized: None,
        }
    }

    pub fn with_adapters(mut self, adapters: AdapterRegistry) -> Self {
        self.adapters = adapters;
        self
    }

    /// [MOC-124 H-2] 注入「chatgpt backend 透传鉴权结果 → 回灌账号状态」回调。src-tauri 侧用
    /// 它注入 `codex_real_account::report_chatgpt_backend_auth`,把服务端 token 失效(本地 JWT
    /// exp 看不到的撤销)/ 恢复反映到前端账号状态。回调参数 `(token_fp, unauthorized)`。
    pub fn with_relogin_notify(
        mut self,
        notify: std::sync::Arc<dyn Fn(u64, bool) + Send + Sync>,
    ) -> Self {
        self.on_chatgpt_unauthorized = Some(notify);
        self
    }
}

#[derive(Debug, Error)]
pub enum ForwardError {
    #[error("read body: {0}")]
    ReadBody(#[from] axum::Error),
    #[error("upstream request: {0}")]
    Upstream(#[from] reqwest::Error),
    #[error("response build: {0}")]
    Response(#[from] axum::http::Error),
    #[error("invalid header: {0}")]
    Header(String),
    #[error("resolve: {0}")]
    Resolve(#[from] ResolveError),
    #[error("adapter: {0}")]
    Adapter(#[from] AdapterError),
    #[error("bad request: {0}")]
    BadRequest(String),
    /// OAuth bearer 不可用(用户没登过 / refresh 失败 / token 文件 IO 错)。
    /// 跟 generic Header 错误区分,IntoResponse 走 401 + 结构化 code 提示用户
    /// 重新登录,**不**走 502 generic 错误体(2026-05-11 silent-failure 修)。
    #[error("OAuth credentials unavailable: {reason}")]
    OauthUnavailable {
        reason: String,
        /// `true` 表示用户必须重新跑 OAuth login flow 才能恢复(NotLoggedIn /
        /// invalid_grant 等);`false` 是临时网络错误用户可重试。
        needs_login: bool,
    },
}

impl axum::response::IntoResponse for ForwardError {
    fn into_response(self) -> Response {
        let message = self.to_string();
        let telemetry = proxy_telemetry();
        telemetry.stats.record(false);
        telemetry
            .logs
            .add("ERROR", format!("proxy request failed: {message}"));

        // OauthUnavailable 单独走 401 + structured JSON,提示用户重新登录(2026-05-11
        // silent-failure 修)。原版走 502 + plain text "proxy error: invalid header: ..."
        // 用户毫无 actionable 信息,以为是 proxy bug 而不是自己 OAuth 失效。
        if let ForwardError::OauthUnavailable {
            reason,
            needs_login,
        } = &self
        {
            let (code, message) = if *needs_login {
                (
                    "oauth_login_required",
                    format!(
                        "Gemini OAuth credentials missing or revoked — please re-run login from \
                         settings. Detail: {reason}"
                    ),
                )
            } else {
                (
                    "oauth_token_refresh_failed",
                    format!(
                        "Gemini OAuth token refresh transiently failed; please retry. Detail: \
                         {reason}"
                    ),
                )
            };
            let body = serde_json::json!({
                "error": {
                    "message": message,
                    "type": "authentication_error",
                    "code": code,
                }
            });
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(body.to_string()))
                .unwrap();
        }

        // PreviousResponseNotFound 单独走 OpenAI SDK-compatible JSON 错误体,
        // 字面对齐 OpenAI Responses API 服务端真实行为(LM Studio bug tracker
        // #1188、Microsoft semantic-kernel #13128 等多源验证)。这样 SDK 的
        // OpenAI error handler、Codex CLI 等客户端都能走标准 invalid_request
        // 路径,而不会把它当作非结构化错误重试。**英文**对齐 SDK 错误处理。
        if let ForwardError::Adapter(AdapterError::PreviousResponseNotFound {
            previous_response_id,
        }) = &self
        {
            let body = serde_json::json!({
                "error": {
                    "message": format!(
                        "Previous response with id '{previous_response_id}' not found."
                    ),
                    "type": "invalid_request_error",
                    "param": "previous_response_id",
                    "code": "previous_response_not_found",
                }
            });
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(body.to_string()))
                .unwrap();
        }

        let (status, body) = match &self {
            ForwardError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            ForwardError::Resolve(re) => (re.status(), format!("proxy resolve error: {re}")),
            ForwardError::Adapter(ae) => (
                StatusCode::BAD_REQUEST,
                format!("proxy adapter error: {ae}"),
            ),
            _ => (StatusCode::BAD_GATEWAY, format!("proxy error: {self}")),
        };
        Response::builder()
            .status(status)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(body))
            .unwrap()
    }
}

/// hop-by-hop 头(RFC 7230 §6.1)+ 一些代理自身需要重写的头,统一剔除。
fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// 出站时必须从客户端请求里剔除的 header(除 hop-by-hop 之外):
///
/// - `authorization`:gateway 鉴权用的 token,绝不能传到上游(上游用 provider.api_key)
/// - **Codex CLI / OpenAI 身份头**:`originator` / `x-codex-*` / `x-openai-*` /
///   `chatgpt-account-id` 等是 Codex CLI 内置注入的身份标记
///   (`codex-rs/login/src/auth/default_client.rs::default_headers`、
///    `codex-rs/core/src/client.rs:481-605` 等),Kimi For Coding 等第三方
///   provider 反爬规则会按这些头判定"非白名单 client"返回 403
///   `access_terminated_error`。Codex 系身份头对第三方 provider 永远没用,
///   统一剔除零业务损失,且能防御未来 Codex CLI 加新 identity 头。
///   provider.extra_headers 已能注入正确身份(如 `User-Agent: KimiCLI/...`)
///   填补必要 client 标记。
fn is_strip_on_forward(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "authorization" {
        return true;
    }
    // **客户端 User-Agent**:Codex CLI 客户端发的 `User-Agent: codex_cli_rs/...`
    // 是反爬识别非白名单 client 的核心字段(实测 Kimi For Coding Windows 403
    // 元凶)。把它进 strip 列表后,后续逻辑会保证有正确的 UA 出站:
    //
    //   (1) 若 `provider.extra_headers` 含 User-Agent(如 Kimi Code preset 的
    //       `KimiCLI/1.40.0`)→ extras 注入路径会上 UA(forward 复制循环跳过
    //       客户端 UA + extras 注入循环上 extras 的 UA = 干净一份)。
    //   (2) 若 extras **没有** User-Agent(如非 Kimi 系 provider 没配)→
    //       `ProxyState::new` 给 reqwest `Client` 设了中性 default
    //       `Codex-App-Transfer/<version>`,reqwest 会自动用这个 default
    //       兜底,确保上游永远收到一个非 codex 的 UA。
    //
    // `codex-rs/login/src/auth/default_client.rs::default_headers` 自带 codex 系
    // identity headers(originator / x-codex-* 等)在前面已经 strip,这里把 UA
    // 也加进来,Codex CLI 整套客户端身份指纹就**完全不会泄漏到上游**。
    if lower == "user-agent" {
        return true;
    }
    // 显式黑名单:Codex / OpenAI / ChatGPT 客户端身份头
    if lower == "originator"
        || lower == "chatgpt-account-id"
        || lower == "session_id"
        || lower == "thread_id"
    {
        return true;
    }
    // 前缀黑名单:防御未来 Codex CLI 新 identity 头
    if lower.starts_with("x-codex-")
        || lower.starts_with("x-openai-")
        || lower.starts_with("x-chatgpt-")
    {
        return true;
    }
    false
}

/// grok.com Web 后端反代必需 / 我们要独占注入的 header 名集合(见
/// `crates/adapters/src/grok_web/auth.rs::apply_grok_headers`)。
///
/// **仅在 `AuthScheme::GrokCookie` 下应用** —— 入站客户端的同名 header
/// 会被 strip,grok_web::auth 拥有这些 header 的独占注入权,防止
/// `reqwest::header()` append 语义触发 dup-header(grok.com 看到双 Cookie 会
/// session 错乱)。
fn is_grok_owned_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "cookie"
            | "origin"
            | "referer"
            | "accept"
            | "accept-language"
            | "accept-encoding"
            | "sec-fetch-site"
            | "sec-fetch-mode"
            | "sec-fetch-dest"
            | "x-statsig-id"
            | "x-xai-request-id"
            | "traceparent"
            | "sentry-trace"
            | "baggage"
    )
}

pub async fn forward_handler(
    State(state): State<ProxyState>,
    req: Request,
) -> Result<Response, ForwardError> {
    let (parts, body) = req.into_parts();

    // 1. 收齐入站 body
    let mut body_bytes: Bytes = axum::body::to_bytes(body, usize::MAX).await?;
    // [MOC-89 forward-trace] 默认关:仅 CAS_DIAG_TRACE=1 时才克隆一份 Codex 原始请求体
    // (rewrite/strip 前),供全过程 trace。关时不 clone、零额外开销。
    let trace_inbound_raw: Option<Bytes> = forward_trace_enabled().then(|| body_bytes.clone());

    // 2. 解析(鉴权 + 路由)
    let client_path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    if parts.method == Method::OPTIONS && is_local_responses_route(&client_path) {
        return Ok(cors_preflight_response()?);
    }

    // [MOC-104 relay 诊断] chatgpt backend 透传:relay 模式 `chatgpt_base_url` 指向本
    // proxy,`/backend-api/*` 是 Codex 的账号/插件/wham 请求(getAccount→userId、
    // plugins install/list 等)。透传真 chatgpt.com 同 path(不走第三方 resolve/
    // adapter),全 path+status+body 摘要落 telemetry log,把这条 TLS 黑盒链路变可见。
    // 复用 state.http(reqwest 默认读系统代理设置 /`scutil --proxy`,跟随系统、非写死端口;
    // chatgpt.com 必须经代理才可达,故绝不能 no_proxy)。
    if is_chatgpt_backend_path(&client_path) {
        return passthrough_chatgpt_backend(
            &state,
            &parts.method,
            &parts.headers,
            &client_path,
            body_bytes,
        )
        .await;
    }

    let original_model = body_model(&body_bytes);
    let resolved = state.resolver.resolve(&parts, &body_bytes)?;

    // 3. 如有 model 重写,改写 body 的 "model" 字段
    if let Some(new_model) = resolved.rewritten_model.as_deref() {
        if let Some(rewritten) = rewrite_model_field(&body_bytes, new_model) {
            body_bytes = rewritten;
        }
    }

    strip_model_suffix_in_place(&mut body_bytes);
    let resolved_model = body_model(&body_bytes);

    // 4. 走 adapter 拿到上游路径 + 改写后的 body。Codex 的本地
    // `/responses` 入口必须先在本地按旧版语义处理,再转为上游协议。
    let adapter = state
        .adapters
        .lookup_for_request(&resolved.provider.api_format, &client_path);
    // 保留一份原始 body_bytes(model 已 rewrite + strip 过),供 web_search
    // transparent retry 路径重新调用 prepare_request 用 —— retry 时 cache 已
    // disable web_search,prepare_request 会输出不带 web_search 工具的 body。
    let original_body_bytes_for_retry = body_bytes.clone();
    let mut plan = adapter.prepare_request(&client_path, body_bytes, &resolved.provider)?;

    // 5. 拼上游 URL —— base 末尾去 `/`,plan.upstream_path 必含 `/`
    let upstream_url = build_upstream_url(&resolved.upstream_base, &plan.upstream_path);
    check_ssrf_safe(&upstream_url).await?;
    let telemetry = proxy_telemetry();
    telemetry
        .logs
        .add("INFO", format!("request: {} {client_path}", parts.method));
    if let Some(original_model) = original_model.as_deref() {
        let mapped = resolved_model.as_deref().unwrap_or(original_model);
        telemetry
            .logs
            .add("INFO", format!("model alias: {original_model} → {mapped}"));
    }
    telemetry
        .logs
        .add("INFO", format!("forwarding → {upstream_url}"));
    let upstream_model = body_model(&plan.body);
    if let Some(upstream_model) = &upstream_model {
        let mapped = resolved_model.as_deref().unwrap_or(upstream_model);
        telemetry
            .logs
            .add("INFO", format!("model: {mapped} → {upstream_model}"));
    }
    // [#304] 本地记录 session → 真实上游模型,供 Usage 页显示真实模型而非 Codex
    // 客户端占位名。只落本地 jsonl,**不进 Codex rollout / 不影响对话**;forward-only。
    // 用 `resolved_model`(rewrite/strip 后、adapter 重定位前的 body model)而非
    // `upstream_model`(adapter 后的 plan.body):gemini_native 等把 model 挪进 URL 的
    // adapter,plan.body 已无 model 字段,只有 resolved_model 仍持有真实上游模型。
    record_session_upstream_model(&parts.headers, resolved_model.as_deref());

    // 6/7. 构造 reqwest 请求 + 发送(抽到 `build_and_send_upstream`,
    // transparent retry 复用)。
    let (initial_resp, mut outbound_headers_snapshot) = build_and_send_upstream(
        &state,
        &parts.method,
        &parts.headers,
        &resolved,
        &plan.body,
        &plan.upstream_headers,
        &upstream_url,
    )
    .await?;

    // ── A+B web_search transparent retry ──
    // 上游 web search 拒绝时(MiMo Token Plan 套餐没开 Web Search Plugin):
    //   { "code": "400", "param": "web search tool found in the request body,
    //     but webSearchEnabled is false" }
    // **不能透传 4xx 给 Codex.app** —— 实测它收到 JSON error body 后期待
    // SSE 流而卡 Thinking,不会让用户看到错误,也不会自动重试触发下一 turn。
    // 必须 transparent retry:① disable cache → ② 重新 prepare_request(B 层
    // cache 命中 web_search 被 drop)→ ③ 重发上游 + 用新响应替代 4xx →
    // 客户端只感知到正常 SSE 流。用户视角:无感降级,session 内后续 turn 都
    // 不再发 web_search,直到用户在 UI 重新打开开关 / 应用重启。
    //
    // 用 Option<Response> + Option<(status, headers, body)> 二选一表示状态:
    //   live_resp = Some + captured_4xx = None → resp 活着(成功 / 5xx / retry 后)
    //   live_resp = None + captured_4xx = Some  → 非 web_search 4xx,resp 已消费
    let mut live_resp: Option<reqwest::Response> = Some(initial_resp);
    let mut captured_4xx: Option<(http::StatusCode, reqwest::header::HeaderMap, Bytes)> = None;
    let need_retry_check = live_resp
        .as_ref()
        .map(|r| r.status() == http::StatusCode::BAD_REQUEST)
        .unwrap_or(false);
    if need_retry_check {
        let resp = live_resp.take().expect("live_resp is Some by check above");
        let st = resp.status();
        let hs = resp.headers().clone();
        let body_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                // H2 修复:静默吞错改为 telemetry log。上游 4xx body read 失败时,
                // web_search retry 检测会失效(is_web_search_upstream_reject 拿空 body
                // → false → 不进 retry 路径),用户看不到 root cause。
                telemetry.logs.add(
                    "WARN",
                    format!("upstream {st} body read failed during web_search retry check: {e}",),
                );
                Bytes::new()
            }
        };
        if is_web_search_upstream_reject(&body_bytes) {
            codex_app_transfer_adapters::disable_web_search_for(&resolved.provider.id);
            telemetry.logs.add(
                "WARN",
                format!(
                    "auto-disabled web_search for provider {} (upstream rejected: webSearchEnabled=false), retrying without web_search...",
                    resolved.provider.id
                ),
            );
            // 重新调 prepare_request,B 层 cache 命中 → web_search 被 drop
            plan = adapter.prepare_request(
                &client_path,
                original_body_bytes_for_retry,
                &resolved.provider,
            )?;
            // upstream_url 不变(同一 provider,plan.upstream_path 跟 web_search 无关)
            let pair = build_and_send_upstream(
                &state,
                &parts.method,
                &parts.headers,
                &resolved,
                &plan.body,
                &plan.upstream_headers,
                &upstream_url,
            )
            .await?;
            telemetry.logs.add(
                "INFO",
                format!(
                    "web_search retry status {} for provider {}",
                    pair.0.status().as_u16(),
                    resolved.provider.id
                ),
            );
            live_resp = Some(pair.0);
            outbound_headers_snapshot = pair.1;
        } else {
            // 非 web_search 4xx,resp 已被 bytes() 消费,把三元组保存
            captured_4xx = Some((st, hs, body_bytes));
        }
    }

    // 4xx / 5xx 诊断:整段缓冲 upstream body,把请求体 + 响应体片段写日志,
    // 然后用同一份字节再造一个 stream 走 adapter / 客户端。错误 body 一般
    // 很小(JSON error),全缓冲不影响延迟;成功路径仍走零拷贝 stream。
    //
    // 成功路径再叠 TracedStream:记录 send → 首字节 → 流末尾的耗时
    // + 总字节数,流被 Drop(adapter / 客户端断流)时出一行"上游耗时"日志,
    // 辅助定位真实 Codex CLI 流量里"几分钟"是单次 reasoning 慢、还是连续
    // 多轮工具循环放大。
    let t_send = Instant::now();
    // [MOC-89 forward-trace] gate 开时构造 trace 请求侧 owned 快照(成功路径 ctx / 错误
    // 路径就地 push 共用)。仅在 forward_trace_enabled() 为真的分支里调用 → 关时不构造、
    // headers/body 不 clone。response_headers 传上游 raw(transform/filter 前)。
    let make_trace_ctx =
        |status: u16, response_headers: reqwest::header::HeaderMap| ForwardTraceCtx {
            method: parts.method.as_str().to_string(),
            client_path: client_path.clone(),
            client_query: parts.uri.query().map(|s| s.to_string()),
            inbound_headers: parts.headers.clone(),
            inbound_body: trace_inbound_raw
                .as_ref()
                .map(|b| b.to_vec())
                .unwrap_or_default(),
            upstream_url: upstream_url.clone(),
            outbound_headers: outbound_headers_snapshot.clone(),
            outbound_body: plan.body.to_vec(),
            status,
            response_headers,
            provider_id: resolved.provider.id.clone(),
            provider_name: resolved.provider.name.clone(),
            api_format: resolved.provider.api_format.clone(),
            auth_scheme: format!("{:?}", resolved.auth_scheme),
            original_model: original_model.clone(),
            resolved_model: resolved_model.clone(),
            upstream_model: body_model(&plan.body),
        };
    let (status, upstream_headers, upstream_stream): (
        http::StatusCode,
        HeaderMap,
        codex_app_transfer_adapters::ByteStream,
    ) = if let Some((st, hs, body)) = captured_4xx {
        // 非 web_search 4xx,resp 已消费,用 captured 三元组
        log_upstream_error_diag(
            &telemetry,
            st,
            &upstream_url,
            &outbound_headers_snapshot,
            &plan.body,
            &body,
        );
        let upstream_model_for_diag = body_model(&plan.body);
        record_upstream_error_bundle(
            &parts.method,
            &client_path,
            &resolved,
            original_model.as_deref(),
            resolved_model.as_deref(),
            upstream_model_for_diag.as_deref(),
            st,
            &upstream_url,
            &outbound_headers_snapshot,
            &plan.body,
            &body,
        );
        // [MOC-89 forward-trace] 错误路径 body 已完整 buffer,gate 开时就地 push 一行
        if forward_trace_enabled() {
            write_trace_from_ctx(&make_trace_ctx(st.as_u16(), hs.clone()), &body, body.len());
        }
        let single = futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) });
        (
            st,
            filter_hop_headers(&hs),
            Box::pin(single) as codex_app_transfer_adapters::ByteStream,
        )
    } else {
        // resp 仍活着(成功路径 / retry 后 / 5xx 路径)
        let resp = live_resp.expect("live_resp is Some when captured_4xx is None");
        let st = resp.status();
        let hs = filter_hop_headers(resp.headers());
        let stream: codex_app_transfer_adapters::ByteStream = if st.is_success() {
            // [MOC-89 forward-trace] gate 开时先 clone 上游 raw headers 再把 resp 消费成流;
            // 响应体由 TracedStream tee(不破流式),Drop 时连同 ctx 写一行 jsonl。
            let trace_ctx = forward_trace_enabled()
                .then(|| make_trace_ctx(st.as_u16(), resp.headers().clone()));
            let raw = Box::pin(
                resp.bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
            );
            Box::pin(TracedStream::new(
                raw,
                t_send,
                st.as_u16(),
                upstream_url.clone(),
                trace_ctx,
            ))
        } else {
            // retry 后再次 4xx 或 5xx
            // [MOC-89 forward-trace] gate 开时先 clone 上游 raw headers 再消费 resp body
            let trace_resp_headers = forward_trace_enabled().then(|| resp.headers().clone());
            let body_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    telemetry.logs.add(
                        "WARN",
                        format!("upstream {st} body read failed after retry: {e}"),
                    );
                    Bytes::new()
                }
            };
            log_upstream_error_diag(
                &telemetry,
                st,
                &upstream_url,
                &outbound_headers_snapshot,
                &plan.body,
                &body_bytes,
            );
            let upstream_model_for_diag = body_model(&plan.body);
            record_upstream_error_bundle(
                &parts.method,
                &client_path,
                &resolved,
                original_model.as_deref(),
                resolved_model.as_deref(),
                upstream_model_for_diag.as_deref(),
                st,
                &upstream_url,
                &outbound_headers_snapshot,
                &plan.body,
                &body_bytes,
            );
            // [MOC-89 forward-trace] body 已完整 buffer,gate 开时(headers 已 clone)就地 push
            if let Some(rh) = trace_resp_headers {
                write_trace_from_ctx(
                    &make_trace_ctx(st.as_u16(), rh),
                    &body_bytes,
                    body_bytes.len(),
                );
            }
            let single =
                futures_util::stream::once(async move { Ok::<_, std::io::Error>(body_bytes) });
            Box::pin(single)
        };
        (st, hs, stream)
    };

    let response_plan = adapter.transform_response_stream(
        status,
        upstream_headers,
        upstream_stream,
        &resolved.provider,
        &plan,
    )?;
    let success = response_plan.status.is_success();
    telemetry.stats.record(success);
    telemetry.logs.add(
        if success { "SUCCESS" } else { "ERROR" },
        format!("upstream status {}", response_plan.status.as_u16()),
    );

    // 8. 把 ResponsePlan 还原成 axum Response
    let mut builder = Response::builder().status(response_plan.status);
    let headers_out = builder
        .headers_mut()
        .ok_or_else(|| ForwardError::Header("response builder lacks headers".into()))?;
    *headers_out = response_plan.headers;
    Ok(builder.body(Body::from_stream(response_plan.stream))?)
}

/// [MOC-104 relay] relay 模式 `chatgpt_base_url=<proxy>/backend-api` 后,Codex 的
/// 账号/插件/wham 请求都以 `/backend-api/` 开头(默认 chatgpt_base_url =
/// `https://chatgpt.com/backend-api`)。这些请求不该走第三方 provider 路由,需透传
/// 真 chatgpt.com。
fn is_chatgpt_backend_path(path: &str) -> bool {
    let p = path.split('?').next().unwrap_or(path);
    p == "/backend-api" || p.starts_with("/backend-api/")
}

/// [MOC-104 relay 诊断] 把 chatgpt backend 请求透传真 chatgpt.com 同 path,逐条 log
/// path/status/body 摘要。复用 `state.http`(走系统代理 → chatgpt.com 可达);响应整体
/// buffer 以便 log body(getAccount/plugins 都是小 JSON、非 SSE,buffer 无碍)。
/// header name/value 用字符串 + from_bytes 复制,避开 reqwest 与 axum 的 http 类型
/// 是否同源的耦合。
async fn passthrough_chatgpt_backend(
    state: &ProxyState,
    method: &Method,
    headers: &HeaderMap,
    client_path: &str,
    body: Bytes,
) -> Result<Response, ForwardError> {
    let upstream = format!("https://chatgpt.com{client_path}");
    let telemetry = proxy_telemetry();
    telemetry.logs.add(
        "INFO",
        format!("[chatgpt-relay] {method} {client_path} → {upstream}"),
    );
    // [MOC-125] gate 开时先 clone Codex 原始请求体(下面会 move 进 rb),供 passthrough 诊断 trace。
    let trace_inbound = forward_trace_enabled().then(|| body.clone());

    // [review M-2] method 解析失败**报错、不降级** —— 把 POST(plugins install 等写操作)
    // 悄悄降级成 GET 是破坏性降级(违反"禁止破坏性降级"硬规则);axum Method 已合法,
    // 失败仅扩展 method 边界,报 BadRequest 让上层显式处理而非吞掉请求意图。
    let rmethod = reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|e| {
        ForwardError::BadRequest(format!("无法转换 chatgpt backend method {method}: {e}"))
    })?;
    let mut rb = state.http.request(rmethod, &upstream);
    for (k, v) in headers.iter() {
        let name = k.as_str();
        // host 让 reqwest 按 upstream 重填;accept-encoding 去掉避免压缩 body 干扰 log
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        rb = rb.header(name, v.as_bytes());
    }
    if !body.is_empty() {
        rb = rb.body(body);
    }

    let resp = rb.send().await?;
    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();
    // [review H-1] body 读失败**冒泡、不吞** —— `unwrap_or_default()` 会把上游连接 reset /
    // TLS 截断 / 读超时伪装成"成功读到空 200",抹掉根因 + 让诊断日志说假话(本模块存在的
    // 意义就是把 TLS 黑盒变可见)。透传场景上游断连本就该回 502 让 Codex 重试。
    let resp_body = resp.bytes().await.map_err(ForwardError::Upstream)?;

    // [review N-3] 不再 log 响应 body preview —— getAccount/plugin 响应含 account id/email,
    // 落 telemetry 是敏感信息泄漏。只记 status + bytes 足够诊断;Authorization 本就不记。
    telemetry.logs.add(
        if (200..300).contains(&status) {
            "INFO"
        } else {
            "WARN"
        },
        format!(
            "[chatgpt-relay] resp {status} {client_path} ({} bytes)",
            resp_body.len()
        ),
    );

    // [MOC-124 H-2] 回灌 chatgpt backend 鉴权结果给 src-tauri 账号状态机。401 = 服务端 token
    // 失效(token_invalidated / refresh 撤销;本地 JWT exp 可能没到 → `detect()` 仍判账号有效、
    // 前端永显正常);2xx = token 仍有效(瞬时 401 后又成功 → 自愈)。只这两类是 token 有效性
    // 信号 —— 403(权限 / plugins gate)、其他 4xx/5xx 不是,跳过。传被撤销 token 的指纹(Codex
    // 透传的 Authorization Bearer token 的 FNV-1a,跟 src-tauri 算 auth.json access_token 同
    // token 同 FNV → 一致),src-tauri 据此区分真撤销(持续 401、保持 relogin)vs 瞬时(后续 2xx、
    // 自愈)。ERROR 日志只首次 401 记(去重 —— token 失效后 Codex 密集重试,避免刷屏)。
    let auth_signal = if is_chatgpt_token_invalidated(status) {
        Some(true)
    } else if (200..300).contains(&status) {
        Some(false)
    } else {
        None
    };
    if let Some(unauthorized) = auth_signal {
        if let Some(notify) = &state.on_chatgpt_unauthorized {
            notify(authorization_token_fingerprint(headers), unauthorized);
        }
        if unauthorized && !RELOGIN_NOTIFIED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            telemetry.logs.add(
                "ERROR",
                format!(
                    "[chatgpt-relay] 上游 401 → chatgpt 账号 token 服务端失效,已回灌 relogin_required(后续 401 静默): {client_path}"
                ),
            );
        }
    }

    // [MOC-125] chatgpt-backend passthrough 诊断:gate 开时记一条(inbound Codex 请求 / outbound
    // 转发 chatgpt.com / response 回包,header 用 cookie 友好脱敏)。定位远程控制 enroll/server
    // 404 死循环的会话连续性(set-cookie Domain 是否不匹配 relay host → Codex 不回带 cookie)。
    if let Some(ref tbody) = trace_inbound {
        let input = ForwardTraceInput {
            method: method.as_str(),
            client_path,
            client_query: None,
            inbound_headers: headers,
            inbound_body: tbody.as_ref(),
            upstream_url: &upstream,
            // [review comment #1] passthrough 不转换协议,trace 的 outbound 段直接复用 inbound
            // headers/body 作镜像 —— 真实发 chatgpt.com 的请求会再 strip host/accept-encoding、
            // reqwest 重填 host/content-length(trace 未反映这层)。诊断重点在 inbound cookie +
            // response set-cookie 的会话连续性,outbound 仅作对照。
            outbound_headers: headers,
            outbound_body: tbody.as_ref(),
            status,
            response_headers: &resp_headers,
            response_body: resp_body.as_ref(),
            response_full_len: resp_body.len(),
            provider_id: "chatgpt-backend",
            provider_name: "ChatGPT Backend (passthrough)",
            api_format: "chatgpt_passthrough",
            auth_scheme: "-",
            original_model: None,
            resolved_model: None,
            upstream_model: None,
        };
        // [review HIGH] 写盘失败补一次去重 WARN —— 对齐 forward-trace(write_trace_from_ctx),
        // 否则开了诊断却因权限/满盘写不出时,这条 passthrough trace 一行不落且零提示。
        if write_chatgpt_backend_trace(&input).is_none()
            && !TRACE_WRITE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            telemetry.logs.add(
                "WARN",
                "forward-trace 已开启(CAS_DIAG_TRACE)但写盘失败(后续不再提示);\
                 检查 ~/.codex-app-transfer/forward-trace/ 目录权限与磁盘空间"
                    .to_string(),
            );
        }
    }

    let mut builder = Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        for (k, v) in resp_headers.iter() {
            let name = k.as_str();
            // content-length/transfer-encoding/content-encoding 由 axum 重算
            if name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
                || name.eq_ignore_ascii_case("content-encoding")
            {
                continue;
            }
            // [review M-1] header 解析失败记日志、不静默丢 —— chatgpt backend 响应可能带
            // 语义 header(chatgpt-account-id / set-cookie),无声丢弃是"幽灵丢 header"、最难
            // 排查。降级(跳过该 header)可接受,但必须可见。
            match (
                HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_bytes(v.as_bytes()),
            ) {
                (Ok(hn), Ok(hv)) => {
                    h.append(hn, hv);
                }
                _ => telemetry.logs.add(
                    "DEBUG",
                    format!("[chatgpt-relay] 跳过无法解析的响应 header: {name}"),
                ),
            }
        }
    }
    Ok(builder.body(Body::from(resp_body))?)
}

/// 在上游 SSE / chunked 流上叠加耗时埋点。流被 Drop(adapter 链路 / 客户端
/// 中断)时,自动写一行 telemetry 日志,记录 send → 首字节(TTFB)/ 总耗时
/// / 总字节数。**对延迟与吞吐零侵入**,只多了 Instant 比较与计数器累加。
/// forward-trace(MOC-89)成功路径上限:tee 的响应体最多缓冲这么多字节(与 diagnostics
/// 的 body cap 一致;`redact_body` 还会再 cap 一次)。仅 gate 开时分配。
const MAX_TRACE_BODY_BYTES: usize = 256 * 1024;

/// forward-trace 成功路径在 [`TracedStream`] 里随流携带的 owned 上下文。流走完(Drop)时
/// 借这些字段 + tee 到的响应体构造 [`ForwardTraceInput`] 写一行 jsonl。仅 gate 开时为
/// `Some`(关时整个 ctx 不构造、headers/body 不 clone)。
struct ForwardTraceCtx {
    method: String,
    client_path: String,
    client_query: Option<String>,
    inbound_headers: reqwest::header::HeaderMap,
    inbound_body: Vec<u8>,
    upstream_url: String,
    outbound_headers: reqwest::header::HeaderMap,
    outbound_body: Vec<u8>,
    status: u16,
    response_headers: reqwest::header::HeaderMap,
    provider_id: String,
    provider_name: String,
    api_format: String,
    auth_scheme: String,
    original_model: Option<String>,
    resolved_model: Option<String>,
    upstream_model: Option<String>,
}

struct TracedStream {
    inner: codex_app_transfer_adapters::ByteStream,
    started_at: Instant,
    first_byte_at: Option<Instant>,
    total_bytes: usize,
    status: u16,
    upstream_url: String,
    /// forward-trace 上下文,仅 gate 开时 `Some`;`Drop` 时取出写 jsonl。
    trace: Option<ForwardTraceCtx>,
    /// tee 到的上游响应体(原样,不破流式),cap 到 [`MAX_TRACE_BODY_BYTES`]。
    resp_buf: Vec<u8>,
}

impl TracedStream {
    fn new(
        inner: codex_app_transfer_adapters::ByteStream,
        started_at: Instant,
        status: u16,
        upstream_url: String,
        trace: Option<ForwardTraceCtx>,
    ) -> Self {
        Self {
            inner,
            started_at,
            first_byte_at: None,
            total_bytes: 0,
            status,
            upstream_url,
            trace,
            resp_buf: Vec::new(),
        }
    }
}

impl Stream for TracedStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if this.first_byte_at.is_none() {
                    this.first_byte_at = Some(Instant::now());
                }
                this.total_bytes += chunk.len();
                // [MOC-89 forward-trace] gate 开时 tee 一份响应体(cap),chunk 原样返回、
                // 无 await、无重排 → 不破流式。关时 trace 为 None,这段跳过。
                if this.trace.is_some() && this.resp_buf.len() < MAX_TRACE_BODY_BYTES {
                    let room = MAX_TRACE_BODY_BYTES - this.resp_buf.len();
                    let take = room.min(chunk.len());
                    this.resp_buf.extend_from_slice(&chunk[..take]);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
    }
}

impl Drop for TracedStream {
    fn drop(&mut self) {
        let total = self.started_at.elapsed();
        let ttfb = self
            .first_byte_at
            .map(|t| t.duration_since(self.started_at));
        let ttfb_str = ttfb
            .map(|d| format!("{:.2}s", d.as_secs_f64()))
            .unwrap_or_else(|| "(none)".to_owned());
        proxy_telemetry().logs.add(
            "INFO",
            format!(
                "upstream timing {} {} TTFB={} total={:.2}s bytes={}",
                self.status,
                self.upstream_url,
                ttfb_str,
                total.as_secs_f64(),
                self.total_bytes,
            ),
        );
        // [MOC-89 forward-trace] 流走完(成功路径)→ 借 owned ctx + tee 到的响应体写一行
        // jsonl。仅 gate 开时 trace 为 Some。同步 append(一行),与上面 telemetry 日志
        // 同属 Drop 内轻量 IO,不阻塞客户端(流已交付完毕)。
        if let Some(ctx) = self.trace.take() {
            // [MOC-169] Drop 时**重查 gate**:请求开始时 gate 开才有 ctx,但若流中途用户关了
            // 诊断,这条 in-flight trace 不再落盘(与 MCP recorder「关即停」一致 —— 关诊断后采集
            // 立即停,不留 in-flight 残尾)。env CAS_DIAG_TRACE 开的恒真、照写。
            if forward_trace_enabled() {
                // resp_buf 可能被 cap 截断;total_bytes 是 tee 累计真实全长 → 传它修正 truncated_bytes
                write_trace_from_ctx(&ctx, &self.resp_buf, self.total_bytes);
            }
        }
    }
}

/// forward-trace 写盘失败只在**首次**记一条 WARN(去重防每请求刷屏)。用户显式开了
/// CAS_DIAG_TRACE 却因权限/满盘一行没写时,至少有一句提示而非完全静默。
static TRACE_WRITE_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// [MOC-124 H-2] chatgpt backend 首次 401 回灌后置真,后续 401 不再重复刷 ERROR 日志
/// (token 失效后 Codex 密集重试,避免日志爆炸)。回调本身仍每次调(幂等)。进程级、不 reset
/// —— ERROR 是「token 服务端失效」的一次性信号,记一次足够;状态正确性由幂等回调保证。
/// 代价:同进程内二次失效(用户重登后新 token 又被撤销)不再记日志 —— 可接受,回调照常 fire、
/// 状态正确,日志仅诊断用;**不要**为补这条日志而 reset 此 flag,否则 retry storm 又刷屏。
static RELOGIN_NOTIFIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [MOC-124 H-2] 上游 status 是否表示「chatgpt 账号 token 被服务端撤销、应回灌 relogin」。
/// **只 401**(明确鉴权失败);403 是权限语义(plugins gate / 地区限制等)非 token 失效,回灌
/// 会误报。提独立 fn + 单测边界文档化「只 401 不含 403」;若将来要纳入其他鉴权失败码,改这里
/// 并同步更新单测。
fn is_chatgpt_token_invalidated(status: u16) -> bool {
    status == 401
}

/// [MOC-124 H-2] 算请求 Authorization Bearer token 的 FNV-1a 64 指纹,作「被撤销 token」的
/// 标识回传 src-tauri。src-tauri 对 auth.json 的 `access_token` 算**同一指纹**比对,判 active
/// token 是否已换(换了 → detect 清 relogin;没换 → 保持)。两侧都对 raw token(无 `Bearer `
/// 前缀)算、用同一 FNV-1a(offset basis `0xcbf29ce484222325` + prime `0x100000001b3`)。
/// 无 Authorization → 0(src-tauri 把 0 当「无撤销记录」)。
fn authorization_token_fingerprint(headers: &HeaderMap) -> u64 {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s))
        .unwrap_or("");
    if token.is_empty() {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 把 owned [`ForwardTraceCtx`] + 响应体借成 [`ForwardTraceInput`] 写一行 jsonl。成功
/// 路径(`TracedStream::Drop`,body 来自 tee、可能被 cap 截断 → 传 `response_full_len`
/// 为真实全长)与错误/retry 路径(handler 内,body 已完整 buffer → full_len = body.len())
/// 共用,避免重复构造。
fn write_trace_from_ctx(ctx: &ForwardTraceCtx, response_body: &[u8], response_full_len: usize) {
    let input = ForwardTraceInput {
        method: &ctx.method,
        client_path: &ctx.client_path,
        client_query: ctx.client_query.as_deref(),
        inbound_headers: &ctx.inbound_headers,
        inbound_body: &ctx.inbound_body,
        upstream_url: &ctx.upstream_url,
        outbound_headers: &ctx.outbound_headers,
        outbound_body: &ctx.outbound_body,
        status: ctx.status,
        response_headers: &ctx.response_headers,
        response_body,
        response_full_len,
        provider_id: &ctx.provider_id,
        provider_name: &ctx.provider_name,
        api_format: &ctx.api_format,
        auth_scheme: &ctx.auth_scheme,
        original_model: ctx.original_model.as_deref(),
        resolved_model: ctx.resolved_model.as_deref(),
        upstream_model: ctx.upstream_model.as_deref(),
    };
    if write_forward_trace_jsonl(&input).is_none()
        && !TRACE_WRITE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        proxy_telemetry().logs.add(
            "WARN",
            "forward-trace 已开启(CAS_DIAG_TRACE)但写盘失败(后续不再提示);\
             检查 ~/.codex-app-transfer/forward-trace/ 目录权限与磁盘空间"
                .to_string(),
        );
    }
}

/// 4xx / 5xx 时把出站 headers + 请求体片段 + 上游响应体片段写到 telemetry 日志,
/// 辅助诊断身份头泄漏 / 反爬识别 / token 配置等问题。
/// 截断到 ~2KB(req)+ 4KB(resp)避免污染日志;headers 全打但脱敏 Authorization /
/// api-key 等敏感字段。
/// 拼上游完整 URL(base 末尾去 `/`,upstream_path 必含 `/` 时直接拼,否则补)。
fn build_upstream_url(upstream_base: &str, upstream_path: &str) -> String {
    let path = if upstream_path.starts_with('/') {
        upstream_path.to_string()
    } else {
        format!("/{}", upstream_path)
    };
    let base = upstream_base.trim_end_matches('/');
    // 容错:用户把完整 endpoint(如 `…/v1/chat/completions`、`…/v1/messages`、
    // `…/responses`)整段填进 base_url 时,adapter 仍会按协议拼标准 endpoint,
    // 拼成 `…/chat/completions/chat/completions` 等 → 上游 404
    // (反馈 fb-3093a382:误把 opencode zen 完整地址填进 baseUrl,MOC-72)。
    // 做法:取 path 在 `/` 段边界上、同时也是 base 末尾的最长前缀作为"重叠段"去掉再拼。
    // 既覆盖 create 路径(base 末尾 `/responses` + path `/responses`),也覆盖子路径
    // (base 末尾 `/responses` + path `/responses/{id}/cancel` → `…/responses/{id}/cancel`,
    // 不翻倍);query 原样保留。非破坏性:base 不含 endpoint 时 overlap=0,按原样拼。
    let (path_no_query, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path.as_str(), None),
    };
    let mut overlap = 0usize;
    let mut acc = String::new();
    for seg in path_no_query.split('/').skip(1) {
        acc.push('/');
        acc.push_str(seg);
        if base.ends_with(&acc) {
            overlap = acc.len();
        }
    }
    let rest = &path_no_query[overlap..];
    match query {
        Some(q) => format!("{base}{rest}?{q}"),
        None => format!("{base}{rest}"),
    }
}

/// SSRF 防护:转发前**只拦"云 metadata"端点**,放行 loopback / 私网 LAN。
///
/// **为什么不拦 loopback/私网**:本 app 是桌面多 provider 代理,用户合法会把
/// provider.baseUrl 配成本地 LLM(Ollama `127.0.0.1:11434` / LM Studio)或本地桥
/// (实测真机就有 `http://127.0.0.1:29090`)。一刀切拦 loopback/私网会直接打断这些
/// 核心用例(MOC-68 review:strict 版会破坏用户现有配置 + 失败 9 个集成测试)。
/// 因此只阻断 SSRF 真正的高价值目标 —— 云 metadata(窃取实例凭据),其余放行。
///
/// **阻断对象(见 `is_cloud_metadata_ip`)**:169.254.0.0/16 链路本地(含
/// 169.254.169.254 AWS/GCP/Azure/Oracle metadata)、100.100.100.200(Alibaba)、
/// fd00:ec2::254(AWS IPv6 IMDS)、`metadata.google.internal`,及其 IPv4-mapped 形式。
///
/// **覆盖路径**:① 字面 IP 直接判;② hostname 异步解析后看是否落 metadata(解析失败
/// 则放行 —— 只防 metadata,不可解析的 host 反正连不上);③ 重定向跟随由 client 的
/// 自定义 redirect policy 对每跳复检(见 `ProxyState::new`),拦 `302 → 169.254.169.254`。
/// **残留 TOCTOU**:检查时解析的 IP 与 reqwest 建连时再次解析的 IP 不保证一致
/// (DNS rebinding 窗口),根治需把 IP pin 给 reqwest —— 留 followup,故不宣称完全防住。
async fn check_ssrf_safe(upstream_url: &str) -> Result<(), ForwardError> {
    let uri: http::Uri = upstream_url
        .parse()
        .map_err(|e| ForwardError::BadRequest(format!("invalid upstream URL: {e}")))?;
    let host = uri.host().unwrap_or("");
    match host_static_ssrf_verdict(host)? {
        SsrfHostVerdict::LiteralAllowed => Ok(()),
        SsrfHostVerdict::NeedsDnsResolution => {
            let port = uri.port_u16().unwrap_or(443);
            // 异步解析:proxy runtime 仅 2 worker,热路径不能用同步 to_socket_addrs 阻塞。
            // 只为捕捉"自定义域名 A 记录指向云 metadata"。解析失败 → 放行(只防 metadata)。
            match tokio::net::lookup_host((host, port)).await {
                Ok(addrs) => {
                    for addr in addrs {
                        if is_cloud_metadata_ip(addr.ip()) {
                            proxy_telemetry().logs.add(
                                "WARN",
                                format!("SSRF blocked: {host} → cloud metadata {}", addr.ip()),
                            );
                            return Err(ForwardError::BadRequest(format!(
                                "upstream URL {host} resolves to cloud metadata IP: {}",
                                addr.ip()
                            )));
                        }
                    }
                    Ok(())
                }
                Err(_) => Ok(()),
            }
        }
    }
}

enum SsrfHostVerdict {
    /// 字面 IP 且非 metadata(含 loopback/私网/公网):无需 DNS,直接放行。
    LiteralAllowed,
    /// 普通 hostname:需调用方做(异步或同步)DNS 解析后看是否落 metadata。
    NeedsDnsResolution,
}

/// SSRF 无 DNS 前置判定:空 host / 字面 metadata IP / metadata hostname → 拒;
/// 其余字面 IP(loopback/私网/公网)→ `LiteralAllowed`;hostname → `NeedsDnsResolution`。
/// 被转发前异步检查与 redirect policy 同步检查共用。
fn host_static_ssrf_verdict(host: &str) -> Result<SsrfHostVerdict, ForwardError> {
    if host.is_empty() {
        return Err(ForwardError::BadRequest("upstream URL has no host".into()));
    }
    // http::Uri / Url 对 IPv6 字面量保留 `[...]`,先剥方括号再 parse,
    // 否则 `[::ffff:169.254.169.254]`(IPv4-mapped metadata)parse 失败会绕过字面校验。
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        if is_cloud_metadata_ip(ip) {
            return Err(ForwardError::BadRequest(format!(
                "upstream URL points to cloud metadata IP: {host}"
            )));
        }
        return Ok(SsrfHostVerdict::LiteralAllowed);
    }
    let mut lower = host.to_ascii_lowercase();
    if lower.ends_with('.') {
        // 去尾点:`metadata.google.internal.` 等价无尾点
        lower.pop();
    }
    // metadata hostname 直接拒(它本就解析到 169.254.169.254,但显式拒错误信息更清晰)。
    // 注意:**不**拦 `localhost` —— loopback 已整体放行(本地 LLM/桥合法)。
    if lower == "metadata.google.internal" || lower == "metadata" {
        return Err(ForwardError::BadRequest(format!(
            "upstream URL points to cloud metadata hostname: {host}"
        )));
    }
    Ok(SsrfHostVerdict::NeedsDnsResolution)
}

/// redirect policy 用的同步 host 安全检查(重定向是少见路径,可接受同步阻塞解析)。
/// 返回 `Err(reason)` 表示该跳目标指向云 metadata,应拒绝跟随。解析失败 → 放行。
fn redirect_host_is_safe(host: &str) -> Result<(), String> {
    match host_static_ssrf_verdict(host).map_err(|e| e.to_string())? {
        SsrfHostVerdict::LiteralAllowed => Ok(()),
        SsrfHostVerdict::NeedsDnsResolution => {
            use std::net::ToSocketAddrs;
            match (host, 443u16).to_socket_addrs() {
                Ok(resolved) => {
                    for addr in resolved {
                        if is_cloud_metadata_ip(addr.ip()) {
                            return Err(format!(
                                "redirect host {host} resolves to cloud metadata IP {}",
                                addr.ip()
                            ));
                        }
                    }
                    Ok(())
                }
                Err(_) => Ok(()),
            }
        }
    }
}

/// 是否是"云 metadata"端点(SSRF 真正高价值目标)。**只拦 metadata,不拦 loopback/私网**。
fn is_cloud_metadata_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_metadata_v4(v4),
        std::net::IpAddr::V6(v6) => {
            // IPv4-mapped(::ffff:a.b.c.d):映射回 v4 判断,
            // 否则 `[::ffff:169.254.169.254]` 会绕过。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_metadata_v4(v4);
            }
            // AWS IPv6 IMDS 端点 fd00:ec2::254
            v6 == std::net::Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

fn is_metadata_v4(v4: std::net::Ipv4Addr) -> bool {
    // 169.254.0.0/16 链路本地 —— 云 metadata 都落 169.254.169.254;link-local 不是
    // 合法上游,整段拦掉既覆盖 metadata 又无误伤。
    v4.is_link_local()
        // Alibaba Cloud metadata(落在 CGNAT 100.64/10,故单独点名)
        || v4.octets() == [100, 100, 100, 200]
}

/// 构造 reqwest 上游请求 + 发送,返回 `(Response, 出站 headers 快照)`。
/// **extras / adapter 同名 header 走 override 语义**:reqwest `RequestBuilder::header()`
/// 是 append,不是 replace。如果客户端(例如 Codex CLI 自己加的
/// `User-Agent: codex-cli/...`)和 `provider.extraHeaders`(例如 kimi-code
/// 的 `User-Agent: KimiCLI/1.40.0`)同名,或客户端 header 跟协议 adapter 的
/// 默认头(如 `anthropic-version`)同名,两条值都会上线,部分上游严格按首条值
/// 判定接入身份。这里在复制客户端 header 时先过滤掉将由 extras / adapter
/// 写入的名字,保证最终只有一份明确值出去。provider.extraHeaders 优先级高于
/// adapter defaults。
///
/// 抽成 helper 是为了 web_search transparent retry 路径复用同一份 header /
/// auth 构造逻辑(forward 主路径调一次,4xx web_search 拒绝时再调一次)。
async fn build_and_send_upstream(
    state: &ProxyState,
    method: &http::Method,
    inbound_headers: &HeaderMap,
    resolved: &ResolvedProvider,
    plan_body: &Bytes,
    adapter_headers: &HeaderMap,
    upstream_url: &str,
) -> Result<(reqwest::Response, HeaderMap), ForwardError> {
    // GoogleOauthCloudCode / GoogleOauthAntigravity authScheme:provider.api_key
    // 是空,真实 token 在 ~/.codex-app-transfer/{gemini,antigravity}-oauth.json。
    // 这里 await load + auto refresh 拿当前可用 access_token,后面 inject_auth 用
    // 它注 Bearer。两个 provider 共用 cloudcode-pa 上游但 token 文件 + refresh
    // 用不同 client_id/secret(antigravity 走 ensure_valid_antigravity_token,
    // gemini-cli 走 ensure_valid_access_token)。
    let oauth_bearer: Option<String> = match resolved.auth_scheme {
        crate::resolver::AuthScheme::GoogleOauthCloudCode => {
            let store =
                codex_app_transfer_gemini_oauth::TokenStore::from_home_env().map_err(|e| {
                    ForwardError::OauthUnavailable {
                        reason: format!(
                            "home directory unavailable; cannot locate token store: {e}"
                        ),
                        needs_login: false,
                    }
                })?;
            let token =
                codex_app_transfer_gemini_oauth::ensure_valid_access_token(&state.http, &store)
                    .await
                    .map_err(classify_oauth_service_error)?;
            Some(token)
        }
        crate::resolver::AuthScheme::GoogleOauthAntigravity => {
            let provider = &codex_app_transfer_gemini_oauth::ANTIGRAVITY_PROVIDER;
            let store = codex_app_transfer_gemini_oauth::TokenStore::for_token_filename(
                provider.token_filename,
            )
            .map_err(|e| ForwardError::OauthUnavailable {
                reason: format!(
                    "home directory unavailable; cannot locate antigravity token store: {e}"
                ),
                needs_login: false,
            })?;
            let token = codex_app_transfer_gemini_oauth::ensure_valid_antigravity_token(
                &state.http,
                &store,
            )
            .await
            .map_err(classify_oauth_service_error)?;
            Some(token)
        }
        _ => None,
    };

    let mut up = state
        .http
        .request(method.clone(), upstream_url)
        .body(plan_body.clone());
    let strip_for_grok = matches!(resolved.auth_scheme, AuthScheme::GrokCookie);
    for (name, value) in inbound_headers.iter() {
        if is_hop_header(name.as_str()) || is_strip_on_forward(name.as_str()) {
            continue;
        }
        if resolved.extra_headers.contains_key(name) {
            continue;
        }
        if adapter_headers.contains_key(name) {
            continue;
        }
        // dup-header 防御(review-feedback A4):GrokCookie scheme 下,grok.com
        // 必需的 headers(Cookie / Origin / Referer / Accept-* / Sec-Fetch-* /
        // x-statsig-id / x-xai-request-id / traceparent)由 grok_web::auth 统一
        // 注入;如果客户端入站的同名 header 跟随过来,reqwest `header()` 会
        // append 而不是 replace,grok.com 看到双 Cookie 会 session 错乱。这里
        // strip 客户端同名,让 GrokCookie 分支独占注入权。
        if strip_for_grok && is_grok_owned_header(name.as_str()) {
            continue;
        }
        up = up.header(name, value);
    }
    up = inject_auth(up, resolved, oauth_bearer.as_deref());
    for (name, value) in resolved.extra_headers.iter() {
        up = up.header(name, value);
    }
    for (name, value) in adapter_headers.iter() {
        if resolved.extra_headers.contains_key(name) {
            continue;
        }
        up = up.header(name, value);
    }
    // Cloud Code Assist 上游 OAuth providers **必须**用各自 UA + X-Goog-Api-Client
    // 才命中"官方客户端"分支;漏/错值上游按"非官方 wire"路径,latent silent
    // failure + quota 划归错 bucket。强制 override inbound/extra_headers 同名值
    // —— Google 协议必需。参考 CLIProxyAPI `header_utils.go` (gemini-cli) +
    // `antigravity_version.go` (antigravity)。
    match resolved.auth_scheme {
        crate::resolver::AuthScheme::GoogleOauthCloudCode => {
            up = up.header(
                "User-Agent",
                codex_app_transfer_gemini_oauth::detect_user_agent(),
            );
            up = up.header(
                "X-Goog-Api-Client",
                codex_app_transfer_gemini_oauth::X_GOOG_API_CLIENT,
            );
        }
        crate::resolver::AuthScheme::GoogleOauthAntigravity => {
            // **2026-05-29 本机 mitmproxy 抓包实证(Antigravity IDE 2.0.10)**:
            // 对 cloudcode-pa 的所有请求(chat `streamGenerateContent` + 控制面
            // `loadCodeAssist`/`fetchAvailableModels`)统一用 UA
            // `antigravity/hub/<ver> <plat>/<arch>`,且**都不发** `X-Goog-Api-Client`
            // (只 Authorization + User-Agent + Content-Type + Accept-Encoding)。
            // 这推翻了之前基于 CLIProxyAPI 的"控制面才发 x-goog / 控制面用 nodejs-client
            // 长 UA"假设。chat 多发 x-goog 会被上游识别成"非 canonical client" →
            // stricter rate limit / 429。见 memory `reference_antigravity_wire_fingerprint`。
            up = up.header(
                "User-Agent",
                codex_app_transfer_gemini_oauth::antigravity_user_agent_chat(),
            );
        }
        crate::resolver::AuthScheme::GrokCookie => {
            // grok.com Web 后端鉴权头(cookie + statsig + xai-request-id + traceparent +
            // origin/referer/UA + accept/sec-fetch-*)。所有头由 grok_web::auth 集中维护,见
            // `crates/adapters/src/grok_web/auth.rs::apply_grok_headers`。
            //
            // Cookie 与 statsigId 来源:`provider.extra["grokWeb"]`;若缺失(用户没填),
            // **短路 surface 错误**(见 review-feedback A3):log + 让 build_and_send_upstream
            // 上层把请求带空 Cookie 发出去会让 Codex APP 卡在 "Thinking" 不可见错误,
            // 同时还会用用户 IP/UA 触发 grok.com Cloudflare bot 升级风险。改成 BadRequest
            // 错误,让 forward 主路径 surface 给客户端清晰的 401 + missing-cookie 信息。
            //
            // **dup-header 防御**(review-feedback A4):reqwest 0.12 `RequestBuilder::header`
            // 是 append 不是 replace。如果客户端入站 headers 里有 Cookie / Origin /
            // Referer / Accept-* / Sec-Fetch-*,grok_headers 会跟客户端值一起 append →
            // grok.com 看到双 Cookie 会 session 错乱。先用 `reqwest::RequestBuilder` 拿到
            // 底层 `HeaderMap` 把这些 header 名 remove 干净,再注入 grok_headers。
            let grok_headers =
                match codex_app_transfer_adapters::grok_web::auth::apply_grok_headers_typed(
                    &resolved.provider,
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!(
                            error_id = "GROK_AUTH_HEADERS_MISSING",
                            provider_id = %resolved.provider_id,
                            error = %e,
                            "grok_web 鉴权头注入失败 — provider.extra.grokWeb 配置缺失;短路 surface 401"
                        );
                        return Err(ForwardError::Adapter(
                            codex_app_transfer_adapters::AdapterError::BadRequest(format!(
                                "grok_web auth config missing: {e}"
                            )),
                        ));
                    }
                };
            // dup-header 防御:上方 inbound headers 复制循环已对 GrokCookie scheme
            // 走 `is_grok_owned_header` 过滤(参见 line 695-703 + line 265 helper),
            // 入站客户端的 Cookie / Origin / Referer / Accept-* / Sec-Fetch-* /
            // x-statsig-id / x-xai-request-id / traceparent 不会进 builder,因此
            // 这里 `headers()` 合并就是干净的一份(reqwest `headers()` 调用 extend,
            // 不会跟空的同名 entry 冲突)。
            // **不依赖** extras 写 grok headers — extras 含 Cookie 在 line 705
            // 循环还会再加一次,GrokCookie scheme 配 extras 是 user error。
            up = up.headers(grok_headers);
        }
        _ => {}
    }
    let req = up.build()?;
    let outbound_headers_snapshot = req.headers().clone();
    let resp = state.http.execute(req).await?;
    Ok((resp, outbound_headers_snapshot))
}

/// 检测上游 4xx 响应 body 是否是"web search plugin / Web Search 能力未开"
/// 这一类错误。命中时 `forward.rs` 主路径会调用
/// `adapters::disable_web_search_for(provider_id)` 把当前 provider 加入本进程
/// 内存 disable cache,避免后续 turn 重复触发同样错误。
///
/// **匹配关键字**(实测覆盖):
/// - MiMo Token Plan / 其他套餐没开 Web Search Plugin:`"webSearchEnabled is false"`
///   / `"web search tool found"`(实测 2026-05-09 dump)
/// - 通用兜底:`"web_search"` + `"not enabled" / "not supported" / "not activated"`
///   未来其他 provider 可能用类似措辞,留个宽松兜底
///
/// 误判风险:**故意宽松**(关键字 OR 命中即触发 disable),最坏情况是用户
/// 没开 web_search_enabled 也"被 disable"(本来就是 disabled,无副作用)。
fn is_web_search_upstream_reject(body_bytes: &[u8]) -> bool {
    let body = match std::str::from_utf8(body_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let lower = body.to_ascii_lowercase();
    // 实测精确字面量(MiMo)
    if lower.contains("websearchenabled is false")
        || lower.contains("web search tool found in the request body")
    {
        return true;
    }
    // 通用兜底
    let mentions_web_search = lower.contains("web_search") || lower.contains("web search");
    let mentions_not_available = lower.contains("not enabled")
        || lower.contains("not supported")
        || lower.contains("not activated")
        || lower.contains("disabled");
    mentions_web_search && mentions_not_available
}

/// [#304] 本地记录 `session_id → 真实上游模型` 到 `~/.codex-app-transfer/session-models.jsonl`。
/// Codex 入站带 `x-session-id` / `session_id` 头(= rollout session uuid),配上 adapter
/// 解析后的真实上游模型,Usage 页据此显示真实模型而非客户端占位名(gpt-5.x)。
///
/// **只本地落 jsonl,不写 Codex rollout、不改回包,故不进对话 / 不影响对话。**
/// best-effort:缺 session 头 / 模型 / 写失败均静默跳过,绝不阻塞转发。forward-only(历史
/// 对话无记录,Usage 页对其仍显示 rollout 里的客户端模型名)。每请求 append 一行,读侧取
/// 每 session 最后一条;文件增长可后续 compact(followup)。
fn record_session_upstream_model(headers: &HeaderMap, upstream_model: Option<&str>) {
    use std::io::Write;
    let Some(model) = upstream_model.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let session_id = headers
        .get("x-session-id")
        .or_else(|| headers.get("session_id"))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(session_id) = session_id else {
        return;
    };
    let Some(dir) = codex_app_transfer_registry::config_dir() else {
        return;
    };
    let path = dir.join("session-models.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let line = serde_json::json!({ "id": session_id, "model": model }).to_string();
        let _ = writeln!(f, "{line}");
    }
}

fn log_upstream_error_diag(
    telemetry: &crate::telemetry::ProxyTelemetry,
    status: StatusCode,
    upstream_url: &str,
    outbound_headers: &reqwest::header::HeaderMap,
    request_body: &Bytes,
    response_body: &Bytes,
) {
    const REQ_MAX: usize = 2048;
    const RESP_MAX: usize = 4096;
    let req_snippet = bytes_preview(request_body, REQ_MAX);
    let resp_snippet = bytes_preview(response_body, RESP_MAX);
    let headers_dump = format_headers_redacted(outbound_headers);
    telemetry.logs.add(
        "ERROR",
        format!(
            "upstream error diag {} {}\n  → outbound headers: [{}]\n  → request body ({} bytes): {}\n  ← response body ({} bytes): {}",
            status.as_u16(),
            upstream_url,
            headers_dump,
            request_body.len(),
            req_snippet,
            response_body.len(),
            resp_snippet,
        ),
    );
}

fn record_upstream_error_bundle(
    method: &http::Method,
    client_path: &str,
    resolved: &ResolvedProvider,
    original_model: Option<&str>,
    resolved_model: Option<&str>,
    upstream_model: Option<&str>,
    status: StatusCode,
    upstream_url: &str,
    outbound_headers: &reqwest::header::HeaderMap,
    request_body: &Bytes,
    response_body: &Bytes,
) {
    let input = UpstreamErrorBundleInput {
        method: method.as_str().to_owned(),
        client_path: client_path.to_owned(),
        upstream_url: upstream_url.to_owned(),
        status_code: status.as_u16(),
        provider_id: resolved.provider.id.clone(),
        provider_name: resolved.provider.name.clone(),
        original_model: original_model.map(str::to_owned),
        resolved_model: resolved_model.map(str::to_owned),
        upstream_model: upstream_model.map(str::to_owned),
        outbound_headers_redacted: format_headers_redacted(outbound_headers),
        request_body: request_body.to_vec(),
        response_body: response_body.to_vec(),
    };
    let _ = write_upstream_error_bundle(&input);
}

/// 把 HeaderMap 渲染成一行 `name=value, name=value, ...` 用于错误诊断日志。
/// 敏感字段的值替换成 `<redacted len=N>`,只暴露长度,不泄露内容到日志。
///
/// 敏感 header 识别规则(精确名 / 前缀 / 子串三层):
/// - **精确名**:authorization / proxy-authorization / api-key / x-api-key /
///   openai-api-key / anthropic-api-key / cookie / set-cookie
/// - **前缀**:`cookie-` / `x-auth-` / `x-csrf-` / `x-session-`(防御自定义)
/// - **子串**:`secret` / `token` / `credential` / `password`
///
/// chatgpt-codex review (PR #57) 指出 cookie 头能携带 session credential,
/// 错误日志全量泄漏会被攻击者捡到 — 合规要求所有可能含 auth-bearing 数据
/// 的 header 都进 redact 列表,不只是 OAuth bearer / api key 类。
fn format_headers_redacted(headers: &reqwest::header::HeaderMap) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        // 精确名黑名单
        let is_exact_sensitive = matches!(
            lower.as_str(),
            "authorization"
                | "proxy-authorization"
                | "api-key"
                | "x-api-key"
                | "openai-api-key"
                | "anthropic-api-key"
                | "cookie"
                | "set-cookie"
        );
        // 前缀黑名单(防御自定义敏感头)
        let is_prefix_sensitive = lower.starts_with("cookie-")
            || lower.starts_with("x-auth-")
            || lower.starts_with("x-csrf-")
            || lower.starts_with("x-session-");
        // 子串黑名单(关键字命中)
        let is_keyword_sensitive = lower.contains("secret")
            || lower.contains("token")
            || lower.contains("credential")
            || lower.contains("password");

        if is_exact_sensitive || is_prefix_sensitive || is_keyword_sensitive {
            let len = value.as_bytes().len();
            parts.push(format!("{}=<redacted len={}>", name, len));
        } else {
            let display = value.to_str().unwrap_or("<binary>");
            parts.push(format!("{}={}", name, display));
        }
    }
    parts.join(", ")
}

fn bytes_preview(body: &Bytes, max: usize) -> String {
    if body.is_empty() {
        return "(empty)".to_owned();
    }
    let s = String::from_utf8_lossy(body);
    if s.len() <= max {
        s.into_owned()
    } else {
        format!("{}…(+{} bytes truncated)", &s[..max], s.len() - max)
    }
}

fn cors_preflight_response() -> Result<Response, axum::http::Error> {
    Response::builder()
        .status(StatusCode::OK)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "POST, OPTIONS")
        .header("access-control-allow-headers", "*")
        .body(Body::empty())
}

fn body_model(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

/// 把 [`codex_app_transfer_gemini_oauth::ServiceError`] 分类成 [`ForwardError::
/// OauthUnavailable`] 的 `needs_login` flag。逻辑提出独立 fn 方便单测覆盖每条
/// case 的 routing(2026-05-11 review 反馈)。
///
/// 分类规则:
/// - `NotLoggedIn` → `needs_login=true`(token 文件不存在)
/// - `Token(_)` → `needs_login=true`(token 文件 IO/JSON 错,大概率 corrupt)
/// - `Refresh(TokenStatus { body, .. })` → 解析 body 为 JSON,看 RFC 6749
///   `error` 字段是不是 `"invalid_grant"`(refresh_token 被 revoke / 已用过)→
///   `needs_login=true`。其他 `error` 值(如 `invalid_client` / `unauthorized_
///   client`)是 client 配置错,**也**需要重登(可能是凭证错)
/// - `Refresh(其他)` → 网络/TLS/JSON 解析等临时错 → `needs_login=false`
fn classify_oauth_service_error(e: codex_app_transfer_gemini_oauth::ServiceError) -> ForwardError {
    use codex_app_transfer_gemini_oauth::{FlowError, ServiceError};
    let needs_login = match &e {
        ServiceError::NotLoggedIn | ServiceError::Token(_) => true,
        ServiceError::Refresh(FlowError::TokenStatus { body, .. }) => {
            // RFC 6749 §5.2 标准错误 code 走 JSON `error` 字段精确匹配,**不**
            // substring `body.contains` 防 "description: ...invalid_grant_request_id"
            // 等假阳性。
            const REVOCATION_CODES: &[&str] =
                &["invalid_grant", "invalid_client", "unauthorized_client"];
            serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .map(|code| REVOCATION_CODES.contains(&code.as_str()))
                .unwrap_or(false)
        }
        ServiceError::Refresh(_) => false, // 网络/TLS/解析等临时错
    };
    ForwardError::OauthUnavailable {
        reason: e.to_string(),
        needs_login,
    }
}

fn inject_auth(
    mut req: reqwest::RequestBuilder,
    resolved: &ResolvedProvider,
    oauth_bearer: Option<&str>,
) -> reqwest::RequestBuilder {
    match resolved.auth_scheme {
        AuthScheme::Bearer => {
            req = req.header("authorization", format!("Bearer {}", resolved.api_key));
        }
        AuthScheme::XApiKey => {
            req = req.header("x-api-key", resolved.api_key.clone());
        }
        AuthScheme::GoogleApiKey => {
            req = req.header("x-goog-api-key", resolved.api_key.clone());
        }
        AuthScheme::GoogleOauthCloudCode | AuthScheme::GoogleOauthAntigravity => {
            // 调用方在 build_and_send_upstream 入口处已 await 过 OAuth token,
            // 这里单纯 Bearer 注入。两个 OAuth scheme 共用 cloudcode-pa 上游 →
            // Bearer header 一样,只是 token 来源(gemini-cli vs antigravity 文件)
            // 不同。**None 是 build_and_send_upstream 的 bug** — 大声 log error_id
            // 让 Sentry/grep 锚定;请求会因缺 Authorization header 上游 401,
            // 用户看到 401 时再交叉看日志(2026-05-11 silent-failure-hunter C1)
            match oauth_bearer {
                Some(token) => {
                    req = req.header("authorization", format!("Bearer {token}"));
                }
                None => {
                    tracing::error!(
                        error_id = "OAUTH_BEARER_MISSING_BUG",
                        scheme = ?resolved.auth_scheme,
                        provider_id = %resolved.provider_id,
                        "OAuth scheme 但 oauth_bearer=None — build_and_send_upstream 应 await 过 token,\
                         上游会因缺 Authorization 返 401。检查 forward.rs 入口 ensure_valid_*_token 调用链"
                    );
                }
            }
        }
        AuthScheme::GrokCookie => {
            // grok.com Web 后端鉴权:cookie + statsig + xai-request-id 一组 headers,
            // 在 build_and_send_upstream 的 `match resolved.auth_scheme` 分支统一注入
            // (走 `codex_app_transfer_adapters::grok_web::auth::apply_grok_headers`),
            // 这里**不写** Authorization Bearer(grok.com 没这 header)。
        }
        AuthScheme::None => {}
    }
    req
}

/// 把 JSON body 中 `model` 字段替换为 `new_model`,失败返回 None(让原 body 走).
fn rewrite_model_field(body: &Bytes, new_model: &str) -> Option<Bytes> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert(
        "model".to_owned(),
        serde_json::Value::String(new_model.to_owned()),
    );
    Some(Bytes::from(serde_json::to_vec(&v).ok()?))
}

fn strip_model_suffix_in_place(body: &mut Bytes) {
    let Some(mut v) = serde_json::from_slice::<serde_json::Value>(body).ok() else {
        return;
    };
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    let Some(model) = obj.get("model").and_then(|v| v.as_str()) else {
        return;
    };
    let stripped = strip_internal_model_suffix(model);
    if stripped == model {
        return;
    }
    obj.insert("model".to_owned(), serde_json::Value::String(stripped));
    if let Ok(next) = serde_json::to_vec(&v) {
        *body = Bytes::from(next);
    }
}

fn filter_hop_headers(src: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (k, v) in src.iter() {
        if is_hop_header(k.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out.append(name, val);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocks_cloud_metadata_ips() {
        use std::net::IpAddr;
        for s in [
            "169.254.169.254",        // AWS/GCP/Azure/Oracle metadata
            "169.254.0.1",            // link-local(整段拦)
            "100.100.100.200",        // Alibaba Cloud metadata
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "fd00:ec2::254",          // AWS IPv6 IMDS
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_cloud_metadata_ip(ip), "{s} 应判为云 metadata");
        }
    }

    #[test]
    fn ssrf_allows_loopback_private_and_public() {
        use std::net::IpAddr;
        // 桌面代理:loopback(本地 LLM/桥)+ 私网 LAN + 公网都应放行
        for s in [
            "127.0.0.1",            // loopback
            "::1",                  // IPv6 loopback
            "::ffff:127.0.0.1",     // IPv4-mapped loopback
            "10.1.2.3",             // RFC1918
            "192.168.1.1",          // RFC1918
            "172.16.0.1",           // RFC1918
            "fc00::1",              // ULA
            "100.64.0.1",           // CGNAT(非 Alibaba metadata)
            "8.8.8.8",              // 公网
            "1.1.1.1",              // 公网
            "2606:4700:4700::1111", // 公网 v6
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_cloud_metadata_ip(ip), "{s} 应放行");
        }
    }

    #[test]
    fn ssrf_static_verdict_blocks_metadata_allows_local() {
        // metadata IP / hostname → 拒
        for url_host in [
            "169.254.169.254",
            "[::ffff:169.254.169.254]",
            "100.100.100.200",
            "metadata.google.internal",
        ] {
            assert!(
                host_static_ssrf_verdict(url_host).is_err(),
                "{url_host} 应被静态判定拒绝"
            );
        }
        // 字面 loopback/私网/公网 → 放行(无需 DNS)
        for url_host in ["127.0.0.1", "[::1]", "10.0.0.1", "8.8.8.8"] {
            assert!(
                matches!(
                    host_static_ssrf_verdict(url_host),
                    Ok(SsrfHostVerdict::LiteralAllowed)
                ),
                "{url_host} 字面应放行"
            );
        }
        // hostname(含 localhost)→ 交 DNS(localhost 解析到 loopback,非 metadata → 放行)
        for url_host in ["api.openai.com", "localhost"] {
            assert!(
                matches!(
                    host_static_ssrf_verdict(url_host),
                    Ok(SsrfHostVerdict::NeedsDnsResolution)
                ),
                "{url_host} 应走 DNS"
            );
        }
    }

    #[tokio::test]
    async fn ssrf_check_blocks_metadata_allows_loopback() {
        // 放行:loopback 上游(本地 LLM / 本地桥,如真机 127.0.0.1:29090)
        assert!(check_ssrf_safe("http://127.0.0.1:6379/").await.is_ok());
        assert!(check_ssrf_safe("http://[::1]:8080/").await.is_ok());
        assert!(check_ssrf_safe("http://127.0.0.1:29090/v1").await.is_ok());
        // 拦:云 metadata
        assert!(check_ssrf_safe("http://169.254.169.254/latest/meta-data/")
            .await
            .is_err());
        assert!(check_ssrf_safe("http://[::ffff:169.254.169.254]/")
            .await
            .is_err());
        // 非法 URL 仍拒
        assert!(check_ssrf_safe("not a url").await.is_err());
    }

    #[tokio::test]
    async fn previous_response_not_found_renders_openai_sdk_compatible_400() {
        // 关键回归(2026-05-08):cache miss + empty input 路径返回的错误体
        // 必须**字面对齐 OpenAI Responses API 服务端真实行为**(LM Studio bug
        // tracker #1188、Microsoft semantic-kernel #13128 等多源验证):
        // HTTP 400 + content-type application/json + body 严格匹配下面四个字段。
        // 客户端 SDK / Codex CLI fail-fast 路径依赖此格式;改字段名 = 破坏 SDK。
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        let err = ForwardError::Adapter(AdapterError::PreviousResponseNotFound {
            previous_response_id: "resp_abc123".to_owned(),
        });
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            ctype.starts_with("application/json"),
            "content-type 必须是 JSON,实际 {ctype}"
        );
        let body_bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("body 必须是合法 JSON");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "previous_response_not_found");
        assert_eq!(body["error"]["param"], "previous_response_id");
        // message 必须包含 ID,客户端 SDK 据此提取 ID 决定是否重发完整 history
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("resp_abc123"),
            "error.message 必须包含失效的 response_id,实际 {message}"
        );
        assert!(
            message.starts_with("Previous response with id"),
            "措辞对齐 OpenAI 服务端,实际 {message}"
        );
    }

    #[tokio::test]
    async fn oauth_unavailable_renders_401_with_login_required_code_when_needs_login() {
        // **Critical** silent-failure C3 修(2026-05-11):OAuth 失败必须返
        // 401 + structured code "oauth_login_required" 让用户看到可操作提示,
        // 不能走 generic 502 + plain text "proxy error: invalid header: ..."
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        let err = ForwardError::OauthUnavailable {
            reason: "token file missing".into(),
            needs_login: true,
        };
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            ctype.starts_with("application/json"),
            "content-type 必须 JSON,实际 {ctype}"
        );
        let body_bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["code"], "oauth_login_required");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("re-run login"),
            "message 必须含 re-login hint,实际 {message}"
        );
    }

    #[tokio::test]
    async fn oauth_unavailable_renders_401_with_refresh_failed_code_when_transient() {
        // 临时网络错误 → needs_login=false → code "oauth_token_refresh_failed",
        // 文案不让用户重登(避免误导用户重做 OAuth 当成永久错误)
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        let err = ForwardError::OauthUnavailable {
            reason: "TLS handshake failed".into(),
            needs_login: false,
        };
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body_bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "oauth_token_refresh_failed");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("retry"),
            "临时错误 message 应提示 retry,实际 {message}"
        );
        assert!(
            !message.contains("re-run login"),
            "临时错误 message 不该让用户重登,实际 {message}"
        );
    }

    #[test]
    fn classify_not_logged_in_needs_login() {
        use codex_app_transfer_gemini_oauth::ServiceError;
        let result = classify_oauth_service_error(ServiceError::NotLoggedIn);
        match result {
            ForwardError::OauthUnavailable { needs_login, .. } => {
                assert!(needs_login, "NotLoggedIn 必须 needs_login=true");
            }
            other => panic!("期待 OauthUnavailable,得到 {other:?}"),
        }
    }

    #[test]
    fn classify_refresh_invalid_grant_needs_login() {
        use codex_app_transfer_gemini_oauth::{FlowError, ServiceError};
        // Google /token revocation response 标准 shape:`{"error":"invalid_grant",...}`
        let body =
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#;
        let result = classify_oauth_service_error(ServiceError::Refresh(FlowError::TokenStatus {
            status: 400,
            body: body.to_owned(),
        }));
        match result {
            ForwardError::OauthUnavailable { needs_login, .. } => {
                assert!(
                    needs_login,
                    "invalid_grant 必须 needs_login=true 让用户重登"
                );
            }
            other => panic!("期待 OauthUnavailable,得到 {other:?}"),
        }
    }

    #[test]
    fn classify_refresh_substring_false_match_does_not_trigger_needs_login() {
        use codex_app_transfer_gemini_oauth::{FlowError, ServiceError};
        // 防御 substring 假阳性:body 含 "invalid_grant_request_id" 但 `error` 字段
        // 是 "server_error" — 不该误归 needs_login
        let body = r#"{"error":"server_error","error_description":"correlated invalid_grant_request_id=xyz"}"#;
        let result = classify_oauth_service_error(ServiceError::Refresh(FlowError::TokenStatus {
            status: 500,
            body: body.to_owned(),
        }));
        match result {
            ForwardError::OauthUnavailable { needs_login, .. } => {
                assert!(
                    !needs_login,
                    "JSON `error` 不是 invalid_grant 时不该 needs_login(防 substring 假阳性)"
                );
            }
            other => panic!("期待 OauthUnavailable,得到 {other:?}"),
        }
    }

    #[test]
    fn classify_refresh_network_error_does_not_need_login() {
        use codex_app_transfer_gemini_oauth::{FlowError, ServiceError};
        let result = classify_oauth_service_error(ServiceError::Refresh(FlowError::TokenParse(
            "TLS handshake failed".into(),
        )));
        match result {
            ForwardError::OauthUnavailable { needs_login, .. } => {
                assert!(!needs_login, "网络/TLS 错不该让用户重登(临时错可重试)");
            }
            other => panic!("期待 OauthUnavailable,得到 {other:?}"),
        }
    }

    #[test]
    fn inject_auth_google_oauth_cloud_code_with_token_sets_bearer() {
        // **Critical** test gap(2026-05-11):inject_auth GoogleOauthCloudCode
        // 分支 0 直接测试。这里 build mock RequestBuilder + 注入 OAuth Bearer
        let resolved = ResolvedProvider {
            provider_id: "test".into(),
            upstream_base: "https://cloudcode-pa.googleapis.com".into(),
            api_key: String::new(), // OAuth 路径 api_key 为空
            extra_headers: HeaderMap::new(),
            rewritten_model: None,
            provider: std::sync::Arc::new(codex_app_transfer_registry::Provider {
                id: "test".into(),
                name: "test".into(),
                base_url: "https://cloudcode-pa.googleapis.com".into(),
                auth_scheme: "google_oauth_cloud_code".into(),
                api_format: "gemini_cli_oauth".into(),
                api_key: String::new(),
                models: indexmap::IndexMap::new(),
                extra_headers: indexmap::IndexMap::new(),
                model_capabilities: indexmap::IndexMap::new(),
                request_options: indexmap::IndexMap::new(),
                is_builtin: true,
                sort_index: 0,
                extra: indexmap::IndexMap::new(),
            }),
            auth_scheme: AuthScheme::GoogleOauthCloudCode,
        };
        let client = reqwest::Client::new();
        let req = client.post("https://cloudcode-pa.googleapis.com/v1internal:test");
        let req = inject_auth(req, &resolved, Some("ya29.test-bearer"));
        let built = req.build().unwrap();
        let auth = built
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(auth, "Bearer ya29.test-bearer");
        // 不该误用 GoogleApiKey 的 x-goog-api-key
        assert!(built.headers().get("x-goog-api-key").is_none());
    }

    #[test]
    fn inject_auth_google_oauth_cloud_code_with_none_skips_silently() {
        // 文档 lock — None bearer 时 inject_auth 不写 Authorization。这是 silent
        // skip(memory note),实际生产 build_and_send_upstream 会先 await
        // ensure_valid_access_token 在前面失败,不该走到这里。本测试只防 future
        // 重构把 if let Some 改成 unwrap 导致 panic。
        let resolved = ResolvedProvider {
            provider_id: "test".into(),
            upstream_base: "https://cloudcode-pa.googleapis.com".into(),
            api_key: String::new(),
            extra_headers: HeaderMap::new(),
            rewritten_model: None,
            provider: std::sync::Arc::new(codex_app_transfer_registry::Provider {
                id: "test".into(),
                name: "test".into(),
                base_url: "https://cloudcode-pa.googleapis.com".into(),
                auth_scheme: "google_oauth_cloud_code".into(),
                api_format: "gemini_cli_oauth".into(),
                api_key: String::new(),
                models: indexmap::IndexMap::new(),
                extra_headers: indexmap::IndexMap::new(),
                model_capabilities: indexmap::IndexMap::new(),
                request_options: indexmap::IndexMap::new(),
                is_builtin: true,
                sort_index: 0,
                extra: indexmap::IndexMap::new(),
            }),
            auth_scheme: AuthScheme::GoogleOauthCloudCode,
        };
        let client = reqwest::Client::new();
        let req = client.post("https://cloudcode-pa.googleapis.com/v1internal:test");
        let req = inject_auth(req, &resolved, None);
        let built = req.build().unwrap();
        assert!(
            built.headers().get("authorization").is_none(),
            "None bearer 不该注入 Authorization(等 build_and_send_upstream 先返 OauthUnavailable)"
        );
    }

    #[test]
    fn hop_headers_recognized() {
        for h in [
            "Connection",
            "keep-alive",
            "TE",
            "Transfer-Encoding",
            "Host",
            "content-length",
        ] {
            assert!(is_hop_header(h), "{h} 应识别为 hop");
        }
        assert!(!is_hop_header("authorization"));
        assert!(!is_hop_header("content-type"));
    }

    #[test]
    fn authorization_stripped_on_forward() {
        assert!(is_strip_on_forward("Authorization"));
        assert!(is_strip_on_forward("authorization"));
        assert!(!is_strip_on_forward("x-api-key"));
    }

    #[test]
    fn user_agent_stripped_on_forward() {
        // 关键回归(2026-05-08 Kimi Windows 403):客户端 codex_cli_rs/... UA
        // 必须被剔除,后续 reqwest default UA 或 extras 的 UA 兜底
        assert!(is_strip_on_forward("User-Agent"));
        assert!(is_strip_on_forward("user-agent"));
        assert!(is_strip_on_forward("USER-AGENT"));
    }

    #[test]
    fn default_outbound_user_agent_is_neutral() {
        // 兜底 default UA 必须是中性的 Codex-App-Transfer/<v>,绝不能含
        // codex_cli / openai / chatgpt 等可能触发反爬的关键字
        let ua = DEFAULT_OUTBOUND_USER_AGENT;
        assert!(ua.starts_with("Codex-App-Transfer/"), "ua: {ua}");
        let lower = ua.to_ascii_lowercase();
        assert!(!lower.contains("codex_cli"), "ua 不应含 codex_cli: {ua}");
        assert!(!lower.contains("reqwest"), "ua 不应含 reqwest: {ua}");
        assert!(!lower.contains("openai"), "ua 不应含 openai: {ua}");
        assert!(!lower.contains("chatgpt"), "ua 不应含 chatgpt: {ua}");
    }

    #[test]
    fn codex_identity_headers_stripped_on_forward() {
        // 精确名黑名单
        assert!(is_strip_on_forward("originator"));
        assert!(is_strip_on_forward("Originator"));
        assert!(is_strip_on_forward("chatgpt-account-id"));
        assert!(is_strip_on_forward("session_id"));
        assert!(is_strip_on_forward("thread_id"));
        // 前缀黑名单
        assert!(is_strip_on_forward("x-codex-installation-id"));
        assert!(is_strip_on_forward("x-codex-window-id"));
        assert!(is_strip_on_forward("X-Codex-Foo-Bar"));
        assert!(is_strip_on_forward("x-openai-subagent"));
        assert!(is_strip_on_forward("x-openai-memgen-request"));
        assert!(is_strip_on_forward("x-chatgpt-anything"));
        // 普通 header 仍然透传(注意:user-agent 现在也被 strip,见
        // user_agent_stripped_on_forward 测试)
        assert!(!is_strip_on_forward("content-type"));
        assert!(!is_strip_on_forward("accept"));
    }

    #[test]
    fn redacts_sensitive_headers_in_diag_log() {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        // 精确名敏感
        h.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer secret-token-xyz"),
        );
        h.insert(
            HeaderName::from_static("api-key"),
            HeaderValue::from_static("sk-1234567890"),
        );
        h.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("session=abc123; user=42"),
        );
        h.insert(
            HeaderName::from_static("set-cookie"),
            HeaderValue::from_static("xyz=789"),
        );
        // 前缀敏感
        h.insert(
            HeaderName::from_static("cookie-flavor"),
            HeaderValue::from_static("oatmeal"),
        );
        h.insert(
            HeaderName::from_static("x-auth-token"),
            HeaderValue::from_static("nope"),
        );
        h.insert(
            HeaderName::from_static("x-csrf-token"),
            HeaderValue::from_static("abc"),
        );
        h.insert(
            HeaderName::from_static("x-session-id"),
            HeaderValue::from_static("xyz"),
        );
        // 子串敏感
        h.insert(
            HeaderName::from_static("my-secret-thing"),
            HeaderValue::from_static("hush"),
        );
        h.insert(
            HeaderName::from_static("refresh-token"),
            HeaderValue::from_static("rt"),
        );
        // 普通 header 应保留
        h.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        h.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("KimiCLI/1.40.0"),
        );
        h.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );

        let dump = format_headers_redacted(&h);

        // 敏感值都不应出现在日志里
        for forbidden in [
            "secret-token-xyz",
            "sk-1234567890",
            "session=abc123",
            "xyz=789",
            "oatmeal",
            "nope",
            "abc",
            "hush",
        ] {
            assert!(
                !dump.contains(forbidden),
                "敏感值 {forbidden:?} 不应出现在 dump 里;dump: {dump}"
            );
        }
        // 全部敏感字段都应有 <redacted len=N> 标记
        for sensitive_name in [
            "authorization",
            "api-key",
            "cookie",
            "set-cookie",
            "cookie-flavor",
            "x-auth-token",
            "x-csrf-token",
            "x-session-id",
            "my-secret-thing",
            "refresh-token",
        ] {
            let pattern = format!("{sensitive_name}=<redacted len=");
            assert!(
                dump.contains(&pattern),
                "敏感 header {sensitive_name} 应被 redact;dump: {dump}"
            );
        }
        // 普通 header 必须保留原值
        assert!(dump.contains("content-type=application/json"));
        assert!(dump.contains("user-agent=KimiCLI/1.40.0"));
        assert!(dump.contains("accept=text/event-stream"));
    }

    #[test]
    fn rewrite_model_in_json_body() {
        let body = Bytes::from_static(br#"{"model":"slug/real","stream":true}"#);
        let new = rewrite_model_field(&body, "real").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&new).unwrap();
        assert_eq!(v["model"], "real");
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn rewrite_returns_none_for_non_json() {
        let body = Bytes::from_static(b"not json");
        assert!(rewrite_model_field(&body, "x").is_none());
    }

    #[test]
    fn strips_internal_model_suffix_before_upstream() {
        let mut body = Bytes::from_static(br#"{"model":"deepseek-v4-pro[1m]","stream":true}"#);
        strip_model_suffix_in_place(&mut body);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "deepseek-v4-pro");
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn keeps_non_internal_model_suffixes() {
        let mut body = Bytes::from_static(br#"{"model":"deepseek-v4-pro[beta]","stream":true}"#);
        strip_model_suffix_in_place(&mut body);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "deepseek-v4-pro[beta]");
        assert_eq!(v["stream"], true);
    }

    // ── is_web_search_upstream_reject 关键字识别(B 层 fallback 触发条件)──

    #[test]
    fn web_search_reject_matches_mimo_exact_literal() {
        // MiMo Token Plan 套餐没开 Web Search Plugin 时实测错误体
        // (2026-05-09 dump 抓到的精确字面量)
        let body = br#"{"error":{"code":"400","message":"Param Incorrect","param":"web search tool found in the request body, but webSearchEnabled is false","type":""}}"#;
        assert!(is_web_search_upstream_reject(body));
    }

    #[test]
    fn web_search_reject_matches_camelcase_variant() {
        let body = br#"{"error":"webSearchEnabled is false"}"#;
        assert!(is_web_search_upstream_reject(body));
    }

    #[test]
    fn web_search_reject_matches_generic_not_enabled_phrasing() {
        // 兜底:其他 provider 可能用类似措辞
        let body = br#"{"error":"web_search is not enabled for this account"}"#;
        assert!(is_web_search_upstream_reject(body));
        let body2 = br#"{"error":"web search not activated"}"#;
        assert!(is_web_search_upstream_reject(body2));
    }

    #[test]
    fn web_search_reject_does_not_match_unrelated_400() {
        // 普通 400 错误不该误触发 fallback(只 disable web_search)
        assert!(!is_web_search_upstream_reject(
            b"{\"error\":\"Invalid model name\"}"
        ));
        assert!(!is_web_search_upstream_reject(
            b"{\"error\":\"token limit exceeded\"}"
        ));
        assert!(!is_web_search_upstream_reject(
            b"{\"error\":\"rate limit reached\"}"
        ));
    }

    #[test]
    fn web_search_reject_handles_non_utf8_safely() {
        // 上游返回非 UTF-8 时不 panic,认为不匹配
        let body: &[u8] = &[0xff, 0xfe, 0xfd, 0x00];
        assert!(!is_web_search_upstream_reject(body));
    }

    #[test]
    fn build_upstream_url_dedups_endpoint_suffix_in_base() {
        // 正常路径:base 不含 endpoint → 照常拼(回归保护)
        assert_eq!(
            build_upstream_url("https://api.moonshot.cn/v1", "/chat/completions"),
            "https://api.moonshot.cn/v1/chat/completions"
        );
        // base 误填完整 chat endpoint(反馈 fb-3093a382:opencode zen)→ 去重不翻倍
        assert_eq!(
            build_upstream_url(
                "https://opencode.ai/zen/go/v1/chat/completions",
                "/chat/completions"
            ),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
        // 去重时保留 query
        assert_eq!(
            build_upstream_url(
                "https://opencode.ai/zen/go/v1/chat/completions",
                "/chat/completions?stream=true"
            ),
            "https://opencode.ai/zen/go/v1/chat/completions?stream=true"
        );
        // anthropic:base 误填 /v1/messages → 去重
        assert_eq!(
            build_upstream_url("https://relay.example.com/v1/messages", "/v1/messages"),
            "https://relay.example.com/v1/messages"
        );
        // anthropic 正常:base 不含 endpoint → 照常补 /v1/messages
        assert_eq!(
            build_upstream_url("https://api.anthropic.com", "/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        // responses:base 误填 /responses → 去重
        assert_eq!(
            build_upstream_url("https://relay.example.com/responses", "/responses"),
            "https://relay.example.com/responses"
        );
        // 不误伤:base=/v1 + /responses(OpenAI 官方 responses-direct)照常拼
        assert_eq!(
            build_upstream_url("https://api.openai.com/v1", "/responses"),
            "https://api.openai.com/v1/responses"
        );
        // 不误伤:responses 子路径(cancel)且 base 不含 endpoint → 照常拼
        assert_eq!(
            build_upstream_url("https://api.openai.com/v1", "/responses/resp_abc/cancel"),
            "https://api.openai.com/v1/responses/resp_abc/cancel"
        );
        // codex-connector P2:base 误填 /responses + 子路径(cancel/retrieve)→ 段边界去重,不翻倍
        assert_eq!(
            build_upstream_url(
                "https://relay.example.com/responses",
                "/responses/resp_abc/cancel"
            ),
            "https://relay.example.com/responses/resp_abc/cancel"
        );
        // 段边界:base 末尾 /v1 + path /v1/responses → 只去掉重叠的 /v1,不误删
        assert_eq!(
            build_upstream_url("https://relay.example.com/v1", "/v1/responses"),
            "https://relay.example.com/v1/responses"
        );
        // base 末尾 / 归一后再判断
        assert_eq!(
            build_upstream_url("https://api.openai.com/v1/", "/responses"),
            "https://api.openai.com/v1/responses"
        );
    }

    // [MOC-124 H-2] chatgpt backend 透传遇上游 401 → 回灌 relogin 的边界:只 401 不含 403
    #[test]
    fn token_invalidated_only_on_401_not_403() {
        assert!(is_chatgpt_token_invalidated(401));
        // 403 是权限语义(plugins gate / 地区限制等),非 token 失效 → 不回灌,避免误报
        assert!(!is_chatgpt_token_invalidated(403));
        assert!(!is_chatgpt_token_invalidated(200));
        assert!(!is_chatgpt_token_invalidated(404));
        assert!(!is_chatgpt_token_invalidated(500));
    }

    // [MOC-124 H-2] relogin 回调:默认无、注入后在位且可触发,且收到被撤销 token 的指纹
    #[test]
    fn with_relogin_notify_injects_callback_and_fires() {
        use crate::resolver::StaticResolver;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let resolver = Arc::new(StaticResolver::new(None, vec![], None));
        // 默认未注入回调(测试 / proxy 独立运行)
        assert!(ProxyState::new(resolver.clone())
            .on_chatgpt_unauthorized
            .is_none());
        // 注入后回调在位且可触发(src-tauri 侧注入 report_chatgpt_backend_auth 即走这条);
        // 回调参数 (token_fp, unauthorized),验最后一次收到的指纹 + 标志。
        let last_fp = Arc::new(AtomicU64::new(0));
        let last_unauth = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (last_fp2, last_unauth2) = (last_fp.clone(), last_unauth.clone());
        let state = ProxyState::new(resolver).with_relogin_notify(Arc::new(move |fp, unauth| {
            last_fp2.store(fp, Ordering::SeqCst);
            last_unauth2.store(unauth, Ordering::SeqCst);
        }));
        let cb = state
            .on_chatgpt_unauthorized
            .as_ref()
            .expect("回调应已注入");
        cb(42, true);
        cb(99, false);
        assert_eq!(
            last_fp.load(Ordering::SeqCst),
            99,
            "回调应收到最后传入的指纹"
        );
        assert!(
            !last_unauth.load(Ordering::SeqCst),
            "回调应收到最后传入的 unauthorized 标志"
        );
    }

    // [MOC-124 H-2] token 指纹:strip Bearer、稳定、空头 → 0、不同 token 不同指纹
    #[test]
    fn authorization_token_fingerprint_strips_bearer_and_is_stable() {
        use axum::http::header::{HeaderValue, AUTHORIZATION};
        let fp = |val: &str| {
            let mut h = HeaderMap::new();
            h.insert(AUTHORIZATION, HeaderValue::from_str(val).unwrap());
            authorization_token_fingerprint(&h)
        };
        // 无 Authorization → 0
        assert_eq!(authorization_token_fingerprint(&HeaderMap::new()), 0);
        // "Bearer X" 跟裸 "X" 同指纹(strip Bearer)→ proxy 与 src-tauri 算同一 token 一致
        assert_eq!(fp("Bearer tok_abc"), fp("tok_abc"));
        // 稳定 + 非 0 + 不同 token 不同指纹
        assert_eq!(fp("Bearer tok_abc"), fp("Bearer tok_abc"));
        assert_ne!(fp("Bearer tok_abc"), 0);
        assert_ne!(fp("Bearer tok_abc"), fp("Bearer tok_xyz"));
    }
}
