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

use codex_app_transfer_proxy::validation::{validate_extra_headers, HeaderValidationError};

use super::super::super::registry_io::{
    load as load_registry, public_provider, with_config_write, ConfigMutation,
};
use super::super::super::state::AdminState;
use super::super::common::err;
use super::super::desktop::switch_provider_and_sync;
use super::{fresh_provider_id, provider_index};

/// 提交时校验 `extraHeaders` 字段的合法性,非法返回 400 + 详细错误。
/// `Value::Null` / 缺字段 / 空对象 → 视为无 extras,通过校验。
fn validate_extra_headers_input(headers: &Value) -> Result<(), Vec<HeaderValidationError>> {
    let Some(obj) = headers.as_object() else {
        return Ok(());
    };
    let errs = validate_extra_headers(
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s))),
    );
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

/// 把 HeaderValidationError 列表渲染成 user-facing 错误消息。
fn format_header_errs(errs: &[HeaderValidationError]) -> String {
    let lines: Vec<String> = errs.iter().map(|e| format!("• {e}")).collect();
    format!(
        "extraHeaders 校验失败({} 项):\n{}",
        errs.len(),
        lines.join("\n")
    )
}

/// 提交时校验 `grokWeb` 字段结构合法性(silent-failure-hunter H2 反馈)。
///
/// 不校验 cookie 值的语义(let grok.com 兜底),仅校验 JSON 结构,防止保存成功
/// 但 chat 时报 500 / "missing cookies" 让用户找不到根因。
///
/// 容忍:`Value::Null` / 缺字段 / 空对象 → 视为无 grokWeb 配置,通过校验
/// (跟现有 `validate_extra_headers_input` 同 pattern)。
fn validate_grok_web_input(gw: &Value) -> Result<(), Vec<String>> {
    if gw.is_null() {
        return Ok(());
    }
    let mut errs: Vec<String> = Vec::new();
    let Some(obj) = gw.as_object() else {
        errs.push("grokWeb 必须是 JSON object 或 null".into());
        return Err(errs);
    };
    if let Some(cookies) = obj.get("cookies") {
        match cookies {
            Value::Object(map) => {
                let sso_ok = map
                    .get("sso")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !sso_ok {
                    errs.push(
                        "grokWeb.cookies.sso 必填(JWT,非空 string);从 grok.com 浏览器 cookies 复制"
                            .into(),
                    );
                }
                for (k, v) in map {
                    if !v.is_string() {
                        errs.push(format!("grokWeb.cookies.{k} 必须是 string"));
                    }
                }
            }
            _ => errs.push("grokWeb.cookies 必须是 JSON object".into()),
        }
    } else {
        errs.push("grokWeb.cookies 必填".into());
    }
    for opt_field in ["statsigId", "userAgent"] {
        if let Some(v) = obj.get(opt_field) {
            if !v.is_string() {
                errs.push(format!("grokWeb.{opt_field} 必须是 string"));
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

fn format_grok_web_errs(errs: &[String]) -> String {
    let lines: Vec<String> = errs.iter().map(|e| format!("• {e}")).collect();
    format!("grokWeb 校验失败({} 项):\n{}", errs.len(), lines.join("\n"))
}

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
        None => err(StatusCode::NOT_FOUND, "provider not found").into_response(),
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
    #[serde(rename = "grokWeb")]
    pub grok_web: Option<Value>,
    /// [MOC-173] auto-review 审查模型槽位 key(如 `gpt_5_4`)。空字符串 = 清除(auto-review
    /// 回退复用主模型)。经 `Provider.extra` flatten 透传持久化为 `reviewModelSlot`。
    #[serde(rename = "reviewModelSlot")]
    pub review_model_slot: Option<String>,
    /// 池化模式的可选模型列表(用户「获取模型」拉取 + 手加),按 provider 隔离持久化为
    /// `pooledModels`(经 `Provider.extra` flatten)。catalog 池化(`unique_pool_slugs` /
    /// `pooled_model_ids`)优先用它,为空才回退槽位映射。`None` = 本次不改动该字段。
    #[serde(rename = "pooledModels")]
    pub pooled_models: Option<Value>,
}

pub async fn add_provider(
    State(state): State<AdminState>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    // 校验 extraHeaders 在保存前合法,避免运行时静默丢 header(实测痛点:Kimi
    // KimiCLI UA 字符串带换行 → resolver 运行时 HeaderValue::from_str 失败 →
    // WARN 后跳过 → Kimi 上游 403 但用户看不到原因)
    if let Some(headers) = input.extra_headers.as_ref() {
        if let Err(errs) = validate_extra_headers_input(headers) {
            return err(StatusCode::BAD_REQUEST, format_header_errs(&errs)).into_response();
        }
    }
    // silent-failure-hunter H2 + chatgpt-codex P2:grokWeb 结构在 save 时校验,
    // 不让 "save success → chat 时报 missing-cookies / upstream 401" 这种迷惑链
    // 发生。**add_provider 额外要求**:apiFormat=grok_web 时 grokWeb 必填(否则
    // 用户填空 cookie 提交 → frontend collectGrokWebPayload 返 null → input 不
    // 带 grokWeb → 这个 if-let-Some 不跑 → 保存成功 → chat 时再炸)。
    //
    // **2026-05-12 user E2E 反馈修**:除了 input.api_format=grok_web,也要拦
    // `baseUrl=https://grok.com` 但 apiFormat 被前端默认成 "openai_chat" 的
    // case —— healing 会在下次 load 时把 apiFormat 改成 grok_web,如果此时没
    // grokWeb cookies,provider 进入"半残"状态(apiFormat=grok_web 但缺 cookies)
    // 让 chat 失败。在 add 端就 anticipate 这个 healing 改写,提前要求 grokWeb。
    let api_format_eff = super::normalize_provider_api_format(input.api_format.as_deref());
    let base_url_norm = input
        .base_url
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .map(|s| {
            s.trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_owned()
        })
        .unwrap_or_default();
    let will_be_grok_web = api_format_eff == "grok_web" || base_url_norm == "grok.com";
    if will_be_grok_web && input.grok_web.as_ref().map(Value::is_null).unwrap_or(true) {
        return err(
            StatusCode::BAD_REQUEST,
            "apiFormat=grok_web(或 baseUrl=https://grok.com)需要 grokWeb.cookies.sso(JWT,非空 string);从 grok.com 浏览器 cookies 复制",
        )
        .into_response();
    }
    if let Some(gw) = input.grok_web.as_ref() {
        if let Err(errs) = validate_grok_web_input(gw) {
            return err(StatusCode::BAD_REQUEST, format_grok_web_errs(&errs)).into_response();
        }
    }

    let new_provider_value = match with_config_write(|cfg| {
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
            Value::String(
                input
                    .name
                    .clone()
                    .unwrap_or_else(|| "Unnamed Provider".into()),
            ),
        );
        new_provider.insert(
            "baseUrl".into(),
            Value::String(input.base_url.clone().unwrap_or_default()),
        );
        new_provider.insert(
            "authScheme".into(),
            Value::String(input.auth_scheme.clone().unwrap_or_else(|| "bearer".into())),
        );
        new_provider.insert(
            "apiFormat".into(),
            Value::String(
                super::normalize_provider_api_format(input.api_format.as_deref()).to_owned(),
            ),
        );
        new_provider.insert(
            "apiKey".into(),
            Value::String(input.api_key.clone().unwrap_or_default()),
        );
        new_provider.insert(
            "models".into(),
            input.models.clone().unwrap_or_else(|| {
                json!({"default":"","gpt_5_5":"","gpt_5_4":"","gpt_5_4_mini":"","gpt_5_3_codex":"","gpt_5_2":""})
            }),
        );
        // 池化:加 provider 时带了已抓取/手加的模型列表 → chat-only 过滤后持久化为 pooledModels。
        if let Some(pooled) = input.pooled_models.clone() {
            new_provider.insert(
                "pooledModels".into(),
                super::models::chat_filter_pooled_value(&pooled),
            );
        }
        new_provider.insert(
            "extraHeaders".into(),
            input.extra_headers.clone().unwrap_or_else(|| json!({})),
        );
        new_provider.insert(
            "modelCapabilities".into(),
            input
                .model_capabilities
                .clone()
                .unwrap_or_else(|| json!({})),
        );
        new_provider.insert(
            "requestOptions".into(),
            input.request_options.clone().unwrap_or_else(|| json!({})),
        );
        // [MOC-173] auto-review 审查模型槽位:trim 后非空才写入(空 → 不写 = 复用主模型)。
        if let Some(slot) = input
            .review_model_slot
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            new_provider.insert("reviewModelSlot".into(), Value::String(slot));
        }
        if let Some(gw) = input.grok_web.clone() {
            if !gw.is_null() {
                new_provider.insert("grokWeb".into(), gw);
            }
        }
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
        Ok(ConfigMutation::Modified(new_provider_value))
    }) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    // 池模式:新 provider 的模型要进全局池 catalog + resolver 反查表 → 立即重建。
    super::resync_pool_if_enabled(&state).await;
    Json(json!({"success": true, "provider": public_provider(&new_provider_value)})).into_response()
}

pub async fn update_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    // 同 add_provider:保存前校验 extraHeaders 合法
    if let Some(headers) = input.extra_headers.as_ref() {
        if let Err(errs) = validate_extra_headers_input(headers) {
            return err(StatusCode::BAD_REQUEST, format_header_errs(&errs)).into_response();
        }
    }
    // silent-failure-hunter H2:grokWeb 结构在 update 时也要校验,同 add_provider
    if let Some(gw) = input.grok_web.as_ref() {
        if let Err(errs) = validate_grok_web_input(gw) {
            return err(StatusCode::BAD_REQUEST, format_grok_web_errs(&errs)).into_response();
        }
    }

    let result = with_config_write(|cfg| {
        let Some(idx) = provider_index(cfg, &id) else {
            return Err("provider not found".into());
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

        if let Some(name) = input.name.clone() {
            updated.insert("name".into(), Value::String(name));
        }
        if !is_builtin {
            if let Some(base_url) = input.base_url.clone() {
                updated.insert("baseUrl".into(), Value::String(base_url));
            }
        }
        if let Some(auth_scheme) = input.auth_scheme.clone() {
            updated.insert("authScheme".into(), Value::String(auth_scheme));
        }
        if let Some(api_format) = input.api_format.clone() {
            let normalized =
                super::normalize_provider_api_format(Some(api_format.as_str())).to_owned();
            updated.insert("apiFormat".into(), Value::String(normalized));
        }
        if let Some(key) = input.api_key.clone().filter(|s| !s.is_empty()) {
            updated.insert("apiKey".into(), Value::String(key));
        }
        if let Some(headers) = input.extra_headers.clone() {
            if !headers.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                updated.insert("extraHeaders".into(), headers);
            }
        }
        if let Some(caps) = input.model_capabilities.clone() {
            updated.insert("modelCapabilities".into(), caps);
        }
        if let Some(opts) = input.request_options.clone() {
            updated.insert("requestOptions".into(), opts);
        }
        // [MOC-173] auto-review 审查模型槽位:非空 insert,空字符串 = 用户清除 → remove(复用主模型)。
        if let Some(slot) = input.review_model_slot.clone() {
            let slot = slot.trim();
            if slot.is_empty() {
                updated.remove("reviewModelSlot");
            } else {
                updated.insert("reviewModelSlot".into(), Value::String(slot.to_string()));
            }
        }
        if let Some(gw) = input.grok_web.clone() {
            if gw.is_null() {
                updated.remove("grokWeb");
            } else {
                updated.insert("grokWeb".into(), gw);
            }
        }
        if let Some(models) = input.models.clone() {
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
        // 池化 pooledModels 维护:
        // - **upstream 身份变了**(baseUrl / apiFormat 与原值不同)→ 旧池属旧端点,清空
        //   pooledModels(回退新映射,等用户对新 upstream「获取模型」重建池)。incoming 此时
        //   多半还是旧缓存,一并丢弃(bot review P2)。
        // - 同一 upstream + 带 pooledModels → **合并**进现有(chat 过滤、去重、**永不收缩**):
        //   表单 incoming 源自易失下拉缓存,fetch 失败/部分态只剩 1-5 个映射值,替换会把已持久化
        //   的完整池删剩映射;合并既保留手加/已抓取又纳入本次映射新模型(bot review P2)。
        // - 同一 upstream + 不带该字段 → 完全不动。
        // 身份 = baseUrl / apiFormat / apiKey。apiKey 变更也算(bot review P2):换 key 常意味换
        // 账号,旧账号可见的模型未必新 key 可用,stale slug 会拿新 key 路由 → invalid-model/权限
        // 错。比对 existing vs updated 的 apiKey:用户没重填 key → 前端不下发 → updated 沿用旧值
        // → 相等不触发,正常编辑不误清;真改了 key 才清。
        let identity_changed = existing.get("baseUrl") != updated.get("baseUrl")
            || existing.get("apiFormat") != updated.get("apiFormat")
            || existing.get("apiKey") != updated.get("apiKey");
        if identity_changed {
            updated.remove("pooledModels");
        } else if let Some(pooled) = input.pooled_models.clone() {
            let incoming = super::models::chat_filter_pooled_value(&pooled);
            let mut merged: Vec<Value> = existing
                .get("pooledModels")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut seen: std::collections::HashSet<String> = merged
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if let Some(arr) = incoming.as_array() {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        if seen.insert(s.to_owned()) {
                            merged.push(v.clone());
                        }
                    }
                }
            }
            updated.insert("pooledModels".into(), Value::Array(merged));
        }
        updated.insert("id".into(), Value::String(id.clone()));
        updated.insert("isBuiltin".into(), Value::Bool(is_builtin));

        let updated_value = Value::Object(updated);
        providers[idx] = updated_value.clone();
        Ok(ConfigMutation::Modified(updated_value))
    });

    let updated_value = match result {
        Ok(v) => v,
        Err(e) if e == "provider not found" => {
            return err(StatusCode::NOT_FOUND, e).into_response();
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    // 池模式:该 provider(即便非 active)的模型在全局池里 → 改动后重建 catalog + 反查表。
    super::resync_pool_if_enabled(&state).await;
    Json(json!({"success": true, "provider": public_provider(&updated_value)})).into_response()
}

pub async fn delete_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = with_config_write(|cfg| {
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
            return Err("provider not found".into());
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
        Ok(ConfigMutation::Modified(()))
    });
    match result {
        Ok(()) => {
            // 池模式:删 provider 后必须从池 catalog + resolver 反查表移除其 slug,
            // 否则 picker 仍显示、且旧反查表仍用陈旧凭据路由到已删 provider(bot review P2)。
            super::resync_pool_if_enabled(&state).await;
            Json(json!({"success": true})).into_response()
        }
        Err(e) if e == "provider not found" => err(StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct SetPoolInput {
    /// 整合开关:把该 provider 加入(true)/移出(false)整合页子集。缺省 = 不动。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 可选模型列表(整合页下方卡池增删的**权威结果**)。缺省 = 不动;
    /// `Some` 时**整列表替换**(curation 既要增也要删,非合并),经 chat 过滤去重。
    #[serde(default)]
    pub models: Option<Value>,
}

/// `PUT /api/providers/{id}/pool` —— 整合页对单个 provider 的池操作。
///
/// 两件事各自独立、缺省即不动:
/// - `enabled`:写 `pooledEnabled`(决定该 provider 是否进入 `unique_pool_slugs` 子集)。
/// - `models`:**权威替换** `pooledModels`(curation 的增删都靠它,不能 merge,
///   否则用户删不掉模型 —— 与 CRUD 的 never-shrink 合并语义相反,这里就是要精确集合)。
pub async fn set_provider_pool(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(input): Json<SetPoolInput>,
) -> impl IntoResponse {
    // `models` 是权威替换:若客户端传了非数组,coerce 成 [] 会**静默清空** pooledModels
    // (守 no-silent-destructive)→ 提前拒为 400,不进写盘闭包。缺省(None)= 不动该项。
    if let Some(models) = input.models.as_ref() {
        if !models.is_array() {
            return err(StatusCode::BAD_REQUEST, "models must be an array").into_response();
        }
    }
    let result = with_config_write(|cfg| {
        let Some(idx) = provider_index(cfg, &id) else {
            return Err("provider not found".into());
        };
        let providers = cfg
            .get_mut("providers")
            .and_then(|v| v.as_array_mut())
            .unwrap();
        // provider_index 已证 idx 在数组内;但元素非 object = config 损坏,
        // 不能静默 no-op 还回 success(否则客户端以为写成功了)。
        let Some(o) = providers[idx].as_object_mut() else {
            return Err("provider entry is not an object (corrupt config)".into());
        };
        if let Some(enabled) = input.enabled {
            o.insert("pooledEnabled".into(), Value::Bool(enabled));
        }
        if let Some(models) = input.models.as_ref() {
            o.insert(
                "pooledModels".into(),
                super::models::chat_filter_pooled_value(models),
            );
        }
        Ok(ConfigMutation::Modified(()))
    });
    match result {
        Ok(()) => {
            // 整合子集 / 池模型列表变化 → 重建池 catalog + resolver 反查表。
            super::resync_pool_if_enabled(&state).await;
            Json(json!({"success": true})).into_response()
        }
        Err(e) if e == "provider not found" => err(StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn set_default_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = switch_provider_and_sync(state.proxy_manager.clone(), id).await;
    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        Json(result).into_response()
    } else {
        let status = if result.get("message").and_then(|v| v.as_str()) == Some("provider not found")
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
                .unwrap_or("provider not found"),
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

pub async fn reorder_providers(
    State(state): State<AdminState>,
    Json(input): Json<ReorderInput>,
) -> impl IntoResponse {
    let result = with_config_write(|cfg| {
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
            return Err("reorder count mismatch".into());
        }
        for (i, p) in ordered.iter_mut().enumerate() {
            if let Some(o) = p.as_object_mut() {
                o.insert("sortIndex".into(), Value::Number(i.into()));
            }
        }
        let public_ordered: Vec<Value> = ordered.iter().map(public_provider).collect();
        let obj = cfg.as_object_mut().unwrap();
        obj.insert("providers".into(), Value::Array(ordered));
        Ok(ConfigMutation::Modified(public_ordered))
    });
    match result {
        Ok(public_ordered) => {
            // 池模式:reorder 改 sortIndex → 改 unique_pool_slugs 的碰撞后缀分配 → 重建。
            super::resync_pool_if_enabled(&state).await;
            Json(json!({"success": true, "providers": public_ordered})).into_response()
        }
        Err(e) if e == "reorder count mismatch" => err(StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// /api/providers/{id}/draft —— v1 当 update 用,我们直接复用
pub async fn save_draft(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    update_provider(State(state), Path(id), Json(input)).await
}

#[derive(Debug, Deserialize)]
pub struct UpdateModelsInput {
    pub models: Value,
}

pub async fn update_models(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateModelsInput>,
) -> impl IntoResponse {
    let result = with_config_write(|cfg| {
        let Some(idx) = provider_index(cfg, &id) else {
            return Err("provider not found".into());
        };
        let was_active = cfg.get("activeProvider").and_then(|v| v.as_str()) == Some(id.as_str());
        let providers = cfg
            .get_mut("providers")
            .and_then(|v| v.as_array_mut())
            .unwrap();
        if let Some(o) = providers[idx].as_object_mut() {
            o.insert("models".into(), input.models.clone());
            // 池化:若该 provider 已在用池(pooledModels 非空),把映射页编辑的模型值并进池
            // (chat 过滤、去重、永不收缩)。否则池模式偏好非空 pooledModels、映射页改/手输的
            // 新模型永不出现(bot review P2)。空池时无需(走 mappings fallback)。
            let mut pooled: Vec<Value> = o
                .get("pooledModels")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if !pooled.is_empty() {
                let mapping_ids: Vec<String> = input
                    .models
                    .as_object()
                    .map(|m| {
                        m.values()
                            .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let mut seen: std::collections::HashSet<String> = pooled
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                for id in super::models::chat_usable_model_ids(&mapping_ids) {
                    if seen.insert(id.clone()) {
                        pooled.push(Value::String(id));
                    }
                }
                o.insert("pooledModels".into(), Value::Array(pooled));
            }
        }
        Ok(ConfigMutation::Modified(was_active))
    });
    match result {
        Ok(was_active) => {
            // 单模式:仅 active provider 的映射影响 catalog/路由 → 只它变了才 sync。
            // 池模式:任何 provider 的模型都在全局池里 → 非 active 也要重建(bot review P2)。
            let pool_on = crate::admin::registry_io::load()
                .ok()
                .map(|cfg| {
                    crate::admin::handlers::common::read_setting_bool(
                        &cfg,
                        "exposeAllProviderModels",
                        false,
                    )
                })
                .unwrap_or(false);
            let desktop_sync = if was_active || pool_on {
                let sync =
                    crate::admin::services::desktop::snapshot::sync_desktop_for_active_provider(
                        &state,
                    )
                    .await;
                Some(sync)
            } else {
                None
            };
            Json(json!({"success": true, "desktopSync": desktop_sync})).into_response()
        }
        Err(e) if e == "provider not found" => err(StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
