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
//! **范围(MOC-241)**:chat-completions 思考系(GLM / DeepSeek / Kimi / 阿里云百炼 Qwen /
//! 小米 MiMo / MiniMax)+ **Gemini 全系**(AI Studio / CLI / Antigravity,gemini_native:`none`/`max`
//! 两档,wire 经 gemini_native 映射 none→thinkingLevel:off / max→high)。Grok、moonshot-v1-* 仍留默认。

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

/// **思考必开 → 单档 `max`**:思考不可关、固定开的模型(MiniMax-M2.x;Gemini 全系按产品决策也归此 ——
/// 不暴露可切的思考档)。**单档**(非空档位/非 none+max):picker 只显「Max」一个固定项,无可切选项
/// (符合「思考不可修改」);且因有真实档 + 默认 max,Codex composer 的 `xp()` 返回 `max`(非回落全局
/// 默认),**不残留「Reasoning / Medium」标签**(空档位会被 Codex 兜底成 medium 残留、去不掉除非 CDP,
/// MOC-241 CDP 实证;单档 max 干净绕开)。
///
/// **wire**:M2.x(chat)思考强制开、`disable_wire`/`on_tier_wire` 皆 `None`(不发 reasoning_effort,
/// minimax sanitize 也会剥);Gemini 走 gemini_native,`max`→`thinkingLevel:high`(Gemini 3 最高;2.x 走
/// thinkingBudget)由 `adapters::gemini_native::request` 映射,不经本表 chat wire。本 spec 只驱动 picker。
static SINGLE_MAX: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_MAX],
    default_level: Some("max"),
    disable_wire: None,
    on_tier_wire: None,
};

// ── QoderWork(阿里 Qoder,Cosy 签名 / remoteChatAsk 通道)专用档位 ──────────────────
// QoderWork 模型的思考不走各上游原生 wire,统一经 remoteChatAsk `parameters.reasoning_effort`
// (`qoder_auth::body::build_parameters` 透传客户端 chat body 的 `reasoning_effort`)。故这里
// **不用** GLM/DeepSeek 那套 provider 特化 disable_wire(build_remote_chat_ask 只透传
// reasoning_effort,那些顶级 thinking 字段会被丢弃)。QoderWork 经 remoteChatAsk **单个
// `reasoning_effort` 字段**控思考(实测 gateway.qoder.com.cn:`none`→不输出 reasoning,`high`/`max`
// →思考):on-tier 用 `HighMax` 写 high/max;「none」档用 `ReasoningEffortNone` 写
// reasoning_effort="none"。档位取自 QoderWork server model-list 的 `thinking_config`(main.log,MOC-297)。

/// QoderWork 二元思考模型(auto / Qwen3.7-Max·Plus / Qwen3.6-Flash / Kimi-K2.7):`none` + `max`。
/// `max`=思考开(不写 effort = 模型默认思考,实测默认即开);`none`=写 reasoning_effort="none" 关。
static QODER_BINARY: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_NONE, TIER_MAX],
    default_level: Some("max"),
    disable_wire: Some(DisableThinkingWire::ReasoningEffortNone),
    on_tier_wire: None,
};

/// QoderWork 三档 effort 模型(DeepSeek-V4-Pro/Flash / GLM-5.2,`thinking_config.efforts=[high,max]`):
/// `none`(写 reasoning_effort="none")+ `high` + `max`(深度档经 reasoning_effort 透传 HighMax)。
static QODER_EFFORT: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[TIER_NONE, TIER_HIGH, TIER_MAX],
    default_level: Some("max"),
    disable_wire: Some(DisableThinkingWire::ReasoningEffortNone),
    on_tier_wire: Some(ReasoningEffortWire::HighMax),
};

/// QoderWork 非思考模型(MiniMax-M2.7,`is_reasoning=false`):隐藏 picker(空档位)。
static QODER_NO_THINKING: ReasoningTierSpec = ReasoningTierSpec {
    levels: &[],
    default_level: None,
    disable_wire: None,
    on_tier_wire: None,
};

