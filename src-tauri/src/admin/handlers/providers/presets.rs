//! `/api/presets` —— 内置 provider presets.

use axum::{response::IntoResponse, Json};
use codex_app_transfer_registry::builtin_presets;
use serde_json::{json, Value};

/// 暂时隐藏的 builtin preset id。
///
/// 2026-05-13 起 `antigravity-oauth` 因上游存在封号风险临时下线 dashboard 入口卡片。
/// 底层 OAuth handler (`/api/antigravity-oauth/*`)、`gemini_oauth::antigravity`、
/// `antigravity_oauth` adapter、registry healing、`builtin_presets.json` golden
/// 快照与 i18n 文案均保留不动:仅在面向用户的 `/api/presets` 列表里跳过，
/// 已存在的 Antigravity provider 依旧可用。恢复时把对应 id 从列表移除即可。
const HIDDEN_PRESET_IDS: &[&str] = &["antigravity-oauth"];

pub async fn list_presets() -> impl IntoResponse {
    let presets: Vec<Value> = builtin_presets()
        .iter()
        .filter(|preset| {
            let id = preset.get("id").and_then(Value::as_str).unwrap_or("");
            !HIDDEN_PRESET_IDS.contains(&id)
        })
        .cloned()
        .collect();
    Json(json!({"presets": presets})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dashboard 入口卡片不允许返回临时下线的 builtin preset。
    #[test]
    fn hidden_preset_ids_are_filtered_out_of_list_presets_payload() {
        let raw_total = builtin_presets().len();
        let visible: Vec<&Value> = builtin_presets()
            .iter()
            .filter(|preset| {
                let id = preset.get("id").and_then(Value::as_str).unwrap_or("");
                !HIDDEN_PRESET_IDS.contains(&id)
            })
            .collect();
        assert_eq!(
            visible.len(),
            raw_total - HIDDEN_PRESET_IDS.len(),
            "list_presets 必须只过滤显式声明的 hidden preset",
        );
        for hidden in HIDDEN_PRESET_IDS {
            assert!(
                visible
                    .iter()
                    .all(|preset| preset.get("id").and_then(Value::as_str) != Some(*hidden)),
                "hidden preset {hidden} 仍然出现在 /api/presets 返回中",
            );
            assert!(
                builtin_presets()
                    .iter()
                    .any(|preset| preset.get("id").and_then(Value::as_str) == Some(*hidden)),
                "hidden preset {hidden} 的底层数据被误删 — 应保留以便快速恢复",
            );
        }
    }
}
