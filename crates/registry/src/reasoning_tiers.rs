//! [MOC-241] 模型 → 「可选思考档位」映射表(全部 thinking 模型的单一真相)。
//!
//! Codex reasoning 选择器默认给所有模型 4 档(low/medium/high/xhigh)。但各上游模型的思考能力
//! 五花八门:有的是「开/关」二元(GLM / Kimi / Qwen / MiMo / MiniMax-M3),有的有 high/max 档
//! (DeepSeek V4),有的**强制思考不可关**(MiniMax-M2.x)。本表把每个 thinking 模型映射到它
//! **真实**的档位 + 关思考 wire;不可关的模型用**空档位**(`levels: &[]`)让 Codex 隐藏 picker
//! (`supported_reasoning_levels` 为空 → picker 不渲染可选项,实测 Codex.app 行为)。
//!
//! **三处消费、单一来源**(杜绝判定漂移,见 MOC-241 PR review):
//! 1. **catalog**(`codex_integration::model_catalog`):用 `levels` / `default_level` 写进
//!    `model_catalog_json` 的 `supported_reasoning_levels`,决定 Codex picker 显哪些档(空 = 隐藏);
//! 2. **reasoning wire**(`crate::reasoning_effort_policy::apply_reasoning_effort`):选「不思考」档
//!    用 `disable_wire` 关思考;选「思考开」的深度档(如 DeepSeek high/max)落到既有
//!    `reasoning_effort_wire` 写 `reasoning_effort`;
//! 3. **compact**(`crate::compact_thinking_policy::compact_disable_thinking_wire`):compact 任务
//!    强制关思考时复用同一 `disable_wire`(整个 compact-disable 名单已收口到本表)。
//!
//! **新增 provider/model**:在 [`reasoning_tiers_for_model`] 加一个分支(精确 id 或谓词)指向一个
//! [`ReasoningTierSpec`] 常量。`effort` 取值必须落在 Codex 闭合枚举
//! `{none, minimal, low, medium, high, xhigh, max}` 内(实测 Codex.app v0.140 UI 校验器只认这些)。
//! 返回 `None` = 无特殊档位,catalog 用 Codex 默认 4 档、wire 不动。
//!
//! **范围(MOC-241 本轮)**:chat-completions 思考系(GLM / DeepSeek / Kimi / 阿里云百炼 Qwen /
//! 小米 MiMo / MiniMax)。Gemini(AI Studio / CLI / Antigravity,gemini_native 协议、thinkingLevel/
//! Budget)与 Grok、moonshot-v1-* 留默认,另行处理。

use crate::compact_thinking_policy::DisableThinkingWire;
use crate::reasoning_effort_policy::ReasoningEffortWire;

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
    /// picker 显示的档位(按显示顺序)。**空 `&[]` = 隐藏 picker**(强制思考、不可选的模型)。
    pub levels: &'static [ReasoningTier],
    /// 默认档(必须是 `levels` 之一的 `effort`);`None` = 隐藏档位时无默认。
    pub default_level: Option<&'static str>,
    /// 选「不思考」(`none`/`off`/`disabled`)档时往上游发的关思考 wire;`None` = 该模型**不可关**
    /// 思考(强制),compact 也不注入 disable。
    pub disable_wire: Option<DisableThinkingWire>,
    /// 选「思考开」的深度档(非 disable 档,如 DeepSeek `high`/`max`)时怎么写 `reasoning_effort`:
    /// `Some(wire)` = 用该 wire 写(DeepSeek = `HighMax`;**按 model 定,不看 provider 名**);
    /// `None` = no-op(二元思考 provider:GLM/Kimi/Qwen/MiMo/M3 不收 `reasoning_effort`,「开」即模型默认)。
    /// **table 命中即由本字段决定 on-tier wire,绝不 fall through 到 provider-名 keyed 的
    /// `reasoning_effort_wire`**(PR #490 bot review P2:否则 GLM/Qwen 挂自定义代理会被误写
    /// `reasoning_effort`、DeepSeek 的 `max` 被 clamp 成 `high`)。
    pub on_tier_wire: Option<ReasoningEffortWire>,
}

const TIER_NONE: ReasoningTier = ReasoningTier {
    effort: "none",
    description: "No thinking",
};
const TIER_HIGH: ReasoningTier = ReasoningTier {
    effort: "high",
    description: "Standard thinking",
};
const TIER_MAX: ReasoningTier = ReasoningTier {
    effort: "max",
    description: "Maximum thinking effort",
};

