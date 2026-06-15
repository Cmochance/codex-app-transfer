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
//!
//! ## [MOC-234 / followup MOC-239] responses provider 上游 WS relay(**默认禁用**)
//! 本模块还含一条 native responses provider 的全程 WS relay([`proxy_responses_upstream_ws`] /
//! [`relay_manual`]):Codex-WS ↔ 上游-WS,经 SOCKS5(VPN 代理)+ 手搓 TLS([`tls_connect`],
//! tokio-rustls)+ 手搓 WS 握手([`from_raw_socket`](WebSocketStream::from_raw_socket) 收发帧)。
//! **实证**:freemodel 等 AWS ALB 后端对 HTTP/1.1 WS Upgrade 一律 426、对 h2 extended CONNECT 400,
//! proxy 重发的握手不可达 → 该 relay **默认关**,native responses 改由 [`crate::server`] 对 WS
//! upgrade 回 426 让 Codex 自降级 HTTP。relay 仅在 `CAS_RESPONSES_WS_PASSTHROUGH=1` 时启用,留待
//! MOC-239 对支持 WS 的端点验证。remote-control 那半(上方)走 reqwest-websocket,与此互不影响。

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket};
use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use reqwest_websocket::{CloseCode, Message as UpMessage, Upgrade, WebSocket as UpWebSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as TungCloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame as TungCloseFrame, Role};
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::WebSocketStream;

use crate::resolver::{AuthScheme, ResolvedProvider};
use crate::telemetry::proxy_telemetry;

/// 远程控制 WS 端点路径。**单一来源** —— [`crate::server`] 的 axum 显式路由直接用此常量
/// 注册(`get` 这条路径 → WS 透传),避免 path 字符串两处硬编码 drift。`/enroll`(HTTP POST
/// 前置)路径更长、不等于此常量,落 fallback 的普通 passthrough。
pub const REMOTE_CONTROL_WS_PATH: &str = "/backend-api/wham/remote/control/server";

/// 远程控制 WS 专用上游 client:`http1_only`(WS upgrade 需 HTTP/1.1)+ rustls + system-proxy,
/// 进程级 `OnceLock` 复用连接池。**独立于 state.http**(reqwest 0.12),用 reqwest 0.13(package
/// `reqwest13`)配 reqwest-websocket 0.6。**仅 [`proxy_remote_control`] 用** —— responses 上游 WS
/// 改用 tokio-tungstenite(见 [`proxy_responses_upstream_ws`]),不经此 client。
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

