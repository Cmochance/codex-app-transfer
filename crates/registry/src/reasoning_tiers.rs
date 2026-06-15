//! [MOC-241] 模型 → 「可选思考档位」映射表(单一真相)。
//!
//! Codex reasoning 选择器默认给所有模型 4 档(low/medium/high/xhigh)。但有些上游模型的思考
//! 能力不是「深度档」而是「开/关」二元(如智谱 GLM:`thinking:{type:enabled|disabled}`,官方
//! 应用只给「不思考 / 最高」两档)。本表把这类模型映射到它**真实**的档位 + 关思考 wire。
//!
//! **三处消费、单一来源**(杜绝判定漂移,见 MOC-241 PR review):
//! 1. **catalog**(`codex_integration::model_catalog`):用 `levels` / `default_level` 写进
//!    `model_catalog_json` 的 `supported_reasoning_levels`,决定 Codex picker 显哪些档;
//! 2. **reasoning wire**(`crate::reasoning_effort_policy::apply_reasoning_effort`):用户选
//!    「不思考」档时用 `disable_wire` 往上游发关思考字段;
//! 3. **compact**(`crate::compact_thinking_policy::compact_disable_thinking_wire`):compact
//!    任务强制关思考时复用同一 `disable_wire`。
//!
//! **新增 provider/model**:在 [`reasoning_tiers_for_model`] 里加一个分支(精确 id 匹配或
//! 自有版本/前缀谓词,如 GLM 的 [`is_glm_thinking_model`])指向一个 [`ReasoningTierSpec`] 常量,
//! 三处消费自动生效。
//! `effort` 取值必须落在 Codex 闭合枚举 `{none, minimal, low, medium, high, xhigh, max}` 内
//! (实测 Codex.app v0.140 UI 校验器只认这些,未知值不渲染)。返回 `None` = 无特殊档位,
//! catalog 用 Codex 默认 4 档、wire 不动。

use crate::compact_thinking_policy::DisableThinkingWire;

/// picker 里的一个可选思考档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningTier {
    /// Codex 闭合枚举 effort 值(`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`)。
    pub effort: &'static str,
    /// 副标题说明(catalog `supported_reasoning_levels[].description`;主标签由 Codex 本地化渲染)。
    pub description: &'static str,
}

/// 一个模型的「可选思考档位」规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningTierSpec {
    /// picker 显示的档位(按显示顺序)。
    pub levels: &'static [ReasoningTier],
    /// 默认档(必须是 `levels` 之一的 `effort`)。
    pub default_level: &'static str,
    /// 用户选「不思考」(`none`/`off`/`disabled`)档时往上游发的关思考 wire。
    pub disable_wire: DisableThinkingWire,
}

/// 智谱 GLM 两档:`none`(不思考)+ `max`(最高)。
///
/// disable wire = [`DisableThinkingWire::GlmDual`](hosted 顶级 `thinking:{type:disabled}` +
/// 自建 `chat_template_kwargs.enable_thinking:false` 双发)。主标签由 Codex 自身按 effort
/// 本地化(中文 UI ≈「无思考 / 最大思考」),不另注入。
static GLM_TWO_TIER: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[
        ReasoningTier {
            effort: "none",
            description: "No thinking",
        },
        ReasoningTier {
            effort: "max",
            description: "Maximum thinking effort",
        },
    ],
    default_level: "max",
    disable_wire: DisableThinkingWire::GlmDual,
};

/// model id(自动 trim + lowercase)→ 可选思考档位规格;`None` = 无特殊档位(用 Codex 默认 4 档)。
///
/// 目前唯一一类有自定义档位的模型 = **支持 `thinking` 切换的 GLM**(见 [`is_glm_thinking_model`]:
/// `glm-` 前缀 + 版本 ≥ 4.5)。新增其它 provider/model 时在此加分支(精确 id 或自有谓词)。
pub fn reasoning_tiers_for_model(model: &str) -> Option<&'static ReasoningTierSpec> {
    if is_glm_thinking_model(model) {
        return Some(&GLM_TWO_TIER);
    }
    None
}

