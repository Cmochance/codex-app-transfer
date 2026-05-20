use codex_app_transfer_registry::Provider;
use serde_json::{json, Value};

use super::provider_looks_like;

/// Responses tool 定义 → Chat tool 定义.
/// 把单个 Responses API tool 转成零或多个 Chat Completions tool。
///
/// 返回 `Vec<Value>` 而非 `Option<Value>` 是为了支持 `type:"namespace"` 展平
/// (Codex CLI 把 MCP server 工具集打成一个 namespace 包,内层 5-26 个具体
/// `type:"function"`,实测 9 个 server 共 88 个 tool 在第三方 chat provider
/// 之前必须展平为顶级 function 数组)。
///
/// 实测形态(2026-05-09 抓本机 ~/.codex/config.toml 配 12+ MCP server 时
/// Codex CLI 的入站 Responses API body):
/// - `function` × 420 / 轮(Codex 内置 + `read_mcp_resource` 等通用 meta)
/// - `namespace` × 218 / 轮(9 个 server 包装,内层 88 个具体 MCP function)
/// - `custom` × 28 / 轮(`apply_patch` 用 lark grammar)
/// - `web_search` × 28 / 轮(server-side built-in,无 name/parameters,
///   chat 端无等价,继续 drop + warn_once 提示用户)
pub fn convert_responses_tool_to_chat_tool(tool: &Value, provider: Option<&Provider>) -> Vec<Value> {
    let Some(obj) = tool.as_object() else {
        return vec![];
    };
    let Some(ttype) = obj.get("type").and_then(|v| v.as_str()) else {
        return vec![];
    };
    match ttype {
        "function" => {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut parameters = obj.get("parameters").cloned().unwrap_or_else(|| json!({}));
            if let Some(po) = parameters.as_object_mut() {
                if !po.contains_key("type") {
                    po.insert("type".into(), Value::String("object".into()));
                }
            }
            let strict = obj.get("strict").and_then(|v| v.as_bool()).unwrap_or(false);
            vec![json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                    "strict": strict,
                },
            })]
        }
        "custom" => {
            // Custom tool(无 JSON schema)降级为接受单字符串 input 的 function
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            vec![json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Free-form input passed verbatim to the tool.",
                            }
                        },
                        "required": ["input"],
                    },
                    "strict": false,
                },
            })]
        }
        "namespace" => {
            // Codex CLI 用 `type:"namespace"` 包装 MCP server 工具集 — 实测
            // `~/.codex/config.toml` 配的每个 `[mcp_servers.<name>]` 在入站
            // Responses API body 里都是一个 `{type:"namespace", name:"mcp__<name>__",
            // tools:[ {type:"function", ...}, ... ]}` 包,内层 5-26 个具体 function。
            // 第三方 chat completions provider 不认 namespace type,**必须递归
            // 展平内层 functions 为顶级 tool 数组**,模型才能看到具体 MCP tools
            // 像 `notion_create_pages` / `figma_get_file_data` 等并直接调用。
            //
            // 借鉴 `7as0nch/mimo2codex` `src/translate/reqToChat.ts:232-250` 同名
            // namespace 展平逻辑(见 reqToChat 注释 "Shape we've seen in the wild")。
            //
            // 不做的:展平内层时**不**改写 tool name(实测内层 function name 已经
            // 自带前缀如 `migrate_pages_to_workers_guide`,无冲突风险);**不**保留
            // namespace 包裹元数据(模型只需看到具体 tool name + description 即可)。
            //
            // **⚠️ 跟 `gemini_native::request.rs::responses_tools_to_chat_tools`
            // 的 `"namespace"` 分支故意分歧**:那边把 `namespace.name + description`
            // 作 prefix 注入到每个内层 function.description(`[MCP server <n>: <d>]`
            // ...)。原因:Gemini 3.x 缺这层 server-level context 时倾向选"动作类"
            // 工具(误选 create 而非 search,user 实测)。Chat completions 上游
            // (OpenAI/Anthropic Messages)未观察到此 bias,故 chat 路径不注入,
            // 保持 wire 干净。如果要让两个路径行为一致,可以把 prefix 逻辑提到
            // 公共 helper — 但当前 chat 路径模型选择没问题,保持差异化最小风险。
            let Some(inner) = obj.get("tools").and_then(|v| v.as_array()) else {
                tracing::debug!(
                    namespace_name = ?obj.get("name").and_then(|v| v.as_str()),
                    "dropping namespace tool with no nested `tools` array"
                );
                return vec![];
            };
            inner
                .iter()
                .flat_map(|inner_tool| convert_responses_tool_to_chat_tool(inner_tool, provider))
                .collect()
        }
        // Codex.app 默认每轮都给 tools 数组传 `{type:"web_search",
        // external_web_access:true, search_content_types:["text","image"]}`
        // (实测 dump 确认),作为 Responses API 标准 server-side built-in。
        // 各家上游 chat completions API 用各自字段表达 web search 能力,
        // 代理层负责 per-provider 适配。本提交先实施 MiMo,Kimi /
        // DeepSeek / MiniMax / Qwen / GLM 留 TODO,逐家文档实证后跟进。
        // 实施跟踪见 `docs/web-search-implementation-tracker.md`。
        "web_search" | "web_search_preview" => convert_web_search_tool(obj, provider),
        // Responses 专属类型(local_shell / file_search / computer_use* /
        // code_interpreter / image_generation / mcp 等)Chat 端点不认,丢弃。
        // warn_once 防多轮重发刷屏(借鉴 mimo2codex `reqToChat.ts:158-172` warnOnce)。
        other => {
            crate::warn_once_drop_tool(other);
            vec![]
        }
    }
}

