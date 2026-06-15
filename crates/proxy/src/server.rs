//! axum router 构造与启动 helper.

use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, Method, Request, Uri},
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use futures_util::StreamExt;
use serde_json::json;

use crate::forward::{forward_handler, ProxyState};
use crate::resolver::SharedResolver;

/// 把所有方法 / 所有路径都路由到 `forward_handler`(裸代理 + B1 路由 + B2 鉴权改写)。
/// Stage 3 起此 router 会再叠 adapter 中间件(provider 协议转换)。
pub fn build_router(resolver: SharedResolver) -> Router {
    build_router_with_state(ProxyState::new(resolver))
}

/// [MOC-124 H-2] 同 [`build_router`],但注入「chatgpt backend 透传遇上游 401 → 回灌账号需重登」
/// 回调。src-tauri 启动 proxy 时走此入口注入
/// `codex_real_account::mark_relogin_required_from_proxy`(回调参数 = 被撤销 token 的指纹);
/// 测试 / proxy 独立运行用无回调的 [`build_router`]。
pub fn build_router_with_relogin(
    resolver: SharedResolver,
    on_chatgpt_unauthorized: std::sync::Arc<dyn Fn(u64) + Send + Sync>,
) -> Router {
    build_router_with_state(ProxyState::new(resolver).with_relogin_notify(on_chatgpt_unauthorized))
}

/// 注册 apply_patch 埋点 sink(进程级一次)。adapter(`codex_app_transfer_adapters`)不能反向
/// 依赖本 crate(循环依赖),故由这里把「补 `seq`/`captured_at`/`proxy_version` 再 push 进
/// [`crate::trace_store`]」的闭包注册给 adapter,gate 复用诊断总开关
/// [`crate::diagnostics::forward_trace_enabled`](env / app 内「诊断模式」,默认关)。
/// `OnceLock`(adapter 内)保证二次调用静默忽略 —— `build_router*` 多次调用 / 测试都安全。
fn register_apply_patch_trace_sink() {
    use codex_app_transfer_adapters::core::apply_patch_trace;
    apply_patch_trace::install(
        crate::diagnostics::forward_trace_enabled,
        Box::new(|mut value| {
            let seq = crate::trace_store::next_seq();
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("seq".to_owned(), seq.into());
                map.insert(
                    "captured_at".to_owned(),
                    chrono::Local::now().to_rfc3339().into(),
                );
                map.insert("proxy_version".to_owned(), env!("CARGO_PKG_VERSION").into());
            }
            crate::trace_store::trace_store().push(
                crate::trace_store::TraceKind::ApplyPatch,
                seq,
                value,
            );
        }),
    );
}

fn build_router_with_state(state: ProxyState) -> Router {
    register_apply_patch_trace_sink();
    Router::new()
        .route(
            "/responses",
            get(responses_websocket_handler)
                .post(forward_handler)
                .options(forward_handler),
        )
        .route(
            "/v1/responses",
            get(responses_websocket_handler)
                .post(forward_handler)
                .options(forward_handler),
        )
        .route(
            "/openai/v1/responses",
            get(responses_websocket_handler)
                .post(forward_handler)
                .options(forward_handler),
        )
        // [MOC-125] Codex 远程控制 WS 端点:真 WS 透传(区别于 /responses 的 ws→http 转换)。
        // relay 模式 chatgpt_base_url 指向本 proxy,这条 GET 是 WebSocket 握手 → 透传到
        // wss://chatgpt.com;显式路由优先于 fallback,其余 /backend-api/* 仍走 passthrough。
        // enroll(POST .../server/enroll)路径不同,走 fallback passthrough。
        .route(
            crate::ws_passthrough::REMOTE_CONTROL_WS_PATH,
            get(remote_control_ws_handler),
        )
        .fallback(any(forward_handler))
        .with_state(state)
}

