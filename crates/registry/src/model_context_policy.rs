use serde_json::Value;

use crate::{has_internal_one_m_suffix, strip_internal_model_suffix};

pub const ONE_M_CONTEXT_WINDOW: u64 = 1_000_000;

/// 与 `presets_data.json` 中 builtin preset 的 `modelCapabilities.context_window`
/// 对齐。用于在未显式配置 capability 时提供保守默认值。
pub fn documented_context_window(model_id: &str) -> Option<u64> {
    // QoderWork 模型(原始 key gm51model/dmodel/l/kmodel/mmodel 等):用其支持的**最大** context
    // (多数 1M,kmodel 256k,auto 180k,mmodel 200k)。key 非人类可读、不会命中下面的显示名 match。
    if let Some(cw) = crate::qoder_catalog::qoder_max_context(model_id) {
        return Some(cw);
    }
    match model_id.trim().to_ascii_lowercase().as_str() {
        // DeepSeek
        "deepseek-v4-pro" | "deepseek-v4-flash" => Some(ONE_M_CONTEXT_WINDOW),
        // Kimi (月之暗面) + Kimi Code
        "kimi-for-coding" | "kimi-k2.5" | "kimi-k2.6" | "kimi-k2.7" | "kimi-2.6" => Some(262_144),
        "moonshot-v1-8k" | "moonshot-v1-8k-vision-preview" => Some(8192),
        "moonshot-v1-32k" | "moonshot-v1-32k-vision-preview" => Some(32768),
        "moonshot-v1-128k" | "moonshot-v1-auto" | "moonshot-v1-128k-vision-preview" => {
            Some(131_072)
        }
        // Xiaomi MiMo
        "mimo-v2-pro" | "mimo-v2.5" | "mimo-v2.5-pro" => Some(ONE_M_CONTEXT_WINDOW),
        "mimo-v2-flash" | "mimo-v2-omni" => Some(262_144),
        // 智谱 GLM
        "glm-5.2" => Some(ONE_M_CONTEXT_WINDOW),
        "glm-5.1" | "glm-4.7" => Some(200_000),
        // 阿里云百炼 Qwen 3.6
        "qwen3.6-plus" | "qwen3.6-flash" => Some(ONE_M_CONTEXT_WINDOW),
        // MiniMax
        "minimax-m2.7" => Some(204_800),
        "minimax-m3" => Some(ONE_M_CONTEXT_WINDOW),
        // Google Gemini:仅限**确有 ≥1M 上下文**的世代 —— Gemini 1.5 / 2.x / 3.x
        // (antigravity 全系落在 2.x / 3.x,且上游 `antigravity_models.json` 对它们都
        // 声明 context_length = 1_048_576)。真机 antigravity 实际映射的是带后缀的 id
        // (`gemini-3-flash-agent` / `gemini-3.1-pro-low` 等,已被版本前缀覆盖),唯
        // `gemini-pro-agent`(antigravity 的 Gemini 3.1 Pro 别名)无版本号前缀,单列。
        // 逐 id 精确枚举追不上上游上新,故按世代前缀统一判 1M。
        // **不**放宽到裸 `gemini-pro` / `gemini-1.0-*` 等老世代(真实上限 ~32k):给它们
        // 误报 1M 会让 autocompact 等到 800k 才触发、反而撑爆上游(#453 P2);老 id 维持
        // None → 落 258_400 兜底(与改动前一致,无回归)。`gemini-*-image` 是图像模型
        // (`context_length` 为 null,见 MOC-222),不参与文本上下文,排除。
        m if !m.contains("image")
            && (m.starts_with("gemini-1.5")
                || m.starts_with("gemini-2")
                || m.starts_with("gemini-3")
                || m.starts_with("gemini-pro-agent")) =>
        {
            Some(ONE_M_CONTEXT_WINDOW)
        }
        _ => None,
    }
}

