//! `/api/*` 路由 handlers —— 1:1 翻译自 `backend/main.py`.
//!
//! 数据形态(请求/响应 JSON shape)严格对齐 v1.4,frontend/js/api.js 不需要
//! 任何修改即可工作。

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use codex_app_transfer_codex_integration::{
    apply_provider, has_snapshot, restore_codex_state, ApplyConfig, CodexPaths,
};
use codex_app_transfer_proxy::{proxy_log_dir, proxy_telemetry};
use codex_app_transfer_registry::{
    builtin_presets, config_dir, normalize_model_mappings, strip_internal_model_suffix, RawConfig,
    MODEL_ORDER,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    multipart, StatusCode as ReqwestStatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::registry_io::{load as load_registry, public_provider, save as save_registry};
use super::state::AdminState;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const FEEDBACK_WORKER_URL: &str = "https://codex-app-transfer-feedback.alysechencn.workers.dev";

struct FeedbackThrottleState {
    last_success: Option<Instant>,
    failure_ts: Vec<Instant>,
    failure_cooldown_until: Option<Instant>,
}

struct FeedbackThrottle {
    inner: Mutex<FeedbackThrottleState>,
}

impl FeedbackThrottle {
    const SUCCESS_COOLDOWN: Duration = Duration::from_secs(60);
    const FAILURE_WINDOW: Duration = Duration::from_secs(300);
    const FAILURE_LIMIT: usize = 5;
    const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

    fn new() -> Self {
        Self {
            inner: Mutex::new(FeedbackThrottleState {
                last_success: None,
                failure_ts: Vec::new(),
                failure_cooldown_until: None,
            }),
        }
    }

    fn acquire(&self) -> Result<(), String> {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();

        if let Some(last_success) = inner.last_success {
            let elapsed = now.saturating_duration_since(last_success);
            if elapsed < Self::SUCCESS_COOLDOWN {
                let wait = Self::SUCCESS_COOLDOWN.saturating_sub(elapsed).as_secs();
                return Err(format!("刚提交成功,请等 {wait} 秒后再发新反馈"));
            }
        }

        if let Some(until) = inner.failure_cooldown_until {
            if now < until {
                let wait = until.saturating_duration_since(now).as_secs();
                return Err(format!("连续提交失败次数过多,请等 {wait} 秒后再试"));
            }
        }

        inner
            .failure_ts
            .retain(|ts| now.saturating_duration_since(*ts) < Self::FAILURE_WINDOW);
        Ok(())
    }

    fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_success = Some(Instant::now());
        inner.failure_ts.clear();
        inner.failure_cooldown_until = None;
    }

    fn record_failure(&self) {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner
            .failure_ts
            .retain(|ts| now.saturating_duration_since(*ts) < Self::FAILURE_WINDOW);
        inner.failure_ts.push(now);
        if inner.failure_ts.len() >= Self::FAILURE_LIMIT {
            inner.failure_cooldown_until = Some(now + Self::FAILURE_COOLDOWN);
        }
    }
}

static FEEDBACK_THROTTLE: OnceLock<FeedbackThrottle> = OnceLock::new();

fn feedback_throttle() -> &'static FeedbackThrottle {
    FEEDBACK_THROTTLE.get_or_init(FeedbackThrottle::new)
}

// ── 工具 ─────────────────────────────────────────────────────────────

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"success": false, "message": msg.into()})),
    )
}

fn open_directory(path: &PathBuf) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(path);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开日志目录: {e}"))
}

fn multipart_text_part(text: String, mime: &str) -> multipart::Part {
    multipart::Part::text(text.clone())
        .mime_str(mime)
        .unwrap_or_else(|_| multipart::Part::text(text))
}

fn active_provider_name(config: &Value) -> String {
    let active_id = config.get("activeProvider").and_then(|v| v.as_str());
    config
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| {
            if let Some(active_id) = active_id {
                providers
                    .iter()
                    .find(|provider| provider.get("id").and_then(|v| v.as_str()) == Some(active_id))
            } else {
                providers.first()
            }
        })
        .and_then(|provider| provider.get("name").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_owned()
}

fn feedback_proxy_tail_part() -> Option<multipart::Part> {
    let log_dir = proxy_log_dir()?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = log_dir.join(format!("proxy-{today}.log"));
    let content = fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(200);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        return None;
    }
    let part =
        multipart::Part::bytes(tail.into_bytes()).file_name(format!("proxy-tail-{today}.log"));
    Some(
        part.mime_str("text/plain")
            .unwrap_or_else(|_| multipart::Part::text("")),
    )
}

static ID_COUNTER: AtomicU32 = AtomicU32::new(0);
fn fresh_provider_id(existing: &[String]) -> String {
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0);
        let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{:08x}", nanos.wrapping_add(counter));
        if !existing.iter().any(|id| id == &candidate) {
            return candidate;
        }
    }
}

fn provider_supports_1m(provider: &Value) -> bool {
    let default_raw = provider
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if codex_app_transfer_registry::has_internal_one_m_suffix(default_raw) {
        return true;
    }
    let default = strip_internal_model_suffix(default_raw).to_lowercase();
    if default.starts_with("deepseek-v4-") || default.starts_with("qwen3.6-") {
        return true;
    }
    if let Some(b) = provider
        .get("modelCapabilities")
        .and_then(|c| c.get(&default))
        .and_then(|v| v.get("supports1m"))
        .and_then(|v| v.as_bool())
    {
        return b;
    }
    false
}

fn provider_default_model(provider: &Value) -> String {
    let raw = provider
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    strip_internal_model_suffix(raw)
}

fn provider_model_mappings(provider: &Value) -> Value {
    provider.get("models").cloned().unwrap_or_else(|| json!({}))
}

