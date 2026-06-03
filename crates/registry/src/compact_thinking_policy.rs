//! compact 路径 disable-thinking 字段注入策略 —— 按真实发到上游的 model ID
//! 查表决定要不要给 chat completions body 注入"关闭思考"的字段、注入哪种 wire。
//!
//! ## 为什么 compact 路径要主动 disable thinking?
//!
//! `compact_thinking_policy` 收录的所有 thinking-capable 模型,在普通 chat 路径
//! 都该按用户配置走 thinking(用户开就开),但在 **compact 任务** 下,语义上 thinking
//! 永远是浪费:
//!
//! - compact prompt 是"把对话历史摘要成 summary",chain-of-thought **零价值** ——
//!   summary 只要"结论"不要"过程"
//! - reasoning_content 与 content **共享 `max_tokens` 池**(Kimi / GLM / DeepSeek
//!   官方文档均明确),thinking 占的每个 token 都从 summary 那里抢
//! - reasoning tokens 按 **output 计费**,也浪费用户钱
//! - 自适应思考模型(Kimi K2.x / DeepSeek V4 / Qwen 3.x 等)在某些 compact 历史
//!   下**仍可能触发思考**,实测 ✅ 是"加 disable 不变坏"的旁证,**不是不加的依据**
//!
//! 因此本注册表的入选标准是 **"文档证明支持 disable + thinking 默认开"**,而**不是**
//! "实测出过 bug"。issue #248 GLM-5.1 真机失败只是引爆点;注册表覆盖面应是
//! 所有四证齐全的模型,**不留 token / 时间 / 钱的浪费空间**。
//!
//! ## 入表四证(每条 entry 必须同时满足)
//!
//! 1. **thinking 默认开启** —— 未显式关时模型也思考
//! 2. **reasoning_content 与 content 共享 `max_tokens` 池** —— 否则 thinking 不抢
//!    summary 预算,无需 disable
//! 3. **官方文档明确支持 disable wire** —— 否则注入无效或被严格 endpoint 报 400
//! 4. 已选定具体 wire 形态(派 A 还是派 B)—— 不能"我猜应该是同一个 wire"
//!
//! ## 不进表的两类情况(也明确收录在本模块,作为完整决策图)
//!
//! - **无解类**(thinking 强制开 + 无 disable wire):MiniMax M2.x —— 没办法修
//! - **无需要类**(根本没 thinking 模式):moonshot-v1-* 老系列 —— 不需要 disable
//!
//! 两类都在下方注释段保留,确保读本模块即看到全 chat 协议模型的完整决策图,
//! 不会有"在模块里看不到的 model = 没考虑过的 model"的盲区。
//!
//! ## 跟其它独立优化的关系
//!
//! - **[`crate::reasoning_effort_policy`]**:本模块的对偶 —— 本模块管 compact 路径
//!   强制 disable thinking(已开 → 关);`reasoning_effort_policy` 管正常请求按
//!   Codex effort 档位映射 thinking(关 → 按档位决定开多深)。compact 路径同时经过
//!   两个 policy 时,disable 优先级高(本模块注入先到位,`reasoning_effort_policy`
//!   不翻案)。
//! - **历史 reasoning_content 剥离**(`docs/followup/44-compact-strip-history-reasoning-content.md`):
//!   独立的 input 侧优化,跟本模块的 output 侧 disable 互补。Anthropic 文档 +
//!   Claude Code 行为都印证历史 reasoning 在 compact 任务下无价值。
//! - **`compact.rs::enforce_compact_chat_message_budget`**:input message 总字节
//!   预算兜底,跟本模块串联(本模块省 output token,enforce_compact 省 input
//!   message 字节)。
//! - **`request.rs::ensure_thinking_tool_call_reasoning`**:thinking enabled 上游下
//!   给历史 assistant tool_call 补 reasoning_content 占位 `" "`。本模块 disable
//!   thinking 后,**当前调用**的模型不思考、不产 reasoning_content;但**历史**
//!   仍可能含上一轮的 reasoning_content placeholder,不影响。

use serde_json::Value;

