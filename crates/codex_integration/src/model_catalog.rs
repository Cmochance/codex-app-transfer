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

use codex_app_transfer_registry::{load_raw_config, save_raw_config};
use serde_json::{json, Value};

use crate::CodexError;

pub const CODEX_MODEL_CATALOG_KEY: &str = "model_catalog_json";

const DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT: u64 = 95;

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
) -> Vec<CatalogModel> {
    let default_model_clean = strip_model_suffix(default_model);
    let default_model = default_model_clean.trim();
    let context_window = if supports_1m { 1_000_000 } else { 258_400 };
    let mut models = vec![
        catalog_model("gpt-5.5", provider_name, default_model, context_window),
        catalog_model("gpt-5.4", provider_name, default_model, context_window),
        catalog_model("gpt-5.4-mini", provider_name, default_model, context_window),
        catalog_model(
            "gpt-5.3-codex",
            provider_name,
            default_model,
            context_window,
        ),
        catalog_model("gpt-5.2", provider_name, default_model, context_window),
    ];
    if !default_model.is_empty() && !models.iter().any(|m| m.slug == default_model) {
        models.push(catalog_model(
            default_model,
            provider_name,
            default_model,
            context_window,
        ));
    }
    models
}

pub fn strip_model_suffix(model: &str) -> String {
    let trimmed = model.trim();
    let Some(end) = trimmed.strip_suffix(']') else {
        return trimmed.to_owned();
    };
    let Some(open) = end.rfind('[') else {
        return trimmed.to_owned();
    };
    end[..open].trim_end().to_owned()
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
    json!({
        "slug": model.slug,
        "display_name": model.display_name,
        "description": format!("Routed through Codex App Transfer as {}.", model.display_name),
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
        "context_window": model.context_window,
        "max_context_window": model.context_window,
        "effective_context_window_percent": model.effective_context_window_percent,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false
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
    }

    #[test]
    fn one_m_catalog_uses_95_percent_effective_window() {
        let models = catalog_models_for_provider("DeepSeek", "deepseek-v4-pro[1m]", true);
        let deepseek = models.iter().find(|m| m.slug == "deepseek-v4-pro").unwrap();
        assert_eq!(deepseek.context_window, 1_000_000);
        assert_eq!(deepseek.effective_context_window_percent, 95);
        assert!(models.iter().any(|m| m.slug == "gpt-5.5"));
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

        let models = catalog_models_for_provider("DeepSeek", "deepseek-v4-pro", true);
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