fn provider_model_capabilities(provider: &Value) -> Value {
    provider
        .get("modelCapabilities")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn provider_display_name(provider: &Value) -> String {
    provider
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Provider")
        .to_owned()
}

fn normalize_provider_api_format(api_format: Option<&str>) -> &'static str {
    match api_format
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "openai" | "openai_chat" | "chat_completions" => "openai_chat",
        _ => "responses",
    }
}

fn build_provider_test_url(base_url: &str, api_format: &str) -> String {
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

fn provider_api_key(provider: &Value) -> String {
    provider
        .get("apiKey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn provider_test_model(provider: &Value) -> String {
    let mappings = normalize_model_mappings(provider.get("models"));
    let default = mappings.get("default").map(|s| s.trim()).unwrap_or("");
    if !default.is_empty() {
        return strip_internal_model_suffix(default);
    }
    for slot in MODEL_ORDER
        .iter()
        .copied()
        .filter(|slot| *slot != "default")
    {
        let model = mappings.get(slot).map(|s| s.trim()).unwrap_or("");
        if !model.is_empty() {
            return strip_internal_model_suffix(model);
        }
    }
    "claude-sonnet-4-6".to_owned()
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

fn provider_test_headers(provider: &Value, include_content_type: bool) -> HeaderMap {
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

fn provider_test_error_label(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "ConnectError"
    } else {
        "RequestError"
    }
}

fn clean_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

fn replace_path_suffix(url: &str, suffixes: &[&str], replacement: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return url.to_owned();
    };
    let mut path = parsed.path().trim_end_matches('/').to_owned();
    let lower = path.to_ascii_lowercase();
    for suffix in suffixes {
        if lower.ends_with(suffix) {
            let keep = path.len().saturating_sub(suffix.len());
            path.truncate(keep);
            break;
        }
    }
    let next = format!(
        "{}/{}",
        path.trim_end_matches('/'),
        replacement.trim_start_matches('/')
    );
    parsed.set_path(&next);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string().trim_end_matches('/').to_owned()
}

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
                "message": "无法自动获取模型列表",
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
        "message": "无法自动获取模型列表",
        "models": [],
        "suggested": {},
        "errors": errors[start..].to_vec(),
    })
}

fn provider_kind(provider: &Value) -> &'static str {
    let probe = format!(
        "{} {}",
        provider.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .to_ascii_lowercase();
    if probe.contains("deepseek") {
        "deepseek"
    } else if probe.contains("siliconflow") {
        "siliconflow"
    } else if probe.contains("openrouter") {
        "openrouter"
    } else if probe.contains("novita") {
        "novita"
    } else if probe.contains("stepfun") || probe.contains("step") {
        "stepfun"
    } else {
        "unknown"
    }
}

fn balance_endpoint(provider: &Value) -> Option<(&'static str, String)> {
    let kind = provider_kind(provider);
    let base = clean_base_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )
    .to_ascii_lowercase();
    match kind {
        "deepseek" => Some((kind, "https://api.deepseek.com/user/balance".to_owned())),
        "siliconflow" => {
            let host = if base.contains(".com") {
                "https://api.siliconflow.com"
            } else {
                "https://api.siliconflow.cn"
            };
            Some((kind, format!("{host}/v1/user/info")))
        }
        "openrouter" => Some((kind, "https://openrouter.ai/api/v1/credits".to_owned())),
        "novita" => Some((kind, "https://api.novita.ai/v3/user/balance".to_owned())),
        "stepfun" => Some((kind, "https://api.stepfun.com/v1/accounts".to_owned())),
        _ => None,
    }
}

