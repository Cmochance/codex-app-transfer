//! Codex `reasoning.effort` → 上游 chat 协议字段的 per-provider 映射策略.
//!
//! ## 为什么需要 per-provider?
//!
//! Codex CLI 在 request body 里发的 `reasoning.effort` 是 OpenAI **Responses API**
//! 的字段(`minimal / low / medium / high / xhigh`)。Chat completions 上游对此字段
//! **没有统一标准**:
//!
//! - **DeepSeek V4** 官方扩展 `reasoning_effort: high|max`(api-docs.deepseek.com/guides/thinking_mode)
//!   — upstream 自己把 low/medium → high、xhigh → max,有真实"max"档,默认 high,
//!   agentic 场景(Claude Code/OpenCode)自动 max
//! - **OpenAI Chat Completions** 不暴露 reasoning_effort(那是 Responses API 字段)
//! - **Kimi / GLM / MiMo / MiniMax / Qwen** 文档 + LiteLLM 实证都**不承认**
//!   reasoning_effort 字段(LiteLLM 的 `get_supported_openai_params` 白名单全部不收)
//! - **Qwen / 阿里云百炼** 有自己的 `thinking_budget: int` (token 数),但 LiteLLM
//!   未给出 effort→budget 数值映射 — 没靠谱上游证据可参照
//!
//! 因此一刀切的"全 chat 协议共用 normalize_chat_reasoning_effort"会:
//!
//! 1. 对 DeepSeek **致命**:把 xhigh/max 砍到 high → DeepSeek max 档不可达 (issue #254)
//! 2. 对 Kimi/GLM/MiMo/MiniMax/Qwen **脏**:塞它们不认的字段,无害但破坏不变量
//!
//! ## 跟 [`crate::compact_thinking_policy`] 的对偶
//!
//! - `compact_thinking_policy` 管 **compact 任务强制 disable thinking**(已开 → 关掉)
//! - `reasoning_effort_policy` 管 **正常请求按档位映射 thinking**(关 → 按 effort 决定开多深)
//!
//! 两表入表证据格式完全对齐,review 友好。compact 路径下两者写**不同 key**
//! (本 policy 写 `reasoning_effort` / `compact_thinking_policy` 写 `thinking` 或
//! `enable_thinking`),无论谁先跑都不互踩;`Drop` 集合本身就不写 `reasoning_effort`,
//! wire 更干净。
//!
//! ## 入表证据(每条 entry 必须同时满足)
//!
//! 1. **官方文档明确**(`reasoning_effort` 是否承认 + 接受档位 + 默认行为)
//! 2. **LiteLLM 上游实现交叉验证**(`docs/litellm/litellm/llms/<provider>/`)
//! 3. **wire 形态选定**(`ReasoningEffortWire` 哪一个变体)
//! 4. 未选定时显式 `Drop`(不主动塞字段)而非"瞎猜一个"
//!
//! ## 范式对齐 `DisableThinkingWire::inject`
//!
//! enum 暴露 [`ReasoningEffortWire::apply`] 方法把"我是谁 + 怎么写入"封在一起,
//! caller 只需 `wire.apply(body, effort)`;映射表收敛到 [`ReasoningEffortWire::upstream_value`]
//! 一处,新增 wire 形态只改一个方法。

use serde_json::{json, Map, Value};

use crate::schema::Provider;

