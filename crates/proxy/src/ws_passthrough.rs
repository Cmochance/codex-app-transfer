//! [MOC-125] Codex 远程控制 WebSocket 透传。
//!
//! Codex 桌面端「远程控制」(Mobile→Mac)经 `GET /backend-api/wham/remote/control/server`
//! 发起 **WebSocket** 握手(`Connection: Upgrade` + `Upgrade: websocket`)。relay 模式下
//! `chatgpt_base_url` 指向本 proxy,这条请求落到 proxy —— 但
//! [`crate::forward::passthrough_chatgpt_backend`] 是纯 HTTP 转发(reqwest GET),**不做 WS
//! upgrade** → chatgpt.com 对非-WS 的 GET 返 404 → 远程控制通道建不起来 → Codex enroll
//! 死循环重试(MOC-125 抓包实证)。
//!
//! 本模块做**真 WS 透传**:
//! - **接收侧**:axum [`WebSocketUpgrade`](axum::extract::ws::WebSocketUpgrade) 接 Codex 连接。
//! - **上游侧**:独立的 reqwest 0.13 + reqwest-websocket(**http1-only**)连 `wss://chatgpt.com`,
//!   注入 Codex 原始 `x-codex-*` + `authorization` header(远程控制 required headers)。
//! - **双向 frame pump**:Codex(axum WS)↔ 上游(reqwest-websocket WS),Text/Binary/Ping/Pong/Close
//!   原样转发,任一端关闭即收束。
//!
//! ## 为什么独立 http1-only client(不复用 state.http)
//! reqwest 默认 ALPN 协商 HTTP/2,而 WS upgrade(RFC 6455)走 HTTP/1.1 `Connection: Upgrade`;
//! h2 会让 reqwest-websocket 报 "server responded with a different http version"(PoC 实证)。
//! state.http 启用 http2 feature、默认 ALPN 协商 h2(给普通转发),故 WS 专用 `http1_only()` client。它用 reqwest
//! **0.13**(reqwest-websocket 0.6 的要求),与 state.http 的 reqwest 0.12 经 package rename
//! 共存 —— **state.http 完全不动**,所有现有上游转发的 CF/ClientHello 指纹零变化(升级范围 A)。
//!
//! PoC 已验证传输层完全打通:reqwest 0.13 + http1_only 连 wss://chatgpt.com 过 CF
//! (cf-ray 放行无 challenge)、过系统代理、http1.1 WS upgrade 到达 OpenAI 应用层。

use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket};
use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use reqwest_websocket::{CloseCode, Message as UpMessage, Upgrade, WebSocket as UpWebSocket};

use crate::telemetry::proxy_telemetry;

/// 远程控制 WS 端点路径。**单一来源** —— [`crate::server`] 的 axum 显式路由直接用此常量
/// 注册(`get` 这条路径 → WS 透传),避免 path 字符串两处硬编码 drift。`/enroll`(HTTP POST
/// 前置)路径更长、不等于此常量,落 fallback 的普通 passthrough。
pub const REMOTE_CONTROL_WS_PATH: &str = "/backend-api/wham/remote/control/server";

/// WS 透传专用上游 client:`http1_only`(WS upgrade 需 HTTP/1.1)+ rustls + system-proxy,
/// 进程级 `OnceLock` 复用连接池。**独立于 state.http**(reqwest 0.12),用 reqwest 0.13
/// (package `reqwest13`)配 reqwest-websocket 0.6。
fn ws_upstream_client() -> &'static reqwest13::Client {
    static CLIENT: OnceLock<reqwest13::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest13::Client::builder()
            .http1_only()
            .use_rustls_tls()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .expect("build WS upstream client")
    })
}