/// GLM 是否为「支持 `thinking` 切换」的型号:`glm-` 前缀 + 版本 **≥ 4.5**(major ≥ 5,或 major==4
/// 且 minor ≥ 5)。
///
/// **按版本号判定、不枚举**:Z.AI 标 GLM-4.5+/5.x 系支持 `thinking.type` 切换
/// (`docs.z.ai/guides/llm/glm-4.5`),且变体繁多(`-air`/`-x`/`-airx`/`-flash`/`-turbo`/`v` 等后缀
/// + 持续新增)。版本谓词自动覆盖所有这些变体(glm-4.5-air / glm-4.5-x / glm-4.6v / glm-5-turbo …),
/// 免逐个枚举漏判(PR #490 bot review P2)。
///
/// **排除 < 4.5 的 legacy / 非 toggle 型号**:glm-4 / glm-4-plus / glm-4-flash / glm-4-air /
/// glm-4-0520 / glm-4-9b / glm-4-32b-* / glm-4v / glm-4.1v-thinking-* —— 这些不支持 thinking 控制,
/// 给两档 picker 再发 disable 会被上游忽略甚至 400。
fn is_glm_thinking_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    let Some(rest) = m.strip_prefix("glm-") else {
        return false;
    };
    let bytes = rest.as_bytes();
    // 取前导 major
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false; // `glm-` 后无版本号(如 `glm-air`)→ 不认
    }
    let major: u32 = rest[..i].parse().unwrap_or(0);
    // 紧跟 `.` 时取 minor,否则 minor=0(如 `glm-5-turbo` / `glm-5v-turbo`)
    let minor: u32 = if i < bytes.len() && bytes[i] == b'.' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        rest[i + 1..j].parse().unwrap_or(0)
    } else {
        0
    };
    major > 4 || (major == 4 && minor >= 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_thinking_models_map_to_two_tier() {
        for m in [
            "glm-5.1",
            "glm-5",
            "glm-5-turbo",
            "glm-5v-turbo",
            "glm-4.7",
            "glm-4.5v",
            "glm-4.6",
            "glm-4.6v",
            "glm-4.5",
            // [PR #490 P2] GLM-4.5 系变体(版本谓词自动覆盖,不枚举):
            "glm-4.5-air",
            "glm-4.5-x",
            "glm-4.5-airx",
            "glm-4.5-flash",
            "glm-5.2",     // 未来 5.x 也自动覆盖
            "  GLM-5.1  ", // trim + lowercase
        ] {
            let spec = reasoning_tiers_for_model(m).unwrap_or_else(|| panic!("{m} 应命中两档表"));
            let efforts: Vec<&str> = spec.levels.iter().map(|l| l.effort).collect();
            assert_eq!(efforts, vec!["none", "max"], "{m} 档位应为 none+max");
            assert_eq!(spec.default_level, "max");
            assert_eq!(spec.disable_wire, DisableThinkingWire::GlmDual);
        }
    }

    #[test]
    fn legacy_glm4_and_non_glm_have_no_special_tiers() {
        // legacy GLM-4 / < 4.5(不支持 thinking 控制)+ 非 GLM → None,用 Codex 默认 4 档
        for m in [
            "glm-4-plus",
            "glm-4-flash",
            "glm-4-32b-0414-128k",
            "glm-4",
            "glm-4-air",
            "glm-4-0520",
            "glm-4v",                   // < 4.5 vision
            "glm-4.1v-thinking-flashx", // 4.1 < 4.5,即便名带 thinking 也不收
            "gpt-5.5",
            "deepseek-v4-pro",
            "kimi-k2.6",
            "glm", // 无版本号
            "",
        ] {
            assert!(
                reasoning_tiers_for_model(m).is_none(),
                "{m} 不应有特殊档位(legacy GLM-4 / 非 GLM)"
            );
        }
    }

    #[test]
    fn default_level_is_one_of_levels() {
        // 不变量:default_level 必须是 levels 里的某个 effort
        let spec = reasoning_tiers_for_model("glm-5.1").unwrap();
        assert!(spec.levels.iter().any(|l| l.effort == spec.default_level));
    }
}
