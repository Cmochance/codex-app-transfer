//! Codex model catalog updater.
//!
//! Codex CLI 0.128+ reads `model_catalog_json` for per-model context windows.
//! The older `model_context_window` root key is kept by `apply.rs` as a
//! compatibility hint, but the catalog is the path verified against current
//! Codex releases.
//!
//! The catalog is merged into Codex App Transfer's existing
//! `~/.codex-app-transfer/config.json` file instead of creating another file
//! under `~/.codex`. Codex ignores unrelated top-level fields and reads the
//! `models` array from the configured JSON path.

use codex_app_transfer_registry::{
    has_internal_one_m_suffix, load_raw_config, normalize_model_mappings, save_raw_config,
    strip_internal_model_suffix, MODEL_SLOTS,
};
use serde_json::{json, Value};

use crate::CodexError;

pub const CODEX_MODEL_CATALOG_KEY: &str = "model_catalog_json";

const DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT: u64 = 95;
const DEFAULT_CONTEXT_WINDOW: u64 = 258_400;
const ONE_M_CONTEXT_WINDOW: u64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub slug: String,
    pub display_name: String,
    pub context_window: u64,
    pub effective_context_window_percent: u64,
}

pub fn upsert_catalog_models(
    path: &std::path::Path,
    models: &[CatalogModel],
) -> Result<(), CodexError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut data = read_json_object(path)?;
    data["models"] = Value::Array(models.iter().map(model_to_json).collect::<Vec<_>>());
    save_raw_config(path, &data)?;
    Ok(())
}

