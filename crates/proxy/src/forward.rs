//! 透传 forward handler。
//!
//! 行为(Stage 3.1,包含 B1 路由 + B2 鉴权改写 + adapter 协议层):
//! 1. 接收 `Request<Body>`,把 body 完整读出
//! 2. 调 `ProviderResolver` 校验 gateway key,选定上游 provider
//! 3. 按 `provider.api_format` 查 adapter,跑 `prepare_request` 得到上游路径 + 改写后的 body
//! 4. 复制非 hop / 非 Authorization 头到出站
//! 5. 按 `provider.auth_scheme` 注入上游凭据(Bearer 或 X-Api-Key)
//! 6. 注入 `provider.extra_headers`(如 kimi-code 的 User-Agent)
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

use crate::resolver::{AuthScheme, ResolveError, ResolvedProvider, SharedResolver};
use crate::telemetry::proxy_telemetry;

#[derive(Clone)]
pub struct ProxyState {
    pub http: reqwest::Client,
    pub resolver: SharedResolver,
    pub adapters: AdapterRegistry,
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
                // 显式设 default UA:client header 复制循环已 strip 客户端
                // user-agent;若 provider.extra_headers 也没配 UA,reqwest
                // 默认会用 `reqwest/<ver>` 作为 default UA,部分 provider
                // 反爬可能 ban "reqwest" 字串。改用中性的 Codex-App-Transfer/<v>
                // 兜底,既不命中 codex 反爬规则,也不在 reqwest 黑名单。
                .user_agent(DEFAULT_OUTBOUND_USER_AGENT)
                .build()
                .expect("reqwest client"),
            resolver,
            adapters: AdapterRegistry::with_builtins(),
        }
    }

    pub fn from_arc(http: reqwest::Client, resolver: SharedResolver) -> Self {
        Self {
            http,
            resolver,
            adapters: AdapterRegistry::with_builtins(),
        }
    }

    pub fn with_adapters(mut self, adapters: AdapterRegistry) -> Self {
        self.adapters = adapters;
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
}

impl axum::response::IntoResponse for ForwardError {
    fn into_response(self) -> Response {
        let message = self.to_string();
        let telemetry = proxy_telemetry();
        telemetry.stats.record(false);
        telemetry
            .logs
            .add("ERROR", format!("proxy request failed: {message}"));

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

pub async fn forward_handler(
    State(state): State<ProxyState>,
    req: Request,
) -> Result<Response, ForwardError> {
    let (parts, body) = req.into_parts();

    // 1. 收齐入站 body
    let mut body_bytes: Bytes = axum::body::to_bytes(body, usize::MAX).await?;

    // 2. 解析(鉴权 + 路由)
    let client_path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    if parts.method == Method::OPTIONS && is_local_responses_route(&client_path) {
        return Ok(cors_preflight_response()?);
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
    if let Some(upstream_model) = body_model(&plan.body) {
        let mapped = resolved_model.as_deref().unwrap_or(&upstream_model);
        telemetry
            .logs
            .add("INFO", format!("model: {mapped} → {upstream_model}"));
    }

    // 6/7. 构造 reqwest 请求 + 发送(抽到 `build_and_send_upstream`,
    // transparent retry 复用)。
    let (initial_resp, mut outbound_headers_snapshot) = build_and_send_upstream(
        &state,
        &parts.method,
        &parts.headers,
        &resolved,
        &plan.body,
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
        let body_bytes = resp.bytes().await.unwrap_or_default();
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
            let raw = Box::pin(
                resp.bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
            );
            Box::pin(TracedStream::new(
                raw,
                t_send,
                st.as_u16(),
                upstream_url.clone(),
            ))
        } else {
            // retry 后再次 4xx 或 5xx
            let body_bytes = resp.bytes().await.unwrap_or_default();
            log_upstream_error_diag(
                &telemetry,
                st,
                &upstream_url,
                &outbound_headers_snapshot,
                &plan.body,
                &body_bytes,
            );
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

/// 在上游 SSE / chunked 流上叠加耗时埋点。流被 Drop(adapter 链路 / 客户端
/// 中断)时,自动写一行 telemetry 日志,记录 send → 首字节(TTFB)/ 总耗时
/// / 总字节数。**对延迟与吞吐零侵入**,只多了 Instant 比较与计数器累加。
struct TracedStream {
    inner: codex_app_transfer_adapters::ByteStream,
    started_at: Instant,
    first_byte_at: Option<Instant>,
    total_bytes: usize,
    status: u16,
    upstream_url: String,
}

impl TracedStream {
    fn new(
        inner: codex_app_transfer_adapters::ByteStream,
        started_at: Instant,
        status: u16,
        upstream_url: String,
    ) -> Self {
        Self {
            inner,
            started_at,
            first_byte_at: None,
            total_bytes: 0,
            status,
            upstream_url,
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
    format!("{}{}", upstream_base.trim_end_matches('/'), path)
}

/// 构造 reqwest 上游请求 + 发送,返回 `(Response, 出站 headers 快照)`。
/// **extras 同名 header 走 override 语义**:reqwest `RequestBuilder::header()`
/// 是 append,不是 replace。如果客户端(例如 Codex CLI 自己加的
/// `User-Agent: codex-cli/...`)和 `provider.extraHeaders`(例如 kimi-code
/// 的 `User-Agent: KimiCLI/1.40.0`)同名,两条值都会上线,部分上游严格按
/// "首条 UA"判定接入身份就会绕过我们的伪装。这里在复制客户端 header 时,
/// 先把 extras 已经覆盖的名字过滤掉,保证最终只有 extras 的值出去。
///
/// 抽成 helper 是为了 web_search transparent retry 路径复用同一份 header /
/// auth 构造逻辑(forward 主路径调一次,4xx web_search 拒绝时再调一次)。
async fn build_and_send_upstream(
    state: &ProxyState,
    method: &http::Method,
    inbound_headers: &HeaderMap,
    resolved: &ResolvedProvider,
    plan_body: &Bytes,
    upstream_url: &str,
) -> Result<(reqwest::Response, HeaderMap), ForwardError> {
    let mut up = state
        .http
        .request(method.clone(), upstream_url)
        .body(plan_body.clone());
    for (name, value) in inbound_headers.iter() {
        if is_hop_header(name.as_str()) || is_strip_on_forward(name.as_str()) {
            continue;
        }
        if resolved.extra_headers.contains_key(name) {
            continue;
        }
        up = up.header(name, value);
    }
    up = inject_auth(up, resolved);
    for (name, value) in resolved.extra_headers.iter() {
        up = up.header(name, value);
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

fn inject_auth(
    mut req: reqwest::RequestBuilder,
    resolved: &ResolvedProvider,
) -> reqwest::RequestBuilder {
    match resolved.auth_scheme {
        AuthScheme::Bearer => {
            req = req.header("authorization", format!("Bearer {}", resolved.api_key));
        }
        AuthScheme::XApiKey => {
            req = req.header("x-api-key", resolved.api_key.clone());
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
}
