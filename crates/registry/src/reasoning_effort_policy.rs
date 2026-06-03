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
/// **匹配方式**:对 `provider.id` / `provider.name` / `provider.base_url` 做
/// 大小写不敏感的 substring 匹配。**故意不只看 `provider.id`** — 因为本项目
/// healing 流程会把 builtin preset 的 id 替换成 UUID(`crates/registry/src/healing.rs`),
/// `provider.id == "deepseek"` 精确匹配在用户真实 saved config 上**永远不会命中**
/// (id 被改成 UUID 但 name/baseUrl 保留原值)。这跟 [`provider_looks_like`]
/// (`crates/adapters/src/responses/request.rs:1320`) 同款匹配范式,确保兼容
/// builtin preset id 跟用户自定义 provider 的命名习惯。
///
/// **needle 安全性**:每个 needle 设计成"足够特殊不误伤其他 provider"。
/// 不用过短 needle(如 `glm`)防自定义 provider 名字偶然命中。
///
/// 返回值约定:
/// - `HighMax` — DeepSeek 专属
/// - `Drop` — Kimi/GLM/MiMo/MiniMax/Qwen 等明确不收的上游
/// - `OpenAIEnum` — 自定义 / 未知 / 自建 OpenAI 兼容上游
pub fn reasoning_effort_wire(provider: &Provider) -> ReasoningEffortWire {
    use ReasoningEffortWire::*;

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
    //
    // needle 选择:`"deepseek"` — id slug / name "DeepSeek" / baseUrl
    // "api.deepseek.com" 三者都含此子串,UUID id 也可被 name/baseUrl 兜住。
    if provider_matches(provider, "deepseek") {
        return HighMax;
    }

    // ─── 不收 reasoning_effort 的上游(LiteLLM 实证) ─────────────────
    //
    // Kimi (Moonshot) + Kimi Code — `llms/moonshot/chat/transformation.py:91-146`
    // 的 `get_supported_openai_params` 不收 reasoning_effort;reasoning 走
    // `fill_reasoning_content` 多轮 tool_call 注入路径(line 148-194),跟
    // effort 档位无关。官方文档(platform.kimi.com/docs/guide/use-kimi-k2-thinking-model)
    // 只暴露 `thinking.type: enabled|disabled` binary 开关 + `keep: "all"` 多轮保留。
    //
    // needle:`"kimi"` 覆盖 builtin "kimi" + "kimi-code" + baseUrl "kimi.com";
    // `"moonshot"` 兜底 baseUrl "api.moonshot.cn"(name 没 kimi 子串的 legacy
    // 配置)。两个 needle 都不会命中 MiniMax / MiMo / DeepSeek / GLM / Qwen。
    if provider_matches(provider, "kimi") || provider_matches(provider, "moonshot") {
        return Drop;
    }

    // 智谱 GLM (Z.AI) — `llms/zai/chat/transformation.py:36-58` 的
    // `get_supported_openai_params` 只承认 `thinking` 字段,不收 reasoning_effort。
    // 官方文档(docs.bigmodel.cn/cn/guide/develop/openai/introduction)只展示
    // `extra_body: {thinking: {type: enabled}}`,无 effort/budget 档位。
    //
    // needle:`"zhipu"`(builtin id)/ `"bigmodel"`(baseUrl "open.bigmodel.cn")
    // 故意不用 `"glm"` — 太短,可能误伤自定义 "glm-proxy" 之类。
    if provider_matches(provider, "zhipu") || provider_matches(provider, "bigmodel") {
        return Drop;
    }

    // 阿里云百炼 Qwen — `llms/dashscope/chat/transformation.py` 全文 82 行,
    // **没有** `get_supported_openai_params` 也没有 `map_openai_params`,
    // 走父类 OpenAIGPTConfig 默认透传(可能被 dashscope silent ignored)。
    // 官方文档(help.aliyun.com/zh/model-studio/deep-thinking)用 `enable_thinking: bool`
    // + `thinking_budget: int` (tokens) — **数值预算**,不是字符串档位。
    // LiteLLM 未给出 effort→budget 数值映射,本项目也不拍脑袋猜 — 让用户
    // 通过 `provider.requestOptions` 显式设 thinking_budget 即可。
    //
    // needle 多路覆盖(builtin 两套 baseUrl 域不同):
    // - `"bailian"`:id slug "bailian" / "bailian-token-plan"
    // - `"dashscope"`:按量计费 baseUrl "dashscope.aliyuncs.com"
    // - `"maas.aliyuncs"`:Token Plan baseUrl "token-plan.cn-beijing.maas.aliyuncs.com"
    //   (阿里云 MaaS 子域专属,不会误伤其他 aliyuncs 反代)
    // - `"百炼"`:中文 name 兜底(用户 healed config name 保留中文)
    //
    // 实机验证 2026-05-25 暴露 audit miss:Token Plan baseUrl 不含 dashscope,
    // name "阿里云百炼 (Token Plan)" 不含 bailian — 漏掉这家 provider 让 Qwen
    // Token Plan 误走 OpenAIEnum fallback、wire 上写 reasoning_effort=high。
    if provider_matches(provider, "bailian")
        || provider_matches(provider, "dashscope")
        || provider_matches(provider, "maas.aliyuncs")
        || provider_matches(provider, "百炼")
    {
        return Drop;
    }

    // 小米 MiMo v2 — LiteLLM `types/utils.py:3333` 仅 `XIAOMI_MIMO = "xiaomi_mimo"`
    // enum 注册,**无 `llms/xiaomi_mimo/` 目录**,无 transformation。走
    // openai_like 通用路径 = 零处理。本项目代码 [`compact_thinking_policy`]
    // 推断 MiMo v2 走 `enable_thinking: false` wire(派 B,跟 Qwen 同款)。
    // reasoning_effort 字段在 MiMo 文档(mimo-v2.com/zh/docs)没有提及,Drop。
    //
    // needle:`"mimo"`(覆盖 builtin "xiaomi-mimo-payg" / "xiaomi-mimo-token-plan"
    // + baseUrl "api.xiaomimimo.com" / "token-plan-*.xiaomimimo.com")。
    if provider_matches(provider, "mimo") {
        return Drop;
    }

    // MiniMax M2.x — `llms/minimax/chat/transformation.py:87-102` 的
    // `get_supported_openai_params` 只承认 `thinking` + 自有 `reasoning_split`,
    // **不收** reasoning_effort。本项目 `sanitize_minimax_chat_body` 已主动
    // 剥掉(详见 [`crate::compact_thinking_policy::__unsupported_model_anchors`])。
    // Drop 是更早一步声明,语义更清晰。
    //
    // needle:`"minimax"`(覆盖 builtin id "minimax" + baseUrl "api.minimaxi.com"
    // — substring `minimax` 也命中 `minimaxi`)。
    if provider_matches(provider, "minimax") {
        return Drop;
    }

    // ─── Fallback:自定义 / 未知 chat-compat 上游 ────────────────────
    //
    // 没有明确证据时走 OpenAI 标准 enum,因为:
    // 1. 该路径是"无害降级"(标准 enum 上限是 high,xhigh 砍到 high 不丢命)
    // 2. 用户自定义反代 / 兼容端点最可能就是 OpenAI 标准
    OpenAIEnum
}