/// [MOC-125] Codex 远程控制 WS 接收侧:axum 接 upgrade,把 Codex 原始 header + path(含
/// query)交给 [`crate::ws_passthrough::proxy_remote_control`] 透传到 chatgpt.com。
async fn remote_control_ws_handler(
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    ws.on_upgrade(move |socket| crate::ws_passthrough::proxy_remote_control(socket, headers, path))
}

async fn responses_websocket_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    // [followup MOC-239] **native responses provider** + ws→ws 透传关(默认)→ 对 WS upgrade 回
    // **426 Upgrade Required**。Codex(codex-rs `core/src/client.rs::stream_responses_websocket`,
    // 实证 @ codex-rs main 2026-06 / Codex Desktop 0.140;**未版本锁定**,Codex 改 fallback 策略则此
    // 假设失效)对 WS 握手的 426 **立即**返回 `WebsocketStreamOutcome::FallbackToHttp`(无重试,置
    // `disable_websockets` 整 session 黏住),转走 HTTP `/responses` transport —— Codex 原生把
    // `previous_response_id` inline 成自包含请求(prev_id=null + 完整上下文),免 proxy 端上下文重建、
    // 免 5/5 reconnect。这就是「让 Codex 自己降级 HTTP」的正解。
    //
    // **chat 类 provider**(openai_chat 等)仍接受 WS 走 ws→http 逐帧转换([`forward_chat_frame`]:
    // responses-WS 帧转 chat-HTTP,`core::input` 从本地 ResponseSessionCache inline 历史)—— 它们没有
    // passthrough prev_id 问题,保持现状。ws→ws ON 时所有 provider 都升级走 [`responses_websocket_loop`]
    // (native → 全程 relay,供 followup 对支持 WS 的端点验证)。解析不出 provider → 不拦,照常 upgrade。
    if !responses_ws_passthrough_enabled() {
        if let Some(resolved) = resolve_provider_for_ws(&state, &headers, &serde_json::json!({})) {
            if is_native_responses_provider(&resolved) {
                // 留痕:否则「Codex 为何降级 HTTP」在诊断日志里无迹可循(决策点静默)。
                crate::telemetry::proxy_telemetry().logs.add(
                    "INFO",
                    format!(
                        "[responses-ws] native provider {} → WS upgrade 回 426,Codex 降级 HTTP transport(CAS_RESPONSES_WS_PASSTHROUGH 未开)",
                        resolved.provider.id
                    ),
                );
                return (
                    axum::http::StatusCode::UPGRADE_REQUIRED,
                    "native responses websocket passthrough disabled; fall back to HTTP",
                )
                    .into_response();
            }
        }
    }
    ws.on_upgrade(move |socket| responses_websocket_loop(socket, state, headers))
        .into_response()
}