/// 进程级一次性安装 rustls 0.23 默认 CryptoProvider。依赖树里 aws-lc-rs(reqwest)+ ring
/// (tokio-tungstenite)同在 → rustls 无法自动选 provider,`client_async_tls` 做 TLS 时会
/// **panic**(WS task 直接死、既不建立也不报错 → Codex 卡住重连)。这里显式装 aws-lc-rs(与
/// reqwest 上游转发一致)。`Once` 保证只装一次;已装(reqwest 等先装过)则 `install_default`
/// 返 Err、忽略。
fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// VPN 的 HTTP 代理 URL —— responses 上游 WS 的手动 HTTP-CONNECT 用(见
/// [`proxy_responses_upstream_ws`])。优先进程 env(`HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`,
/// 大小写都认),否则读 `~/.codex/.env`(用户给 Codex 配的「全通信走 VPN」代理)。返回首个非空
/// 代理 URL;无(非 VPN 用户)→ `None` = 直连(行为不变)。
fn vpn_http_proxy() -> Option<String> {
    for k in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let home = std::env::var("HOME").ok()?;
    let content =
        std::fs::read_to_string(std::path::Path::new(&home).join(".codex").join(".env")).ok()?;
    for raw in content.lines() {
        let line = raw.trim().strip_prefix("export ").unwrap_or(raw.trim());
        for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
            if let Some(rest) = line.strip_prefix(&format!("{k}=")) {
                let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
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

    // [MOC-124 H-2 note] 这条 WS upgrade 失败(含上游 401 = chatgpt token 服务端失效)**不**单独
    // 回灌账号 relogin —— H-2 的回灌只挂在 HTTP passthrough(forward.rs `passthrough_chatgpt_backend`)。
    // 同一个被撤销的 token 必然让 Codex 的 HTTP `getAccount`/`plugins` poll 也 401、被那条捕获回灌,
    // 故 WS 这条不重复处理(HTTP poll 是可靠兜底,Codex 持续 poll backend)。
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

/// [MOC-234] native responses provider 的**全程 WS 透传**:Codex-WS ↔ proxy ↔ 上游-WS。
///
/// 背景:Codex `/responses` 默认走 Responses WebSocket v2(`provider.supports_websockets`,
/// 内置 openai provider 恒 true、即便 `openai_base_url` 被指到本 proxy 也保持)。此前本 proxy
/// 把 Codex 的 WS 帧**转成 HTTP** 发上游(ws→http),导致:① 只在 WS v2 支持 `previous_response_id`
/// 的上游(如 freemodel.dev)对每个续轮 400;② 上游 SSE 经 re-framing 回灌引起整段文字闪烁。
/// 本函数对 native responses provider **不再转 HTTP**,而是把 Codex 帧原样 relay 到上游的
/// Responses WS v2(保 `previous_response_id`、保原生流式),与 direct 直连时一致。
///
/// `resolved` 给出上游 base / 鉴权;`handshake_headers` 是 Codex 的 WS 握手头(透传
/// `OpenAI-Beta: responses_websockets` / `x-codex-*`,剥 gateway authorization);`first_frame`
/// 是已从 Codex 读到的首个 `response.create` 帧(解析过 model 用于路由,这里**原样**发上游)。
/// 上游握手失败 → 给 Codex 发 Close(error)让其按 WS 不可用处理,**不**回退到已失败的 ws→http。
pub async fn proxy_responses_upstream_ws(
    client_socket: WebSocket,
    resolved: ResolvedProvider,
    handshake_headers: HeaderMap,
    first_frame: AxMessage,
) {
    ensure_crypto_provider();
    let telemetry = proxy_telemetry();
    let Some(upstream_url) = responses_ws_url(&resolved.upstream_base) else {
        telemetry.logs.add(
            "WARN",
            format!(
                "[responses-ws] 无法从 upstream_base 构造 WS URL: {}",
                resolved.upstream_base
            ),
        );
        close_client(client_socket, "bad upstream base url").await;
        return;
    };
    let Some((host, port)) = parse_ws_target(&upstream_url) else {
        telemetry.logs.add(
            "WARN",
            format!("[responses-ws] 无法解析 WS host:port: {upstream_url}"),
        );
        close_client(client_socket, "bad upstream ws url").await;
        return;
    };
    let path = ws_path(&upstream_url);
    telemetry.logs.add(
        "INFO",
        format!(
            "[responses-ws] upgrade → {upstream_url}(provider {})",
            resolved.provider_id
        ),
    );

    // 收集非-WS 头(Codex 真实头值 + 鉴权)。手搓握手里(见 [`relay_manual`])排布逐字节复刻
    // Codex 真实握手(tcpdump 实证):WS 握手头(Host/Connection/Upgrade/Sec-WebSocket-*)在**前**,
    // 这些自定义头在后。(Codex 真实握手末尾还带 `permessage-deflate` extension,但本 relay 无压缩
    // 处理故**不 offer**,见 [`relay_manual`]。)
    let mut headers: Vec<(String, String)> = Vec::new();
    // 鉴权放自定义头块最前(整块仍在 WS 握手头之后,见上):第三方注入 provider 凭据;
    // chatgpt.com(key 空)透传 Codex token。
    if resolved.api_key.is_empty() {
        if let Some(auth) = handshake_headers.get(axum::http::header::AUTHORIZATION) {
            if let Ok(v) = auth.to_str() {
                headers.push(("authorization".to_string(), v.to_string()));
            }
        }
    } else {
        match resolved.auth_scheme {
            AuthScheme::XApiKey => {
                headers.push(("x-api-key".to_string(), resolved.api_key.clone()))
            }
            _ => headers.push((
                "authorization".to_string(),
                format!("Bearer {}", resolved.api_key),
            )),
        }
    }
    // Codex 真实握手头(跳过 authorization + WS 握手头);记名字便于诊断。
    let mut forwarded_names: Vec<String> = Vec::new();
    for (k, v) in handshake_headers.iter() {
        if should_forward_responses_ws_header(k.as_str()) {
            if let Ok(val) = v.to_str() {
                headers.push((k.as_str().to_string(), val.to_string()));
                forwarded_names.push(k.as_str().to_string());
            }
        }
    }
    for (k, v) in resolved.extra_headers.iter() {
        if let Ok(val) = v.to_str() {
            headers.push((k.as_str().to_string(), val.to_string()));
        }
    }
    telemetry.logs.add(
        "INFO",
        format!(
            "[responses-ws] 转发 Codex 握手头: [{}]",
            forwarded_names.join(", ")
        ),
    );

    // 建到上游的隧道:有 VPN 代理(见 [`vpn_http_proxy`])走 SOCKS5(proxy 端解析域名拿真实 IP,
    // 绕开客户端 fake-ip);无代理直连。隧道流交给 [`relay_manual`] 做 TLS + 手搓握手 + 双向 relay。
    match vpn_http_proxy() {
        Some(proxy) => {
            let Some((ph, pp)) = parse_authority(&proxy) else {
                telemetry.logs.add(
                    "WARN",
                    format!("[responses-ws] 代理 URL 无 host:port: {proxy}"),
                );
                close_client(client_socket, "bad proxy url").await;
                return;
            };
            telemetry.logs.add(
                "INFO",
                format!("[responses-ws] 经 SOCKS5 代理 {ph}:{pp} → {host}:{port}"),
            );
            match tokio_socks::tcp::Socks5Stream::connect((ph.as_str(), pp), (host.as_str(), port))
                .await
            {
                Ok(s) => relay_manual(client_socket, host, path, headers, s, first_frame).await,
                Err(e) => {
                    telemetry
                        .logs
                        .add("WARN", format!("[responses-ws] SOCKS5 连接失败: {e}"));
                    close_client(client_socket, "socks5 connect failed").await;
                }
            }
        }
        None => match TcpStream::connect((host.as_str(), port)).await {
            Ok(s) => relay_manual(client_socket, host, path, headers, s, first_frame).await,
            Err(e) => {
                telemetry
                    .logs
                    .add("WARN", format!("[responses-ws] 上游直连失败: {e}"));
                close_client(client_socket, "upstream connect failed").await;
            }
        },
    }
}

/// TLS(tokio-rustls + webpki-roots)+ **手搓 WS 握手**(WS 头在前、自定义头在后,逐字节复刻 Codex
/// 真实握手;`http` HeaderMap 控制不了顺序,故手写字节;**不 offer permessage-deflate**,见函数体)+
/// `from_raw_socket` 收发帧。握手非 101 → 给 Codex 发 Close + 日志状态码。`headers` 为自定义头
/// (含鉴权,**不含** WS 握手头)。
async fn relay_manual<S>(
    client_socket: WebSocket,
    host: String,
    path: String,
    headers: Vec<(String, String)>,
    stream: S,
    first_frame: AxMessage,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let telemetry = proxy_telemetry();
    let mut tls = match tls_connect(&host, stream).await {
        Ok(t) => t,
        Err(e) => {
            telemetry
                .logs
                .add("WARN", format!("[responses-ws] 上游 TLS 失败: {e}"));
            close_client(client_socket, "upstream tls failed").await;
            return;
        }
    };
    let wskey = random_ws_key();
    // 逐字节复刻 Codex 真实握手排布(tcpdump 实证):WS 握手头在**前**,然后自定义头(鉴权 +
    // Codex 真实头值)。**不 offer `permessage-deflate`**:Codex 真实握手带该 extension,但本 relay
    // 用 `from_raw_socket(.., None)` 无压缩处理,若上游接受 offer 并发压缩帧,tokio-tungstenite 见
    // 非零 RSV bit 即关流、relay 断(reviewer)。故不 offer(上游不会压缩,裸帧 relay 安全)。
    // [followup MOC-239] 若某 WS 端点要求 permessage-deflate,再启 tokio-tungstenite deflate feature
    // 做真压缩协商,而非裸 offer。
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    req.push_str(&format!(
        "Host: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {wskey}\r\n"
    ));
    for (k, v) in &headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    if tls.write_all(req.as_bytes()).await.is_err() {
        telemetry
            .logs
            .add("WARN", "[responses-ws] 写握手失败".to_string());
        close_client(client_socket, "upstream write failed").await;
        return;
    }
    let (status, head) = match read_http_status(&mut tls).await {
        Ok(s) => s,
        Err(e) => {
            telemetry
                .logs
                .add("WARN", format!("[responses-ws] 读握手响应失败: {e}"));
            close_client(client_socket, "upstream handshake read failed").await;
            return;
        }
    };
    // 诊断:记上游握手响应的状态行 + **白名单头**(看 101 + 是否回 Sec-WebSocket-Extensions=接受压缩)。
    // 只记状态行 / sec-websocket-* / cf-ray / content-type —— 避免把 Set-Cookie(CF/ALB 会话 cookie)、
    // 鉴权头等凭据级敏感值写进诊断日志。
    let safe_head = head
        .lines()
        .filter(|line| {
            let low = line.to_ascii_lowercase();
            low.starts_with("http/")
                || low.starts_with("sec-websocket-")
                || low.starts_with("cf-ray")
                || low.starts_with("content-type")
        })
        .collect::<Vec<_>>()
        .join(" | ");
    telemetry
        .logs
        .add("INFO", format!("[responses-ws] 上游握手响应: {safe_head}"));
    if status != 101 {
        telemetry.logs.add(
            "WARN",
            format!("[responses-ws] 上游 WS 握手失败 status {status}"),
        );
        close_client(client_socket, "upstream ws handshake non-101").await;
        return;
    }
    telemetry.logs.add(
        "INFO",
        "[responses-ws] 上游 WS 建立(101),首帧 + 双向 relay 开始".to_string(),
    );
    let mut upstream = WebSocketStream::from_raw_socket(tls, Role::Client, None).await;
    if upstream.send(ax_to_tung(first_frame)).await.is_err() {
        telemetry
            .logs
            .add("WARN", "[responses-ws] 首帧写上游失败".to_string());
        close_client(client_socket, "upstream write failed").await;
        return;
    }
    tung_pump(client_socket, upstream).await;
    telemetry
        .logs
        .add("INFO", "[responses-ws] relay 结束,通道关闭".to_string());
}

/// tokio-rustls TLS connect(webpki-roots 根证书,SNI=host)。进程级 OnceLock 复用 ClientConfig。
async fn tls_connect<S>(
    host: &str,
    stream: S,
) -> std::io::Result<tokio_rustls::client::TlsStream<S>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    let connector = tokio_rustls::TlsConnector::from(config.clone());
    let sni = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad sni: {e}"))
    })?;
    connector.connect(sni, stream).await
}