/// `provider.id` / `provider.name` / `provider.base_url` 任一字段(大小写不敏感)
/// 含 `needle` 子串即返回 true.
///
/// 跟 `crates/adapters/src/responses/request.rs::provider_looks_like` 同款匹配
/// 范式,但因 registry crate 不能反向依赖 adapters,在此独立实现。
fn provider_matches(provider: &Provider, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    [&provider.id, &provider.name, &provider.base_url]
        .iter()
        .any(|value| value.to_ascii_lowercase().contains(&needle))
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
    let mut wire = reasoning_effort_wire(provider);
    // MiniMax-M3 起原生接受 `reasoning_effort`(2026-06-03 真机实测 api.minimaxi.com
    // 直连:200;M2.x 同字段 400)。即便实测当前档位不改变 M3 思考深度,也按 OpenAI
    // 规范把客户端意图透传给上游(交由上游决定),不主动剥除。M2.x 仍 Drop(白名单外
    // 字段会 400)。model 取实际请求体(可能非 provider.default)。
    if matches!(wire, ReasoningEffortWire::Drop)
        && provider_matches(provider, "minimax")
        && body
            .get("model")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.to_ascii_lowercase().starts_with("minimax-m3"))
    {
        wire = ReasoningEffortWire::OpenAIEnum;
    }
    wire.apply(body, codex_effort, &provider.id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn provider(id: &str) -> Provider {
        provider_full(id, id, "https://example.test")
    }

    fn provider_full(id: &str, name: &str, base_url: &str) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            base_url: base_url.into(),
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
        assert!(apply("deepseek", "ultra").as_object().unwrap().is_empty());
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

    #[test]
    fn minimax_m3_passes_through_reasoning_effort_but_m2_drops() {
        // 真机实测(2026-06-03,api.minimaxi.com 直连):MiniMax-M3 接受
        // reasoning_effort(200),M2.x 同字段 400。M3 按 OpenAI 规范透传给上游
        // (不主动剥),M2.x 仍 Drop。model 取请求体实际值(可能非 provider.default)。
        let p = provider_full("minimax", "MiniMax", "https://api.minimaxi.com/v1");

        let mut m3 = Map::new();
        m3.insert("model".into(), Value::String("MiniMax-M3".into()));
        apply_reasoning_effort(&mut m3, &p, "high");
        assert_eq!(
            m3.get("reasoning_effort").and_then(|v| v.as_str()),
            Some("high"),
            "M3 应透传 reasoning_effort=high"
        );

        let mut m2 = Map::new();
        m2.insert("model".into(), Value::String("MiniMax-M2.7".into()));
        apply_reasoning_effort(&mut m2, &p, "high");
        assert!(
            !m2.contains_key("reasoning_effort"),
            "M2.x 必须 Drop reasoning_effort(白名单外字段会 400)"
        );
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

    // ─── healed config 形态(UUID id + 自然 name/baseUrl) ───────────────────
    //
    // healing 流程会把 builtin preset 的 id 替换成 UUID,真实用户 saved config
    // 的 DeepSeek provider id 形如 "34fe2433"。precise id 匹配在此场景会失效 —
    // 必须 fallback 到 name / baseUrl substring(本测试组验证)。

    #[test]
    fn deepseek_uuid_id_matched_by_name() {
        let p = provider_full("34fe2433", "DeepSeek", "https://api.deepseek.com/v1");
        assert_eq!(
            reasoning_effort_wire(&p),
            ReasoningEffortWire::HighMax,
            "healed UUID id 必须靠 name/baseUrl 兜住,否则 issue #254 修复对真实用户无效"
        );
    }

    #[test]
    fn deepseek_uuid_id_xhigh_real_user_e2e() {
        // 真实用户 config 形态端到端测试:Codex 发 xhigh → wire 上是 max
        let p = provider_full("34fe2433", "DeepSeek", "https://api.deepseek.com/v1");
        let mut body = Map::new();
        apply_reasoning_effort(&mut body, &p, "xhigh");
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn kimi_uuid_id_matched_by_baseurl() {
        // Kimi builtin healed:UUID id + name "Kimi (月之暗面)" + baseUrl moonshot.cn
        let p = provider_full("11e7e07c", "Kimi", "https://api.moonshot.cn/v1");
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }

    #[test]
    fn mimo_uuid_id_matched_by_baseurl() {
        let p = provider_full(
            "b863a67c",
            "Xiaomi MiMo (Token Plan)",
            "https://token-plan-sgp.xiaomimimo.com/v1",
        );
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }

    #[test]
    fn minimax_uuid_id_matched_by_baseurl() {
        let p = provider_full("abc123", "MiniMax", "https://api.minimaxi.com/v1");
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }

    #[test]
    fn zhipu_uuid_id_matched_by_baseurl() {
        let p = provider_full("xyz789", "GLM", "https://open.bigmodel.cn/api/paas/v4");
        // 注:zhipu 走 bigmodel needle 而非 glm(glm 太短易误伤)
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }

    #[test]
    fn bailian_uuid_id_matched_by_baseurl() {
        let p = provider_full(
            "qwe456",
            "阿里云百炼",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        );
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }

    #[test]
    fn bailian_token_plan_uuid_matched_by_maas_subdomain() {
        // 实机暴露 audit miss(2026-05-25):Token Plan baseUrl 域不同于按量计费,
        // 必须有 needle 兜住,否则 Qwen Token Plan 会走 OpenAIEnum fallback。
        let p = provider_full(
            "tokenplan-uuid",
            "阿里云百炼 (Token Plan)",
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        );
        assert_eq!(
            reasoning_effort_wire(&p),
            ReasoningEffortWire::Drop,
            "阿里云百炼 Token Plan(maas.aliyuncs 子域)必须命中 Drop"
        );
    }

    #[test]
    fn bailian_token_plan_matched_by_chinese_name() {
        // baseUrl 完全没 maas / aliyuncs 关键字时,中文 name "百炼" 兜底
        let p = provider_full("custom-uuid", "百炼自建反代", "https://my.proxy.example/v1");
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }

    // ─── 防误伤测试:确保 needle 不会把无关 provider 错分类 ─────────────────

    #[test]
    fn custom_proxy_without_any_needle_stays_openai_enum() {
        let p = provider_full(
            "user-proxy-1",
            "my-internal-proxy",
            "https://api.foo.bar/v1",
        );
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::OpenAIEnum);
    }

    #[test]
    fn openai_official_stays_openai_enum() {
        // OpenAI 官方 chat completions 应走 OpenAIEnum(虽然 OpenAI 自家 chat 不暴露
        // reasoning_effort,但 fallback 路径下 wire 写出来是无害的标准字段)
        let p = provider_full("openai", "OpenAI", "https://api.openai.com/v1");
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::OpenAIEnum);
    }

    #[test]
    fn needle_kimi_does_not_match_unrelated() {
        // 自定义 provider 名字偶然不含 kimi/moonshot 不该被误判
        let p = provider_full("custom", "MyProxy", "https://example.com");
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::OpenAIEnum);
    }

    #[test]
    fn minimax_substring_in_minimaxi_baseurl_matches() {
        // baseUrl 真实形态 api.minimaxi.com 含 "minimax" 子串,需保证命中
        let p = provider_full("xx", "MiniMax", "https://api.minimaxi.com/v1");
        assert_eq!(reasoning_effort_wire(&p), ReasoningEffortWire::Drop);
    }
}