async fn responses_websocket_loop(mut socket: WebSocket, state: ProxyState, headers: HeaderMap) {
    // [MOC-234] 首个 `response.create` 帧到达时解析 provider 决定传输,本连接固定:
    // - **native responses provider**(api_format = responses / openai_responses,含
    //   chatgpt.com)→ 全程 WS relay(Codex-WS ↔ 上游-WS,见
    //   [`crate::ws_passthrough::proxy_responses_upstream_ws`])。保 `previous_response_id`、保
    //   原生流式(不 re-framing),与 direct 直连一致;不再 ws→http 把 WS 降级成 HTTP。
    // - **chat 类 provider**(无 WS / 无 responses API,如 mimo/glm)→ 维持 ws→http 转换
    //   ([`forward_chat_frame`]):responses-WS 帧转 chat-HTTP POST 经 forward_handler。
    let mut chat_decided = false;
    while let Some(message) = socket.next().await {
        let Ok(message) = message else {
            break;
        };
        let text = match &message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text,
                Err(_) => {
                    send_ws_error(&mut socket, "Invalid UTF-8 message").await;
                    continue;
                }
            },
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(message_json) = serde_json::from_str::<serde_json::Value>(&text) else {
            send_ws_error(&mut socket, "Invalid JSON").await;
            continue;
        };
        if message_json.get("type").and_then(|v| v.as_str()) != Some("response.create") {
            continue;
        }

        // 首个 response.create:解析 provider。native responses **且** ws→ws 透传开关开启(默认关,
        // 见 [`responses_ws_passthrough_enabled`])→ 交给全程 WS relay(原始首帧一并交出,relay 接管
        // socket,本 loop 退出)。否则(默认 / 非 responses / 解析失败)走 ws→http 转换
        // ([`forward_chat_frame`]):responses-WS 帧转 HTTP POST `/responses`,SSE 回灌 WS。
        //
        // [followup MOC-239] freemodel 等 **AWS ALB 后端**对 HTTP/1.1 WS Upgrade 一律 426、对 h2
        // extended CONNECT(RFC 8441)400,原生 ws→ws relay 实证不可达 → 默认走 ws→http;ws→ws 待对
        // 支持的端点(chatgpt.com)实现 + 验证后再 flip 默认 / 按 provider 决策。
        if !chat_decided {
            if responses_ws_passthrough_enabled() {
                let body = extract_response_create_body(&message_json);
                if let Some(resolved) = resolve_provider_for_ws(&state, &headers, &body) {
                    if is_native_responses_provider(&resolved) {
                        crate::ws_passthrough::proxy_responses_upstream_ws(
                            socket, resolved, headers, message,
                        )
                        .await;
                        return;
                    }
                }
            }
            chat_decided = true;
        }

        if !forward_chat_frame(&mut socket, &state, &headers, message_json).await {
            break;
        }
    }
}

/// chat 类 provider 的单帧 ws→http 转换(原 loop body)。warmup / 空帧直接 Close 让 Codex
/// 立即 fallback HTTP;否则 body 转 HTTP POST `/responses` 经 [`forward_handler`],SSE 响应
/// 再逐行回灌 WS。返回 `false` = 应收束连接。
async fn forward_chat_frame(
    socket: &mut WebSocket,
    state: &ProxyState,
    headers: &HeaderMap,
    message_json: serde_json::Value,
) -> bool {
    let mut body = extract_response_create_body(&message_json);
    // Codex CLI ws warmup(`generate: false`)与"新 session 首帧 input 为空 + 无
    // previous_response_id"这两类 frame 转到任何 chat-completions 兼容 provider 必然 400
    // (转换后 messages 空)。直接 Close 让 Codex 立即 try_switch_fallback_transport → HTTP
    // (`input: [] + previous_response_id != ""` 是 ws incremental delta=0 续轮,不命中、走转发)。
    if should_skip_upstream_warmup(&body) {
        let _ = socket
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: axum::extract::ws::close_code::UNSUPPORTED,
                reason: "warmup / empty-input frame not supported; fall back to HTTP".into(),
            })))
            .await;
        return false;
    }
    if body.get("stream").is_none() {
        body["stream"] = serde_json::Value::Bool(true);
    }
    let body_bytes = match serde_json::to_vec(&body) {
        Ok(bytes) => bytes,
        Err(error) => {
            send_ws_error(socket, &format!("Invalid response body: {error}")).await;
            return true;
        }
    };
    let req = websocket_forward_request(headers, body_bytes);
    let response = match forward_handler(State(state.clone()), req).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    stream_forward_response_to_websocket(response, socket).await
}

