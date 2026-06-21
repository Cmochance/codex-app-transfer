//! Provider 连通性测试 + compatibility 矩阵.

use std::time::{Duration, Instant};

use axum::{response::IntoResponse, Json};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    StatusCode as ReqwestStatusCode,
};
use serde_json::{json, Value};

use super::{normalize_provider_api_format, provider_api_key, provider_test_model};

pub(super) fn build_provider_test_url(base_url: &str, api_format: &str) -> String {
    let clean = base_url.trim().trim_end_matches('/');
    let lower = clean.to_ascii_lowercase();
    if api_format == "gemini_native" {
        // Gemini native:用 `/v1beta/models` (list public models) 探测端点。
        // 不带 key → 401 / 403(我们 v2.1.3 已让 401/403 走"绿色 + auth not
        // verified" 路径);带 key → 200。两版本(v1alpha for Gemini 3+ /
        // v1beta for 2.x)的 `models` 端点都返 200,探测 v1beta 即可。
        if lower.ends_with("/v1beta/models") || lower.ends_with("/v1alpha/models") {
            return clean.to_owned();
        }
        if lower.ends_with("/v1beta") || lower.ends_with("/v1alpha") {
            return format!("{clean}/models");
        }
        return format!("{clean}/v1beta/models");
    }
    if api_format == "anthropic_messages" {
        if lower.ends_with("/v1/messages") || lower.ends_with("/messages") {
            return clean.to_owned();
        }
        if lower.ends_with("/v1") {
            return format!("{clean}/messages");
        }
        return format!("{clean}/v1/messages");
    }
    if api_format == "openai_chat" {
        if lower.ends_with("/chat/completions") {
            return clean.to_owned();
        }
        return format!("{clean}/chat/completions");
    }
    if lower.ends_with("/v1/responses") {
        return clean.to_owned();
    }
    if lower.ends_with("/v1") {
        return format!("{clean}/responses");
    }
    format!("{clean}/v1/responses")
}

fn provider_test_body(provider: &Value, api_format: &str) -> Value {
    let model = provider_test_model(provider);
    if api_format == "openai_chat" || api_format == "anthropic_messages" {
        return json!({
            "model": model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 8,
            "stream": false,
        });
    }
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 8,
    })
}

pub(super) fn provider_test_headers(provider: &Value, include_content_type: bool) -> HeaderMap {
    let api_key = provider_api_key(provider);
    let mut headers = HeaderMap::new();
    if include_content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    if api_format == "anthropic_messages" {
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
    }

    if !api_key.is_empty() {
        let auth_scheme = provider
            .get("authScheme")
            .and_then(|v| v.as_str())
            .unwrap_or("bearer")
            .trim()
            .to_ascii_lowercase();
        match auth_scheme.as_str() {
            "x-api-key" | "x_api_key" | "xapikey" | "apikey" => {
                if let Ok(value) = HeaderValue::from_str(&api_key) {
                    headers.insert(HeaderName::from_static("x-api-key"), value);
                }
            }
            "google_api_key" | "x-goog-api-key" | "x_goog_api_key" | "google" | "gemini" => {
                // Google AI Studio Gemini API:`x-goog-api-key: <key>` header
                // (LiteLLM 注释:API key 不放 URL 防 traceback 泄露)。
                if let Ok(value) = HeaderValue::from_str(&api_key) {
                    headers.insert(HeaderName::from_static("x-goog-api-key"), value);
                }
            }
            "none" | "no" => {}
            _ => {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, value);
                }
            }
        }
    }

    if let Some(extra) = provider.get("extraHeaders").and_then(|v| v.as_object()) {
        for (key, value) in extra {
            let Some(raw_value) = value.as_str() else {
                continue;
            };
            let header_value = raw_value.replace("{apiKey}", &api_key);
            let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(&header_value),
            ) else {
                continue;
            };
            headers.insert(name, value);
        }
    }

    let provider_id = provider.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let base_url = provider
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if provider_id == "kimi-code" || base_url.contains("api.kimi.com/coding") {
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("KimiCLI/1.40.0"),
        );
    }

    // GLM Coding Plan 路径的「测试 provider」探测也要带完整 ZCode 指纹头,与
    // forward.rs 的 `injects_zcode_source_headers` 注入路径对齐:`zhipu-coding`
    // (Bearer + coding/paas/v4 端点)走 API key,`zai-login`/`bigmodel-login`
    // (zai/bigmodel OAuth)走组织 key。否则测试请求是普通 API-key 调用、无 ZCode
    // 指纹,BigModel 可能误报 key/端点无效或划错配额桶,与正常 chat 行为不一致。
    // `insert` 覆盖语义保证不与上方 extraHeaders 复制出的同名头重复。
    let zcode_auth_scheme = provider
        .get("authScheme")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_");
    let injects_zcode = matches!(
        zcode_auth_scheme.as_str(),
        "zai_oauth" | "zai" | "bigmodel_oauth" | "bigmodel"
    ) || base_url.contains("coding/paas/v4");
    if injects_zcode {
        for (name, value) in codex_app_transfer_gemini_oauth::zai::constants::zcode_source_headers()
        {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                headers.insert(n, v);
            }
        }
    }

    headers
}

