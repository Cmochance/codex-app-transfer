//! `/api/providers/*` CRUD handler —— 增删改 / activate / reorder
//! (草稿暂存 / 模型映射均随 update_provider 的 body 一并保存,无独立 draft / update_models 端点)。

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

    // [MOC-257 review] 标记本次是否新建了「首个 provider」(自动成 active)——闭包内置位,闭包外据此补
    // apply unlock(Cell 内部可变,只读借用即可在 FnMut 闭包里 set)。
    let became_first_active = std::cell::Cell::new(false);
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
        became_first_active.set(was_empty);

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
    // [MOC-257 review] 加的是**首个** provider(自动成 active)→ relay 刚变可用,补 apply 当前生效三态
    // (同 set_default_provider):否则无 provider 时启动跳过的 synthetic/real unlock 永不生效。add 走的
    // 不是 /default 路径,故这里单独补。off 不依赖 provider、无需。idempotent + best-effort。
    if became_first_active.get() {
        let mode = crate::codex_real_account::resolve_plugin_unlock_mode();
        if !matches!(mode, crate::codex_real_account::PluginUnlockMode::Off) {
            if let Err(e) =
                crate::admin::services::desktop::snapshot::apply_plugin_unlock_mode(&state, mode)
                    .await
            {
                tracing::warn!("[PluginUnlock] 加首个 provider 后 apply {mode:?} 失败: {e}");
            }
        }
    }
    Json(json!({"success": true, "provider": public_provider(&new_provider_value)})).into_response()
}

pub async fn update_provider(
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
    Json(json!({"success": true, "provider": public_provider(&updated_value)})).into_response()
}

