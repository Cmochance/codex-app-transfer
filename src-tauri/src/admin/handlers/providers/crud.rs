//! `/api/providers/*` CRUD handler —— 增删改 / activate / reorder /
//! draft / update_models.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::super::registry_io::{
    load as load_registry, public_provider, save as save_registry,
};
use super::super::super::state::AdminState;
use super::super::common::err;
use super::super::desktop::switch_provider_and_sync;
use super::{fresh_provider_id, provider_index};

pub async fn list_providers() -> impl IntoResponse {
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
        .map(public_provider)
        .collect();
    let active_id = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    Json(json!({
        "providers": providers,
        "activeId": active_id,
    }))
    .into_response()
}

pub async fn get_secret(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let provider = providers.iter().find(|p| {
        p.as_object()
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            == Some(id.as_str())
    });
    match provider {
        Some(p) => Json(json!({
            "apiKey": p.get("apiKey").and_then(|v| v.as_str()).unwrap_or(""),
            "extraHeaders": p.get("extraHeaders").cloned().unwrap_or_else(|| json!({})),
        }))
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "提供商不存在").into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddProviderInput {
    pub name: Option<String>,
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "authScheme")]
    pub auth_scheme: Option<String>,
    #[serde(rename = "apiFormat")]
    pub api_format: Option<String>,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    pub models: Option<Value>,
    #[serde(rename = "extraHeaders")]
    pub extra_headers: Option<Value>,
    #[serde(rename = "modelCapabilities")]
    pub model_capabilities: Option<Value>,
    #[serde(rename = "requestOptions")]
    pub request_options: Option<Value>,
}

pub async fn add_provider(Json(input): Json<AddProviderInput>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let existing_ids: Vec<String> = providers
        .iter()
        .filter_map(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
        })
        .collect();
    let new_id = fresh_provider_id(&existing_ids);

    let mut new_provider = serde_json::Map::new();
    new_provider.insert("id".into(), Value::String(new_id.clone()));
    new_provider.insert(
        "name".into(),
        Value::String(input.name.unwrap_or_else(|| "Unnamed Provider".into())),
    );
    new_provider.insert(
        "baseUrl".into(),
        Value::String(input.base_url.unwrap_or_default()),
    );
    new_provider.insert(
        "authScheme".into(),
        Value::String(input.auth_scheme.unwrap_or_else(|| "bearer".into())),
    );
    new_provider.insert(
        "apiFormat".into(),
        Value::String(
            input
                .api_format
                .filter(|s| matches!(s.as_str(), "openai_chat" | "responses"))
                .unwrap_or_else(|| "responses".into()),
        ),
    );
    new_provider.insert(
        "apiKey".into(),
        Value::String(input.api_key.unwrap_or_default()),
    );
    new_provider.insert(
        "models".into(),
        input.models.unwrap_or_else(|| {
            json!({"default":"","gpt_5_5":"","gpt_5_4":"","gpt_5_4_mini":"","gpt_5_3_codex":"","gpt_5_2":""})
        }),
    );
    new_provider.insert(
        "extraHeaders".into(),
        input.extra_headers.unwrap_or_else(|| json!({})),
    );
    new_provider.insert(
        "modelCapabilities".into(),
        input.model_capabilities.unwrap_or_else(|| json!({})),
    );
    new_provider.insert(
        "requestOptions".into(),
        input.request_options.unwrap_or_else(|| json!({})),
    );
    new_provider.insert("isBuiltin".into(), Value::Bool(false));
    new_provider.insert("sortIndex".into(), Value::Number(providers.len().into()));

    let new_provider_value = Value::Object(new_provider);
    let mut new_providers = providers;
    new_providers.push(new_provider_value.clone());
    let was_empty = new_providers.len() == 1;

    let obj = cfg.as_object_mut().unwrap();
    obj.insert("providers".into(), Value::Array(new_providers));
    if was_empty {
        obj.insert("activeProvider".into(), Value::String(new_id));
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true, "provider": public_provider(&new_provider_value)})).into_response()
}