fn float_or_none(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if !s.is_empty() => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn money_item(
    label: impl Into<String>,
    remaining: Option<f64>,
    total: Option<f64>,
    used: Option<f64>,
    unit: impl Into<String>,
) -> Value {
    json!({
        "label": label.into(),
        "remaining": remaining,
        "total": total,
        "used": used,
        "unit": unit.into(),
    })
}

fn normalize_balance_payload(kind: &str, payload: &Value) -> Vec<Value> {
    if kind == "deepseek" {
        let mut items = Vec::new();
        for item in payload
            .get("balance_infos")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let currency = obj
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("CNY")
                .to_owned();
            items.push(money_item(
                currency.clone(),
                float_or_none(obj.get("total_balance")),
                float_or_none(obj.get("granted_balance")),
                float_or_none(obj.get("topped_up_balance")),
                currency,
            ));
        }
        return items;
    }

    if kind == "openrouter" {
        let data = payload.get("data").unwrap_or(payload);
        let total = float_or_none(data.get("total_credits"));
        let used = float_or_none(data.get("total_usage"));
        let remaining = match (total, used) {
            (Some(total), Some(used)) => Some(total - used),
            _ => None,
        };
        return vec![money_item("credits", remaining, total, used, "USD")];
    }

    let data = payload.get("data").unwrap_or(payload);
    if let Some(obj) = data.as_object() {
        for remaining_key in [
            "balance",
            "remaining",
            "available_balance",
            "availableBalance",
            "credit",
        ] {
            if obj.contains_key(remaining_key) {
                let unit = obj
                    .get("currency")
                    .or_else(|| obj.get("unit"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                return vec![money_item(
                    "balance",
                    float_or_none(obj.get(remaining_key)),
                    float_or_none(
                        obj.get("total")
                            .or_else(|| obj.get("totalBalance"))
                            .or_else(|| obj.get("total_credits")),
                    ),
                    float_or_none(
                        obj.get("used")
                            .or_else(|| obj.get("usage"))
                            .or_else(|| obj.get("usedBalance")),
                    ),
                    unit,
                )];
            }
        }
    }
    Vec::new()
}

async fn query_provider_usage_impl(provider: &Value) -> Value {
    if provider_api_key(provider).is_empty() {
        return json!({"success": false, "message": "请先保存 API Key"});
    }
    let Some((kind, endpoint)) = balance_endpoint(provider) else {
        return json!({
            "success": true,
            "supported": false,
            "items": [],
            "message": "这个提供商暂未适配余额/用量接口",
        });
    };

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
                "success": true,
                "supported": true,
                "ok": false,
                "message": format!("查询失败：{}", provider_test_error_label(&error)),
                "items": [],
            });
        }
    };
    let response = match client.get(&endpoint).headers(headers).send().await {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": format!("查询失败：{}", provider_test_error_label(&error)),
                "items": [],
            });
        }
    };
    if !response.status().is_success() {
        return json!({
            "success": true,
            "supported": true,
            "ok": false,
            "statusCode": response.status().as_u16(),
            "message": format!("余额接口返回 HTTP {}", response.status().as_u16()),
            "items": [],
        });
    }
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": "余额接口返回了非 JSON 响应",
                "items": [],
            });
        }
    };
    let items = normalize_balance_payload(kind, &payload);
    let ok = !items.is_empty();
    let message = if ok {
        "查询完成"
    } else {
        "余额接口响应中未识别到余额字段"
    };
    json!({
        "success": true,
        "supported": true,
        "ok": ok,
        "endpoint": endpoint,
        "items": items,
        "message": message,
    })
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
                "message": format!("连接失败：{}", provider_test_error_label(&error)),
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
                "message": format!("连接失败：{}", provider_test_error_label(&error)),
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
                    "message": format!("连接失败：{}", provider_test_error_label(&error)),
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
                    "message": format!("连接失败：{}", provider_test_error_label(&error)),
                });
            }
        };
    }

    let latency_ms = started.elapsed().as_millis();
    let status_code = response.status().as_u16();
    let mut reachable = status_code < 500;
    let message = if (200..300).contains(&status_code) {
        format!("连接正常，{latency_ms} ms")
    } else if matches!(status_code, 401 | 403) {
        reachable = false;
        if is_kimi_provider(provider) {
            format!(
                "Kimi 认证失败，HTTP {status_code}。Kimi Platform Key 请使用 https://api.moonshot.cn/v1；Kimi Code 会员 Key 请使用 https://api.kimi.com/coding，{latency_ms} ms"
            )
        } else {
            format!(
                "认证失败，HTTP {status_code}，请检查 API Key 和 API 地址是否匹配，{latency_ms} ms"
            )
        }
    } else if matches!(status_code, 404 | 405) {
        reachable = false;
        format!("接口不可用，HTTP {status_code}，请检查 API 地址是否填到了兼容 Codex 的接口，{latency_ms} ms")
    } else {
        format!("地址可达，HTTP {status_code}，{latency_ms} ms")
    };

    json!({
        "success": true,
        "ok": reachable,
        "latencyMs": latency_ms,
        "statusCode": status_code,
        "message": message,
    })
}

fn read_proxy_port(cfg: &RawConfig) -> u16 {
    cfg.get("settings")
        .and_then(|s| s.get("proxyPort"))
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(18080)
}

fn read_gateway_key(cfg: &RawConfig) -> String {
    cfg.get("gatewayApiKey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn ensure_settings_object(cfg: &mut RawConfig) -> &mut serde_json::Map<String, Value> {
    let obj = cfg.as_object_mut().expect("registry root is object");
    obj.entry("settings".to_owned())
        .or_insert_with(|| json!({}));
    obj.get_mut("settings")
        .and_then(|v| v.as_object_mut())
        .expect("settings is object")
}

fn provider_index(cfg: &RawConfig, id: &str) -> Option<usize> {
    cfg.get("providers")
        .and_then(|v| v.as_array())?
        .iter()
        .position(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                == Some(id)
        })
}

fn active_provider(cfg: &RawConfig) -> Option<Value> {
    let active_id = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let providers = cfg.get("providers").and_then(|v| v.as_array())?;
    let chosen = match active_id {
        Some(id) => providers.iter().find(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                == Some(id.as_str())
        }),
        None => providers.first(),
    };
    chosen.cloned()
}

fn generate_gateway_key_value() -> String {
    let mut buf = [0u8; 32];
    let _ = getrandom::getrandom(&mut buf);
    format!("cas_{}", URL_SAFE_NO_PAD.encode(buf))
}