/// 在 WS 握手上下文里解析 provider:用 Codex 的握手 header + 首帧(response.create)body 合成
/// 一个 `POST /responses` 请求喂给 resolver(复用 HTTP 路径同一套 slot/model 路由)。失败 → None。
fn resolve_provider_for_ws(
    state: &ProxyState,
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Option<crate::resolver::ResolvedProvider> {
    let mut builder = Request::builder().method(Method::POST).uri("/responses");
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let (parts, _) = builder.body(()).ok()?.into_parts();
    let body_bytes = serde_json::to_vec(body).unwrap_or_default();
    state.resolver.resolve(&parts, &body_bytes).ok()
}

/// provider 是否原生 Responses 协议(走全程 WS relay)。chat 类(openai_chat 等)走 ws→http 转换。
fn is_native_responses_provider(resolved: &crate::resolver::ResolvedProvider) -> bool {
    matches!(
        resolved.provider.api_format.as_str(),
        "responses" | "openai_responses"
    )
}

/// native responses 是否走 **ws→ws 全程透传**(env `CAS_RESPONSES_WS_PASSTHROUGH=1`/`true`)。
/// **默认关** = 走 ws→http 转换。[followup MOC-239] 上游(freemodel 等 ALB 后端)的 WS 端点实证
/// 不可达(HTTP/1.1 Upgrade → 426、h2 extended CONNECT → 400),原生 relay 暂禁用;待对支持 WS 的
/// 端点实现 + 验证后再 flip 默认。开关保留以便 followup 用 chatgpt.com 等真 WS 端点测透传链路。
fn responses_ws_passthrough_enabled() -> bool {
    matches!(
        std::env::var("CAS_RESPONSES_WS_PASSTHROUGH").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

fn extract_response_create_body(message: &serde_json::Value) -> serde_json::Value {
    if let Some(response) = message.get("response").filter(|v| v.is_object()) {
        return response.clone();
    }
    let mut body = serde_json::Map::new();
    if let Some(obj) = message.as_object() {
        for (key, value) in obj {
            if key != "type" {
                body.insert(key.clone(), value.clone());
            }
        }
    }
    serde_json::Value::Object(body)
}

fn websocket_forward_request(headers: &HeaderMap, body: Vec<u8>) -> axum::extract::Request {
    let mut builder = Request::builder().method(Method::POST).uri("/responses");
    for (name, value) in headers {
        if name == axum::http::header::AUTHORIZATION {
            builder = builder.header(name, value);
        }
    }
    builder
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("websocket forward request")
}

async fn stream_forward_response_to_websocket(
    response: axum::response::Response,
    socket: &mut WebSocket,
) -> bool {
    let status = response.status();
    let body = response.into_body();
    if !status.is_success() {
        let bytes = to_bytes(body, 64 * 1024).await.unwrap_or_default();
        let message = String::from_utf8_lossy(&bytes);
        send_ws_error(
            socket,
            &format!("unexpected status {}: {}", status.as_u16(), message.trim()),
        )
        .await;
        return true;
    }

    let mut stream = body.into_data_stream();
    let mut pending = String::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            // fix(#210 P1-2): 流中断时先发 response.failed 事件让 Codex CLI
            // 明确知道本轮回复已终止(而不是继续等待更多 SSE 数据),再发 error
            // 描述。这样客户端可以正确清理本轮状态并进入重试/新对话路径。
            // schema 跟 grok_web/gemini_native/anthropic_messages adapter 一致:
            // {type, response:{id, object, status, error:{code, message}}}
            let failed_event = serde_json::json!({
                "type": "response.failed",
                "response": {
                    "id": "",
                    "object": "response",
                    "status": "failed",
                    "error": {
                        "code": "stream_interrupted",
                        "message": "upstream stream read failed — response incomplete"
                    }
                }
            });
            let _ = socket
                .send(Message::Text(failed_event.to_string().into()))
                .await;
            send_ws_error(socket, "stream read failed").await;
            return true;
        };
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = pending.find('\n') {
            let mut line = pending[..idx].to_owned();
            pending.drain(..idx + 1);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                if socket
                    .send(Message::Text(data.to_owned().into()))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
        }
    }
    true
}

async fn send_ws_error(socket: &mut WebSocket, message: &str) {
    let payload = json!({
        "type": "error",
        "error": {
            "message": message,
        },
    })
    .to_string();
    let _ = socket.send(Message::Text(payload.into())).await;
}

/// 识别**应当跳过上游转发**的 ws frame —— 这些 frame 转发到任何 chat-completions
/// 兼容 provider 必然 400(messages 空),应当直接 ws 错误响应让 Codex CLI 立即
/// 进 stream-retry / fallback-HTTP 路径,避免一次无意义的上游 RTT。
///
/// 当前匹配两类:
/// 1. **显式 warmup**:`generate: false`(Codex CLI `prewarm_websocket` /
///    `stream_responses_websocket(warmup=true)` 会显式设这个字段,见
///    `codex-rs/core/src/client.rs:1334-1343`)。语义是"预热 ws 连接,不真正
///    生成内容",上游 chat-completions API 不支持这个语义。
///
/// 2. **空 input + 无 previous_response_id**:任何来源(可能是客户端 bug /
///    探活 / 边界场景)都不可能产生合法的 chat 请求(转换后 messages 必空)。
///    保留 `previous_response_id != ""` 的空 input 帧不命中本规则 —— 那是 ws
///    incremental delta=0 续轮,走 ResponseSessionCache 查历史的合法路径。
///
/// 不识别 instructions:即使有 instructions(system message),没有真实 user
/// turn 仍然是一次纯 system 请求,部分 provider 也会 400;但 instructions
/// 路径较少出现在 ws frame 里,先不做特殊处理避免误杀。
fn should_skip_upstream_warmup(body: &serde_json::Value) -> bool {
    let generate_false = body
        .get("generate")
        .and_then(|v| v.as_bool())
        .map(|b| !b)
        .unwrap_or(false);
    if generate_false {
        return true;
    }

    let input_empty = match body.get("input") {
        None => true,
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(arr)) => arr.is_empty(),
        Some(serde_json::Value::String(s)) => s.trim().is_empty(),
        // input 是其它形式(object 等)—— 极少见,但既然不空就别拦
        Some(_) => false,
    };
    if !input_empty {
        return false;
    }

    let prev_id = body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    // 空 input + 无 previous_response_id = 纯空 frame,跳过上游
    prev_id.is_empty()
}