/// compact 路径 disable-thinking 的 wire 形态。
///
/// 不同 model family 用不同 JSON 字段表达"关闭思考":
/// - 派 A `ThinkingTypeDisabled` —— 顶级 `"thinking": {"type": "disabled"}`
/// - 派 B `EnableThinkingFalse` —— 顶级 `"enable_thinking": false`
///
/// **故意不为"不能 disable"的模型(如 MiniMax M2.x)定义变体** —— 注册表缺位
/// 即"做不到",避免 caller 误以为"有变体 = 一定能 disable"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableThinkingWire {
    /// 派 A:顶级 `"thinking": {"type": "disabled"}`。
    ///
    /// 适用上游:
    /// - GLM 全系(`docs.z.ai/api-reference/llm/chat-completion` 的 `ChatThinking` schema)
    /// - Kimi K2.5 / K2.6 / kimi-for-coding(`platform.kimi.com/docs/guide/use-kimi-k2-thinking-model`)
    /// - DeepSeek V4 系列(`api-docs.deepseek.com/guides/thinking_mode`;
    ///   OpenAI SDK 走 `extra_body` 透传到顶级,wire 形态等价)
    ThinkingTypeDisabled,

    /// 派 B:顶级 `"enable_thinking": false`。
    ///
    /// 适用上游:
    /// - 阿里云百炼 Qwen 3.x 混合思考模式(`help.aliyun.com/zh/model-studio/deep-thinking`;
    ///   官方原话"由于 enable_thinking 非 OpenAI 标准参数,需要通过 extra_body 传入",
    ///   wire 上等价于顶级字段)
    /// - 小米 MiMo v2.x 全系(`help.aliyun.com/zh/model-studio/mimo` 跟 Qwen 同款
    ///   `enable_thinking` wire)
    EnableThinkingFalse,
}

impl DisableThinkingWire {
    /// 在已构造好的 chat completions body 上注入 disable 字段。
    ///
    /// **不覆盖**已有的 `thinking` / `enable_thinking` 字段 —— 语义保守,允许
    /// 上层(future caller)显式开 thinking 的极少数边界场景(虽然 compact 路径
    /// 当前没这种场景,但接口契约不应强制覆盖)。
    ///
    /// **当 thinking 确实被关掉时移除 `reasoning_effort`**(MOC-87):thinking 关掉后
    /// `reasoning_effort` 已无意义,且 DeepSeek V4 强制「`thinking.type=disabled` 与
    /// `reasoning_effort` 不可并存」—— 二者并存会被上游拒成 400(真机实证 `thinking
    /// options type cannot be disabled when reasoning_effort is set`)。两派 disable
    /// 都删:Kimi / MiMo 删它同样安全(同为「关思考」语义,`reasoning_effort` 此时无效)。
    ///
    /// **仅在 disable 真正生效时删**(chatgpt-codex-connector P2):因为本方法不覆盖
    /// 已有的 `thinking`/`enable_thinking`,若 body 已显式 **enable** thinking,则不关
    /// 思考、也保留用户/provider 的 `reasoning_effort`(不能无脑删,否则丢配置)。
    pub fn inject(self, chat_body: &mut Value) {
        let Some(obj) = chat_body.as_object_mut() else {
            return;
        };
        // 先 insert disable wire(不覆盖已有值),并判定 thinking 最终是否真被关。
        let disabled = match self {
            Self::ThinkingTypeDisabled => {
                let entry = obj
                    .entry("thinking".to_owned())
                    .or_insert_with(|| serde_json::json!({"type": "disabled"}));
                entry.get("type").and_then(|t| t.as_str()) == Some("disabled")
            }
            Self::EnableThinkingFalse => {
                let entry = obj
                    .entry("enable_thinking".to_owned())
                    .or_insert_with(|| serde_json::json!(false));
                entry.as_bool() == Some(false)
            }
        };
        // 仅当 thinking 确实被关掉才删 reasoning_effort(见上方 P2 说明)。
        if disabled {
            obj.remove("reasoning_effort");
        }
    }
}

