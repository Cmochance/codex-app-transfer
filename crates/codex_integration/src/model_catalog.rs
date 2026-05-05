//! Codex model catalog writer.
//!
//! Codex CLI 0.128+ reads `model_catalog_json` for per-model context windows.
//! The older `model_context_window` root key is kept by `apply.rs` as a
//! compatibility hint, but the catalog is the path verified against current
//! Codex releases.

use std::fs;

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

pub fn write_catalog(path: &std::path::Path, models: &[CatalogModel]) -> Result<(), CodexError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = json!({
        "fetched_at": "codex-app-transfer",
        "client_version": "codex-app-transfer",
        "models": models.iter().map(model_to_json).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec_pretty(&data)?;
    fs::write(path, [bytes.as_slice(), b"\n"].concat())?;
    Ok(())
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
}