#[cfg(test)]
mod tests {
    use super::should_skip_upstream_warmup;
    use serde_json::json;

    #[test]
    fn skips_explicit_warmup_with_generate_false() {
        // Codex CLI prewarm_websocket: `ws_payload.generate = Some(false)`
        let body = json!({
            "input": [],
            "generate": false,
        });
        assert!(should_skip_upstream_warmup(&body));
    }

    #[test]
    fn skips_empty_input_without_previous_response_id() {
        // 新 session 首帧:input 空 + 没有 previous_response_id
        // (典型场景:客户端误发 / 探活 / 实测真机 13:03:06 case)
        let body = json!({"input": []});
        assert!(should_skip_upstream_warmup(&body));
        let body_no_input = json!({});
        assert!(should_skip_upstream_warmup(&body_no_input));
        let body_null_input = json!({"input": null});
        assert!(should_skip_upstream_warmup(&body_null_input));
        let body_empty_string = json!({"input": "  "});
        assert!(should_skip_upstream_warmup(&body_empty_string));
    }

    #[test]
    fn does_not_skip_incremental_turn_with_previous_response_id() {
        // ws incremental delta=0:input 空 + previous_response_id 命中 cache
        // → 走 ResponseSessionCache 查历史(PR #65 sqlite 持久化覆盖),
        // **不能跳**,要让 forward_handler 处理。
        let body = json!({
            "input": [],
            "previous_response_id": "resp_abc123",
        });
        assert!(!should_skip_upstream_warmup(&body));
    }

    #[test]
    fn does_not_skip_normal_turn_with_user_message() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "hi"}
            ]
        });
        assert!(!should_skip_upstream_warmup(&body));
    }

    #[test]
    fn does_not_skip_string_input() {
        let body = json!({"input": "non-empty user prompt"});
        assert!(!should_skip_upstream_warmup(&body));
    }

    #[test]
    fn generate_true_does_not_skip_even_with_empty_input() {
        // 边界:client 明确 generate=true 但 input 空 + 无 prev id
        // 仍然按"空 input + 无 prev id"跳过(generate=true 不抢救它)
        let body = json!({
            "input": [],
            "generate": true,
        });
        assert!(should_skip_upstream_warmup(&body));
    }
}
