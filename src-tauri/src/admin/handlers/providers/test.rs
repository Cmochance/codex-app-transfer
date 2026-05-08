//! Provider 连通性测试 + compatibility 矩阵.

use std::time::{Duration, Instant};

use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    StatusCode as ReqwestStatusCode,
};
use serde_json::{json, Value};

use super::super::super::registry_io::load as load_registry;
use super::super::common::err;
use super::{normalize_provider_api_format, provider_api_key, provider_test_model};

pub(super) fn build_provider_test_url(base_url: &str, api_format: &str) -> String {
    let clean = base_url.trim().trim_end_matches('/');
    let lower = clean.to_ascii_lowercase();
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
    if api_format == "openai_chat" {
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

fn is_kimi_provider(provider: &Value) -> bool {
    let probe = format!(
        "{} {}",
        provider.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .to_ascii_lowercase();
    probe.contains("kimi") || probe.contains("moonshot")
}

pub(super) fn provider_test_headers(provider: &Value, include_content_type: bool) -> HeaderMap {
    let api_key = provider_api_key(provider);
    let mut headers = HeaderMap::new();
    if include_content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

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

    headers
}

pub(super) fn provider_test_error_label(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "ConnectError"
    } else {
        "RequestError"
    }
}

fn provider_compatibility_item(provider: &Value) -> Value {
    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    let id = provider.get("id").cloned().unwrap_or(Value::Null);
    let name = provider.get("name").cloned().unwrap_or(Value::Null);
    if api_format == "responses" {
        return json!({
            "id": id,
            "name": name,
            "apiFormat": api_format,
            "level": "stable",
            "message": "Responses 兼容接口，适合 Codex App 主流程。",
            "checks": {
                "models": true,
                "text": true,
                "stream": true,
                "tools": true,
                "streamingTools": true,
            },
        });
    }
    if api_format == "openai_chat" {
        return json!({
            "id": id,
            "name": name,
            "apiFormat": api_format,
            "level": "experimental",
            "message": "OpenAI Chat 实验适配：文本和非流式工具调用可测试，流式工具调用暂不作为稳定能力。",
            "checks": {
                "models": true,
                "text": true,
                "stream": true,
                "tools": true,
                "streamingTools": false,
            },
        });
    }
    json!({
        "id": id,
        "name": name,
        "apiFormat": api_format,
        "level": "unsupported",
        "message": format!("{api_format} 暂未适配。"),
        "checks": {
            "models": false,
            "text": false,
            "stream": false,
            "tools": false,
            "streamingTools": false,
        },
    })
}

async fn test_provider_connection(provider: &Value) -> Value {
    let api_format = normalize_provider_api_format(
        provider
            .get("apiFormat")
            .and_then(|v| v.as_str())
            .or(Some("responses")),
    );
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
    ) && !provider_api_key(provider).is_empty()
    {
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
    let message = if (200..300).contains(&status_code) {
        format!("connection OK, {latency_ms} ms")
    } else if matches!(status_code, 401 | 403) {
        reachable = false;
        if is_kimi_provider(provider) {
            format!(
                "Kimi auth failed, HTTP {status_code}. Use https://api.moonshot.cn/v1 for Kimi Platform key, or https://api.kimi.com/coding for Kimi Code subscription key. ({latency_ms} ms)"
            )
        } else {
            format!(
                "auth failed, HTTP {status_code}. Check that the API key and base URL match. ({latency_ms} ms)"
            )
        }
    } else if matches!(status_code, 404 | 405) {
        reachable = false;
        format!("endpoint unavailable, HTTP {status_code}. Verify the base URL points to a Codex-compatible endpoint. ({latency_ms} ms)")
    } else {
        format!("reachable, HTTP {status_code} ({latency_ms} ms)")
    };

    json!({
        "success": true,
        "ok": reachable,
        "latencyMs": latency_ms,
        "statusCode": status_code,
        "message": message,
    })
}

pub async fn test_provider(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| {
            providers.iter().find(|provider| {
                provider
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str())
                    == Some(id.as_str())
            })
        });
    let Some(provider) = provider else {
        return err(StatusCode::NOT_FOUND, "provider not found").into_response();
    };
    Json(test_provider_connection(provider).await).into_response()
}

pub async fn provider_compatibility() -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers: Vec<Value> = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(provider_compatibility_item)
        .collect();
    let experimental_count = providers
        .iter()
        .filter(|item| item.get("level").and_then(|v| v.as_str()) == Some("experimental"))
        .count();
    Json(json!({
        "success": true,
        "providers": providers,
        "experimentalCount": experimental_count,
    }))
    .into_response()
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
            assert_eq!(result["ok"], json!(false));
            assert_eq!(result["statusCode"], json!(401));
            assert!(result["message"]
                .as_str()
                .unwrap_or("")
                .contains("auth failed"));
        });
    }

    #[test]
    fn provider_compatibility_matches_legacy_matrix() {
        let responses = provider_compatibility_item(&json!({
            "id": "responses",
            "name": "Responses",
            "apiFormat": "responses",
        }));
        assert_eq!(responses["level"], json!("stable"));
        assert_eq!(responses["checks"]["streamingTools"], json!(true));

        let openai_chat = provider_compatibility_item(&json!({
            "id": "chat",
            "name": "OpenAI Chat",
            "apiFormat": "openai_chat",
        }));
        assert_eq!(openai_chat["level"], json!("experimental"));
        assert_eq!(openai_chat["checks"]["models"], json!(true));
        assert_eq!(openai_chat["checks"]["streamingTools"], json!(false));

        let legacy_alias = provider_compatibility_item(&json!({
            "id": "legacy",
            "name": "Legacy",
            "apiFormat": "anthropic",
        }));
        assert_eq!(legacy_alias["apiFormat"], json!("responses"));
        assert_eq!(legacy_alias["level"], json!("stable"));
        assert_eq!(legacy_alias["checks"]["models"], json!(true));
    }
}