/// model id(自动 trim + lowercase)→ 可选思考档位规格;`None` = 无特殊档位(用 Codex 默认 4 档)。
///
/// **全局表(不含 QoderWork)**:key 假定跨 provider 全局唯一的真实模型名(glm-5.2 / deepseek-v4-pro
/// 等,同名 = 同底层模型 → 共用档位,MOC-241 设计)。QoderWork 的网关 key 有 `auto` / `l` 这类**通用
/// 别名**(与 WorkBuddy 的 `auto` 撞名),不在此表 —— 见 [`reasoning_tiers_for_qoder_model`] +
/// [`reasoning_tiers_for_model_scoped`],只在 qoder provider 上下文里生效,避免污染同名的其它 provider。
pub fn reasoning_tiers_for_model(model: &str) -> Option<&'static ReasoningTierSpec> {
    let m = model.trim().to_ascii_lowercase();

    // 智谱 GLM 4.5+/5.x(版本谓词,自动覆盖变体)
    if is_glm_thinking_model(&m) {
        return Some(&GLM_TWO_TIER);
    }
    // MiniMax M2.x:thinking 强制开、上游不支持 disable(platform.minimaxi.com)→ 单档 max(固定开,不可切)
    if m.starts_with("minimax-m2") {
        return Some(&SINGLE_MAX);
    }
    // Gemini 全系(AI Studio / CLI / Antigravity,gemini_native):按产品决策不暴露可切思考档 → 单档 max
    //(固定最高思考)。不用空档位隐藏(会被 Codex 兜底成残留 medium、去不掉除非 CDP);单档 max 干净。
    // wire 经 gemini_native 映射 max→thinkingLevel:high(非本表 chat wire)。
    if m.starts_with("gemini") {
        return Some(&SINGLE_MAX);
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

/// QoderWork(阿里 Qoder)原始 model key → 思考档位规格;`None` = 非 qoder key。
///
/// **provider-scoped**:qoder 网关 key(`auto`/`l`/`gm51model` 等)含通用别名,与其它 provider 撞名,
/// 故**独立于全局表**、只在 qoder provider 上下文里查(见 [`reasoning_tiers_for_model_scoped`] +
/// [`crate::qoder_catalog::is_qoder_auth_scheme`])。思考经 remoteChatAsk `reasoning_effort` 透传,
/// 档位取自 QoderWork server model-list(MOC-297)。数据同源 [`crate::qoder_catalog::QODER_MODELS`]。
pub fn reasoning_tiers_for_qoder_model(model: &str) -> Option<&'static ReasoningTierSpec> {
    match model.trim().to_ascii_lowercase().as_str() {
        "gm51model" | "dmodel" | "dfmodel" => Some(&QODER_EFFORT),
        "auto" | "qmodel_latest" | "qmodel" | "l" | "kmodel" => Some(&QODER_BINARY),
        "mmodel" => Some(&QODER_NO_THINKING),
        _ => None,
    }
}