/// 统一的 1M 判定策略(优先级从高到低):
/// 1. `[1m]` 内部后缀
/// 2. 显式 `modelCapabilities[model].context_window` 数值(权威:>= 1M → true,< 1M → false;
///    不再 fall through 到 documented,与 `model_catalog::explicit_context_window` 同优先级)
/// 3. 文档化 context_window >= 1M
/// 4. `modelCapabilities[model].supports1m = true/false`
pub fn model_supports_1m(original_model: &str, model_capabilities: Option<&Value>) -> bool {
    if has_internal_one_m_suffix(original_model) {
        return true;
    }
    let clean_model = strip_internal_model_suffix(original_model);
    let clean_model = clean_model.trim();
    if clean_model.is_empty() {
        return false;
    }

    // [MOC-241] 显式 context_window 数值是最高优先级权威:用户/前端显式声明窗口时直接按它
    // 判 1M,**不再** fall through 到 documented。否则 documented 对 Gemini 1.5/2/3 先判 1M,
    // Gemini 1M 开关「关」写的 600000 cap 形同虚设 → apply.rs 的 legacy `model_context_window`
    // root key 仍写 1M、与 catalog 的 600K 打架(#490 bot review)。与 explicit_context_window 对齐。
    if let Some(n) = capability_u64(
        model_capabilities,
        original_model,
        clean_model,
        "context_window",
    ) {
        return n >= ONE_M_CONTEXT_WINDOW;
    }

    if documented_context_window(clean_model).is_some_and(|n| n >= ONE_M_CONTEXT_WINDOW) {
        return true;
    }

    capability_bool(
        model_capabilities,
        original_model,
        clean_model,
        "supports1m",
    )
    .unwrap_or(false)
}

fn capability_bool(
    model_capabilities: Option<&Value>,
    original_model: &str,
    clean_model: &str,
    field: &str,
) -> Option<bool> {
    let caps = model_capabilities.and_then(Value::as_object)?;
    for key in capability_lookup_keys(original_model, clean_model) {
        if let Some(b) = caps
            .get(key.as_str())
            .and_then(|v| v.get(field))
            .and_then(Value::as_bool)
        {
            return Some(b);
        }
    }
    None
}

fn capability_u64(
    model_capabilities: Option<&Value>,
    original_model: &str,
    clean_model: &str,
    field: &str,
) -> Option<u64> {
    let caps = model_capabilities.and_then(Value::as_object)?;
    for key in capability_lookup_keys(original_model, clean_model) {
        if let Some(n) = caps
            .get(key.as_str())
            .and_then(|v| v.get(field))
            .and_then(Value::as_u64)
        {
            return Some(n);
        }
    }
    None
}