pub async fn delete_provider(Path(id): Path<String>) -> impl IntoResponse {
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
            // [MOC-277 followup] 删除 provider 改写了 activeProvider 但**不**走 apply/switch_provider_and_sync
            // → 主动收敛 superpowers:删掉的是 active Antigravity 时新 active 已非 antigravity → 卸我方挂载;
            // 否则不动。best-effort,不影响删除结果。
            crate::admin::services::desktop::snapshot::reconcile_superpowers_from_config();
            Json(json!({"success": true})).into_response()
        }
        Err(e) if e == "provider not found" => err(StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// [MOC-211] 触发小米账号内嵌 webview 登录,抓取网页 session cookie 存到该 provider 的
/// `mimoCookie`(masked,见 public_provider),daemon 之后带它查 MiMo 套餐用量。仅对 MiMo
/// token-plan provider 有意义(前端只在该类 provider 上显示登录按钮)。阻塞到登录成功 /
/// 超时 / 用户关窗。
pub async fn mimo_login(Path(id): Path<String>) -> impl IntoResponse {
    // 先确认 provider 存在(避免登录成功后才发现 id 无效)。
    let exists = load_registry()
        .ok()
        .and_then(|cfg| {
            cfg.get("providers").and_then(|v| v.as_array()).map(|ps| {
                ps.iter()
                    .any(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            })
        })
        .unwrap_or(false);
    if !exists {
        return err(StatusCode::NOT_FOUND, "provider not found").into_response();
    }
    // 开内嵌 webview 登录,抓 session cookie(httpOnly serviceToken 等)。
    let cookie = match crate::mimo_session::login_and_capture().await {
        Ok(Some(c)) => c,
        // 用户关窗 / 超时未完成 → 非错误,前端显「未登录」不弹错。
        Ok(None) => return Json(json!({"success": true, "captured": false})).into_response(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, e).into_response(),
    };
    // 落库到该 provider 的 mimoCookie。
    let result = with_config_write(|cfg| {
        let providers = cfg
            .as_object_mut()
            .and_then(|o| o.get_mut("providers"))
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| "providers missing".to_string())?;
        let p = providers
            .iter_mut()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .ok_or_else(|| "provider not found".to_string())?;
        let obj = p
            .as_object_mut()
            .ok_or_else(|| "provider not object".to_string())?;
        obj.insert("mimoCookie".into(), Value::String(cookie));
        Ok(ConfigMutation::Modified(()))
    });
    match result {
        Ok(()) => Json(json!({"success": true, "captured": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// [CAT-256] 触发 OpenCode 账号内嵌 webview 登录,抓取控制台 session cookie 存到该 provider 的
/// `opencodeCookie`,后续 quota daemon 带它查 OpenCode Go 套餐用量(5h/周/月)。仅对 OpenCode Go
/// provider 有意义(前端只在 baseUrl 含 `opencode.ai` 的 provider 上显示登录按钮)。阻塞到登录
/// 成功 / 超时 / 用户关窗。镜像 [`mimo_login`]。
pub async fn opencode_login(Path(id): Path<String>) -> impl IntoResponse {
    let exists = load_registry()
        .ok()
        .and_then(|cfg| {
            cfg.get("providers").and_then(|v| v.as_array()).map(|ps| {
                ps.iter()
                    .any(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            })
        })
        .unwrap_or(false);
    if !exists {
        return err(StatusCode::NOT_FOUND, "provider not found").into_response();
    }
    let (cookie, workspace_id) = match crate::opencode_session::login_and_capture().await {
        Ok(Some(c)) => c,
        Ok(None) => return Json(json!({"success": true, "captured": false})).into_response(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, e).into_response(),
    };
    let result = with_config_write(|cfg| {
        let providers = cfg
            .as_object_mut()
            .and_then(|o| o.get_mut("providers"))
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| "providers missing".to_string())?;
        let p = providers
            .iter_mut()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .ok_or_else(|| "provider not found".to_string())?;
        let obj = p
            .as_object_mut()
            .ok_or_else(|| "provider not object".to_string())?;
        obj.insert("opencodeCookie".into(), Value::String(cookie.clone()));
        // workspace id 抓 Go 用量端点 `/workspace/<id>/go` 必需(非敏感,不 mask)。
        if let Some(ws) = workspace_id.clone() {
            obj.insert("opencodeWorkspaceId".into(), Value::String(ws));
        }
        Ok(ConfigMutation::Modified(()))
    });
    match result {
        Ok(()) => Json(json!({"success": true, "captured": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// [CAT-256 后续] 触发 Kimi 账号内嵌 webview 登录,抓取控制台 session cookie 存到该 provider 的
/// `kimiCookie`,后续 quota daemon 带它查 Kimi Code 套餐用量(5h/周/月)。仅对 Kimi Code provider
/// 有意义(前端只在 kimi-code preset 上显示登录按钮)。镜像 [`mimo_login`] / [`opencode_login`]。
pub async fn kimi_login(Path(id): Path<String>) -> impl IntoResponse {
    let exists = load_registry()
        .ok()
        .and_then(|cfg| {
            cfg.get("providers").and_then(|v| v.as_array()).map(|ps| {
                ps.iter()
                    .any(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            })
        })
        .unwrap_or(false);
    if !exists {
        return err(StatusCode::NOT_FOUND, "provider not found").into_response();
    }
    let cookie = match crate::kimi_session::login_and_capture().await {
        Ok(Some(c)) => c,
        Ok(None) => return Json(json!({"success": true, "captured": false})).into_response(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, e).into_response(),
    };
    let result = with_config_write(|cfg| {
        let providers = cfg
            .as_object_mut()
            .and_then(|o| o.get_mut("providers"))
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| "providers missing".to_string())?;
        let p = providers
            .iter_mut()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .ok_or_else(|| "provider not found".to_string())?;
        let obj = p
            .as_object_mut()
            .ok_or_else(|| "provider not object".to_string())?;
        obj.insert("kimiCookie".into(), Value::String(cookie));
        Ok(ConfigMutation::Modified(()))
    });
    match result {
        Ok(()) => Json(json!({"success": true, "captured": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn set_default_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = switch_provider_and_sync(state.proxy_manager.clone(), id).await;
    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        // [MOC-257 review] 激活/切 provider 后 relay 可用了 → 重新 apply 当前生效三态(synthetic/real),
        // 弥补「无 provider 时启动跳过的 unlock apply」:否则 status 一直报 resolve 默认 synthetic 却从没真
        // apply、前端 re-select 同档 no-op、unlock 永不生效。off 不依赖 provider、无需。idempotent + best-effort。
        let mode = crate::codex_real_account::resolve_plugin_unlock_mode();
        if !matches!(mode, crate::codex_real_account::PluginUnlockMode::Off) {
            if let Err(e) =
                crate::admin::services::desktop::snapshot::apply_plugin_unlock_mode(&state, mode)
                    .await
            {
                tracing::warn!("[PluginUnlock] 切 provider 后重 apply {mode:?} 失败: {e}");
            }
        }
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

pub async fn reorder_providers(Json(input): Json<ReorderInput>) -> impl IntoResponse {
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
            Json(json!({"success": true, "providers": public_ordered})).into_response()
        }
        Err(e) if e == "reorder count mismatch" => err(StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// [MOC-261 一-10] save_draft(POST /api/providers/{id}/draft)已删:它只是 update_provider 的纯
// 别名(v1 拿来做编辑自动保存),无独立草稿存储 / 无读端点,前后端零引用 → 按死代码移除。

// [MOC-261 一-7] update_models(PUT /api/providers/{id}/models)已删:模型映射经主
// update_provider(PUT /api/providers/{id},body 带 models)保存,该专用端点前后端零引用。