/// 按 model id + **是否 qoder provider** 查思考档位。qoder provider 优先查 qoder 表(通用别名如
/// `auto` 才解析成 qoder 档),再兜底全局表(qoder 也可能挂真实模型名);非 qoder 只查全局表
/// (WorkBuddy 的 `auto` → 全局无 → `None`,回落各 provider 原行为,**不被 qoder 污染**)。
pub fn reasoning_tiers_for_model_scoped(
    model: &str,
    is_qoder: bool,
) -> Option<&'static ReasoningTierSpec> {
    if is_qoder {
        reasoning_tiers_for_qoder_model(model).or_else(|| reasoning_tiers_for_model(model))
    } else {
        reasoning_tiers_for_model(model)
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

/// 本表所有 spec 的可选思考档位**全集**(去重 + 排序)。
///
/// **用途([MOC-285])**:Codex 26.623(codex-cli 0.142.3)起新增用户级持久设置
/// `enabled-reasoning-efforts`(webview LM atom,`hostStorage.kind="persisted-atom"`,默认
/// `["low","medium","high","xhigh","ultra"]`)。reasoning picker 实际显示 =
/// 模型 `supported_reasoning_levels` ∩ `enabled-reasoning-efforts`。我们 catalog 给 GLM 等写的
/// `none` / `max` 不在默认启用集 → 交集为空 → picker 兜底成残留「Medium」(none/max 档不显示)。
///
/// app 在「退出 Codex → 启动前」窗口把**本全集**并入 `enabled-reasoning-efforts` 持久 atom
///(union,非覆盖),即可让这些档位正常显示。**单一来源**:与 catalog 显档、wire 关思考同一张
/// `reasoning_tiers` 表(本模块),不另列一份 effort 名单,杜绝 MOC-241 强调的判定漂移。
///
/// 当前返回 `["high", "max", "none"]`(GLM/Kimi/Qwen/MiMo 的 none+max、DeepSeek 的 none+high+max、
/// Gemini/MiniMax-M2.x 的 max)。
///
/// ⚠️ **维护**:下方 `ALL_TIER_SPECS` 与 [`reasoning_tiers_for_model`] 的 match/谓词分发是**两处独立
/// 枚举**,不会自动联动 —— **新增一个 `ReasoningTierSpec` 常量并接进 `reasoning_tiers_for_model` 时,
/// 必须同步把它加进 `ALL_TIER_SPECS`**,否则该模型的档位不会被 seed 进 Codex 启用集、picker 又塌成
/// Medium(MOC-285 复发)。测试 `union_covers_specs_reachable_from_dispatch` 用模型语料兜底捕获遗漏。
pub fn all_reasoning_tier_efforts() -> Vec<&'static str> {
    const ALL_TIER_SPECS: &[&ReasoningTierSpec] = &[
        &GLM_TWO_TIER,
        &DEEPSEEK_TIERS,
        &BINARY_THINKING_TYPE,
        &BINARY_ENABLE_THINKING,
        &SINGLE_MAX,
        &QODER_BINARY,
        &QODER_EFFORT,
        &QODER_NO_THINKING,
    ];
    let mut efforts: Vec<&'static str> = Vec::new();
    for spec in ALL_TIER_SPECS {
        for tier in spec.levels {
            if !efforts.contains(&tier.effort) {
                efforts.push(tier.effort);
            }
        }
    }
    efforts.sort_unstable();
    efforts
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
    fn minimax_m2_single_max() {
        // M2.x 思考强制开、不可关 → 单档 max(固定开,picker 无可切项);无 disable wire。
        for m in ["minimax-m2.7", "minimax-m2", "MiniMax-M2.7"] {
            let s = reasoning_tiers_for_model(m).unwrap_or_else(|| panic!("{m} 应命中单档 max"));
            assert_eq!(efforts(s), vec!["max"], "{m} 应单档 max");
            assert_eq!(s.default_level, Some("max"));
            assert_eq!(s.disable_wire, None, "{m} 强制思考、不可关");
        }
    }

    #[test]
    fn qoder_keys_map_to_qoder_specs() {
        // qoder key 只在 qoder-scoped 表(reasoning_tiers_for_qoder_model / scoped is_qoder=true)里解析。
        // effort 模型(DeepSeek/GLM):none/high/max,reasoning_effort 透传(HighMax)
        for m in ["gm51model", "dmodel", "dfmodel"] {
            let s = reasoning_tiers_for_qoder_model(m).unwrap_or_else(|| panic!("{m} 应命中"));
            assert_eq!(efforts(s), vec!["none", "high", "max"], "{m}");
            assert_eq!(s.on_tier_wire, Some(ReasoningEffortWire::HighMax), "{m}");
            // scoped(is_qoder=true) 等价
            assert_eq!(reasoning_tiers_for_model_scoped(m, true), Some(s), "{m}");
        }
        // 二元思考(auto/Qwen/Kimi):none/max
        for m in ["auto", "qmodel_latest", "qmodel", "l", "kmodel"] {
            let s = reasoning_tiers_for_qoder_model(m).unwrap_or_else(|| panic!("{m} 应命中"));
            assert_eq!(efforts(s), vec!["none", "max"], "{m}");
        }
        // 非思考(MiniMax-M2.7):隐藏 picker(空档位)
        let mm = reasoning_tiers_for_qoder_model("mmodel").unwrap();
        assert!(mm.levels.is_empty(), "mmodel 应隐藏 picker");
        // qoder 关思考 = 写 reasoning_effort="none"(实测 QoderWork 单字段控思考)
        assert_eq!(
            reasoning_tiers_for_qoder_model("gm51model")
                .unwrap()
                .disable_wire,
            Some(DisableThinkingWire::ReasoningEffortNone)
        );
    }

    #[test]
    fn qoder_generic_alias_keys_absent_from_global_table() {
        // [HIGH 回归防护] qoder 的通用别名 key(`auto`/`l`)不得进全局表 —— 否则会撞 WorkBuddy 等
        // 同名 model,静默改掉它们的 reasoning wire/picker/compact(correctness review HIGH)。
        // 全局表对这些 key 必须返 None;非 qoder provider(is_qoder=false)scoped 亦然。
        for m in ["auto", "l"] {
            assert!(
                reasoning_tiers_for_model(m).is_none(),
                "{m} 不应在全局 reasoning_tiers 表(会污染同名 provider)"
            );
            assert!(
                reasoning_tiers_for_model_scoped(m, false).is_none(),
                "{m} 非 qoder provider 不应解析成 qoder 档"
            );
        }
        // qoder 的机器名 key(gm51model 等)也一律不进全局表,统一走 qoder-scoped。
        for m in [
            "gm51model",
            "dmodel",
            "dfmodel",
            "qmodel",
            "qmodel_latest",
            "kmodel",
            "mmodel",
        ] {
            assert!(
                reasoning_tiers_for_model(m).is_none(),
                "{m} 全局表应为空(qoder key 统一 scoped)"
            );
        }
    }

    #[test]
    fn every_qoder_model_has_reasoning_and_context() {
        // [test-coverage review rate 7] 跨表一致性机械守卫:QODER_MODELS 是 QoderWork 的单一真相,
        // 每个 key 必须在 qoder-scoped reasoning 档 + scoped context 两表都有归宿。加第 10 个模型忘同步
        // 任一表 → 半坏(如 context 对但 reasoning 缺档、picker 塌成默认 4 档),正是本仓最想防的漂移。
        use crate::model_context_policy::documented_context_window_scoped;
        use crate::qoder_catalog::QODER_MODELS;
        for m in QODER_MODELS {
            assert!(
                reasoning_tiers_for_qoder_model(m.key).is_some(),
                "QODER_MODELS key `{}` 缺 qoder reasoning 档(reasoning_tiers_for_qoder_model 漏同步)",
                m.key
            );
            assert_eq!(
                documented_context_window_scoped(m.key, true),
                Some(m.max_context),
                "QODER_MODELS key `{}` 的 context 与 documented_context_window_scoped 不一致",
                m.key
            );
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
            "grok-420-computer-use-sa",
            "",
        ] {
            assert!(reasoning_tiers_for_model(m).is_none(), "{m} 不应有 spec");
        }
    }

    #[test]
    fn gemini_all_single_max() {
        // Gemini 全系(AI Studio + Antigravity 变体)→ 单档 max(固定最高思考,不暴露可切档);
        // wire 经 gemini_native(max→thinkingLevel:high),非本表 chat wire,故 disable/on_tier 均 None。
        for m in [
            "gemini-3-pro",
            "gemini-3-flash",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-1.5-pro",
            "gemini-3.5-flash-low",
            "gemini-3-flash-agent",
            "gemini-pro-agent",
            "gemini-3.1-pro-high",
            "  Gemini-3-Pro  ",
        ] {
            let s = reasoning_tiers_for_model(m).unwrap_or_else(|| panic!("{m} 应命中单档 max"));
            assert_eq!(efforts(s), vec!["max"], "{m}");
            assert_eq!(s.default_level, Some("max"), "{m} 默认 max");
            assert_eq!(s.disable_wire, None, "{m} wire 经 gemini_native 不在本表");
            assert_eq!(s.on_tier_wire, None, "{m}");
        }
    }

    #[test]
    fn all_efforts_union_covers_none_high_max_deduped_sorted() {
        // [MOC-285] enabled-reasoning-efforts seeding 用的全集 = 所有 spec 档位并集。
        // 当前表:GLM/Kimi/Qwen/MiMo(none,max)+ DeepSeek(none,high,max)+ Gemini/MiniMax-M2.x(max)
        // → 去重 = {high, max, none}。
        let efforts = all_reasoning_tier_efforts();
        assert_eq!(efforts, vec!["high", "max", "none"], "并集去重 + 排序");
        // 去重不变量:无重复
        let mut sorted = efforts.clone();
        sorted.dedup();
        assert_eq!(sorted, efforts, "不得有重复档位");
        // 默认隐藏的 none/max 必在内(本 issue 核心)
        assert!(efforts.contains(&"none") && efforts.contains(&"max"));
    }

    #[test]
    fn union_covers_specs_reachable_from_dispatch() {
        // [MOC-285 PR review HIGH] 防 ALL_TIER_SPECS 与 reasoning_tiers_for_model 漂移:
        // 模型语料里每个命中的 spec,其全部档位必须都在 all_reasoning_tier_efforts() 全集内。
        // 新增 spec 只接进 dispatch、忘了加 ALL_TIER_SPECS,只要其模型在本语料里就会被本测捕获。
        let union = all_reasoning_tier_efforts();
        let corpus = [
            "glm-5.2",
            "glm-5.1",
            "glm-4.7",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "kimi-k2.6",
            "minimax-m3",
            "qwen3.6-plus",
            "mimo-v2.5-pro",
            "minimax-m2.7",
            "gemini-3-pro",
        ];
        for m in corpus {
            let spec = reasoning_tiers_for_model(m).unwrap_or_else(|| panic!("{m} 应命中 spec"));
            for tier in spec.levels {
                assert!(
                    union.contains(&tier.effort),
                    "{m} 的档位 {} 不在 all_reasoning_tier_efforts() 全集 —— ALL_TIER_SPECS 漏了对应 spec",
                    tier.effort
                );
            }
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