/// Codex `reasoning.effort` 转换成上游接受的字段形态.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffortWire {
    /// **DeepSeek V4 风格** — `reasoning_effort: string`,但有效值只有 `"high"` / `"max"`.
    ///
    /// 映射:
    /// - `minimal` / `low` / `medium` / `high` → `"high"`
    /// - `xhigh` / `max` / `highest` → `"max"`
    /// - `none` / `off` / `disabled` → drop(让默认行为兜底)
    ///
    /// 注:LiteLLM `llms/deepseek/chat/transformation.py:41-63` 把所有非 none 值
    /// 折叠成 `thinking.type=enabled`,不区分档位 — 比 DeepSeek 官方 docs 保守。
    /// 本项目信官方 docs 而非 LiteLLM 保守实现(用户报告 issue #254 — xhigh 砍到 high
    /// 让 DeepSeek max 档完全不可达,违反用户期望)。
    HighMax,

    /// **OpenAI Responses 标准 enum** — `reasoning_effort: string` 接 minimal/low/medium/high.
    ///
    /// 映射:
    /// - `minimal` / `low` / `medium` / `high` → 同名透传(lowercase)
    /// - `xhigh` / `max` / `highest` → `"high"`(标准 enum 上限)
    /// - `none` / `off` / `auto` / `disabled` → drop
    ///
    /// 适用:自定义 / 未知 chat-compat 上游的保守 fallback,以及无 provider 上下文
    /// 的旁路场景(测试 / 早期协议解析阶段)。
    OpenAIEnum,

    /// **完全丢弃 reasoning_effort 字段**,什么都不传给上游.
    ///
    /// 适用:Kimi / Kimi Code / GLM / MiMo / MiniMax / Qwen — 这些上游
    /// **不承认 reasoning_effort 字段**(LiteLLM 白名单全部排除),让 upstream 用
    /// 自家默认 thinking 行为(通常默认开 + 自适应深度),或让用户在
    /// `provider.requestOptions` 显式覆盖 `thinking_budget` / `enable_thinking` 等
    /// provider-native 字段。
    ///
    /// **故意不主动注入 `thinking.type=enabled`** — 上游默认就开,主动加可能
    /// 跟 [`crate::compact_thinking_policy`] 的 disable 逻辑互踩(虽然 disable
    /// 走 `entry().or_insert()` 不覆盖已存在,但额外注入仍违反"最小干预"原则)。
    Drop,
}

impl ReasoningEffortWire {
    /// 把 Codex effort 字符串映射成上游接受的 `reasoning_effort` 值.
    ///
    /// 返回 `None` 表示**不应写入** `reasoning_effort` 字段(Drop variant、
    /// none/off/disabled/auto 等关闭语义、或未知 effort)。
    fn upstream_value(self, effort: &str) -> Option<&'static str> {
        match self {
            Self::HighMax => match effort {
                "none" | "off" | "disabled" | "auto" => None,
                "xhigh" | "max" | "highest" => Some("max"),
                "minimal" | "low" | "medium" | "high" => Some("high"),
                _ => None,
            },
            Self::OpenAIEnum => match effort {
                "none" | "off" | "auto" | "disabled" => None,
                "xhigh" | "max" | "highest" => Some("high"),
                "minimal" => Some("minimal"),
                "low" => Some("low"),
                "medium" => Some("medium"),
                "high" => Some("high"),
                _ => None,
            },
            Self::Drop => None,
        }
    }

    /// 把 Codex effort 写进 chat body.
    ///
    /// `effort` 应已 trim + lowercase + 非空(caller 责任,本方法不再 normalize)。
    /// `provider_id` 仅用于 tracing log,不影响行为。
    ///
    /// 行为:
    /// - 命中合法映射 → 写入 `reasoning_effort` 字段
    /// - 命中"主动 drop"(Drop variant、关闭语义)→ `debug` log,什么都不写
    /// - 未知 effort 字符串 → `warn` log(可能是协议变更 / 用户 typo),什么都不写
    pub fn apply(self, body: &mut Map<String, Value>, effort: &str, provider_id: &str) {
        match (self, self.upstream_value(effort)) {
            (_, Some(upstream)) => {
                body.insert("reasoning_effort".into(), json!(upstream));
            }
            (Self::Drop, None) => {
                tracing::debug!(
                    target: "registry::reasoning_effort_policy",
                    provider = provider_id,
                    codex_effort = effort,
                    "provider does not accept reasoning_effort wire; relying on upstream default; user can override via provider.requestOptions"
                );
            }
            (Self::HighMax | Self::OpenAIEnum, None) => {
                let is_disable = matches!(effort, "none" | "off" | "disabled" | "auto");
                if is_disable {
                    tracing::debug!(
                        target: "registry::reasoning_effort_policy",
                        provider = provider_id,
                        codex_effort = effort,
                        "codex requested reasoning disable; not writing reasoning_effort"
                    );
                } else {
                    tracing::warn!(
                        target: "registry::reasoning_effort_policy",
                        provider = provider_id,
                        codex_effort = effort,
                        "unknown codex reasoning.effort value; dropping (possible protocol change or user typo)"
                    );
                }
            }
        }
    }
}