pub(super) fn provider_test_error_label(error: &reqwest::Error) -> &'static str {
    // M2 (silent-failure-hunter review):TLS / decode 错单独区分,用户 toast 看
    // 到 label 就能 self-debug(原来都吃成 "RequestError" 看不出哪步出错)。
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "ConnectError"
    } else if error.is_redirect() {
        "RedirectError"
    } else if error.is_decode() {
        "DecodeError"
    } else if error.is_request() {
        "RequestError"
    } else if error.is_body() {
        "BodyError"
    } else {
        "OtherError"
    }
}

async fn test_provider_connection(provider: &Value) -> Value {
    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    let base_url = build_provider_test_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        api_format,
    );
    let parsed = reqwest::Url::parse(&base_url);
    let valid_url = parsed
        .as_ref()
        .map(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .unwrap_or(false);
    if !valid_url {
        return json!({
            "message": "API 地址无效",
            "success": false,
        });
    }

    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "success": true,
                "ok": false,
                "latencyMs": started.elapsed().as_millis(),
                "message": format!("connection failed: {}", provider_test_error_label(&error)),
            });
        }
    };

    let probe_headers = provider_test_headers(provider, false);
    let content_headers = provider_test_headers(provider, true);
    let mut response = match client
        .head(&base_url)
        .headers(probe_headers.clone())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "success": true,
                "ok": false,
                "latencyMs": started.elapsed().as_millis(),
                "message": format!("connection failed: {}", provider_test_error_label(&error)),
            });
        }
    };

    if matches!(
        response.status(),
        ReqwestStatusCode::NOT_FOUND | ReqwestStatusCode::METHOD_NOT_ALLOWED
    ) {
        response = match client.get(&base_url).headers(probe_headers).send().await {
            Ok(response) => response,
            Err(error) => {
                return json!({
                    "success": true,
                    "ok": false,
                    "latencyMs": started.elapsed().as_millis(),
                    "message": format!("connection failed: {}", provider_test_error_label(&error)),
                });
            }
        };
    }

    if matches!(
        response.status(),
        ReqwestStatusCode::NOT_FOUND | ReqwestStatusCode::METHOD_NOT_ALLOWED
    ) {
        // ╔═══════════════════════════════════════════════════════════════════════════╗
        // ║ ⚠️ 防回归 (2026-05-10):POST fallback **绝对不能** 加                       ║
        // ║       `&& !provider_api_key(provider).is_empty()` 限制                    ║
        // ╠═══════════════════════════════════════════════════════════════════════════╣
        // ║ 部分 LLM endpoint **不实现 HEAD/GET**,只接受 POST chat completions:      ║
        // ║   • Google AI Studio Gemini OpenAI 兼容层 HEAD → 404,POST 不带 key       ║
        // ║     → 400 "Missing or invalid Authorization header"(实证 2026-05-10)    ║
        // ║   • Kimi `/v1/chat/completions` HEAD → 404                                ║
        // ║                                                                           ║
        // ║ 如果只在 `provider_api_key` 非空时 fallback 到 POST,用户测速时没填 key   ║
        // ║ (测连接性本来不需要 key)→ 永远卡 HEAD 404 → UI 红色 "endpoint            ║
        // ║ unavailable" → 误以为 baseUrl 错。实际 baseUrl 完全 work。               ║
        // ║                                                                           ║
        // ║ **正确语义**:测速测"endpoint 是否存在 + 是否响应",鉴权层(401/403/      ║
        // ║ 400 Missing Authorization)v2.1.3 已让绿色 + 文案 "auth not verified"。   ║
        // ║ POST 不带 key 返 4xx-non-404 = endpoint exists ✅。                       ║
        // ║                                                                           ║
        // ║ 看到这条注释又想加回 `is_empty()` 限制 —— **不要改**。回归测试            ║
        // ║ `head_404_post_400_treats_as_reachable_no_key_required` 就是 防 你 改回去。║
        // ╚═══════════════════════════════════════════════════════════════════════════╝
        response = match client
            .post(&base_url)
            .headers(content_headers)
            .json(&provider_test_body(provider, api_format))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return json!({
                    "success": true,
                    "ok": false,
                    "latencyMs": started.elapsed().as_millis(),
                    "message": format!("connection failed: {}", provider_test_error_label(&error)),
                });
            }
        };
    }

    let latency_ms = started.elapsed().as_millis();
    let status_code = response.status().as_u16();
    let mut reachable = status_code < 500;
    // 401/403 = endpoint 已响应 + 需鉴权(server 认得请求)= **baseUrl 连接性 OK**。
    // 但鉴权层语义跟连接层语义解耦,**测速绿色 + 文案明示鉴权未验证** 比黄色更准确:
    // 黄色容易让用户误以为 baseUrl 错(2026-05-10 用户实测痛点 — proxy.mochance.xyz/v1
    // 实际可达但显示橙色 "auth required or invalid" 看起来像配错)。改回绿色 + 文案
    // "connection OK; API key not configured or auth not verified" 明示连接成功 +
    // 鉴权状态 — 保留 authStatus 字段(信息完整,留给未来 UI 决策),但 frontend
    // helper 不再依据它标黄。
    let auth_status = if matches!(status_code, 401 | 403) {
        "auth_required_or_invalid"
    } else {
        "ok"
    };
    let message = if (200..300).contains(&status_code) {
        format!("connection OK, {latency_ms} ms")
    } else if matches!(status_code, 401 | 403) {
        format!("connection OK; API key not configured or auth not verified, {latency_ms} ms")
    } else if matches!(status_code, 404 | 405) {
        reachable = false;
        format!("endpoint unavailable, HTTP {status_code}. Verify the base URL points to a Codex-compatible endpoint. ({latency_ms} ms)")
    } else {
        format!("reachable, HTTP {status_code} ({latency_ms} ms)")
    };

    json!({
        "success": true,
        "ok": reachable,
        "authStatus": auth_status,
        "latencyMs": latency_ms,
        "statusCode": status_code,
        "message": message,
    })
}