pub fn clear_catalog_models(path: &std::path::Path) -> Result<(), CodexError> {
    let mut data = match load_raw_config(path) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(_) => return Ok(()),
        Err(codex_app_transfer_registry::IoError::NotFound(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let Some(obj) = data.as_object_mut() else {
        return Ok(());
    };
    if obj.remove("models").is_some() {
        save_raw_config(path, &data)?;
    }
    Ok(())
}

fn read_json_object(path: &std::path::Path) -> Result<Value, CodexError> {
    match load_raw_config(path) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(_) => Ok(default_registry_config_value()),
        Err(codex_app_transfer_registry::IoError::NotFound(_)) => {
            Ok(default_registry_config_value())
        }
        Err(e) => Err(e.into()),
    }
}

fn default_registry_config_value() -> Value {
    serde_json::to_value(codex_app_transfer_registry::Config::default())
        .unwrap_or_else(|_| json!({}))
}

pub fn catalog_models_for_provider(
    provider_name: &str,
    default_model: &str,
    supports_1m: bool,
    model_mappings: Option<&Value>,
    model_capabilities: Option<&Value>,
) -> Vec<CatalogModel> {
    let default_model_clean = strip_internal_model_suffix(default_model);
    let default_model = default_model_clean.trim();
    let mappings = normalize_model_mappings(model_mappings);
    let mut models = Vec::new();
    for slot in MODEL_SLOTS {
        let Some(openai_id) = slot.openai_id else {
            continue;
        };
        let mapped = mappings.get(slot.key).map(|s| s.trim()).unwrap_or("");
        let target = if mapped.is_empty() {
            default_model
        } else {
            mapped
        };
        let target_clean = strip_internal_model_suffix(target);
        let context_window = context_window_for_model(
            target,
            target_clean.trim(),
            default_model,
            supports_1m,
            model_capabilities,
        );
        models.push(catalog_model(
            openai_id,
            provider_name,
            target_clean.trim(),
            context_window,
        ));
    }
    if !default_model.is_empty() && !models.iter().any(|m| m.slug == default_model) {
        models.push(catalog_model(
            default_model,
            provider_name,
            default_model,
            if supports_1m {
                ONE_M_CONTEXT_WINDOW
            } else {
                DEFAULT_CONTEXT_WINDOW
            },
        ));
    }
    models
}

pub fn strip_model_suffix(model: &str) -> String {
    strip_internal_model_suffix(model)
}

fn context_window_for_model(
    original_model: &str,
    clean_model: &str,
    default_model: &str,
    default_supports_1m: bool,
    model_capabilities: Option<&Value>,
) -> u64 {
    if clean_model.is_empty() {
        return DEFAULT_CONTEXT_WINDOW;
    }
    if clean_model == default_model {
        if default_supports_1m {
            ONE_M_CONTEXT_WINDOW
        } else {
            DEFAULT_CONTEXT_WINDOW
        }
    } else if model_supports_1m(original_model, clean_model, model_capabilities) {
        ONE_M_CONTEXT_WINDOW
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

fn model_supports_1m(
    original_model: &str,
    clean_model: &str,
    model_capabilities: Option<&Value>,
) -> bool {
    if has_internal_one_m_suffix(original_model) {
        return true;
    }
    let lower = clean_model.to_ascii_lowercase();
    if lower.starts_with("deepseek-v4-") || lower.starts_with("qwen3.6-") {
        return true;
    }
    let Some(caps) = model_capabilities.and_then(Value::as_object) else {
        return false;
    };
    for key in [clean_model, original_model.trim()] {
        if let Some(b) = caps
            .get(key)
            .and_then(|v| v.get("supports1m"))
            .and_then(Value::as_bool)
        {
            return b;
        }
    }
    false
}

fn catalog_model(
    slug: &str,
    provider_name: &str,
    default_model: &str,
    context_window: u64,
) -> CatalogModel {
    let target = if default_model.is_empty() {
        slug
    } else {
        default_model
    };
    CatalogModel {
        slug: slug.to_owned(),
        display_name: format!("{provider_name} / {target}"),
        context_window,
        effective_context_window_percent: DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT,
    }
}

fn model_to_json(model: &CatalogModel) -> Value {
    let mut entry = codex_builtin_template(&model.slug).unwrap_or_else(generic_model_template);
    entry["slug"] = Value::String(model.slug.clone());
    entry["display_name"] = Value::String(model.display_name.clone());
    entry["description"] = Value::String(format!(
        "Routed through Codex App Transfer as {}.",
        model.display_name
    ));
    entry["context_window"] = json!(model.context_window);
    entry["max_context_window"] = json!(model.context_window);
    entry["effective_context_window_percent"] = json!(model.effective_context_window_percent);
    entry
}

fn codex_builtin_template(slug: &str) -> Option<Value> {
    match slug {
        "gpt-5.5" => Some(codex_model_template(
            slug,
            "GPT-5.5",
            "Frontier model for complex coding, research, and real-world work.",
            "medium",
            codex_reasoning_levels(),
            0,
            json!(["fast"]),
            json!({
                "message": "GPT-5.5 is now available in Codex. It's our strongest agentic coding model yet, built to reason through large codebases, check assumptions with tools, and keep going until the work is done.\n\nLearn more: https://openai.com/index/introducing-gpt-5-5/\n\n"
            }),
            Value::Null,
            "none",
            "low",
            "text_and_image",
            json!({"mode": "tokens", "limit": 10000}),
            true,
        )),
        "gpt-5.4" => Some(codex_model_template(
            slug,
            "gpt-5.4",
            "Strong model for everyday coding.",
            "xhigh",
            codex_reasoning_levels(),
            2,
            json!(["fast"]),
            Value::Null,
            Value::Null,
            "none",
            "low",
            "text_and_image",
            json!({"mode": "tokens", "limit": 10000}),
            true,
        )),
        "gpt-5.4-mini" => Some(codex_model_template(
            slug,
            "GPT-5.4-Mini",
            "Small, fast, and cost-efficient model for simpler coding tasks.",
            "medium",
            codex_reasoning_levels(),
            4,
            json!([]),
            Value::Null,
            Value::Null,
            "none",
            "medium",
            "text_and_image",
            json!({"mode": "tokens", "limit": 10000}),
            true,
        )),
        "gpt-5.3-codex" => Some(codex_model_template(
            slug,
            "gpt-5.3-codex",
            "Coding-optimized model.",
            "medium",
            codex_reasoning_levels(),
            6,
            json!([]),
            Value::Null,
            gpt54_upgrade(),
            "none",
            "low",
            "text",
            json!({"mode": "tokens", "limit": 10000}),
            true,
        )),
        "gpt-5.2" => Some(codex_model_template(
            slug,
            "gpt-5.2",
            "Optimized for professional work and long-running agents.",
            "medium",
            gpt52_reasoning_levels(),
            10,
            json!([]),
            Value::Null,
            gpt54_upgrade(),
            "auto",
            "low",
            "text",
            json!({"mode": "bytes", "limit": 10000}),
            false,
        )),
        _ => None,
    }
}

fn codex_model_template(
    slug: &str,
    display_name: &str,
    description: &str,
    default_reasoning_level: &str,
    supported_reasoning_levels: Value,
    priority: u64,
    additional_speed_tiers: Value,
    availability_nux: Value,
    upgrade: Value,
    default_reasoning_summary: &str,
    default_verbosity: &str,
    web_search_tool_type: &str,
    truncation_policy: Value,
    supports_image_detail_original: bool,
) -> Value {
    json!({
        "slug": slug,
        "display_name": display_name,
        "description": description,
        "default_reasoning_level": default_reasoning_level,
        "supported_reasoning_levels": supported_reasoning_levels,
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "additional_speed_tiers": additional_speed_tiers,
        "availability_nux": availability_nux,
        "upgrade": upgrade,
        "base_instructions": "",
        "supports_reasoning_summaries": true,
        "default_reasoning_summary": default_reasoning_summary,
        "support_verbosity": true,
        "default_verbosity": default_verbosity,
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": web_search_tool_type,
        "truncation_policy": truncation_policy,
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": supports_image_detail_original,
        "context_window": 272000,
        "max_context_window": 272000,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": true
    })
}

fn generic_model_template() -> Value {
    json!({
        "slug": "",
        "display_name": "",
        "description": "",
        "default_reasoning_level": "high",
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balanced speed and reasoning depth"},
            {"effort": "high", "description": "Greater reasoning depth for complex tasks"}
        ],
        "shell_type": "default",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 10,
        "additional_speed_tiers": [],
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": "",
        "supports_reasoning_summaries": false,
        "default_reasoning_summary": "auto",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "web_search_tool_type": "text",
        "truncation_policy": {"mode": "bytes", "limit": 4000000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 258400,
        "max_context_window": 258400,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false
    })
}

fn codex_reasoning_levels() -> Value {
    json!([
        {"effort": "low", "description": "Fast responses with lighter reasoning"},
        {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
        {"effort": "high", "description": "Greater reasoning depth for complex problems"},
        {"effort": "xhigh", "description": "Extra high reasoning depth for complex problems"}
    ])
}

fn gpt52_reasoning_levels() -> Value {
    json!([
        {"effort": "low", "description": "Balances speed with some reasoning; useful for straightforward queries and short explanations"},
        {"effort": "medium", "description": "Provides a solid balance of reasoning depth and latency for general-purpose tasks"},
        {"effort": "high", "description": "Maximizes reasoning depth for complex or ambiguous problems"},
        {"effort": "xhigh", "description": "Extra high reasoning for complex problems"}
    ])
}

fn gpt54_upgrade() -> Value {
    json!({
        "model": "gpt-5.4",
        "migration_markdown": "Introducing GPT-5.4\n\nCodex just got an upgrade with GPT-5.4, our most capable model for professional work. It outperforms prior models while being more token efficient, with notable improvements on long-running tasks, tool calling, computer use, and frontend development.\n\nLearn more: https://openai.com/index/introducing-gpt-5-4\n\nYou can always keep using GPT-5.3-Codex if you prefer.\n"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_suffix_keeps_upstream_model_id_clean() {
        assert_eq!(strip_model_suffix("deepseek-v4-pro[1m]"), "deepseek-v4-pro");
        assert_eq!(
            strip_model_suffix("deepseek-v4-pro [1M]"),
            "deepseek-v4-pro"
        );
        assert_eq!(strip_model_suffix("deepseek-v4-pro"), "deepseek-v4-pro");
        assert_eq!(
            strip_model_suffix("deepseek-v4-pro[beta]"),
            "deepseek-v4-pro[beta]"
        );
    }

    #[test]
    fn one_m_catalog_uses_95_percent_effective_window() {
        let models =
            catalog_models_for_provider("DeepSeek", "deepseek-v4-pro[1m]", true, None, None);
        let deepseek = models.iter().find(|m| m.slug == "deepseek-v4-pro").unwrap();
        assert_eq!(deepseek.context_window, 1_000_000);
        assert_eq!(deepseek.effective_context_window_percent, 95);
        assert!(models.iter().any(|m| m.slug == "gpt-5.5"));
    }

    #[test]
    fn builtin_slug_catalog_preserves_codex_capabilities() {
        let models = catalog_models_for_provider("DeepSeek", "deepseek-v4-pro", true, None, None);
        let gpt55 = models.iter().find(|m| m.slug == "gpt-5.5").unwrap();
        let entry = model_to_json(gpt55);

        assert_eq!(entry["context_window"], 1_000_000);
        assert_eq!(entry["max_context_window"], 1_000_000);
        assert_eq!(entry["apply_patch_tool_type"], "freeform");
        assert_eq!(entry["supports_parallel_tool_calls"], true);
        assert_eq!(entry["supports_search_tool"], true);
        assert_eq!(entry["supports_reasoning_summaries"], true);
        assert_eq!(entry["web_search_tool_type"], "text_and_image");
    }

    #[test]
    fn catalog_uses_slot_mapping_and_per_model_windows() {
        let mappings = json!({
            "default": "deepseek-v4-pro",
            "gpt_5_5": "short-context-model",
            "gpt_5_4": "qwen3.6-plus",
            "gpt_5_4_mini": "custom-long-model"
        });
        let capabilities = json!({
            "short-context-model": {"supports1m": false},
            "custom-long-model": {"supports1m": true}
        });

        let models = catalog_models_for_provider(
            "Mixed",
            "deepseek-v4-pro",
            true,
            Some(&mappings),
            Some(&capabilities),
        );
        let gpt55 = models.iter().find(|m| m.slug == "gpt-5.5").unwrap();
        let gpt54 = models.iter().find(|m| m.slug == "gpt-5.4").unwrap();
        let mini = models.iter().find(|m| m.slug == "gpt-5.4-mini").unwrap();
        let codex = models.iter().find(|m| m.slug == "gpt-5.3-codex").unwrap();

        assert_eq!(gpt55.display_name, "Mixed / short-context-model");
        assert_eq!(gpt55.context_window, 258_400);
        assert_eq!(gpt54.display_name, "Mixed / qwen3.6-plus");
        assert_eq!(gpt54.context_window, 1_000_000);
        assert_eq!(mini.context_window, 1_000_000);
        assert_eq!(
            codex.display_name, "Mixed / deepseek-v4-pro",
            "empty slot mappings should still document the default fallback target"
        );
        assert_eq!(codex.context_window, 1_000_000);
    }

    #[test]
    fn clear_catalog_models_removes_only_top_level_catalog_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = serde_json::json!({
            "version": "1.0.4",
            "providers": [],
            "models": [{"slug": "gpt-5.5"}],
            "settings": {"theme": "default"}
        });
        codex_app_transfer_registry::save_raw_config(&path, &original).unwrap();

        clear_catalog_models(&path).unwrap();

        let v = codex_app_transfer_registry::load_raw_config(&path).unwrap();
        assert_eq!(v["version"], "1.0.4");
        assert_eq!(v["settings"]["theme"], "default");
        assert!(v.get("models").is_none());
    }

    #[test]
    fn upsert_catalog_models_preserves_existing_config_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = serde_json::json!({
            "version": "1.0.4",
            "activeProvider": null,
            "gatewayApiKey": "cas_test",
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
        });
        codex_app_transfer_registry::save_raw_config(&path, &original).unwrap();

        let models = catalog_models_for_provider("DeepSeek", "deepseek-v4-pro", true, None, None);
        upsert_catalog_models(&path, &models).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.ends_with(b"\n"),
            "main config.json keeps existing no-newline convention"
        );
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["version"], "1.0.4");
        assert_eq!(v["gatewayApiKey"], "cas_test");
        assert_eq!(v["settings"]["theme"], "default");
        assert!(v["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["slug"] == "deepseek-v4-pro"));
        let _typed: codex_app_transfer_registry::Config =
            serde_json::from_value(v).expect("top-level models must not break registry config");
    }
}