/// 按 provider 查 reasoning_effort wire 策略.
///
/// 返回值约定:
/// - `HighMax` — DeepSeek 专属
/// - `Drop` — Kimi/GLM/MiMo/MiniMax/Qwen 等明确不收的上游
/// - `OpenAIEnum` — 自定义 / 未知 / 自建 OpenAI 兼容上游
pub fn reasoning_effort_wire(provider: &Provider) -> ReasoningEffortWire {
    use ReasoningEffortWire::*;

    // 按 provider.id 精确匹配 builtin preset(`presets_data.json` 各家 `id` 字段)。
    // 自定义 provider 的 id 通常是用户输入的任意字符串,不命中任何 arm → 走 fallback。
    match provider.id.as_str() {
        // ─── DeepSeek V4 ─────────────────────────────────────────────────
        //
        // 官方文档(api-docs.deepseek.com/guides/thinking_mode)原话:
        // "在思考模式中,为了兼容性,`low` 和 `medium` 被映射到 `high`,
        // `xhigh` 被映射到 `max`。在思考模式中,常规请求的默认努力程度为 `high`;
        // 对于某些复杂代理请求(如 Claude Code、OpenCode),努力程度自动设置为 `max`"。
        //
        // OpenAI 格式 wire:`{"reasoning_effort": "high|max"}`
        // Anthropic 格式 wire:`{"output_config": {"effort": "high|max"}}`
        //
        // LiteLLM `llms/deepseek/chat/transformation.py:41-63` 实际把所有非 none
        // 折叠成 `thinking.type=enabled`,**不区分档位** — 比官方 docs 保守。本
        // 项目信官方 docs(issue #254 报告:LiteLLM 这种处理让用户选 xhigh 时
        // DeepSeek max 档完全不可达,违反预期)。
        "deepseek" => HighMax,

        // ─── 不收 reasoning_effort 的上游(LiteLLM 实证) ─────────────────
        //
        // Kimi (Moonshot) — `llms/moonshot/chat/transformation.py:91-146` 的
        // `get_supported_openai_params` 不收 reasoning_effort;reasoning 走
        // `fill_reasoning_content` 多轮 tool_call 注入路径(line 148-194),跟
        // effort 档位无关。官方文档(platform.kimi.com/docs/guide/use-kimi-k2-thinking-model)
        // 只暴露 `thinking.type: enabled|disabled` binary 开关 + `keep: "all"` 多轮保留。
        "kimi" | "kimi-code" => Drop,

        // 智谱 GLM (Z.AI) — `llms/zai/chat/transformation.py:36-58` 的
        // `get_supported_openai_params` 只承认 `thinking` 字段,不收 reasoning_effort。
        // 官方文档(docs.bigmodel.cn/cn/guide/develop/openai/introduction)只展示
        // `extra_body: {thinking: {type: enabled}}`,无 effort/budget 档位。
        "zhipu" => Drop,

        // 阿里云百炼 Qwen — `llms/dashscope/chat/transformation.py` 全文 82 行,
        // **没有** `get_supported_openai_params` 也没有 `map_openai_params`,
        // 走父类 OpenAIGPTConfig 默认透传(可能被 dashscope silent ignored)。
        // 官方文档(help.aliyun.com/zh/model-studio/deep-thinking)用 `enable_thinking: bool`
        // + `thinking_budget: int` (tokens) — **数值预算**,不是字符串档位。
        // LiteLLM 未给出 effort→budget 数值映射,本项目也不拍脑袋猜 — 让用户
        // 通过 `provider.requestOptions` 显式设 thinking_budget 即可。
        "bailian" | "bailian-token-plan" => Drop,

        // 小米 MiMo v2 — LiteLLM `types/utils.py:3333` 仅 `XIAOMI_MIMO = "xiaomi_mimo"`
        // enum 注册,**无 `llms/xiaomi_mimo/` 目录**,无 transformation。走
        // openai_like 通用路径 = 零处理。本项目代码 [`compact_thinking_policy`]
        // 推断 MiMo v2 走 `enable_thinking: false` wire(派 B,跟 Qwen 同款)。
        // reasoning_effort 字段在 MiMo 文档(mimo-v2.com/zh/docs)没有提及,Drop。
        "xiaomi-mimo-payg" | "xiaomi-mimo-token-plan" => Drop,

        // MiniMax M2.x — `llms/minimax/chat/transformation.py:87-102` 的
        // `get_supported_openai_params` 只承认 `thinking` + 自有 `reasoning_split`,
        // **不收** reasoning_effort。本项目 `sanitize_minimax_chat_body` 已主动
        // 剥掉(详见 [`crate::compact_thinking_policy::__unsupported_model_anchors`])。
        // Drop 是更早一步声明,语义更清晰。
        "minimax" => Drop,

        // ─── Fallback:自定义 / 未知 chat-compat 上游 ────────────────────
        //
        // 没有明确证据时走 OpenAI 标准 enum,因为:
        // 1. 该路径是"无害降级"(标准 enum 上限是 high,xhigh 砍到 high 不丢命)
        // 2. 用户自定义反代 / 兼容端点最可能就是 OpenAI 标准
        _ => OpenAIEnum,
    }
}