/// 智谱 GLM(4.5+/5.x):二元 `none`(不思考)+ `max`(最高)。disable = `GlmDual`(hosted 顶级
/// `thinking:{type:disabled}` + 自建 `chat_template_kwargs.enable_thinking:false` 双发)。
static GLM_TWO_TIER: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_NONE, TIER_MAX],
    default_level: Some("max"),
    disable_wire: Some(DisableThinkingWire::GlmDual),
    on_tier_wire: None,
};

/// DeepSeek V4(pro/flash):`none` + `high` + `max`(官方 reasoning_effort 有 high/max 两档,
/// low/medium→high,默认 high)。`none` 关思考走顶级 `thinking:{type:disabled}`(派 A);
/// `high`/`max` 落既有 `reasoning_effort_wire`(HighMax)写 `reasoning_effort`。
static DEEPSEEK_TIERS: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_NONE, TIER_HIGH, TIER_MAX],
    default_level: Some("high"),
    disable_wire: Some(DisableThinkingWire::ThinkingTypeDisabled),
    // 深度档 high/max → reasoning_effort:high/max(HighMax 按 model 定,不看 provider 名)
    on_tier_wire: Some(ReasoningEffortWire::HighMax),
};

/// 二元 + 顶级 `thinking:{type:disabled}` 关思考:Kimi K2 全系 + MiniMax-M3。
/// `none`(不思考)+ `max`(思考开,= 模型默认,无 effort 透传/或上游默认深度)。
static BINARY_THINKING_TYPE: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_NONE, TIER_MAX],
    default_level: Some("max"),
    disable_wire: Some(DisableThinkingWire::ThinkingTypeDisabled),
    on_tier_wire: None,
};

/// 二元 + 顶级 `enable_thinking:false` 关思考:阿里云百炼 Qwen 3.x + 小米 MiMo v2.x。
/// `none`(不思考)+ `max`(思考开,= 模型默认;无 effort→budget 映射故不主动塞 budget)。
static BINARY_ENABLE_THINKING: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_NONE, TIER_MAX],
    default_level: Some("max"),
    disable_wire: Some(DisableThinkingWire::EnableThinkingFalse),
    on_tier_wire: None,
};

/// **强制思考、不可关**(如 MiniMax-M2.x):空档位 → 隐藏 picker;无 disable wire。
static FORCED_HIDDEN: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[],
    default_level: None,
    disable_wire: None,
    on_tier_wire: None,
};

/// model id(自动 trim + lowercase)→ 可选思考档位规格;`None` = 无特殊档位(用 Codex 默认 4 档)。
pub fn reasoning_tiers_for_model(model: &str) -> Option<&'static ReasoningTierSpec> {
    let m = model.trim().to_ascii_lowercase();

    // 智谱 GLM 4.5+/5.x(版本谓词,自动覆盖变体)
    if is_glm_thinking_model(&m) {
        return Some(&GLM_TWO_TIER);
    }
    // MiniMax M2.x:thinking 强制开、上游不支持 disable(platform.minimaxi.com)→ 隐藏档位
    if m.starts_with("minimax-m2") {
        return Some(&FORCED_HIDDEN);
    }

    match m.as_str() {
        // DeepSeek V4(api-docs.deepseek.com/guides/thinking_mode)
        "deepseek-v4-pro" | "deepseek-v4-flash" => Some(&DEEPSEEK_TIERS),

        // 二元 thinking.type=disabled:Kimi K2(platform.kimi.com)+ MiniMax-M3
        //(api.minimaxi.com 实测仅顶级 thinking.type 生效)
        "kimi-k2.5" | "kimi-k2.6" | "kimi-for-coding" | "minimax-m3" => Some(&BINARY_THINKING_TYPE),

        // 二元 enable_thinking=false:阿里云百炼 Qwen 3.x(help.aliyun.com)+ 小米 MiMo v2.x
        "qwen3.6-plus" | "qwen3.6-flash" | "qwen3-plus" | "qwen3-flash" | "mimo-v2.5-pro"
        | "mimo-v2.5" | "mimo-v2-pro" | "mimo-v2-flash" | "mimo-v2-omni" => {
            Some(&BINARY_ENABLE_THINKING)
        }

        _ => None,
    }
}

