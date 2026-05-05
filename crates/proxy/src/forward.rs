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
    http::{HeaderMap, HeaderName, StatusCode},
    response::Response,
};
use bytes::Bytes;
use codex_app_transfer_adapters::{AdapterError, AdapterRegistry};
use codex_app_transfer_registry::strip_internal_model_suffix;
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

impl ProxyState {
    pub fn new(resolver: SharedResolver) -> Self {
        Self {
            http: reqwest::Client::builder()
                .pool_idle_timeout(std::time::Duration::from_secs(30))
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
            .add("ERROR", format!("代理请求失败: {message}"));
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

/// Authorization 单独剔除:gateway 鉴权用的 token 不能传到上游.
fn is_strip_on_forward(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
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

    // 4. 走 adapter 拿到上游路径 + 改写后的 body(openai_chat 路径下 body 等同)
    let adapter = state.adapters.lookup(&resolved.provider.api_format);
    let plan = adapter.prepare_request(&client_path, body_bytes, &resolved.provider)?;

    // 5. 拼上游 URL —— base 末尾去 `/`,plan.upstream_path 必含 `/`
    let path = if plan.upstream_path.starts_with('/') {
        plan.upstream_path.clone()
    } else {
        format!("/{}", plan.upstream_path)
    };
    let upstream_url = format!("{}{}", resolved.upstream_base.trim_end_matches('/'), path);
    let telemetry = proxy_telemetry();
    telemetry
        .logs
        .add("INFO", format!("请求: {} {client_path}", parts.method));
    if let Some(original_model) = original_model.as_deref() {
        let mapped = resolved_model.as_deref().unwrap_or(original_model);
        telemetry
            .logs
            .add("INFO", format!("模型映射: {original_model} → {mapped}"));
    }
    telemetry
        .logs
        .add("INFO", format!("转发请求 → {upstream_url}"));
    if let Some(upstream_model) = body_model(&plan.body) {
        let mapped = resolved_model.as_deref().unwrap_or(&upstream_model);
        telemetry
            .logs
            .add("INFO", format!("模型: {mapped} → {upstream_model}"));
    }

    // 6. 构造 reqwest 请求 —— 头复制 + 鉴权改写 + extras 注入
    let mut up = state
        .http
        .request(parts.method.clone(), &upstream_url)
        .body(plan.body);
    for (name, value) in parts.headers.iter() {
        if is_hop_header(name.as_str()) || is_strip_on_forward(name.as_str()) {
            continue;
        }
        up = up.header(name, value);
    }
    up = inject_auth(up, &resolved);
    for (name, value) in resolved.extra_headers.iter() {
        up = up.header(name, value);
    }

    // 7. 发起 + 转译响应(adapter 默认透传;Stage 3.2 起 SSE 状态机会重写流)
    let resp = up.send().await?;
    let status = resp.status();
    let upstream_headers = filter_hop_headers(resp.headers());
    let upstream_stream: codex_app_transfer_adapters::ByteStream = Box::pin(
        resp.bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
    );

    let response_plan = adapter.transform_response_stream(
        status,
        upstream_headers,
        upstream_stream,
        &resolved.provider,
    )?;
    let success = response_plan.status.is_success();
    telemetry.stats.record(success);
    telemetry.logs.add(
        if success { "SUCCESS" } else { "ERROR" },
        format!("上游响应 {}", response_plan.status.as_u16()),
    );

    // 8. 把 ResponsePlan 还原成 axum Response
    let mut builder = Response::builder().status(response_plan.status);
    let headers_out = builder
        .headers_mut()
        .ok_or_else(|| ForwardError::Header("response builder lacks headers".into()))?;
    *headers_out = response_plan.headers;
    Ok(builder.body(Body::from_stream(response_plan.stream))?)
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
}
