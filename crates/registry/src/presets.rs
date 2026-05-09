//! 内置 provider 预设(对应 `backend/config.py` BUILTIN_PRESETS).
//!
//! 这里把 Python 中的 8 条预设固化为 JSON 字面量,运行时一次性 parse 成
//! `Vec<Value>`(保留键顺序).采用 JSON 而非 Rust struct 的原因:
//! - 不同预设字段集差异大(notices / baseUrlOptions / requestOptionPresets
//!   / extraHeaders / baseUrlHint 等只在部分项出现)
//! - 直接拷自 Python 源,人工 diff 容易,后续迁移负担最小
//! - 与 Python 版输出做字节级 diff 时,对象 key 顺序与 Python dict 一致
//!
//! **维护契约**:此处任何字段调整都必须 1:1 同步到 backend/config.py;
//! 测试 `presets_json_matches_python_dump` 会校验之.

use once_cell::sync::Lazy;
use serde_json::Value;

const BUILTIN_PRESETS_JSON: &str = include_str!("./presets_data.json");

pub fn builtin_presets() -> &'static [Value] {
    PRESETS.as_slice()
}

static PRESETS: Lazy<Vec<Value>> = Lazy::new(|| {
    let v: Value =
        serde_json::from_str(BUILTIN_PRESETS_JSON).expect("BUILTIN_PRESETS JSON parse failed");
    let arr = v
        .as_array()
        .expect("BUILTIN_PRESETS_JSON must be a JSON array");
    arr.clone()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_count_matches_python() {
        // 当前 9 条 builtin presets:
        // deepseek / kimi / kimi-code / xiaomi-mimo-payg / xiaomi-mimo-token-plan
        // / zhipu / bailian / minimax / google-ai-studio
        // (2026-05-10 加 Google AI Studio Gemini preset)
        assert_eq!(builtin_presets().len(), 9);
    }

    #[test]
    fn minimax_preset_exists_and_uses_official_minimaxi_base_url() {
        let minimax = builtin_presets()
            .iter()
            .find(|p| p["id"] == "minimax")
            .expect("MiniMax preset must exist as builtin entry");
        assert_eq!(minimax["baseUrl"], "https://api.minimaxi.com/v1");
        assert_eq!(minimax["apiFormat"], "openai_chat");
        assert_eq!(minimax["isBuiltin"], true);
        assert!(
            minimax["models"]["default"]
                .as_str()
                .unwrap_or("")
                .starts_with("MiniMax-M2"),
            "default model 必须是 MiniMax-M2.x 系列"
        );
    }

    #[test]
    fn google_ai_studio_preset_exists_with_openai_compat_endpoint() {
        let g = builtin_presets()
            .iter()
            .find(|p| p["id"] == "google-ai-studio")
            .expect("Google AI Studio preset must exist as builtin entry");
        assert_eq!(
            g["baseUrl"],
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(
            g["apiFormat"], "openai_chat",
            "Google 官方 OpenAI Chat Completions 兼容 endpoint,走 chat 路径"
        );
        assert_eq!(g["authScheme"], "bearer");
        assert_eq!(g["isBuiltin"], true);
        let default_model = g["models"]["default"].as_str().unwrap_or("");
        assert!(
            default_model.starts_with("gemini-3"),
            "default model 必须是 Gemini 3.x 系列(2026-05 主流),实际:{default_model}"
        );
    }

    #[test]
    fn every_preset_has_id_name_baseurl() {
        for p in builtin_presets() {
            let obj = p.as_object().expect("preset 必须是对象");
            assert!(obj.contains_key("id"));
            assert!(obj.contains_key("name"));
            assert!(obj.contains_key("baseUrl"));
            assert!(obj.contains_key("apiFormat"));
        }
    }

    #[test]
    fn every_preset_is_builtin_true() {
        for p in builtin_presets() {
            assert_eq!(p["isBuiltin"], serde_json::Value::Bool(true));
        }
    }
}