/// GLM 是否为「支持 `thinking` 切换」的型号:`glm-` 前缀 + 版本 **≥ 4.5**(major ≥ 5,或 major==4
/// 且 minor ≥ 5)。
///
/// **按版本号判定、不枚举**:Z.AI 标 GLM-4.5+/5.x 系支持 `thinking.type` 切换
/// (`docs.z.ai/guides/llm/glm-4.5`),变体繁多(`-air`/`-x`/`-airx`/`-flash`/`-turbo`/`v` 等后缀)。
/// 版本谓词自动覆盖所有这些变体,免逐个枚举漏判(PR #490 bot review P2)。排除 < 4.5 的 legacy /
/// 非 toggle 型号(glm-4 / glm-4-plus / glm-4-flash / glm-4v / glm-4.1v-thinking 等)。
fn is_glm_thinking_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    let Some(rest) = m.strip_prefix("glm-") else {
        return false;
    };
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false; // `glm-` 后无版本号(如 `glm-air`)→ 不认
    }
    let major: u32 = rest[..i].parse().unwrap_or(0);
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

    fn efforts(spec: &ReasoningTierSpec) -> Vec<&'static str> {
        spec.levels.iter().map(|l| l.effort).collect()
    }

    #[test]
    fn glm_thinking_models_two_tier_glmdual() {
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
            "glm-4.5-air",
            "glm-4.5-x",
            "glm-4.5-airx",
            "glm-5.2",
            "  GLM-5.1  ",
        ] {
            let s = reasoning_tiers_for_model(m).unwrap_or_else(|| panic!("{m} 应命中"));
            assert_eq!(efforts(s), vec!["none", "max"], "{m}");
            assert_eq!(s.default_level, Some("max"));
            assert_eq!(s.disable_wire, Some(DisableThinkingWire::GlmDual));
        }
    }

    #[test]
    fn deepseek_three_tier_thinking_type() {
        for m in ["deepseek-v4-pro", "deepseek-v4-flash"] {
            let s = reasoning_tiers_for_model(m).unwrap();
            assert_eq!(efforts(s), vec!["none", "high", "max"], "{m}");
            assert_eq!(s.default_level, Some("high"));
            assert_eq!(
                s.disable_wire,
                Some(DisableThinkingWire::ThinkingTypeDisabled)
            );
        }
    }

    #[test]
    fn kimi_and_m3_two_tier_thinking_type() {
        for m in ["kimi-k2.5", "kimi-k2.6", "kimi-for-coding", "minimax-m3"] {
            let s = reasoning_tiers_for_model(m).unwrap();
            assert_eq!(efforts(s), vec!["none", "max"], "{m}");
            assert_eq!(
                s.disable_wire,
                Some(DisableThinkingWire::ThinkingTypeDisabled)
            );
        }
    }

    #[test]
    fn qwen_and_mimo_two_tier_enable_thinking() {
        for m in [
            "qwen3.6-plus",
            "qwen3.6-flash",
            "qwen3-plus",
            "qwen3-flash",
            "mimo-v2.5-pro",
            "mimo-v2.5",
            "mimo-v2-pro",
            "mimo-v2-flash",
            "mimo-v2-omni",
        ] {
            let s = reasoning_tiers_for_model(m).unwrap();
            assert_eq!(efforts(s), vec!["none", "max"], "{m}");
            assert_eq!(
                s.disable_wire,
                Some(DisableThinkingWire::EnableThinkingFalse)
            );
        }
    }

    #[test]
    fn minimax_m2_forced_hidden() {
        for m in ["minimax-m2.7", "minimax-m2", "MiniMax-M2.7"] {
            let s = reasoning_tiers_for_model(m).unwrap_or_else(|| panic!("{m} 应命中隐藏档"));
            assert!(s.levels.is_empty(), "{m} 应空档位(隐藏 picker)");
            assert_eq!(s.default_level, None);
            assert_eq!(s.disable_wire, None, "{m} 强制思考、不可关");
        }
    }

    #[test]
    fn unknown_and_deferred_models_have_no_spec() {
        // legacy GLM-4 / 非 thinking / 暂留默认的 provider → None(用 Codex 默认 4 档)
        for m in [
            "glm-4-plus",
            "glm-4-flash",
            "glm-4v",
            "glm-4.1v-thinking-flashx",
            "gpt-5.5",
            "moonshot-v1-32k",
            "gemini-3-flash-agent",
            "grok-420-computer-use-sa",
            "",
        ] {
            assert!(reasoning_tiers_for_model(m).is_none(), "{m} 不应有 spec");
        }
    }

    #[test]
    fn default_level_is_within_levels_when_present() {
        // 不变量:非隐藏 spec 的 default_level 必须是 levels 之一
        for m in ["glm-5.1", "deepseek-v4-pro", "kimi-k2.6", "qwen3.6-plus"] {
            let s = reasoning_tiers_for_model(m).unwrap();
            let d = s.default_level.unwrap();
            assert!(s.levels.iter().any(|l| l.effort == d), "{m} default 越界");
        }
    }
}