/// 读 HTTP 响应头(到 `\r\n\r\n`),返回 (状态码, 完整响应头原文)。流随后停在帧数据起点
/// (供 `from_raw_socket`)。响应头原文供诊断(看 `Sec-WebSocket-Extensions` 上游是否接受压缩)。
async fn read_http_status<S>(stream: &mut S) -> std::io::Result<(u16, String)>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "上游在握手期间关闭",
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16384 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "握手响应头过大",
            ));
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "无法解析状态行"))?;
    Ok((status, head))
}

/// 随机 16 字节 base64 Sec-WebSocket-Key(避免 RFC 示例值被 WAF 当扫描器拉黑)。
fn random_ws_key() -> String {
    use base64::Engine;
    let mut kb = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut kb);
    }
    base64::engine::general_purpose::STANDARD.encode(kb)
}

/// 从 `ws(s)://authority/path...` 取 path(含 query);无 path → `/`。
fn ws_path(ws_url: &str) -> String {
    let after = ws_url.split_once("://").map(|(_, r)| r).unwrap_or(ws_url);
    match after.find('/') {
        Some(i) => after[i..].to_string(),
        None => "/".to_string(),
    }
}

/// 由 provider 的 `upstream_base`(http/https)构造上游 Responses WS URL:scheme 换
/// `ws`/`wss`,path 追加 `/responses`(与 HTTP 转发的 `build_upstream_url(base, "/responses")`
/// 同口径)。非 http(s)/ws(s) → None。
fn responses_ws_url(upstream_base: &str) -> Option<String> {
    let base = upstream_base.trim_end_matches('/');
    let swapped = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("wss://") || base.starts_with("ws://") {
        base.to_string()
    } else {
        return None;
    };
    Some(format!("{swapped}/responses"))
}