/// 远程控制 WS 透传主流程:连 `wss://chatgpt.com{client_path}` 上游 → 双向 frame pump。
///
/// `headers` 是 Codex 原始请求头(含 `x-codex-*` + `authorization`);`client_path` 是 relay
/// 收到的原始 path(含 query,已确认是远程控制 WS)。上游握手失败时给 Codex 发 Close 让其
/// 立即重试,不静默挂起。
pub async fn proxy_remote_control(
    client_socket: WebSocket,
    headers: HeaderMap,
    client_path: String,
) {
    let telemetry = proxy_telemetry();
    let upstream_url = format!("wss://chatgpt.com{client_path}");
    telemetry.logs.add(
        "INFO",
        format!("[remote-control-ws] upgrade → {upstream_url}"),
    );

    // 上游 WS 握手:注入 Codex 的 x-codex-* + authorization(远程控制 required headers);
    // 跳过 WS 协议握手 header(reqwest-websocket 自己生成上游段的)。
    let mut req = ws_upstream_client().get(&upstream_url);
    for (k, v) in headers.iter() {
        if should_forward_ws_header(k.as_str()) {
            req = req.header(k.as_str(), v.as_bytes());
        }
    }

    let upstream: UpWebSocket = match req.upgrade().send().await {
        Ok(resp) => match resp.into_websocket().await {
            Ok(ws) => ws,
            Err(e) => {
                telemetry.logs.add(
                    "WARN",
                    format!("[remote-control-ws] 上游 upgrade 失败(非 101): {e}"),
                );
                close_client(client_socket, "upstream upgrade failed").await;
                return;
            }
        },
        Err(e) => {
            telemetry
                .logs
                .add("WARN", format!("[remote-control-ws] 上游连接失败: {e}"));
            close_client(client_socket, "upstream connect failed").await;
            return;
        }
    };
    telemetry.logs.add(
        "INFO",
        "[remote-control-ws] 上游 WS 建立(101),双向 pump 开始".to_string(),
    );

    pump(client_socket, upstream).await;
    telemetry
        .logs
        .add("INFO", "[remote-control-ws] pump 结束,通道关闭".to_string());
}

/// 哪些 Codex 原始 header 透传给上游 WS。透传 `authorization` + `x-codex-*`(远程控制
/// required headers),**跳过** WS 协议握手 header —— `host`(reqwest 按 upstream 重填)、
/// `connection`/`upgrade`/`sec-websocket-*`(client↔proxy 段的握手字段,proxy↔upstream
/// 段由 reqwest-websocket 重新生成)、`accept-encoding`/`content-length`(WS GET 无 body)。
///
/// 边界:`sec-websocket-protocol`(subprotocol)也被这条 skip 掉。当前 Codex 远程控制握手
/// **不带** subprotocol(抓包实证),故无碍;若将来 Codex 改用 subprotocol,需单独把它透传到
/// 上游(reqwest-websocket `.protocols()`)并在接收侧 echo,否则 client 握手会失败 —— 届时再补。
fn should_forward_ws_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !(lower == "host"
        || lower == "connection"
        || lower == "upgrade"
        || lower.starts_with("sec-websocket")
        || lower == "accept-encoding"
        || lower == "content-length")
}

/// 双向 frame pump:Codex(axum)↔ 上游(reqwest-websocket)。`tokio::select!` 两个方向,
/// 转换两库各自的 `Message` 类型;任一端 `Close` / 读到 `None` / 写失败即收束,并尽力给
/// 对端发 Close。
async fn pump(client: WebSocket, upstream: UpWebSocket) {
    let telemetry = proxy_telemetry();
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    loop {
        tokio::select! {
            // Codex → 上游
            msg = client_rx.next() => match msg {
                Some(Ok(m)) => {
                    let is_close = matches!(m, AxMessage::Close(_));
                    if up_tx.send(ax_to_up(m)).await.is_err() {
                        telemetry
                            .logs
                            .add("WARN", "[remote-control-ws] 写上游失败,收束通道".to_string());
                        break;
                    }
                    if is_close {
                        break;
                    }
                }
                // 区分:读错误(TLS reset / 协议违例)记 WARN 带 error 文本,clean EOF(None)静默
                // 收束 —— 否则诊断模块("把 TLS 黑盒变可见")里中途断连与优雅关闭日志无从区分。
                Some(Err(e)) => {
                    telemetry
                        .logs
                        .add("WARN", format!("[remote-control-ws] Codex 侧读错误: {e}"));
                    break;
                }
                None => break,
            },
            // 上游 → Codex
            msg = up_rx.next() => match msg {
                Some(Ok(m)) => {
                    let is_close = matches!(m, UpMessage::Close { .. });
                    if client_tx.send(up_to_ax(m)).await.is_err() {
                        telemetry
                            .logs
                            .add("WARN", "[remote-control-ws] 写 Codex 失败,收束通道".to_string());
                        break;
                    }
                    if is_close {
                        break;
                    }
                }
                Some(Err(e)) => {
                    telemetry
                        .logs
                        .add("WARN", format!("[remote-control-ws] 上游侧读错误: {e}"));
                    break;
                }
                None => break,
            },
        }
    }

    let _ = up_tx.close().await;
    let _ = client_tx.close().await;
}

/// axum WS 帧 → reqwest-websocket 帧(Codex → 上游方向)。
fn ax_to_up(m: AxMessage) -> UpMessage {
    match m {
        AxMessage::Text(t) => UpMessage::Text(t.to_string()),
        AxMessage::Binary(b) => UpMessage::Binary(b),
        AxMessage::Ping(b) => UpMessage::Ping(b),
        AxMessage::Pong(b) => UpMessage::Pong(b),
        AxMessage::Close(frame) => match frame {
            Some(f) => UpMessage::Close {
                code: CloseCode::from(f.code),
                reason: f.reason.to_string(),
            },
            None => UpMessage::Close {
                code: CloseCode::Normal,
                reason: String::new(),
            },
        },
    }
}

