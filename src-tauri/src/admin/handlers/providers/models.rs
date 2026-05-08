//! Provider 模型列表抓取 + autofill.

use std::collections::HashSet;
use std::time::Duration;

use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_registry::MODEL_ORDER;
use serde_json::{json, Value};

use super::super::super::registry_io::{load as load_registry, save as save_registry};
use super::super::common::err;
use super::test::{build_provider_test_url, provider_test_error_label, provider_test_headers};
use super::{clean_base_url, normalize_provider_api_format, provider_index, replace_path_suffix};

fn model_endpoint_candidates(provider: &Value) -> Vec<String> {
    let base_url = clean_base_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    if base_url.is_empty() {
        return Vec::new();
    }

    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    let upstream = build_provider_test_url(&base_url, api_format);
    let mut candidates = Vec::new();

    if api_format == "openai_chat" {
        candidates.push(replace_path_suffix(
            &upstream,
            &["/chat/completions", "/completions"],
            "/models",
        ));
        candidates.push(format!("{base_url}/models"));
    } else {
        candidates.push(replace_path_suffix(
            &upstream,
            &["/v1/responses", "/responses"],
            "/v1/models",
        ));
        if base_url.to_ascii_lowercase().ends_with("/v1") {
            candidates.push(format!("{base_url}/models"));
        }
        candidates.push(format!("{base_url}/models"));
        if let Ok(parsed) = reqwest::Url::parse(&base_url) {
            let stripped_path = parsed.path().trim_end_matches('/');
            let lower = stripped_path.to_ascii_lowercase();
            if lower.ends_with("/anthropic") || lower.ends_with("/v1") {
                let root_path = if lower.ends_with("/anthropic") {
                    &stripped_path[..stripped_path.len().saturating_sub("/anthropic".len())]
                } else {
                    &stripped_path[..stripped_path.len().saturating_sub("/v1".len())]
                };
                let mut root = parsed.clone();
                root.set_path(root_path.trim_end_matches('/'));
                root.set_query(None);
                root.set_fragment(None);
                let root_url = root.to_string().trim_end_matches('/').to_owned();
                candidates.push(format!("{root_url}/models"));
                candidates.push(format!("{root_url}/v1/models"));
            }
        }
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|item| !item.is_empty() && seen.insert(item.clone()))
        .collect()
}