/// 从代理 URL 取 `host`/`port`(剥 scheme + userinfo + path)。无显式端口 → None(.env 的代理
/// 恒带端口,如 `http://127.0.0.1:7897`)。
fn parse_authority(url: &str) -> Option<(String, u16)> {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after.split(['/', '?']).next().unwrap_or(after);
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    let (h, p) = hostport.rsplit_once(':')?;
    Some((h.to_string(), p.parse().ok()?))
}

/// 从 `ws(s)://host[:port]/path` 取 `host`/`port`(无端口按 scheme 取默认 443/80)。
fn parse_ws_target(ws_url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = ws_url.split_once("://")?;
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    let default = if scheme == "wss" { 443 } else { 80 };
    match hostport.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => Some((h.to_string(), port)),
            Err(_) => Some((hostport.to_string(), default)),
        },
        None => Some((hostport.to_string(), default)),
    }
}

/// responses WS relay 透传哪些 Codex 握手头给上游。同 [`should_forward_ws_header`],但**额外
/// 跳过 `authorization`** —— responses relay 的鉴权由 [`proxy_responses_upstream_ws`] 决定
/// (第三方注入 provider 凭据 / chatgpt.com 透传 Codex token),不在通用透传里处理。
fn should_forward_responses_ws_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower != "authorization" && should_forward_ws_header(name)
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