/// Per-provider `web_search` / `web_search_preview` 适配。Codex.app 入站默认
/// 每轮发 OpenAI Responses API 标准的 `{type:"web_search", external_web_access:true,
/// search_content_types:["text","image"]}`,本函数转成各上游 chat API 真实
/// 支持的形态。
///
/// **逐家文档实证后才能加映射**(`docs/web-search-implementation-tracker.md`)。
/// 暂未实证 of provider 走 `_ => warn_once + drop`,模型退化到用 MCP 工具(如
/// 用户配的 Node Repl + JS fetch DDG 这种自带能力)联网,**功能仍可用,只是
/// 不走最高效路径**。
///
/// ## 已实证 provider
///
/// ### Xiaomi MiMo(`platform.xiaomimimo.com`)
///
/// 1:1 复刻 `7as0nch/mimo2codex@fe79178` `src/translate/reqToChat.ts:196-209`。
/// MiMo chat 端原生支持 `type:"web_search"`(MiMo 私有扩展,**需要在 MiMo
/// 控制台开 Web Search Plugin** —— https://platform.xiaomimimo.com/#/console/plugin)。
///
/// 字段透传:`user_location` / `max_keyword` / `force_search` / `limit`(全可选)。
/// OpenAI 的 `external_web_access` / `search_content_types` / `search_context_size`
/// 在 MiMo 无等价,silent drop(对齐 mimo2codex)。
fn convert_web_search_tool(
    obj: &serde_json::Map<String, Value>,
    provider: Option<&Provider>,
) -> Vec<Value> {
    let Some(provider) = provider else {
        crate::warn_once_drop_tool("web_search:no-provider");
        return vec![];
    };

    // A 层:配置开关。`request_options.web_search_enabled` 默认 false。
    // 用户必须主动在 codex-app-transfer config 里标 true 才会启用;UI 提示
    // 文案:"web_search 需要先在 Xiaomi MiMo 控制台付费启用后才能正常使用"。
    if !provider_web_search_enabled(provider) {
        crate::warn_once_drop_tool("web_search:disabled-by-config");
        return vec![];
    }

    // B 层:运行时自动 disable cache。上游 4xx 失败一次后(forward.rs 调
    // `disable_web_search_for`),本进程后续 turn 立即 drop,避免每个 turn
    // 都触发同样错误。本次启动有效;用户去 UI 关 `web_search_enabled = false`
    // 才是持久关闭。
    if crate::is_web_search_disabled_for(&provider.id) {
        crate::warn_once_drop_tool("web_search:auto-disabled-after-failure");
        return vec![];
    }

    if provider_looks_like(provider, "xiaomimimo") || provider_looks_like(provider, "mimo") {
        // MiMo 私有 chat 端 web_search 形态(reqToChat.ts:196-209)
        let mut out = serde_json::Map::new();
        out.insert("type".into(), Value::String("web_search".into()));
        for field in ["user_location", "max_keyword", "force_search", "limit"] {
            if let Some(v) = obj.get(field) {
                out.insert(field.to_string(), v.clone());
            }
        }
        return vec![Value::Object(out)];
    }

    if provider_looks_like(provider, "kimi") || provider_looks_like(provider, "moonshot") {
        // Kimi 内置 `$web_search` builtin_function(WebFetch
        // `platform.kimi.ai/docs/guide/use-web-search` 真原文实证):
        //   {"type":"builtin_function", "function":{"name":"$web_search"}}
        // **不透传任何子字段**(Kimi 文档明确只要 type + function.name)。
        // 配套强制 `thinking:{type:"disabled"}` 顶级字段在
        // `responses_body_to_chat_body_for_provider_with_session` body 后处理
        // 注入(`contains_kimi_web_search_tool` 检测命中即写)。
        // 计费:每次搜索调用 $0.005(独立于 token),搜索结果计入 prompt_tokens。
        return vec![serde_json::json!({
            "type": "builtin_function",
            "function": {
                "name": "$web_search",
            },
        })];
    }

    // ── 文档实证不支持 web_search 的 provider ──
    // 这些 provider 的 chat completions API 明确只接受 `type:"function"`,
    // 没有 builtin web_search / native search / extra_body 顶级开关等任何
    // 形式的 server-side web 搜索能力。用户启用 web_search_enabled=true 也
    // 不会 work,只能走 P5 修通的 namespace MCP 工具(如 Node Repl + JS
    // fetch)绕路联网。warn_once 写明具体 provider 帮用户理解。

    // DeepSeek(WebFetch `api-docs.deepseek.com/api/create-chat-completion`
    // 真原文实证 2026-05-09):"Currently, only `function` is supported."
    // tools 数组只接受 type:"function",最多 128 个,无 builtin / web_search
    // / 任何 server-side 搜索能力。
    if provider_looks_like(provider, "deepseek") {
        crate::warn_once_drop_tool("web_search:not-supported-by-deepseek-api");
        return vec![];
    }

    // MiniMax(三方实证 2026-05-09:WebFetch `platform.minimaxi.com/docs/api-reference/`
    // + `platform.minimax.io/docs/api-reference/text-openai-api` + liteLLM
    // MiniMax provider 文档):MiniMax chat completions API(`api.minimaxi.com/v1`)
    // tools 仅 `type:"function"`,**无任何 builtin web_search / native search /
    // 顶级 enable_search 字段**。MiniMax 自家的 web_search 能力**仅作 Token Plan
    // MCP 工具存在**,不在 chat completions API 内。用户需联网搜索 → 走 P5
    // 修通的 namespace MCP 路径(`~/.codex/config.toml` 加 mcp_servers 条目)。
    if provider_looks_like(provider, "minimax") || provider_looks_like(provider, "minimaxi") {
        crate::warn_once_drop_tool("web_search:not-supported-by-minimax-api");
        return vec![];
    }

    // 其他 provider 尚未文档实证,走 drop + warn_once。
    // 用户实地反馈"模型不能直接用 web_search,绕路 MCP 工具/Node Repl 写
    // JS fetch HTML"是预期当前行为(P5 namespace MCP 修复后这条路是通的);
    // 后续逐家移植后会让模型直接走 chat 原生 web search,效率更高。
    crate::warn_once_drop_tool("web_search:provider-not-implemented");
    vec![]
}

