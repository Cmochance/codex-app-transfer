//! `/api/providers/*` + `/api/presets` —— provider 增删改 / 测连通性 /
//! 模型抓取 / 余额 / 内置 presets.
//!
//! 二级拆分:
//! - `crud`:增删改 + activate / reorder / draft / update_models
//! - `test`:连通性测试 + compatibility
//! - `models`:模型列表抓取 + autofill
//! - `balance`:余额 / 用量查询
//! - `presets`:内置 presets

pub mod balance;
pub mod crud;
pub mod models;
pub mod presets;
pub mod test;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use codex_app_transfer_registry::{
    normalize_model_mappings, strip_internal_model_suffix, RawConfig, MODEL_ORDER,
};
use serde_json::{json, Value};

static ID_COUNTER: AtomicU32 = AtomicU32::new(0);

pub(super) fn fresh_provider_id(existing: &[String]) -> String {
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

pub(super) fn provider_supports_1m(provider: &Value) -> bool {
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

pub(super) fn provider_default_model(provider: &Value) -> String {
    let raw = provider
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    strip_internal_model_suffix(raw)
}

pub(super) fn provider_model_mappings(provider: &Value) -> Value {
    provider.get("models").cloned().unwrap_or_else(|| json!({}))
}

pub(super) fn provider_model_capabilities(provider: &Value) -> Value {
    provider
        .get("modelCapabilities")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

pub(super) fn provider_display_name(provider: &Value) -> String {
    provider
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Provider")
        .to_owned()
}

pub(super) fn normalize_provider_api_format(api_format: Option<&str>) -> &'static str {
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

pub(super) fn provider_api_key(provider: &Value) -> String {
    provider
        .get("apiKey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

pub(super) fn provider_test_model(provider: &Value) -> String {
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

pub(super) fn provider_index(cfg: &RawConfig, id: &str) -> Option<usize> {
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

pub(super) fn active_provider(cfg: &RawConfig) -> Option<Value> {
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

pub(super) fn clean_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

pub(super) fn replace_path_suffix(url: &str, suffixes: &[&str], replacement: &str) -> String {
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