pub async fn update_provider(
    Path(id): Path<String>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(idx) = provider_index(&cfg, &id) else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let providers = cfg
        .get_mut("providers")
        .and_then(|v| v.as_array_mut())
        .expect("providers array");
    let existing = providers[idx].as_object().unwrap().clone();
    let mut updated = existing.clone();
    let is_builtin = existing
        .get("isBuiltin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(name) = input.name {
        updated.insert("name".into(), Value::String(name));
    }
    if !is_builtin {
        if let Some(base_url) = input.base_url {
            updated.insert("baseUrl".into(), Value::String(base_url));
        }
    }
    if let Some(auth_scheme) = input.auth_scheme {
        updated.insert("authScheme".into(), Value::String(auth_scheme));
    }
    if let Some(api_format) = input.api_format {
        let normalized = if matches!(api_format.as_str(), "openai_chat" | "responses") {
            api_format
        } else {
            "responses".to_owned()
        };
        updated.insert("apiFormat".into(), Value::String(normalized));
    }
    // apiKey 留空表示"不修改"
    if let Some(key) = input.api_key.filter(|s| !s.is_empty()) {
        updated.insert("apiKey".into(), Value::String(key));
    }
    if let Some(headers) = input.extra_headers {
        if !headers.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            updated.insert("extraHeaders".into(), headers);
        }
    }
    if let Some(caps) = input.model_capabilities {
        updated.insert("modelCapabilities".into(), caps);
    }
    if let Some(opts) = input.request_options {
        updated.insert("requestOptions".into(), opts);
    }
    if let Some(models) = input.models {
        if models.is_object() {
            let mut merged = existing
                .get("models")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            for (k, v) in models.as_object().unwrap() {
                merged.insert(k.clone(), v.clone());
            }
            updated.insert("models".into(), Value::Object(merged));
        }
    }
    updated.insert("id".into(), Value::String(id));
    updated.insert("isBuiltin".into(), Value::Bool(is_builtin));

    providers[idx] = Value::Object(updated.clone());
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true, "provider": public_provider(&Value::Object(updated))}))
        .into_response()
}

pub async fn delete_provider(Path(id): Path<String>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let original_len = providers.len();
    let mut remaining: Vec<Value> = providers
        .into_iter()
        .filter(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                != Some(id.as_str())
        })
        .collect();
    if remaining.len() == original_len {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    }
    for (i, p) in remaining.iter_mut().enumerate() {
        if let Some(o) = p.as_object_mut() {
            o.insert("sortIndex".into(), Value::Number(i.into()));
        }
    }
    let active = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let new_active = match active {
        Some(a) if a == id => remaining
            .first()
            .and_then(|p| p.as_object())
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.to_owned()))
            .unwrap_or(Value::Null),
        Some(a) => Value::String(a),
        None => Value::Null,
    };
    let obj = cfg.as_object_mut().unwrap();
    obj.insert("providers".into(), Value::Array(remaining));
    obj.insert("activeProvider".into(), new_active);
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true})).into_response()
}

pub async fn set_default_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = switch_provider_and_sync(state.proxy_manager.clone(), id).await;
    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        Json(result).into_response()
    } else {
        let status = if result.get("message").and_then(|v| v.as_str()) == Some("提供商不存在")
        {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        err(
            status,
            result
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("提供商不存在"),
        )
        .into_response()
    }
}

pub async fn activate_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    set_default_provider(State(state), Path(id)).await
}

#[derive(Debug, Deserialize)]
pub struct ReorderInput {
    #[serde(rename = "providerIds")]
    pub provider_ids: Vec<String>,
}

pub async fn reorder_providers(Json(input): Json<ReorderInput>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let by_id: std::collections::HashMap<String, Value> = providers
        .iter()
        .filter_map(|p| {
            let id = p
                .as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())?
                .to_owned();
            Some((id, p.clone()))
        })
        .collect();
    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in &input.provider_ids {
        if let Some(p) = by_id.get(id) {
            if seen.insert(id.clone()) {
                ordered.push(p.clone());
            }
        }
    }
    for p in &providers {
        if let Some(id) = p
            .as_object()
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
        {
            if seen.insert(id.to_owned()) {
                ordered.push(p.clone());
            }
        }
    }
    if ordered.len() != providers.len() {
        return err(StatusCode::BAD_REQUEST, "排序数量不一致").into_response();
    }
    for (i, p) in ordered.iter_mut().enumerate() {
        if let Some(o) = p.as_object_mut() {
            o.insert("sortIndex".into(), Value::Number(i.into()));
        }
    }
    let public_ordered: Vec<Value> = ordered.iter().map(public_provider).collect();
    let obj = cfg.as_object_mut().unwrap();
    obj.insert("providers".into(), Value::Array(ordered));
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true, "providers": public_ordered})).into_response()
}

// /api/providers/{id}/draft —— v1 当 update 用,我们直接复用
pub async fn save_draft(
    Path(id): Path<String>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    update_provider(Path(id), Json(input)).await
}

#[derive(Debug, Deserialize)]
pub struct UpdateModelsInput {
    pub models: Value,
}

pub async fn update_models(
    Path(id): Path<String>,
    Json(input): Json<UpdateModelsInput>,
) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(idx) = provider_index(&cfg, &id) else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let providers = cfg
        .get_mut("providers")
        .and_then(|v| v.as_array_mut())
        .unwrap();
    if let Some(o) = providers[idx].as_object_mut() {
        o.insert("models".into(), input.models);
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true})).into_response()
}