/// reqwest-websocket 帧 → axum WS 帧(上游 → Codex 方向)。
fn up_to_ax(m: UpMessage) -> AxMessage {
    match m {
        UpMessage::Text(s) => AxMessage::Text(s.into()),
        UpMessage::Binary(b) => AxMessage::Binary(b),
        UpMessage::Ping(b) => AxMessage::Ping(b),
        UpMessage::Pong(b) => AxMessage::Pong(b),
        UpMessage::Close { code, reason } => AxMessage::Close(Some(CloseFrame {
            code: u16::from(code),
            reason: reason.into(),
        })),
    }
}

/// 上游握手失败时给 Codex 端发 Close(理由 reason),让其立即按 WS 不可用处理 → 重试,
/// 不静默挂起到 idle timeout。
async fn close_client(mut socket: WebSocket, reason: &str) {
    // best-effort:client 已断时发不出是正常的(它本就不会再 hang);但若 client 还在而 Close
    // 发失败,它会挂到 idle timeout —— 正是本函数要防的,故失败记一条 WARN 让其可见。
    if socket
        .send(AxMessage::Close(Some(CloseFrame {
            code: axum::extract::ws::close_code::ERROR,
            reason: reason.to_string().into(),
        })))
        .await
        .is_err()
    {
        proxy_telemetry().logs.add(
            "WARN",
            format!("[remote-control-ws] 给 Codex 发 Close 失败({reason}),客户端可能挂起到 idle timeout"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_control_path_constant_is_server_endpoint() {
        // 常量是单一来源(server.rs route 直接用);enroll 路径更长、不等于它,走 fallback。
        assert_eq!(
            REMOTE_CONTROL_WS_PATH,
            "/backend-api/wham/remote/control/server"
        );
        assert_ne!(
            REMOTE_CONTROL_WS_PATH,
            "/backend-api/wham/remote/control/server/enroll"
        );
    }

    #[test]
    fn forwards_codex_headers_skips_ws_handshake_headers() {
        // 远程控制 required headers 透传
        assert!(should_forward_ws_header("authorization"));
        assert!(should_forward_ws_header("x-codex-installation-id"));
        assert!(should_forward_ws_header("x-codex-protocol-version"));
        assert!(should_forward_ws_header("x-codex-name"));
        assert!(should_forward_ws_header("x-codex-server-id"));
        // WS 握手 header 由 reqwest-websocket 重新生成,不透传
        assert!(!should_forward_ws_header("host"));
        assert!(!should_forward_ws_header("Connection"));
        assert!(!should_forward_ws_header("Upgrade"));
        assert!(!should_forward_ws_header("Sec-WebSocket-Key"));
        assert!(!should_forward_ws_header("sec-websocket-version"));
        assert!(!should_forward_ws_header("accept-encoding"));
    }

    #[test]
    fn close_frame_roundtrips_code_and_reason() {
        // axum → up → axum 的 Close code 应保持(用一个非 Normal 的 IANA code)
        let ax = AxMessage::Close(Some(CloseFrame {
            code: 1011,
            reason: "boom".to_string().into(),
        }));
        let up = ax_to_up(ax);
        match &up {
            UpMessage::Close { code, reason } => {
                assert_eq!(u16::from(*code), 1011);
                assert_eq!(reason, "boom");
            }
            _ => panic!("expected Close"),
        }
        match up_to_ax(up) {
            AxMessage::Close(Some(f)) => {
                assert_eq!(f.code, 1011);
                assert_eq!(f.reason.as_str(), "boom");
            }
            _ => panic!("expected Close"),
        }
    }

    #[test]
    fn text_binary_roundtrip() {
        match ax_to_up(AxMessage::Text("hi".to_string().into())) {
            UpMessage::Text(s) => assert_eq!(s, "hi"),
            _ => panic!("expected Text"),
        }
        match up_to_ax(UpMessage::Text("yo".to_string())) {
            AxMessage::Text(t) => assert_eq!(t.as_str(), "yo"),
            _ => panic!("expected Text"),
        }
        let payload = bytes::Bytes::from_static(b"\x00\x01\x02");
        match ax_to_up(AxMessage::Binary(payload.clone())) {
            UpMessage::Binary(b) => assert_eq!(b, payload),
            _ => panic!("expected Binary"),
        }
    }
}