/// 双向 frame pump(responses 上游 WS):Codex(axum)↔ 上游(tokio-tungstenite)。同 [`pump`]
/// 但对接 tungstenite 的 `Message` 类型。任一端 `Close` / 读到 `None` / 写失败即收束。
async fn tung_pump<S>(client: WebSocket, upstream: WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let telemetry = proxy_telemetry();
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    loop {
        tokio::select! {
            msg = client_rx.next() => match msg {
                Some(Ok(m)) => {
                    let is_close = matches!(m, AxMessage::Close(_));
                    if up_tx.send(ax_to_tung(m)).await.is_err() {
                        break;
                    }
                    if is_close {
                        break;
                    }
                }
                Some(Err(e)) => {
                    telemetry
                        .logs
                        .add("WARN", format!("[responses-ws] Codex 侧读错误: {e}"));
                    break;
                }
                None => break,
            },
            msg = up_rx.next() => match msg {
                Some(Ok(m)) => {
                    let is_close = matches!(m, TungMessage::Close(_));
                    if client_tx.send(tung_to_ax(m)).await.is_err() {
                        break;
                    }
                    if is_close {
                        break;
                    }
                }
                Some(Err(e)) => {
                    telemetry
                        .logs
                        .add("WARN", format!("[responses-ws] 上游侧读错误: {e}"));
                    break;
                }
                None => break,
            },
        }
    }

    let _ = up_tx.close().await;
    let _ = client_tx.close().await;
}