fn capability_lookup_keys(original_model: &str, clean_model: &str) -> Vec<String> {
    let mut keys = Vec::<String>::with_capacity(3);
    for candidate in [
        clean_model.trim(),
        original_model.trim(),
        &clean_model.to_ascii_lowercase(),
    ] {
        let c = candidate.trim();
        if !c.is_empty() && !keys.iter().any(|k| k == c) {
            keys.push(c.to_owned());
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn documented_context_window_contains_builtin_defaults() {
        assert_eq!(documented_context_window("moonshot-v1-8k"), Some(8192));
        assert_eq!(documented_context_window("qwen3.6-plus"), Some(1_000_000));
        assert_eq!(documented_context_window("MiniMax-M2.7"), Some(204_800));
        assert_eq!(documented_context_window("MiniMax-M3"), Some(1_000_000));
        assert_eq!(documented_context_window("kimi-k2.7"), Some(262_144));
        assert_eq!(documented_context_window("glm-5.2"), Some(1_000_000));
        assert_eq!(documented_context_window("unknown-model"), None);
        // QoderWork 原始 key → 各模型支持的最大 context(gm51model/dmodel/l=1M, kmodel=256k,
        // auto=180k, mmodel=200k)。key 非人类可读,不撞上面显示名分支。
        assert_eq!(documented_context_window("gm51model"), Some(1_000_000));
        assert_eq!(documented_context_window("l"), Some(1_000_000));
        assert_eq!(documented_context_window("kmodel"), Some(256_000));
        assert_eq!(documented_context_window("auto"), Some(180_000));
        assert_eq!(documented_context_window("mmodel"), Some(200_000));
    }

    #[test]
    fn documented_context_window_matches_all_gemini_text_models_as_1m() {
        // 真机 antigravity 实际映射到 Codex 槽位的带后缀 id —— 之前逐 id 精确匹配
        // 全部漏掉,落 258_400 兜底;前缀匹配后统一 1M。
        for id in [
            "gemini-3-flash-agent",
            "gemini-3.5-flash-low",
            "gemini-3.5-flash-extra-low",
            "gemini-3.1-pro-low",
            "gemini-3.1-pro-high",
            "gemini-pro-agent", // 注意:以 `gemini-pro` 开头,非 `gemini-3`
            // 仍保留原精确匹配的三个
            "gemini-3-flash",
            "gemini-2.5-flash",
            "gemini-3.1-flash-lite",
            "gemini-2.5-pro",
        ] {
            assert_eq!(
                documented_context_window(id),
                Some(ONE_M_CONTEXT_WINDOW),
                "{id} 应判 1M",
            );
        }
        // 大小写/空白归一
        assert_eq!(
            documented_context_window("  Gemini-3-Flash-Agent  "),
            Some(ONE_M_CONTEXT_WINDOW),
        );
    }

    #[test]
    fn documented_context_window_excludes_gemini_image_models() {
        // 图像模型 context_length 为 null(MOC-222 单独接入),不走文本上下文兜底。
        assert_eq!(documented_context_window("gemini-3.1-flash-image"), None);
        assert_eq!(
            documented_context_window("gemini-3-pro-image-preview"),
            None
        );
    }

    #[test]
    fn documented_context_window_excludes_legacy_gemini_without_1m() {
        // 老世代 gemini(真实上限 ~32k)不能误报 1M —— 否则手动映射这些 id 时
        // autocompact 等到 800k 才触发、反而撑爆上游(#453 P2)。维持 None → 兜底。
        for id in [
            "gemini-pro",     // 裸名 = Gemini 1.0 Pro
            "gemini-1.0-pro", // 1.0 世代
            "gemini-1.0-pro-vision",
        ] {
            assert_eq!(documented_context_window(id), None, "{id} 不应判 1M");
        }
        // 但 Gemini 1.5+ 世代仍判 1M
        assert_eq!(
            documented_context_window("gemini-1.5-pro"),
            Some(ONE_M_CONTEXT_WINDOW),
        );
    }

    #[test]
    fn model_supports_1m_accepts_documented_and_capability_paths() {
        assert!(model_supports_1m("deepseek-v4-pro", None));
        assert!(model_supports_1m("mimo-v2.5-pro", None));
        assert!(!model_supports_1m("moonshot-v1-32k", None));
        assert!(model_supports_1m("any-model[1m]", None));

        let caps = json!({
            "custom": {"supports1m": true},
            "small": {"context_window": 8192},
            "big": {"context_window": 1000000}
        });
        assert!(model_supports_1m("custom", Some(&caps)));
        assert!(!model_supports_1m("small", Some(&caps)));
        assert!(model_supports_1m("big", Some(&caps)));
    }

    #[test]
    fn explicit_context_window_cap_overrides_documented_1m() {
        // [MOC-241] Gemini 1M 开关「关」写 context_window=600000:即便 documented 判
        // gemini-3-pro 为 1M,显式 cap 也让 supports_1m=false —— 使 apply.rs 的 legacy
        // model_context_window root key 与 catalog 的 600K 一致(#490 bot review)。
        let capped = json!({"gemini-3-pro": {"context_window": 600000}});
        assert!(!model_supports_1m("gemini-3-pro", Some(&capped)));
        // 「开」写 1000000(+supports1m)→ true
        let full = json!({"gemini-3-pro": {"context_window": 1000000, "supports1m": true}});
        assert!(model_supports_1m("gemini-3-pro", Some(&full)));
        // 无显式 cap → 仍按 documented 判 1M(回归保护:不影响未配 cap 的现有 provider)
        assert!(model_supports_1m("gemini-3-pro", None));
    }

    #[test]
    fn capability_lookup_accepts_original_case_key() {
        let caps = json!({
            "MiniMax-M2.7": {"supports1m": true}
        });
        assert!(model_supports_1m("MiniMax-M2.7", Some(&caps)));
    }
}