/// 扫 outbound tools 数组,看是否含 Kimi 内置 `$web_search`
/// (`type:"builtin_function"` + `function.name == "$web_search"`)。
/// 命中时调用方需要在 body 顶级注入 `thinking:{type:"disabled"}` —— Kimi
/// 文档强制要求(see `docs/web-search-implementation-tracker.md` §2.1.2)。
pub fn contains_kimi_web_search_tool(tools: &[Value]) -> bool {
    tools.iter().any(|t| {
        t.get("type").and_then(|v| v.as_str()) == Some("builtin_function")
            && t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                == Some("$web_search")
    })
}

/// 读 `provider.request_options.web_search_enabled`(boolean,默认 false)。
/// 用户必须显式在 codex-app-transfer 配置里标 true 才启用;**默认关闭**
/// 是因为很多 provider(如 MiMo Token Plan 套餐)没开 Web Search Plugin
/// 时,发 web_search 工具会被 400 拒绝。配套 4xx fallback 自动降级
/// (`crate::disable_web_search_for`)防止重复失败。
pub fn provider_web_search_enabled(provider: &Provider) -> bool {
    provider
        .request_options
        .get("web_search_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn normalize_tool_choice(tool_choice: &Value) -> Value {
    let Some(obj) = tool_choice.as_object() else {
        return tool_choice.clone();
    };
    if obj
        .get("function")
        .and_then(|v| v.as_object())
        .and_then(|f| f.get("name"))
        .is_some()
    {
        return tool_choice.clone();
    }
    match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "auto" => Value::String("auto".into()),
        "none" => Value::String("none".into()),
        "required" | "tool" | "any" => Value::String("required".into()),
        "function" if obj.get("function").is_none() => Value::String("required".into()),
        _ => tool_choice.clone(),
    }
}