/// axum WS 帧 → tokio-tungstenite 帧(Codex → 上游方向)。
fn ax_to_tung(m: AxMessage) -> TungMessage {
    match m {
        AxMessage::Text(t) => TungMessage::Text(t.to_string().into()),
        AxMessage::Binary(b) => TungMessage::Binary(b),
        AxMessage::Ping(b) => TungMessage::Ping(b),
        AxMessage::Pong(b) => TungMessage::Pong(b),
        AxMessage::Close(frame) => TungMessage::Close(frame.map(|f| TungCloseFrame {
            code: TungCloseCode::from(f.code),
            reason: f.reason.to_string().into(),
        })),
    }
}

/// tokio-tungstenite 帧 → axum WS 帧(上游 → Codex 方向)。`Frame`(原始帧)在读路径不应出现,
/// 兜底成空 Binary(无害)。
fn tung_to_ax(m: TungMessage) -> AxMessage {
    match m {
        TungMessage::Text(t) => AxMessage::Text(t.as_str().to_owned().into()),
        TungMessage::Binary(b) => AxMessage::Binary(b),
        TungMessage::Ping(b) => AxMessage::Ping(b),
        TungMessage::Pong(b) => AxMessage::Pong(b),
        TungMessage::Close(frame) => AxMessage::Close(frame.map(|f| CloseFrame {
            code: u16::from(f.code),
            reason: f.reason.to_string().into(),
        })),
        TungMessage::Frame(_) => AxMessage::Binary(bytes::Bytes::new()),
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

    #[test]
    fn responses_ws_url_swaps_scheme_and_appends_responses_path() {
        // https→wss / http→ws,尾随 `/` 归一,path 追加 /responses(同 HTTP build_upstream_url)。
        assert_eq!(
            responses_ws_url("https://api.freemodel.dev").as_deref(),
            Some("wss://api.freemodel.dev/responses")
        );
        assert_eq!(
            responses_ws_url("http://127.0.0.1:18080").as_deref(),
            Some("ws://127.0.0.1:18080/responses")
        );
        assert_eq!(
            responses_ws_url("https://host/v1/").as_deref(),
            Some("wss://host/v1/responses")
        );
        // 已是 ws/wss 原样保留
        assert_eq!(
            responses_ws_url("wss://host").as_deref(),
            Some("wss://host/responses")
        );
        // 非 http(s)/ws(s) → None
        assert_eq!(responses_ws_url("ftp://nope"), None);
    }

    #[test]
    fn responses_ws_header_filter_skips_authorization_keeps_beta() {
        // 鉴权由 proxy_responses_upstream_ws 单独处理(注入 provider / 透传 Codex),不走通用透传
        assert!(!should_forward_responses_ws_header("authorization"));
        assert!(!should_forward_responses_ws_header("Authorization"));
        // OpenAI-Beta / x-codex-* 必须透传(上游 Responses WS v2 握手需要)
        assert!(should_forward_responses_ws_header("openai-beta"));
        assert!(should_forward_responses_ws_header(
            "x-codex-installation-id"
        ));
        // WS 握手头 / host 仍跳过(reqwest-websocket 重新生成)
        assert!(!should_forward_responses_ws_header("sec-websocket-key"));
        assert!(!should_forward_responses_ws_header("host"));
    }

    #[test]
    fn parse_authority_extracts_host_port_strips_scheme_userinfo_path() {
        assert_eq!(
            parse_authority("http://127.0.0.1:7897"),
            Some(("127.0.0.1".to_string(), 7897))
        );
        assert_eq!(
            parse_authority("http://user:pass@host:1080/x"),
            Some(("host".to_string(), 1080))
        );
        // 无显式端口 → None(.env 代理恒带端口)
        assert_eq!(parse_authority("http://127.0.0.1"), None);
    }

    #[test]
    fn parse_ws_target_extracts_host_port_with_scheme_default() {
        assert_eq!(
            parse_ws_target("wss://api.freemodel.dev/responses"),
            Some(("api.freemodel.dev".to_string(), 443))
        );
        assert_eq!(
            parse_ws_target("ws://127.0.0.1:18080/responses"),
            Some(("127.0.0.1".to_string(), 18080))
        );
        assert_eq!(
            parse_ws_target("wss://host:8443/responses?x=1"),
            Some(("host".to_string(), 8443))
        );
    }
}
