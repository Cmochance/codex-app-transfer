//! axum router 构造与启动 helper.

use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, Method, Request},
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
    let state = ProxyState::new(resolver);
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
        .fallback(any(forward_handler))
        .with_state(state)
}

async fn responses_websocket_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| responses_websocket_loop(socket, state, headers))
}

async fn responses_websocket_loop(mut socket: WebSocket, state: ProxyState, headers: HeaderMap) {
    while let Some(message) = socket.next().await {
        let Ok(message) = message else {
            break;
        };
        let text = match message {
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
        let mut body = extract_response_create_body(&message_json);
        if body.get("stream").is_none() {
            body["stream"] = serde_json::Value::Bool(true);
        }
        let body_bytes = match serde_json::to_vec(&body) {
            Ok(bytes) => bytes,
            Err(error) => {
                send_ws_error(&mut socket, &format!("Invalid response body: {error}")).await;
                continue;
            }
        };
        let req = websocket_forward_request(&headers, body_bytes);
        let response = match forward_handler(State(state.clone()), req).await {
            Ok(response) => response,
            Err(error) => error.into_response(),
        };
        if !stream_forward_response_to_websocket(response, &mut socket).await {
            break;
        }
    }
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