fn random_hex(bytes_len: usize) -> String {
    let mut buf = vec![0u8; bytes_len];
    let _ = getrandom::getrandom(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn app_config_dir() -> Result<PathBuf, String> {
    config_dir().ok_or_else(|| "无法定位用户配置目录".to_owned())
}

fn app_config_file() -> Result<PathBuf, String> {
    Ok(app_config_dir()?.join("config.json"))
}

fn app_backup_dir() -> Result<PathBuf, String> {
    Ok(app_config_dir()?.join("backups"))
}

fn system_time_iso_seconds(time: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = time.into();
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn default_config_value() -> Value {
    json!({
        "version": APP_VERSION,
        "activeProvider": null,
        "gatewayApiKey": null,
        "providers": [],
        "settings": {
            "theme": "default",
            "language": "zh",
            "proxyPort": 18080,
            "adminPort": 18081,
            "autoStart": false,
            "autoApplyOnStart": true,
            "exposeAllProviderModels": false,
            "restoreCodexOnExit": true,
            "updateUrl": "https://github.com/Cmochance/codex-app-transfer/releases/latest/download/latest.json"
        }
    })
}

fn normalize_imported_provider(provider: &Value) -> Option<Value> {
    let src = provider.as_object()?;
    let mut normalized = src.clone();
    let provider_id = normalized.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let safe_id: String = provider_id
        .chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
        .take(64)
        .collect();
    normalized.insert(
        "id".into(),
        Value::String(if safe_id.is_empty() {
            random_hex(4)
        } else {
            safe_id
        }),
    );
    normalized
        .entry("name")
        .or_insert_with(|| Value::String("Unnamed Provider".into()));
    normalized
        .entry("baseUrl")
        .or_insert_with(|| Value::String(String::new()));
    normalized
        .entry("authScheme")
        .or_insert_with(|| Value::String("bearer".into()));
    normalized
        .entry("apiFormat")
        .or_insert_with(|| Value::String("responses".into()));
    normalized
        .entry("apiKey")
        .or_insert_with(|| Value::String(String::new()));
    normalized
        .entry("extraHeaders")
        .or_insert_with(|| json!({}));
    normalized
        .entry("modelCapabilities")
        .or_insert_with(|| json!({}));
    normalized
        .entry("requestOptions")
        .or_insert_with(|| json!({}));
    normalized.entry("isBuiltin").or_insert(Value::Bool(false));
    normalized
        .entry("sortIndex")
        .or_insert(Value::Number(0.into()));

    let models = serde_json::to_value(normalize_model_mappings(normalized.get("models"))).ok()?;
    normalized.insert("models".into(), models);
    Some(Value::Object(normalized))
}

fn normalize_imported_config(data: &Value) -> Result<Value, String> {
    let root = data
        .as_object()
        .ok_or_else(|| "配置文件必须是 JSON 对象".to_owned())?;
    let source = root
        .get("config")
        .and_then(|v| v.as_object())
        .map(|obj| Value::Object(obj.clone()))
        .unwrap_or_else(|| data.clone());
    let source_obj = source
        .as_object()
        .ok_or_else(|| "配置文件必须是 JSON 对象".to_owned())?;

    let mut normalized = default_config_value();
    {
        let obj = normalized.as_object_mut().expect("default config object");
        for key in [
            "version",
            "activeProvider",
            "gatewayApiKey",
            "providers",
            "settings",
        ] {
            if let Some(value) = source_obj.get(key) {
                obj.insert(key.to_owned(), value.clone());
            }
        }
        obj.insert(
            "version".into(),
            source_obj
                .get("version")
                .cloned()
                .unwrap_or_else(|| Value::String(APP_VERSION.to_owned())),
        );
    }

    let mut settings = default_config_value()
        .get("settings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let (Some(settings_obj), Some(imported)) = (
        settings.as_object_mut(),
        source_obj.get("settings").and_then(|v| v.as_object()),
    ) {
        for (key, value) in imported {
            settings_obj.insert(key.clone(), value.clone());
        }
    }
    normalized["settings"] = settings;

    let providers = source_obj
        .get("providers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "providers 必须是数组".to_owned())?;
    let mut normalized_providers = Vec::new();
    let mut seen_ids = HashSet::new();
    for provider in providers {
        let Some(mut normalized_provider) = normalize_imported_provider(provider) else {
            continue;
        };
        let provider_id = normalized_provider
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if !seen_ids.insert(provider_id.clone()) {
            if let Some(obj) = normalized_provider.as_object_mut() {
                obj.insert(
                    "id".into(),
                    Value::String(format!("{provider_id}-{}", random_hex(2))),
                );
            }
        }
        if let Some(id) = normalized_provider.get("id").and_then(|v| v.as_str()) {
            seen_ids.insert(id.to_owned());
        }
        normalized_providers.push(normalized_provider);
    }
    normalized["providers"] = Value::Array(normalized_providers);

    let provider_ids: HashSet<String> = normalized["providers"]
        .as_array()
        .map(|providers| {
            providers
                .iter()
                .filter_map(|p| p.get("id").and_then(|v| v.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let active_provider = source_obj.get("activeProvider").and_then(|v| v.as_str());
    normalized["activeProvider"] = if let Some(active) = active_provider {
        if provider_ids.contains(active) {
            Value::String(active.to_owned())
        } else {
            normalized["providers"]
                .as_array()
                .and_then(|providers| providers.first())
                .and_then(|p| p.get("id"))
                .cloned()
                .unwrap_or(Value::Null)
        }
    } else {
        normalized["providers"]
            .as_array()
            .and_then(|providers| providers.first())
            .and_then(|p| p.get("id"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    if let Some(key) = source_obj.get("gatewayApiKey").filter(|v| !v.is_null()) {
        normalized["gatewayApiKey"] = key.clone();
    }

    Ok(normalized)
}

fn preserve_existing_provider_secrets(imported: &mut Value, current: &Value) {
    let Some(imported_providers) = imported.get_mut("providers").and_then(|v| v.as_array_mut())
    else {
        return;
    };
    let current_providers = current
        .get("providers")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    for provider in imported_providers {
        let Some(provider_obj) = provider.as_object_mut() else {
            continue;
        };
        let Some(provider_id) = provider_obj.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(current_provider) = current_providers
            .iter()
            .find(|item| item.get("id").and_then(|v| v.as_str()) == Some(provider_id))
            .and_then(|v| v.as_object())
        else {
            continue;
        };

        let imported_key_is_blank = provider_obj
            .get("apiKey")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if imported_key_is_blank {
            if let Some(existing_key) = current_provider
                .get("apiKey")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                provider_obj.insert("apiKey".into(), Value::String(existing_key.to_owned()));
            }
        }

        let imported_headers_empty = provider_obj
            .get("extraHeaders")
            .and_then(|v| v.as_object())
            .map(|obj| obj.is_empty())
            .unwrap_or(true);
        if imported_headers_empty {
            if let Some(existing_headers) = current_provider
                .get("extraHeaders")
                .and_then(|v| v.as_object())
                .filter(|obj| !obj.is_empty())
            {
                provider_obj.insert(
                    "extraHeaders".into(),
                    Value::Object(existing_headers.clone()),
                );
            }
        }
    }
}

fn create_config_backup(reason: &str) -> Result<Value, String> {
    let backup_dir = app_backup_dir()?;
    fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    let config_file = app_config_file()?;
    if !config_file.exists() {
        let cfg = load_registry()?;
        save_registry(&cfg)?;
    }

    let safe_reason: String = reason
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
        .take(32)
        .collect();
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%f");
    let filename = format!(
        "config-{timestamp}-{}-{}.json",
        if safe_reason.is_empty() {
            "manual"
        } else {
            safe_reason.as_str()
        },
        random_hex(2)
    );
    let target = backup_dir.join(&filename);
    fs::copy(&config_file, &target).map_err(|e| format!("复制配置备份失败: {e}"))?;
    let stat = fs::metadata(&target).map_err(|e| format!("读取备份元数据失败: {e}"))?;
    Ok(json!({
        "name": filename,
        "size": stat.len(),
        "createdAt": system_time_iso_seconds(stat.modified().unwrap_or_else(|_| SystemTime::now())),
    }))
}

fn list_config_backups() -> Result<Vec<Value>, String> {
    let backup_dir = app_backup_dir()?;
    fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    let mut backups = Vec::new();
    let entries = fs::read_dir(&backup_dir).map_err(|e| format!("读取备份目录失败: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") || !path.is_file() {
            continue;
        }
        let stat = match fs::metadata(&path) {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        let name = match path.file_name().and_then(|v| v.to_str()) {
            Some(name) => name.to_owned(),
            None => continue,
        };
        backups.push(json!({
            "name": name,
            "size": stat.len(),
            "createdAt": system_time_iso_seconds(stat.modified().unwrap_or_else(|_| SystemTime::now())),
        }));
    }
    backups.sort_by(|a, b| {
        let a = a.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
        let b = b.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
        b.cmp(a)
    });
    Ok(backups)
}

// ── /api/instance-info & /api/instance-show-window ───────────────────

pub async fn instance_info() -> Json<Value> {
    Json(json!({
        "app": "codex-app-transfer",
        "version": APP_VERSION,
        "pid": std::process::id(),
    }))
}

pub async fn instance_show_window() -> Json<Value> {
    // 由 main.rs 通过 channel/event 拉前主窗口;这里至少回 ack
    Json(json!({"success": true}))
}

// ── /api/status ──────────────────────────────────────────────────────

pub async fn status(State(state): State<AdminState>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers_count = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let active = active_provider(&cfg).map(|p| public_provider(&p));
    let active_id = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let proxy_port = read_proxy_port(&cfg);
    let proxy_status = state.proxy_manager.status();
    let codex_paths = CodexPaths::from_home_env().ok();
    let codex_configured = codex_paths.as_ref().map(has_snapshot).unwrap_or(false);

    Json(json!({
        "desktopConfigured": codex_configured,
        "proxyRunning": proxy_status.running,
        "proxyPort": proxy_port,
        "desktopMode": "gateway",
        "desktopRequiresProxy": true,
        "activeProvider": active,
        "activeProviderId": active_id,
        "providerCount": providers_count,
        "desktopHealth": {
            "needsApply": !codex_configured,
            "issues": [],
        },
        "exposeAllProviderModels": false,
    }))
    .into_response()
}

// ── /api/providers ────────────────────────────────────────────────────

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

pub async fn set_default_provider(Path(id): Path<String>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    if provider_index(&cfg, &id).is_none() {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    }
    let obj = cfg.as_object_mut().unwrap();
    obj.insert("activeProvider".into(), Value::String(id));
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true})).into_response()
}

pub async fn activate_provider(Path(id): Path<String>) -> impl IntoResponse {
    set_default_provider(Path(id)).await
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

// ── /api/presets ─────────────────────────────────────────────────────

pub async fn list_presets() -> impl IntoResponse {
    let presets: Vec<Value> = builtin_presets().to_vec();
    Json(json!({"presets": presets})).into_response()
}

// ── /api/desktop/* ───────────────────────────────────────────────────

pub async fn desktop_status() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let configured = has_snapshot(&paths);
    let base_url = std::fs::read_to_string(&paths.config_toml)
        .ok()
        .and_then(|s| {
            for line in s.lines() {
                let stripped = line.trim_start();
                if !stripped.starts_with("openai_base_url") {
                    continue;
                }
                let after = &stripped["openai_base_url".len()..];
                let mut rest = after.trim_start();
                if !rest.starts_with('=') {
                    continue;
                }
                rest = rest[1..].trim();
                if let Some(idx) = rest.find('#') {
                    rest = rest[..idx].trim_end();
                }
                let trimmed = rest.trim_matches(|c: char| c == '"' || c == '\'');
                return Some(trimmed.to_owned());
            }
            None
        });
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    let proxy_port = read_proxy_port(&cfg);
    Json(json!({
        "configured": configured,
        "health": {"needsApply": !configured, "issues": []},
        "keys": {
            "inferenceProvider": "gateway",
            "inferenceGatewayBaseUrl": base_url.unwrap_or_else(|| format!("http://127.0.0.1:{proxy_port}")),
            "inferenceGatewayApiKey": if read_gateway_key(&cfg).is_empty() { "" } else { "******" },
            "inferenceGatewayAuthScheme": "bearer",
            "inferenceModels": "[\"sonnet\",\"haiku\",\"opus\"]",
        },
    }))
    .into_response()
}

pub async fn desktop_configure() -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if providers.is_empty() {
        return err(StatusCode::BAD_REQUEST, "请先添加 provider").into_response();
    }
    let active_id = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let active = match active_id {
        Some(id) => providers.iter().find(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                == Some(id.as_str())
        }),
        None => providers.first(),
    };
    let Some(active) = active else {
        return err(StatusCode::BAD_REQUEST, "未找到 active provider").into_response();
    };
    let supports_1m = provider_supports_1m(active);
    let provider_name = provider_display_name(active);
    let default_model = provider_default_model(active);
    let model_mappings = provider_model_mappings(active);
    let model_capabilities = provider_model_capabilities(active);
    let port = read_proxy_port(&cfg);

    // 缺 gateway key 自动补
    let mut gateway_key = read_gateway_key(&cfg);
    if gateway_key.is_empty() {
        gateway_key = generate_gateway_key_value();
        let obj = cfg.as_object_mut().unwrap();
        obj.insert("gatewayApiKey".into(), Value::String(gateway_key.clone()));
        if let Err(e) = save_registry(&cfg) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }

    let base_url = format!("http://127.0.0.1:{port}");
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if let Err(e) = apply_provider(
        &paths,
        &ApplyConfig {
            base_url: &base_url,
            gateway_api_key: &gateway_key,
            supports_1m,
            provider_name: &provider_name,
            default_model: &default_model,
            model_mappings: Some(&model_mappings),
            model_capabilities: Some(&model_capabilities),
            app_version: APP_VERSION,
        },
    ) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("apply 失败: {e}"),
        )
        .into_response();
    }
    Json(json!({"success": true})).into_response()
}

pub async fn desktop_clear() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match restore_codex_state(&paths) {
        Ok(_) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn desktop_snapshot_status() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(json!({
        "hasSnapshot": has_snapshot(&paths),
    }))
    .into_response()
}

// ── /api/proxy/* ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StartProxyInput {
    pub port: Option<u16>,
}

pub async fn start_proxy(
    State(state): State<AdminState>,
    body: Option<Json<StartProxyInput>>,
) -> impl IntoResponse {
    let port = body
        .and_then(|b| b.0.port)
        .or_else(|| load_registry().ok().map(|cfg| read_proxy_port(&cfg)))
        .unwrap_or(18080);
    match state.proxy_manager.start(port).await {
        Ok(s) => Json(json!({
            "success": true,
            "running": s.running,
            "port": s.addr.and_then(|a| a.split(':').last().and_then(|p| p.parse::<u16>().ok())).unwrap_or(port),
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn stop_proxy(State(state): State<AdminState>) -> impl IntoResponse {
    state.proxy_manager.stop_silent();
    Json(json!({"success": true, "running": false})).into_response()
}

pub async fn proxy_status(State(state): State<AdminState>) -> impl IntoResponse {
    let s = state.proxy_manager.status();
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    let port = s
        .addr
        .as_ref()
        .and_then(|a| a.split(':').last().and_then(|p| p.parse::<u16>().ok()))
        .unwrap_or_else(|| read_proxy_port(&cfg));
    Json(json!({
        "running": s.running,
        "port": port,
        "stats": proxy_telemetry().stats.snapshot(),
    }))
    .into_response()
}

pub async fn proxy_logs() -> impl IntoResponse {
    Json(json!({"logs": proxy_telemetry().logs.get_all()})).into_response()
}

pub async fn proxy_logs_clear() -> impl IntoResponse {
    proxy_telemetry().logs.clear();
    Json(json!({"success": true})).into_response()
}

pub async fn proxy_logs_open_dir() -> impl IntoResponse {
    let Some(path) = proxy_log_dir() else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "无法定位日志目录").into_response();
    };
    if let Err(e) = fs::create_dir_all(&path) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("无法创建日志目录: {e}"),
        )
        .into_response();
    }
    match open_directory(&path) {
        Ok(_) => Json(json!({"success": true, "path": path.to_string_lossy()})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── /api/settings ────────────────────────────────────────────────────

pub async fn get_settings() -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let settings = cfg.get("settings").cloned().unwrap_or_else(|| json!({}));
    Json(settings).into_response()
}

pub async fn save_settings(Json(input): Json<Value>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let s = ensure_settings_object(&mut cfg);
    if let Some(obj) = input.as_object() {
        for (k, v) in obj {
            s.insert(k.clone(), v.clone());
        }
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let settings = cfg.get("settings").cloned().unwrap_or_else(|| json!({}));
    Json(json!({"success": true, "settings": settings})).into_response()
}

// ── /api/config/* ────────────────────────────────────────────────────

pub async fn create_backup() -> impl IntoResponse {
    match create_config_backup("manual") {
        Ok(backup) => Json(json!({"success": true, "backup": backup})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn list_backups() -> impl IntoResponse {
    match list_config_backups() {
        Ok(backups) => Json(json!({"backups": backups})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn export_config() -> impl IntoResponse {
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    Json(json!({
        "format": "codex-app-transfer.config",
        "exportedAt": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "config": cfg,
    }))
    .into_response()
}

pub async fn import_config(Json(data): Json<Value>) -> impl IntoResponse {
    let backup = match create_config_backup("before-import") {
        Ok(backup) => backup,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let mut normalized = match normalize_imported_config(&data) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let current = load_registry().unwrap_or_else(|_| json!({}));
    preserve_existing_provider_secrets(&mut normalized, &current);
    if let Err(e) = save_registry(&normalized) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({
        "success": true,
        "message": "配置已导入",
        "backup": backup,
    }))
    .into_response()
}

// ── /api/update/* ────────────────────────────────────────────────────

pub async fn update_check() -> impl IntoResponse {
    Json(json!({
        "hasUpdate": false,
        "currentVersion": APP_VERSION,
        "message": "暂不检测更新",
    }))
    .into_response()
}

pub async fn update_install(Json(_input): Json<Value>) -> impl IntoResponse {
    err(StatusCode::NOT_IMPLEMENTED, "update install 暂未实现").into_response()
}

// ── /api/feedback ────────────────────────────────────────────────────

pub async fn submit_feedback(Json(input): Json<Value>) -> impl IntoResponse {
    let throttle = feedback_throttle();
    if let Err(reason) = throttle.acquire() {
        return err(StatusCode::TOO_MANY_REQUESTS, reason).into_response();
    }

    let title = input
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let body_text = input
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let include_diag = input
        .get("include_diagnostics")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if body_text.is_empty() {
        return err(StatusCode::BAD_REQUEST, "请填写描述").into_response();
    }

    let mut meta = json!({"app_version": APP_VERSION});
    if include_diag {
        let active_name = load_registry()
            .ok()
            .map(|cfg| active_provider_name(&cfg))
            .unwrap_or_default();
        if let Some(obj) = meta.as_object_mut() {
            obj.insert(
                "os".to_owned(),
                Value::String(std::env::consts::OS.to_owned()),
            );
            obj.insert(
                "arch".to_owned(),
                Value::String(std::env::consts::ARCH.to_owned()),
            );
            obj.insert(
                "active_provider_name".to_owned(),
                Value::String(active_name),
            );
            obj.insert("include_diagnostics".to_owned(), Value::Bool(true));
        }
    }

    let mut form = multipart::Form::new()
        .part(
            "meta",
            multipart_text_part(meta.to_string(), "application/json"),
        )
        .part("title", multipart_text_part(title, "text/plain"))
        .part("body", multipart_text_part(body_text, "text/plain"));

    let mut shot_idx = 0usize;
    let mut log_idx = 0usize;
    if let Some(attachments) = input.get("attachments").and_then(|v| v.as_array()) {
        for attachment in attachments {
            let Some(obj) = attachment.as_object() else {
                continue;
            };
            let content_b64 = obj
                .get("content_b64")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Ok(raw) = STANDARD.decode(content_b64.as_bytes()) else {
                continue;
            };
            if raw.is_empty() || raw.len() > 5 * 1024 * 1024 {
                continue;
            }
            let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("log");
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or("attachment.bin")
                .to_owned();
            let content_type = obj
                .get("content_type")
                .and_then(|v| v.as_str())
                .filter(|v| v.contains('/'))
                .unwrap_or("application/octet-stream")
                .to_owned();
            let field = if kind == "screenshot" {
                let field = format!("screenshot{shot_idx}");
                shot_idx += 1;
                field
            } else {
                let field = format!("log{log_idx}");
                log_idx += 1;
                field
            };
            let part = multipart::Part::bytes(raw.clone()).file_name(name.clone());
            let part = part
                .mime_str(&content_type)
                .unwrap_or_else(|_| multipart::Part::bytes(raw).file_name(name));
            form = form.part(field, part);
        }
    }

    if include_diag {
        if let Some(part) = feedback_proxy_tail_part() {
            form = form.part("log_proxy_tail", part);
        }
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            throttle.record_failure();
            return err(StatusCode::BAD_GATEWAY, format!("反馈服务暂不可用:{e}")).into_response();
        }
    };

    let response = match client
        .post(FEEDBACK_WORKER_URL)
        .multipart(form)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            throttle.record_failure();
            return err(StatusCode::BAD_GATEWAY, format!("反馈服务暂不可用:{e}")).into_response();
        }
    };
    let status = response.status();
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false);
    let data = if is_json {
        response.json::<Value>().await.unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !status.is_success() || data.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        throttle.record_failure();
        let status_code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let status_code = if status_code.is_client_error() || status_code.is_server_error() {
            status_code
        } else {
            StatusCode::BAD_GATEWAY
        };
        let message = data
            .get("error")
            .or_else(|| data.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("上游错误");
        return err(status_code, message).into_response();
    }

    throttle.record_success();
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
    Json(json!({
        "success": true,
        "id": id,
        "message": format!("反馈已收到 (ID: {id})"),
        "email_sent": data.get("email_sent").and_then(|v| v.as_bool()).unwrap_or(false),
    }))
    .into_response()
}

// ── 测速 / 模型探测 / 兼容性 ─────────────────────────────────

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
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    Json(test_provider_connection(provider).await).into_response()
}

pub async fn query_provider_usage(Path(id): Path<String>) -> impl IntoResponse {
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
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let result = query_provider_usage_impl(provider).await;
    Json(result).into_response()
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
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
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
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
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
        "message": "模型映射已自动填充",
    }))
    .into_response()
}

#[allow(dead_code)]
pub fn _state_typecheck(_s: Arc<AdminState>) -> bool {
    true
}

#[derive(Serialize)]
pub struct _Marker;

#[cfg(test)]
mod tests {
    use super::*;

    fn with_isolated_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        static HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = HOME_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        struct EnvGuard {
            home: Option<std::ffi::OsString>,
            userprofile: Option<std::ffi::OsString>,
            root: PathBuf,
        }

        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.userprofile {
                    Some(value) => std::env::set_var("USERPROFILE", value),
                    None => std::env::remove_var("USERPROFILE"),
                }
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        let root = std::env::temp_dir().join(format!(
            "cas-admin-test-{}-{}",
            std::process::id(),
            random_hex(6)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let env_guard = EnvGuard {
            home: std::env::var_os("HOME"),
            userprofile: std::env::var_os("USERPROFILE"),
            root: root.clone(),
        };
        std::env::set_var("HOME", &root);
        std::env::remove_var("USERPROFILE");

        let result = f(&root);
        drop(env_guard);
        result
    }

    fn config_with_secret() -> Value {
        json!({
            "version": APP_VERSION,
            "activeProvider": "p1",
            "gatewayApiKey": "cas_existing",
            "providers": [{
                "id": "p1",
                "name": "Provider One",
                "baseUrl": "https://api.example.com/v1",
                "authScheme": "bearer",
                "apiFormat": "openai_chat",
                "apiKey": "sk-existing",
                "extraHeaders": {"x-extra-secret": "secret-header"},
                "models": {"default": "model-one"},
                "sortIndex": 0
            }],
            "settings": {
                "theme": "default",
                "language": "zh",
                "proxyPort": 18080,
                "adminPort": 18081,
                "autoStart": false,
                "autoApplyOnStart": true,
                "exposeAllProviderModels": false,
                "restoreCodexOnExit": true,
                "updateUrl": "https://github.com/Cmochance/codex-app-transfer/releases/latest/download/latest.json"
            }
        })
    }

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
                .contains("认证失败"));
        });
    }

    #[test]
    fn provider_usage_preserves_legacy_no_key_and_unsupported_payloads() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let no_key = json!({
                "name": "DeepSeek",
                "baseUrl": "https://api.deepseek.com",
            });
            let result = query_provider_usage_impl(&no_key).await;
            assert_eq!(result["success"], json!(false));
            assert_eq!(result["message"], json!("请先保存 API Key"));

            let unsupported = json!({
                "name": "Unknown",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "test-key",
            });
            let result = query_provider_usage_impl(&unsupported).await;
            assert_eq!(result["success"], json!(true));
            assert_eq!(result["supported"], json!(false));
            assert_eq!(result["items"], json!([]));
        });
    }

    #[test]
    fn balance_payloads_match_legacy_normalization() {
        let deepseek = normalize_balance_payload(
            "deepseek",
            &json!({
                "balance_infos": [{
                    "currency": "CNY",
                    "total_balance": "8.5",
                    "granted_balance": "10",
                    "topped_up_balance": "1.5"
                }]
            }),
        );
        assert_eq!(deepseek[0]["label"], json!("CNY"));
        assert_eq!(deepseek[0]["remaining"], json!(8.5));
        assert_eq!(deepseek[0]["total"], json!(10.0));
        assert_eq!(deepseek[0]["used"], json!(1.5));

        let openrouter = normalize_balance_payload(
            "openrouter",
            &json!({"data": {"total_credits": 12.0, "total_usage": 5.25}}),
        );
        assert_eq!(openrouter[0]["label"], json!("credits"));
        assert_eq!(openrouter[0]["remaining"], json!(6.75));
        assert_eq!(openrouter[0]["unit"], json!("USD"));

        let generic = normalize_balance_payload(
            "siliconflow",
            &json!({"data": {"availableBalance": "3.25", "totalBalance": "4", "usedBalance": "0.75", "currency": "CNY"}}),
        );
        assert_eq!(generic[0]["remaining"], json!(3.25));
        assert_eq!(generic[0]["total"], json!(4.0));
        assert_eq!(generic[0]["used"], json!(0.75));
        assert_eq!(generic[0]["unit"], json!("CNY"));
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

    #[test]
    fn config_backup_list_uses_real_files() {
        with_isolated_home(|home| {
            let cfg = config_with_secret();
            save_registry(&cfg).unwrap();

            let backup = create_config_backup("manual").unwrap();
            let name = backup.get("name").and_then(|v| v.as_str()).unwrap();
            assert!(name.starts_with("config-"));
            assert!(name.ends_with(".json"));
            assert!(backup.get("size").and_then(|v| v.as_u64()).unwrap() > 0);

            let backup_path = home.join(".codex-app-transfer").join("backups").join(name);
            assert!(backup_path.is_file());
            let saved: Value =
                serde_json::from_str(&fs::read_to_string(&backup_path).unwrap()).unwrap();
            assert_eq!(saved["providers"][0]["apiKey"], json!("sk-existing"));

            let backups = list_config_backups().unwrap();
            assert_eq!(backups.len(), 1);
            assert_eq!(backups[0]["name"], backup["name"]);
        });
    }

    #[test]
    fn import_config_backs_up_and_preserves_existing_provider_secrets_when_missing() {
        with_isolated_home(|_| {
            save_registry(&config_with_secret()).unwrap();

            let incoming = json!({
                "format": "codex-app-transfer.config",
                "config": {
                    "version": "1.0.3",
                    "activeProvider": "p1",
                    "gatewayApiKey": "cas_imported",
                    "providers": [{
                        "id": "p1",
                        "name": "Imported Provider",
                        "baseUrl": "https://imported.example.com/v1",
                        "authScheme": "bearer",
                        "apiFormat": "openai_chat",
                        "models": {"default": "imported-model"}
                    }],
                    "settings": {"proxyPort": 19090}
                }
            });

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let response = runtime.block_on(async { import_config(Json(incoming)).await });
            assert_eq!(response.into_response().status(), StatusCode::OK);

            let saved = load_registry().unwrap();
            assert_eq!(saved["activeProvider"], json!("p1"));
            assert_eq!(saved["gatewayApiKey"], json!("cas_imported"));
            assert_eq!(saved["settings"]["proxyPort"], json!(19090));
            assert_eq!(saved["providers"][0]["name"], json!("Imported Provider"));
            assert_eq!(saved["providers"][0]["apiKey"], json!("sk-existing"));
            assert_eq!(
                saved["providers"][0]["extraHeaders"]["x-extra-secret"],
                json!("secret-header")
            );

            let backups = list_config_backups().unwrap();
            assert_eq!(backups.len(), 1);
            assert!(backups[0]
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap()
                .contains("before-import"));
        });
    }
}