fn model_id_from_item(item: &Value) -> Option<String> {
    if let Some(s) = item.as_str() {
        return Some(s.to_owned());
    }
    let obj = item.as_object()?;
    for key in ["id", "name", "model", "model_id"] {
        if let Some(value) = obj.get(key).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn extract_model_ids(payload: &Value) -> Vec<String> {
    let mut candidates: Vec<Value> = Vec::new();
    if let Some(items) = payload.as_array() {
        candidates = items.clone();
    } else if let Some(obj) = payload.as_object() {
        for key in ["data", "models", "items", "result"] {
            if let Some(items) = obj.get(key).and_then(|v| v.as_array()) {
                candidates = items.clone();
                break;
            }
        }
        if candidates.is_empty() {
            if let Some(data) = obj.get("data").and_then(|v| v.as_object()) {
                for key in ["models", "items"] {
                    if let Some(items) = data.get(key).and_then(|v| v.as_array()) {
                        candidates = items.clone();
                        break;
                    }
                }
            }
        }
    }

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for item in candidates {
        let Some(model_id) = model_id_from_item(&item) else {
            continue;
        };
        if seen.insert(model_id.clone()) {
            ids.push(model_id);
        }
    }
    ids
}

fn usable_model_ids(model_ids: &[String]) -> Vec<String> {
    const EXCLUDE: &[&str] = &[
        "embedding",
        "rerank",
        "moderation",
        "whisper",
        "tts",
        "image",
        "vision",
        "audio",
    ];
    let usable: Vec<String> = model_ids
        .iter()
        .filter(|model_id| {
            let lower = model_id.to_ascii_lowercase();
            !EXCLUDE.iter().any(|keyword| lower.contains(keyword))
        })
        .cloned()
        .collect();
    if usable.is_empty() {
        model_ids.to_vec()
    } else {
        usable
    }
}

fn pick_model(model_ids: &[String], keywords: &[&str], fallback_index: usize) -> String {
    for keyword in keywords {
        for model_id in model_ids {
            if model_id.to_ascii_lowercase().contains(keyword) {
                return model_id.clone();
            }
        }
    }
    if model_ids.is_empty() {
        String::new()
    } else {
        model_ids[std::cmp::min(fallback_index, model_ids.len() - 1)].clone()
    }
}

fn empty_model_mappings_value() -> Value {
    let mut out = serde_json::Map::new();
    for slot in MODEL_ORDER.iter().copied() {
        out.insert(slot.to_owned(), Value::String(String::new()));
    }
    Value::Object(out)
}

fn suggest_model_mappings(model_ids: &[String]) -> Value {
    let usable = usable_model_ids(model_ids);
    let mut result = empty_model_mappings_value();
    if usable.is_empty() {
        return result;
    }
    let chosen = pick_model(
        &usable,
        &["pro", "plus", "coder", "max", "reasoner", "v4"],
        0,
    );
    if let Some(obj) = result.as_object_mut() {
        obj.insert("default".to_owned(), Value::String(chosen));
    }
    result
}

async fn fetch_provider_models_impl(provider: &Value) -> Value {
    let endpoints = model_endpoint_candidates(provider);
    if endpoints.is_empty() {
        return json!({"success": false, "message": "API 地址无效", "models": [], "suggested": {}});
    }

    let headers = provider_test_headers(provider, false);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "success": false,
                "message": "cannot auto-fetch model list",
                "models": [],
                "suggested": {},
                "errors": [format!("client: {}", provider_test_error_label(&error))],
            });
        }
    };

    let mut errors: Vec<String> = Vec::new();
    for endpoint in endpoints {
        let response = match client.get(&endpoint).headers(headers.clone()).send().await {
            Ok(response) => response,
            Err(error) => {
                errors.push(format!("{endpoint}: {}", provider_test_error_label(&error)));
                continue;
            }
        };
        if !response.status().is_success() {
            errors.push(format!("{endpoint}: HTTP {}", response.status().as_u16()));
            continue;
        }
        let payload = match response.json::<Value>().await {
            Ok(payload) => payload,
            Err(_) => {
                errors.push(format!("{endpoint}: 非 JSON 响应"));
                continue;
            }
        };
        let model_ids = extract_model_ids(&payload);
        if !model_ids.is_empty() {
            return json!({
                "success": true,
                "endpoint": endpoint,
                "models": model_ids,
                "suggested": suggest_model_mappings(&model_ids),
            });
        }
        errors.push(format!("{endpoint}: 未发现模型列表"));
    }

    let start = errors.len().saturating_sub(5);
    json!({
        "success": false,
        "message": "cannot auto-fetch model list",
        "models": [],
        "suggested": {},
        "errors": errors[start..].to_vec(),
    })
}

pub async fn fetch_provider_models(Path(id): Path<String>) -> impl IntoResponse {
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
    let result = fetch_provider_models_impl(provider).await;
    let status = if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(result)).into_response()
}

pub async fn fetch_provider_models_payload(Json(payload): Json<Value>) -> impl IntoResponse {
    let result = fetch_provider_models_impl(&payload).await;
    let status = if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(result)).into_response()
}

pub async fn autofill_provider_models(Path(id): Path<String>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(idx) = provider_index(&cfg, &id) else {
        return err(StatusCode::NOT_FOUND, "provider not found").into_response();
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| providers.get(idx))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = fetch_provider_models_impl(&provider).await;
    if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return (StatusCode::BAD_REQUEST, Json(result)).into_response();
    }
    let suggested = result
        .get("suggested")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(providers) = cfg.get_mut("providers").and_then(|v| v.as_array_mut()) {
        if let Some(provider) = providers.get_mut(idx).and_then(|v| v.as_object_mut()) {
            provider.insert("models".into(), suggested.clone());
        }
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({
        "success": true,
        "models": result.get("models").cloned().unwrap_or_else(|| json!([])),
        "suggested": suggested,
        "endpoint": result.get("endpoint").cloned().unwrap_or(Value::Null),
        "message": "model mappings auto-filled",
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_provider_models_reads_openai_compatible_models() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/models",
                get(|| async {
                    Json(json!({
                        "data": [
                            {"id": "text-embedding-3-small"},
                            {"id": "deepseek-v4-pro"},
                            {"id": "deepseek-chat"}
                        ]
                    }))
                }),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let provider = json!({
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "responses",
                "authScheme": "none"
            });
            let result = fetch_provider_models_impl(&provider).await;
            server.abort();

            assert_eq!(result.get("success").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(
                result.get("endpoint").and_then(|v| v.as_str()),
                Some(format!("http://{addr}/v1/models").as_str())
            );
            assert_eq!(
                result.get("models").and_then(|v| v.as_array()).cloned(),
                Some(vec![
                    json!("text-embedding-3-small"),
                    json!("deepseek-v4-pro"),
                    json!("deepseek-chat"),
                ])
            );
            assert_eq!(result["suggested"]["default"], json!("deepseek-v4-pro"));
        });
    }
}