/// 按发到上游的 model ID 查 compact-disable 策略。
///
/// 返回 `None` 表示 **本模型不应注入 disable 字段**,可能是因为:
/// - 模型没有 thinking 模式(`moonshot-v1-*` 老系列)
/// - 模型强制 thinking 且不支持 disable wire(`MiniMax-M2.x`)
/// - 模型未知 / 用户自定义未收录
///
/// 两类已知"故意不进表"的模型在本文件末尾的 [`__unsupported_model_anchors`]
/// doc-only 模块里保留专属注释段,**完整呈现** chat 协议全模型决策图。
pub fn compact_disable_thinking_wire(model: &str) -> Option<DisableThinkingWire> {
    let m = model.trim().to_ascii_lowercase();
    match m.as_str() {
        // ─── 派 A:顶级 thinking.type=disabled ──────────────────────────
        //
        // 智谱 GLM 全系。Z.AI `ChatThinking` schema 原文:
        // "When enabled, GLM-5.1 GLM-5 GLM-5-Turbo GLM-5V-Turbo GLM-4.7 GLM-4.5V
        // will think compulsorily, while GLM-4.6, GLM-4.6V, GLM-4.5 and others
        // will automatically determine whether to think, default: enabled"
        //
        // - **compulsorily 名单**(强制思考,issue #248 GLM-5.1 真机失败实证):
        //   `glm-5.1` / `glm-5` / `glm-5-turbo` / `glm-5v-turbo` / `glm-4.7` / `glm-4.5v`
        // - **自适应名单**(默认开但 compact 任务自适应可能少思考):
        //   `glm-4.6` / `glm-4.6v` / `glm-4.5`
        //   即便自适应,文档明确支持 `thinking.type=disabled` + 默认开 → 入表是稳赚不亏
        // 文档:https://docs.z.ai/api-reference/llm/chat-completion
        "glm-5.1"
        | "glm-5"
        | "glm-5-turbo"
        | "glm-5v-turbo"
        | "glm-4.7"
        | "glm-4.5v"
        | "glm-4.6"
        | "glm-4.6v"
        | "glm-4.5"
        // Kimi K2 系列(月之暗面平台 + Kimi Code 平台)
        // thinking 默认开 + 自适应 + 共享 max_tokens 池 + 支持
        // `thinking.type=disabled` 顶级字段。
        // 用户实测 compact ✅(2026-05-24,kimi-k2.6 直连)是"加 disable 不变坏"
        // 的旁证 —— **不是不加的依据**。compact 任务下 reasoning 仍是浪费 budget。
        // `kimi-for-coding` 是 Kimi Code 平台稳定 alias,后端映射最新 K2.6+。
        // 文档:https://platform.kimi.com/docs/guide/use-kimi-k2-thinking-model
        | "kimi-k2.5"
        | "kimi-k2.6"
        | "kimi-for-coding"
        // DeepSeek V4 实名 model(项目 preset `presets_data.json:13-21` 收录的两个)。
        // thinking 默认开 + 自适应 + 支持 `thinking.type=disabled`(OpenAI SDK
        // 走 extra_body,wire 上是顶级字段)。
        // PR #224 prompt 简化已让 compact 实测稳,加 disable 是进一步优化:
        // 1. 节省 output token(reasoning 不占 budget)
        // 2. 节省 wall time(实测 #224 数据:短 prompt 44s / 长 prompt 94s,
        //    主要差异在模型思考时长)
        // 3. 节省钱(reasoning 按 output token 计费)
        //
        // **故意不收 `deepseek-chat` / `deepseek-reasoner` alias**:
        // - `deepseek-reasoner` 历史上是 R1/V3-class thinking-only 模型 wrapper,
        //   thinking 是模型设计的 integral 行为而非 toggleable;disable 可能
        //   silently ignored 或上游 400 unknown field。
        // - `deepseek-chat` 历史上指 non-thinking 模式,inject disable 是 no-op
        //   占代码无意义。
        // - 两个 alias 在 2026-04 后传言"统一指向 deepseek-v4-flash",但没找到
        //   具体 doc URL 实证 + 没真机验证 disable 接受性 → 保守不收,等真实
        //   issue 报告 + 真机验证再加。
        // 文档:https://api-docs.deepseek.com/guides/thinking_mode
        | "deepseek-v4-pro"
        | "deepseek-v4-flash"
        // MiniMax-M3:thinking 默认开(输出 <think>)。真机实测(2026-06-03,
        // api.minimaxi.com 直连 MiniMax-M3 + 真实 key):**只有**顶级
        // `thinking.type=disabled` 能关掉思考(content 直出 "Four." 无 <think>);
        // `enable_thinking=false` / `reasoning_effort=none` /
        // `chat_template_kwargs.enable_thinking=false` 全部被上游忽略(仍 <think>)。
        // 故入派 A。注意:M2.x 系列(见下方无解类)直连不支持 disable,M3 与之不同。
        | "minimax-m3" => Some(DisableThinkingWire::ThinkingTypeDisabled),

        // ─── 派 B:顶级 enable_thinking=false ───────────────────────────
        //
        // 阿里云百炼 Qwen 3.x 混合思考模式。
        // 官方原话:"qwen3.6-plus 和 qwen3.6-flash 默认开启思考模式 ...
        // 由于 enable_thinking 非 OpenAI 标准参数,需要通过 extra_body 传入"。
        // wire 上 SDK 的 `extra_body={"enable_thinking": False}` 透传后等价于
        // chat body 顶级 `"enable_thinking": false`。
        // 混合思考是自适应,但 compact 任务下"不思考"永远是对的 → 入表。
        // 文档:https://help.aliyun.com/zh/model-studio/deep-thinking
        "qwen3.6-plus"
        | "qwen3.6-flash"
        | "qwen3-plus"
        | "qwen3-flash"
        // 小米 MiMo v2 全系。跟 Qwen 同款 `enable_thinking` wire(都走百炼
        // OpenAI 兼容路径,共用同一参数命名约定)。
        // `mimo-v2.5-pro` / `mimo-v2.5` / `mimo-v2-pro` 通过 xiaomimimo.com 直连
        // 或百炼第三方上架,wire 一致。`mimo-v2-flash` / `mimo-v2-omni` 同源 family。
        // 文档:https://help.aliyun.com/zh/model-studio/mimo
        //       https://www.mimo-v2.com/zh/docs/quick-start/first-api-call
        | "mimo-v2.5-pro"
        | "mimo-v2.5"
        | "mimo-v2-pro"
        | "mimo-v2-flash"
        | "mimo-v2-omni" => Some(DisableThinkingWire::EnableThinkingFalse),

        // ─── 未知 model:不注入(保守) ──────────────────────────────────
        // 用户自定义 provider 上配置的 model ID 不在表中 → 不注入,保持
        // current behavior。下方 `__unsupported_model_anchors` 注释段记录
        // **已知** 但故意不入表的模型,见那里。
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────
// 故意不入表的 model 决策锚点(rustdoc 不暴露,只给源码读者看)
//
// 本节文字记录所有"已知 chat 协议 model 但故意不进
// `compact_disable_thinking_wire` 白名单"的模型,确保读本文件 = 看到
// 全 chat 协议模型的**完整决策图**。任何在 README 兼容矩阵 / preset
// `apiFormat=openai_chat` 出现的 model,都应在本节或上面的 match arms
// **必有归宿**,不能"在模块里看不到 → 没考虑过"。
//
// === 无解类:thinking 强制开 + 无 disable wire(注册表救不了)===
//
// - `MiniMax-M2.7` —— MiniMax M2 系列 thinking 是 interleaved 强制设计,
//   `platform.minimaxi.com/docs/guides/text-m2-function-call` 明示
//   **不支持 disable**;`reasoning_split` 只控制 thinking 是否塞
//   `<think>` 标签,不能关思考本身。即便 reasoning 占满 budget,本模块
//   也救不了 —— 需要 MiniMax 上游加 disable 接口,届时再加 entry。
//   相关 issue:https://github.com/can1357/oh-my-pi/issues/626
//
// === 无需要类:模型本身没有 thinking 模式(不需要 disable)===
//
// - `moonshot-v1-8k` / `moonshot-v1-32k` / `moonshot-v1-128k` /
//   `moonshot-v1-auto` —— Kimi/Moonshot 老 base 模型(K2 系列之前),
//   纯 content-only 输出,不产生 reasoning_content。社区共识:
//   "可作为不用 thinking 的替代"。注册表 entry 是 no-op,故意不加。
// - `moonshot-v1-8k-vision-preview` / `moonshot-v1-32k-vision-preview` /
//   `moonshot-v1-128k-vision-preview` —— 上述 vision 变体,同上,
//   无 thinking。
//
// === 未实证类:疑似可加但缺真机验证(等 issue 触发再激活)===
//
// - `deepseek-chat` / `deepseek-reasoner`(alias)—— 详见上方 DeepSeek
//   match arm 的解释注释。reasoner alias 历史是 thinking-only 模型,
//   disable 行为存疑;chat alias 是 non-thinking,disable 是 no-op。
//   两个 alias 都不收,只收 `deepseek-v4-pro` / `deepseek-v4-flash` 实名。
//
// （`MiniMax-M3` 真机实测支持 `thinking.type=disabled`,已入上面的派 A
//  match arm,不在未实证类;实测细节见该 arm 注释。)
//
// === 未列入但应该归类的模型(future PR 或用户上报触发)===
//
// 任何新增的 OpenAI Chat 协议(`apiFormat == "openai_chat"`)builtin
// preset 模型,**必须**走以下决策树并落到本节或上面的 match arms:
//
// 1. 模型没 thinking → 写入本"无需要类"段
// 2. 模型 thinking 强制 + 上游无 disable 接口 → 写入"无解类"段
// 3. 模型 thinking 默认开 + 支持 disable wire → 加到
//    `compact_disable_thinking_wire` 对应派(A / B / 新派)的
//    match arm,**带文档链接 + 入表四证简述**
// 4. 文档不全 / 行为存疑 / 无真机数据 → 写入本"未实证类"段
//
// 决策记录方式:PR description 引用本模块,reviewer 对照本节 +
// `crates/registry/src/presets_data.json` 的 model 列表确认全覆盖。
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── lookup 行为 ─────────────────────────────────────────────────

    #[test]
    fn glm_compulsorily_models_resolve_to_thinking_type_disabled() {
        // Z.AI compulsorily 名单 6 个,全部派 A
        for m in [
            "glm-5.1",
            "glm-5",
            "glm-5-turbo",
            "glm-5v-turbo",
            "glm-4.7",
            "glm-4.5v",
        ] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::ThinkingTypeDisabled),
                "GLM compulsorily model {m} 必须走 ThinkingTypeDisabled wire"
            );
        }
    }

    #[test]
    fn glm_adaptive_models_also_resolve_to_thinking_type_disabled() {
        // 自适应名单(Z.AI 文档说会"automatically determine whether to think")
        // 仍入表 —— compact 任务"永远不需要 thinking"对自适应模型也成立
        for m in ["glm-4.6", "glm-4.6v", "glm-4.5"] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::ThinkingTypeDisabled),
                "GLM 自适应 model {m} 也应走 ThinkingTypeDisabled wire"
            );
        }
    }

    #[test]
    fn kimi_k2_series_resolves_to_thinking_type_disabled() {
        for m in ["kimi-k2.5", "kimi-k2.6", "kimi-for-coding"] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::ThinkingTypeDisabled),
                "Kimi K2 model {m} 必须走 ThinkingTypeDisabled wire"
            );
        }
    }

    #[test]
    fn deepseek_v4_real_models_resolve_to_thinking_type_disabled() {
        // 只收 v4-pro / v4-flash 实名 model(项目 preset 真实 model ID)
        for m in ["deepseek-v4-pro", "deepseek-v4-flash"] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::ThinkingTypeDisabled),
                "DeepSeek V4 实名 model {m} 必须走 ThinkingTypeDisabled wire"
            );
        }
    }

    #[test]
    fn deepseek_chat_and_reasoner_aliases_intentionally_not_in_registry() {
        // 故意不收:reasoner alias 历史是 thinking-only,disable 行为存疑;
        // chat alias 历史是 non-thinking,disable 是 no-op。见模块底部
        // "未实证类" 决策注释。
        for m in ["deepseek-chat", "deepseek-reasoner"] {
            assert!(
                compact_disable_thinking_wire(m).is_none(),
                "DeepSeek alias {m} 故意不进白名单(行为存疑,等真机验证)"
            );
        }
    }

    #[test]
    fn qwen3_series_resolves_to_enable_thinking_false() {
        for m in ["qwen3.6-plus", "qwen3.6-flash", "qwen3-plus", "qwen3-flash"] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::EnableThinkingFalse),
                "Qwen 3.x model {m} 必须走 EnableThinkingFalse wire(派 B)"
            );
        }
    }

    #[test]
    fn mimo_v2_series_resolves_to_enable_thinking_false() {
        for m in [
            "mimo-v2.5-pro",
            "mimo-v2.5",
            "mimo-v2-pro",
            "mimo-v2-flash",
            "mimo-v2-omni",
        ] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::EnableThinkingFalse),
                "MiMo v2 model {m} 必须走 EnableThinkingFalse wire(派 B)"
            );
        }
    }

    #[test]
    fn minimax_returns_none_unsupported_disable() {
        // MiniMax M2.x 故意不入表 —— 上游不支持 disable,无解(见模块顶部决策锚点)。
        // 注意 M3 不同:真机实测支持 thinking.type=disabled,入派 A,见下方专测。
        for m in ["MiniMax-M2.7", "MiniMax-M2.5", "MiniMax-M2"] {
            assert!(
                compact_disable_thinking_wire(m).is_none(),
                "MiniMax {m} 必须返回 None(无 disable wire,见模块顶部决策锚点)"
            );
        }
    }

    #[test]
    fn minimax_m3_resolves_to_thinking_type_disabled() {
        // 真机实测(2026-06-03,api.minimaxi.com 直连 MiniMax-M3 + 真实 key):
        // 只有顶级 thinking.type=disabled 能关思考(派 A);enable_thinking=false /
        // reasoning_effort=none / chat_template_kwargs 全被上游忽略。
        for m in ["MiniMax-M3", "minimax-m3"] {
            assert_eq!(
                compact_disable_thinking_wire(m),
                Some(DisableThinkingWire::ThinkingTypeDisabled),
                "MiniMax-M3 必须走派 A(thinking.type=disabled),实测支持"
            );
        }
    }

    #[test]
    fn moonshot_v1_legacy_returns_none_no_thinking() {
        // moonshot-v1-* 老 base 模型故意不入表 —— 没有 thinking 模式
        for m in [
            "moonshot-v1-8k",
            "moonshot-v1-32k",
            "moonshot-v1-128k",
            "moonshot-v1-auto",
            "moonshot-v1-8k-vision-preview",
            "moonshot-v1-32k-vision-preview",
            "moonshot-v1-128k-vision-preview",
        ] {
            assert!(
                compact_disable_thinking_wire(m).is_none(),
                "moonshot-v1 老模型 {m} 必须返回 None(无 thinking 模式,无需 disable)"
            );
        }
    }

    #[test]
    fn unknown_models_return_none() {
        for m in ["", "  ", "gpt-5.5", "custom-model", "unknown"] {
            assert!(
                compact_disable_thinking_wire(m).is_none(),
                "未知 model {m:?} 必须返回 None"
            );
        }
    }

    #[test]
    fn lookup_is_case_insensitive_and_trims_whitespace() {
        assert_eq!(
            compact_disable_thinking_wire("  GLM-5.1  "),
            Some(DisableThinkingWire::ThinkingTypeDisabled)
        );
        assert_eq!(
            compact_disable_thinking_wire("Kimi-K2.6"),
            Some(DisableThinkingWire::ThinkingTypeDisabled)
        );
        assert_eq!(
            compact_disable_thinking_wire("Qwen3.6-Plus"),
            Some(DisableThinkingWire::EnableThinkingFalse)
        );
        // 派 A DeepSeek 实名 model 大小写也必须 case-insensitive
        assert_eq!(
            compact_disable_thinking_wire("DeepSeek-V4-Pro"),
            Some(DisableThinkingWire::ThinkingTypeDisabled)
        );
        // 派 B MiMo 实名 model 大小写也必须 case-insensitive
        assert_eq!(
            compact_disable_thinking_wire("MIMO-V2.5-PRO"),
            Some(DisableThinkingWire::EnableThinkingFalse)
        );
    }

    // ── 注入行为 ────────────────────────────────────────────────────

    #[test]
    fn inject_thinking_type_disabled_adds_top_level_field() {
        let mut body = json!({"model": "glm-5.1", "messages": []});
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn inject_enable_thinking_false_adds_top_level_field() {
        let mut body = json!({"model": "qwen3.6-plus", "messages": []});
        DisableThinkingWire::EnableThinkingFalse.inject(&mut body);
        assert_eq!(body["enable_thinking"], json!(false));
    }

    #[test]
    fn inject_does_not_overwrite_existing_thinking_field() {
        // 已有用户显式 thinking 设置时不覆盖(语义保守)
        let mut body = json!({
            "model": "kimi-k2.6",
            "messages": [],
            "thinking": {"type": "enabled"}
        });
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled"}),
            "已有 thinking 字段时 inject 不应覆盖"
        );
    }

    #[test]
    fn inject_does_not_overwrite_existing_enable_thinking_field() {
        let mut body = json!({
            "model": "qwen3.6-plus",
            "messages": [],
            "enable_thinking": true
        });
        DisableThinkingWire::EnableThinkingFalse.inject(&mut body);
        assert_eq!(
            body["enable_thinking"],
            json!(true),
            "已有 enable_thinking 字段时 inject 不应覆盖"
        );
    }

    #[test]
    fn inject_strips_reasoning_effort_when_disabling_thinking_moc87() {
        // MOC-87 回归守卫:deepseek-v4(派A)compact body 带 reasoning_effort 时,
        // inject 必须**删掉** reasoning_effort 并加 thinking.disabled —— 否则二者并存
        // 被 DeepSeek V4 拒成 400(thinking options type cannot be disabled when
        // reasoning_effort is set)。
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [],
            "reasoning_effort": "medium"
        });
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(
            body.get("reasoning_effort").is_none(),
            "关思考后必须删 reasoning_effort,否则 deepseek-v4 compact 400;实际:{body}"
        );
        // 派B(enable_thinking=false)同样删:reasoning_effort 此时无意义
        let mut body = json!({
            "model": "mimo-v2-omni",
            "messages": [],
            "reasoning_effort": "high"
        });
        DisableThinkingWire::EnableThinkingFalse.inject(&mut body);
        assert_eq!(body["enable_thinking"], json!(false));
        assert!(body.get("reasoning_effort").is_none());
        // 没有 reasoning_effort 时不报错(remove 幂等)
        let mut body = json!({"model": "deepseek-v4-flash", "messages": []});
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn inject_keeps_reasoning_effort_when_thinking_explicitly_enabled() {
        // chatgpt-codex-connector P2:body 已显式 enable thinking 时,inject 不覆盖
        // (思考没关)→ 必须**保留** reasoning_effort,不能误删用户/provider 的配置。
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [],
            "thinking": {"type": "enabled"},
            "reasoning_effort": "high"
        });
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled"}),
            "不覆盖已有 enabled"
        );
        assert_eq!(
            body["reasoning_effort"], "high",
            "思考没被关时必须保留 reasoning_effort"
        );
        // 派B 同理:已显式 enable_thinking:true → 保留 reasoning_effort
        let mut body = json!({
            "model": "mimo-v2-omni",
            "messages": [],
            "enable_thinking": true,
            "reasoning_effort": "medium"
        });
        DisableThinkingWire::EnableThinkingFalse.inject(&mut body);
        assert_eq!(body["enable_thinking"], json!(true));
        assert_eq!(body["reasoning_effort"], "medium");
        // 已有值就是 disabled/false 时,仍删(disable 生效)
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [],
            "thinking": {"type": "disabled"},
            "reasoning_effort": "low"
        });
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert!(body.get("reasoning_effort").is_none(), "已 disabled 时也删");
    }

    #[test]
    fn inject_into_non_object_is_noop() {
        // chat_body 不是 object 时静默 noop(防御性,实际不会触发)
        let mut body = json!("not an object");
        DisableThinkingWire::ThinkingTypeDisabled.inject(&mut body);
        assert_eq!(body, json!("not an object"));

        let mut body = json!(["array"]);
        DisableThinkingWire::EnableThinkingFalse.inject(&mut body);
        assert_eq!(body, json!(["array"]));
    }
}