/// 便捷函数:按 provider 查 policy + 写 effort.
///
/// 等价于 `reasoning_effort_wire(provider).apply(body, effort, &provider.id)`。
///
/// `codex_effort` 应已 trim + lowercase + 非空(caller 责任,本函数不再 normalize)。
pub fn apply_reasoning_effort(
    body: &mut Map<String, Value>,
    provider: &Provider,
    codex_effort: &str,
) {
    if codex_effort.is_empty() {
        return;
    }
    reasoning_effort_wire(provider).apply(body, codex_effort, &provider.id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            base_url: "https://example.test".into(),
            api_format: "openai_chat".into(),
            auth_scheme: "bearer".into(),
            api_key: String::new(),
            models: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            extra_headers: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    fn apply(provider_id: &str, effort: &str) -> Value {
        let mut body = Map::new();
        apply_reasoning_effort(&mut body, &provider(provider_id), effort);
        Value::Object(body)
    }

    // ─── DeepSeek: xhigh/max → "max", 其他 → "high", none → drop ───────

    #[test]
    fn deepseek_xhigh_maps_to_max() {
        assert_eq!(apply("deepseek", "xhigh")["reasoning_effort"], "max");
    }

    #[test]
    fn deepseek_max_maps_to_max() {
        assert_eq!(apply("deepseek", "max")["reasoning_effort"], "max");
    }

    #[test]
    fn deepseek_high_maps_to_high() {
        assert_eq!(apply("deepseek", "high")["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_low_maps_to_high() {
        // DeepSeek 官方:low/medium 被上游 normalize 成 high。本端也 normalize
        // 一次(冗余但语义清晰),或直接发 low 让上游处理。这里选本端 normalize。
        assert_eq!(apply("deepseek", "low")["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_medium_maps_to_high() {
        assert_eq!(apply("deepseek", "medium")["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_none_drops_field() {
        assert!(apply("deepseek", "none").as_object().unwrap().is_empty());
    }

    #[test]
    fn deepseek_unknown_drops_field() {
        // 未知 effort 字符串走 warn log + drop(测试不验 log,只验行为)
        assert!(apply("deepseek", "ultra")
            .as_object()
            .unwrap()
            .is_empty());
    }

    // ─── Drop 类:全部不写 reasoning_effort ─────────────────────────────

    #[test]
    fn kimi_drops_all_efforts() {
        for effort in ["low", "medium", "high", "xhigh", "max", "minimal"] {
            assert!(
                apply("kimi", effort).as_object().unwrap().is_empty(),
                "kimi effort={effort} should drop"
            );
        }
    }

    #[test]
    fn kimi_code_drops() {
        assert!(apply("kimi-code", "xhigh").as_object().unwrap().is_empty());
    }

    #[test]
    fn zhipu_drops() {
        assert!(apply("zhipu", "max").as_object().unwrap().is_empty());
    }

    #[test]
    fn bailian_drops() {
        assert!(apply("bailian", "xhigh").as_object().unwrap().is_empty());
    }

    #[test]
    fn bailian_token_plan_drops() {
        assert!(apply("bailian-token-plan", "high")
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn xiaomi_mimo_payg_drops() {
        assert!(apply("xiaomi-mimo-payg", "xhigh")
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn xiaomi_mimo_token_plan_drops() {
        assert!(apply("xiaomi-mimo-token-plan", "max")
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn minimax_drops() {
        assert!(apply("minimax", "high").as_object().unwrap().is_empty());
    }

    // ─── Fallback (自定义 provider): OpenAI 标准 enum ────────────────────

    #[test]
    fn custom_provider_xhigh_clamps_to_high() {
        assert_eq!(
            apply("custom-openai-compat", "xhigh")["reasoning_effort"],
            "high"
        );
    }

    #[test]
    fn custom_provider_max_clamps_to_high() {
        assert_eq!(apply("my-proxy", "max")["reasoning_effort"], "high");
    }

    #[test]
    fn custom_provider_low_passthrough() {
        assert_eq!(apply("anything", "low")["reasoning_effort"], "low");
    }

    #[test]
    fn custom_provider_minimal_passthrough() {
        assert_eq!(apply("anything", "minimal")["reasoning_effort"], "minimal");
    }

    #[test]
    fn custom_provider_unknown_drops() {
        assert!(apply("anything", "weird-value")
            .as_object()
            .unwrap()
            .is_empty());
    }

    // ─── 空 / 边界 ──────────────────────────────────────────────────────

    #[test]
    fn empty_effort_short_circuits() {
        // apply_reasoning_effort 在 caller 已经 trim+lowercase 后,空串直接 short-circuit
        assert!(apply("deepseek", "").as_object().unwrap().is_empty());
    }

    // ─── enum 方法 / wire 查询直接测试(为未来新增 wire 形态保留) ─────────

    #[test]
    fn upstream_value_drop_returns_none_for_all_efforts() {
        let wire = ReasoningEffortWire::Drop;
        for effort in ["low", "medium", "high", "xhigh", "max", "none", "weird"] {
            assert!(
                wire.upstream_value(effort).is_none(),
                "Drop variant 对 effort={effort} 必须返回 None"
            );
        }
    }

    #[test]
    fn wire_selection_for_known_provider_ids() {
        assert_eq!(
            reasoning_effort_wire(&provider("deepseek")),
            ReasoningEffortWire::HighMax
        );
        assert_eq!(
            reasoning_effort_wire(&provider("kimi")),
            ReasoningEffortWire::Drop
        );
        assert_eq!(
            reasoning_effort_wire(&provider("unknown-custom")),
            ReasoningEffortWire::OpenAIEnum
        );
    }
}