pub async fn test_provider_payload(Json(payload): Json<Value>) -> impl IntoResponse {
    Json(test_provider_connection(&payload).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_test_url_matches_legacy_chat_rules() {
        assert_eq!(
            build_provider_test_url("https://api.example.com/v1", "openai_chat"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            build_provider_test_url("https://api.example.com/v1/chat/completions", "openai_chat"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn provider_test_url_matches_legacy_responses_rules() {
        assert_eq!(
            build_provider_test_url("https://api.example.com/v1", "responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            build_provider_test_url("https://api.example.com", "responses"),
            "https://api.example.com/v1/responses"
        );
    }

    #[test]
    fn provider_test_url_anthropic_messages_uses_messages_endpoint() {
        assert_eq!(
            build_provider_test_url("https://api.anthropic.com/v1", "anthropic_messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            build_provider_test_url("https://api.anthropic.com", "anthropic_messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            build_provider_test_url(
                "https://proxy.example/anthropic/v1/messages",
                "anthropic_messages"
            ),
            "https://proxy.example/anthropic/v1/messages"
        );
    }

    #[test]
    fn provider_test_url_gemini_native_uses_models_endpoint() {
        // Gemini native:用 /v1beta/models 探测(不带 key 走 401 / "auth not verified",
        // 带 key 200)。base_url 不带版本号 → 自动补 /v1beta/models。
        assert_eq!(
            build_provider_test_url("https://generativelanguage.googleapis.com", "gemini_native"),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        // 用户在 base_url 已指定 /v1beta → 只补 /models
        assert_eq!(
            build_provider_test_url(
                "https://generativelanguage.googleapis.com/v1beta",
                "gemini_native"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        // Gemini 3+ v1alpha 也支持
        assert_eq!(
            build_provider_test_url(
                "https://generativelanguage.googleapis.com/v1alpha",
                "gemini_native"
            ),
            "https://generativelanguage.googleapis.com/v1alpha/models"
        );
        // 完整 URL 已带 /v1beta/models → 不重复加
        assert_eq!(
            build_provider_test_url(
                "https://generativelanguage.googleapis.com/v1beta/models",
                "gemini_native"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
    }

    #[test]
    fn provider_test_headers_anthropic_messages_includes_version_and_allows_override() {
        let provider = json!({
            "apiFormat": "anthropic_messages",
            "apiKey": "sk-ant-test",
            "extraHeaders": {"anthropic-version": "2024-01-01"},
        });
        let headers = provider_test_headers(&provider, true);
        assert_eq!(
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some("2024-01-01"),
            "用户 extraHeaders 必须能覆盖 Anthropic 默认版本头"
        );
        assert_eq!(
            headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn provider_test_headers_google_api_key_uses_x_goog_header() {
        let provider = json!({
            "apiKey": "AQ.Ab8RN6Jg_secret_key",
            "authScheme": "google_api_key",
        });
        let headers = provider_test_headers(&provider, false);
        assert_eq!(
            headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()),
            Some("AQ.Ab8RN6Jg_secret_key"),
            "google_api_key authScheme 必须用 x-goog-api-key header,不是 Bearer"
        );
        assert!(
            headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "Gemini 不能用 Authorization: Bearer(那是 OpenAI 兼容路径,native 走 x-goog-api-key)"
        );
    }

    #[test]
    fn provider_test_headers_glm_coding_injects_zcode_fingerprint() {
        // zhipu-coding(Bearer + coding/paas/v4):测试探测必须带 ZCode 指纹头,
        // 与 forward.rs chat 路径一致(否则 Test provider 看到普通 API-key 调用)。
        let provider = json!({
            "apiFormat": "openai_chat",
            "authScheme": "bearer",
            "apiKey": "glm-test-key",
            "baseUrl": "https://open.bigmodel.cn/api/coding/paas/v4",
            "extraHeaders": {},
        });
        let headers = provider_test_headers(&provider, true);
        assert_eq!(
            headers.get("user-agent").and_then(|v| v.to_str().ok()),
            Some("ZCode/3.1.0"),
            "GLM Coding 测试探测 UA 必须是 ZCode,不是 reqwest 默认 UA"
        );
        assert!(
            headers.get("x-platform").is_some(),
            "ZCode 指纹必含运行时 X-Platform"
        );
        assert_eq!(
            headers.get("http-referer").and_then(|v| v.to_str().ok()),
            Some("https://zcode.z.ai")
        );
        assert!(headers.get("x-zcode-app-version").is_some());
        assert_eq!(
            headers.get("x-title").and_then(|v| v.to_str().ok()),
            Some("Z Code@electron")
        );
    }

    #[test]
    fn provider_test_headers_plain_bearer_no_zcode_fingerprint() {
        // 普通 Bearer provider(非 coding 端点,如开放平台 zhipu /api/paas/v4)
        // 不应注入 ZCode 指纹头 —— 避免误命中。
        let provider = json!({
            "apiFormat": "openai_chat",
            "authScheme": "bearer",
            "apiKey": "plain-key",
            "baseUrl": "https://open.bigmodel.cn/api/paas/v4",
            "extraHeaders": {},
        });
        let headers = provider_test_headers(&provider, true);
        assert!(
            headers.get("x-platform").is_none(),
            "非 GLM Coding 路径不该带 ZCode X-Platform"
        );
        assert!(
            headers.get("http-referer").is_none(),
            "非 GLM Coding 路径不该带 ZCode HTTP-Referer"
        );
    }

    #[test]
    fn provider_test_model_prefers_real_provider_mapping() {
        let provider = json!({
            "models": {
                "default": "kimi-k2.6[1m]",
                "gpt_5_5": "gpt-side-name"
            }
        });

        assert_eq!(provider_test_model(&provider), "kimi-k2.6");
    }

    #[test]
    fn provider_connection_posts_legacy_minimal_ping_after_probe_fallback() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::post, Router};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/chat/completions",
                post(Json(json!({"id": "ok", "choices": []}))),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let provider = json!({
                "name": "Mock OpenAI Chat",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "openai_chat",
                "apiKey": "test-key",
                "models": {"default": "deepseek-chat"}
            });
            let result = test_provider_connection(&provider).await;
            server.abort();

            assert_eq!(result.get("success").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(result.get("statusCode").and_then(|v| v.as_u64()), Some(200));
        });
    }

    #[test]
    fn provider_connection_posts_anthropic_messages_ping_with_version_header() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{
                body::Bytes,
                http::{HeaderMap as AxumHeaderMap, Method, StatusCode as AxumStatusCode},
                routing::any,
                Router,
            };
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/messages",
                any(
                    |method: Method, headers: AxumHeaderMap, body: Bytes| async move {
                        if method != Method::POST {
                            return (
                                AxumStatusCode::METHOD_NOT_ALLOWED,
                                Json(json!({"error": "POST only"})),
                            );
                        }
                        let parsed: Value =
                            serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                        let has_version = headers
                            .get("anthropic-version")
                            .and_then(|v| v.to_str().ok())
                            == Some("2023-06-01");
                        let has_messages = parsed
                            .get("messages")
                            .and_then(|v| v.as_array())
                            .map(|items| {
                                items.len() == 1
                                    && items[0] == json!({"role": "user", "content": "ping"})
                            })
                            .unwrap_or(false);
                        if has_version && has_messages && parsed.get("input").is_none() {
                            (
                                AxumStatusCode::OK,
                                Json(json!({"id": "msg_ok", "content": []})),
                            )
                        } else {
                            (
                                AxumStatusCode::BAD_REQUEST,
                                Json(json!({"error": "bad anthropic ping"})),
                            )
                        }
                    },
                ),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let provider = json!({
                "name": "Mock Claude",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "anthropic_messages",
                "authScheme": "none",
                "models": {"default": "claude-sonnet-4-6"}
            });
            let result = test_provider_connection(&provider).await;
            server.abort();

            assert_eq!(result.get("success").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(result.get("statusCode").and_then(|v| v.as_u64()), Some(200));
        });
    }

    #[test]
    fn provider_connection_distinguishes_invalid_url_and_bad_key() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let invalid = json!({
                "baseUrl": "not a url",
                "apiFormat": "responses",
            });
            let result = test_provider_connection(&invalid).await;
            assert_eq!(result["success"], json!(false));
            assert_eq!(result["message"], json!("API 地址无效"));

            use axum::{
                http::{HeaderMap as AxumHeaderMap, StatusCode as AxumStatusCode},
                routing::post,
                Router,
            };
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/chat/completions",
                post(|headers: AxumHeaderMap| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer good-key")
                    {
                        (AxumStatusCode::OK, Json(json!({"id": "ok", "choices": []})))
                    } else {
                        (
                            AxumStatusCode::UNAUTHORIZED,
                            Json(json!({"error": "bad key"})),
                        )
                    }
                }),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let bad_key = json!({
                "name": "Mock Provider",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "openai_chat",
                "apiKey": "bad-key",
                "models": {"default": "deepseek-chat"}
            });
            let result = test_provider_connection(&bad_key).await;
            server.abort();

            assert_eq!(result["success"], json!(true));
            // 401 = endpoint 已响应 + 需鉴权 → baseUrl 连接性 OK,绿色显示
            // (2026-05-10 反转:之前文案 "auth required or invalid" 标黄误导
            //  用户以为 baseUrl 错;改回绿色 + 文案明示连接成功+鉴权未验证)
            assert_eq!(result["ok"], json!(true));
            assert_eq!(result["authStatus"], json!("auth_required_or_invalid"));
            assert_eq!(result["statusCode"], json!(401));
            assert!(result["message"]
                .as_str()
                .unwrap_or("")
                .contains("connection OK"));
            assert!(result["message"]
                .as_str()
                .unwrap_or("")
                .contains("API key not configured or auth not verified"));
        });
    }

    #[test]
    fn provider_connection_403_marks_auth_required() {
        // 防回归:403 跟 401 同样视为 reachable + auth_required_or_invalid
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use axum::http::StatusCode as AxumStatusCode;
            use axum::routing::post;
            use axum::Json;
            use axum::Router;
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        AxumStatusCode::FORBIDDEN,
                        Json(json!({"error": "WAF blocked"})),
                    )
                }),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let provider = json!({
                "name": "Mock 403",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "openai_chat",
                "apiKey": "any-key",
                "models": {"default": "x"}
            });
            let result = test_provider_connection(&provider).await;
            server.abort();
            assert_eq!(result["ok"], json!(true), "403 仍 reachable");
            assert_eq!(result["authStatus"], json!("auth_required_or_invalid"));
            assert_eq!(result["statusCode"], json!(403));
            // 防文案回归 — 403 跟 401 共用 match arm,message 必须含
            // "connection OK" + "API key not configured or auth not verified"
            assert!(result["message"]
                .as_str()
                .unwrap_or("")
                .contains("connection OK"));
            assert!(result["message"]
                .as_str()
                .unwrap_or("")
                .contains("API key not configured or auth not verified"));
        });
    }

    #[test]
    fn head_404_post_400_treats_as_reachable_no_key_required() {
        // 关键防回归(2026-05-10):部分 LLM endpoint(如 Google AI Studio Gemini
        // OpenAI 兼容层)HEAD/GET 不实现返 404,POST 不带 key 返 400/401 表明
        // endpoint 存在。此测试 mock 这个场景:
        //   • HEAD /v1/chat/completions → 404
        //   • POST /v1/chat/completions → 400 "Missing or invalid Authorization header"
        // form 没填 apiKey 时也必须 fallback POST(过去一旦加回 `&& !key.is_empty()`
        // 限制就会失效,死在 HEAD 404 红色 endpoint unavailable)。
        //
        // 对应代码:`test.rs::test_provider_connection` 的 POST fallback 必须
        // 不依赖 key 非空。**如果你看到这条测试又想"优化"那条 if 加 key 限制,
        // 这里就是兜底。**
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use axum::http::{Method, StatusCode as AxumStatusCode};
            use axum::routing::any;
            use axum::Json;
            use axum::Router;
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            // 单一 any handler 按 method 分流(POST 返 400 模拟 Gemini Missing
            // Authorization,HEAD/GET 返 404 模拟 endpoint 不实现 HEAD)
            let app = Router::new().route(
                "/v1/chat/completions",
                any(|method: Method| async move {
                    if method == Method::POST {
                        (
                            AxumStatusCode::BAD_REQUEST,
                            Json(json!({"error": {"code": 400, "message": "Missing or invalid Authorization header.", "status": "INVALID_ARGUMENT"}})),
                        )
                    } else {
                        (AxumStatusCode::NOT_FOUND, Json(json!({})))
                    }
                }),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let provider_no_key = json!({
                "name": "Gemini-like (HEAD 404, POST 400 no key)",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "openai_chat",
                "apiKey": "",  // ← 关键:用户没填 key,旧逻辑会卡 HEAD 404 死
                "models": {"default": "x"}
            });
            let result = test_provider_connection(&provider_no_key).await;
            server.abort();
            // POST 必须被 fallback 到(不再因 key 空跳过)
            // POST 返 400 → 走 else 分支 reachable, HTTP 400(reachable=true → ok=true)
            assert_eq!(result["success"], json!(true));
            assert_eq!(
                result["statusCode"], json!(400),
                "POST fallback 必须发生(不能因 key 空就跳过 POST 死在 HEAD 404)"
            );
            assert_eq!(
                result["ok"], json!(true),
                "400 < 500 → reachable=true → ok=true,UI 不标 bad → 绿色"
            );
            let msg = result["message"].as_str().unwrap_or("");
            assert!(
                !msg.contains("endpoint unavailable"),
                "绝不能显示 endpoint unavailable(那是 baseUrl 错的红色文案)。实际:{msg}"
            );
        });
    }
}
