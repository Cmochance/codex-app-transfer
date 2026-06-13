//! [MOC-231] 上下文 by-source 明细:把发往上游的**已拼接 chat 上下文**(完整历史 +
//! 本轮)按内容来源分类计 token,供 Codex Desktop footer 圆环旁的下拉面板展示。
//!
//! **为什么在 chat 格式上算**:完整历史只在 chat 格式下可得 —— `session_cache` 存的是
//! chat messages,带 `previous_response_id` 的入站 Responses `input` 只有增量当前轮
//! (实测 `~/.codex-app-transfer/forward-trace/`:增量轮 input 仅 1 条)。
//! `request.rs::responses_body_to_chat_body_for_provider_with_session` 在 merge 历史
//! + dedupe 瘦身后产出最终 chat messages,本模块在那一步对其分类。
//!
//! **口径**:各分类用 o200k_base(Codex 模型同一 tokenizer,见 [`count_tokens`])对发往
//! 上游的 wire 文本精确计数。与 Codex footer 圆环(模型回报 usage)总数仍可能略差 ——
//! 圆环是模型侧 usage、本模块是 proxy 侧发送内容的逐段归因,测量点不同 —— 面板用本模块
//! 自洽的总数。
//!
//! **边界**:仅 adapter 转换路径(openai_chat/gemini/anthropic 等第三方)proxy 持有
//! 完整拼接上下文、可精确分类;官方 ChatGPT 走 passthrough 不解析 body,算不了。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use serde::Serialize;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

/// [MOC-231 perf] 上下文明细面板是否启用。由 src-tauri 的 quota daemon 每 tick 按
/// `codexQuotaEnabled` 设置。**关闭(默认)时跳过 o200k 逐 item tokenize**,不在每个转发
/// 请求的热路径上白付计算 —— 面板默认关,绝大多数用户/请求据此免算 breakdown。
static BREAKDOWN_ENABLED: AtomicBool = AtomicBool::new(false);

/// 由 src-tauri quota daemon 调,跟随面板开关同步。
pub fn set_breakdown_enabled(enabled: bool) {
    BREAKDOWN_ENABLED.store(enabled, Ordering::Relaxed);
}

/// 面板是否启用(adapter 计算 breakdown 前先查;关则跳过 o200k 计算)。
pub(crate) fn breakdown_enabled() -> bool {
    BREAKDOWN_ENABLED.load(Ordering::Relaxed)
}

/// o200k_base tokenizer —— GPT-4o / GPT-5 / Codex 系的真实 BPE(vocab 由 tiktoken-rs
/// `include_bytes!` 打包、离线可用)。`LazyLock` 缓存:构建 BPE 要加载 vocab(百 ms 级),
/// 绝不能每请求重建。计数与 Codex 模型同一 tokenizer,故各分类是真实 token 数(非估算)。
static O200K: LazyLock<CoreBPE> =
    LazyLock::new(|| tiktoken_rs::o200k_base().expect("o200k_base vocab bundled in tiktoken-rs"));

/// 单个分类(展示一行:label + tokens + items)。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BreakdownCategory {
    /// 稳定机读 key(前端按此匹配 i18n / 颜色,不随 label 文案变)。
    pub key: &'static str,
    /// 该类累计 token 数(o200k_base 计)。
    pub tokens: u64,
    /// 该类条目数(message / tool 个数),供面板显示「119」式计数。
    pub items: u64,
}

/// 一次请求的上下文明细。`categories` 已按 tokens 降序排好,前端可直接渲染。
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct ContextBreakdown {
    /// 各分类 token 之和(= 面板自洽总数)。
    pub total_tokens: u64,
    pub categories: Vec<BreakdownCategory>,
}

/// 分类稳定 key(前端 i18n/着色锚点)。
mod keys {
    pub const SYSTEM_PROMPT: &str = "system_prompt";
    pub const DEVELOPER: &str = "developer";
    pub const MESSAGES: &str = "messages";
    pub const TOOL_CALLS: &str = "tool_calls";
    pub const REASONING: &str = "reasoning";
    pub const TOOLS: &str = "tools";
}

/// token 计数:o200k_base BPE(Codex 模型同一 tokenizer)对文本编码后的 token 数。
fn count_tokens(text: &str) -> u64 {
    O200K.encode_ordinary(text).len() as u64
}

/// 把一个 JSON 值序列化后计 token(message / tool 整体)。
fn count_value(v: &Value) -> u64 {
    count_tokens(&serde_json::to_string(v).unwrap_or_default())
}

/// system/developer message 是否承载 Codex 注入的系统侧大块(权限/环境/AGENTS/skills)。
/// 实测这些块都用 XML 式标签包裹;base instructions(顶层 instructions 转的 system 头)
/// 很短、无这些标签 → 归 system_prompt。
fn is_developer_block(content_text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "<permissions instructions>",
        "<environment_context>",
        "<user_instructions>",
        "AGENTS.md",
    ];
    MARKERS.iter().any(|m| content_text.contains(m))
}

/// 提取 chat message 的 content 文本(content 可能是 string 或 parts 数组)。
fn message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// assistant message 是否带非空 tool_calls(→ 归工具调用类)。
fn has_tool_calls(msg: &Value) -> bool {
    msg.get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// 单分类累加桶:tokens 之和 + 条目数(具名优于裸 `(u64, u64)`,调用点自解释)。
#[derive(Default, Clone, Copy)]
struct Bucket {
    tokens: u64,
    items: u64,
}

impl Bucket {
    fn add(&mut self, tokens: u64) {
        self.tokens += tokens;
        self.items += 1;
    }
}

/// 累加器:按 key 汇总各分类。
#[derive(Default)]
struct Acc {
    system_prompt: Bucket,
    developer: Bucket,
    messages: Bucket,
    tool_calls: Bucket,
    reasoning: Bucket,
    tools: Bucket,
}

/// 计算一次请求的上下文明细。
///
/// - `messages`:已拼接(历史 + 本轮)、dedupe 瘦身后的 **chat 格式** messages
///   (role=system/developer/user/assistant/tool)。
/// - `tools`:chat 格式 tools 数组(`{type:"function", function:{...}}`),无则空。
///
/// 分类按真实承载(实测 gpt-5.5/990 items:tool 调用往返占 ~62% 是大头,跟 Claude
/// 的 Messages 大头不同):
/// - system/developer message → 按内容标签分 `system_prompt`(base)/ `developer`(权限/env/AGENTS/skills)
/// - user / 纯文本 assistant → `messages`
/// - 带 tool_calls 的 assistant + role=tool → `tool_calls`(工具调用与输出)
/// - assistant 的 `reasoning_content` 字段单独抽出 → `reasoning`(避免与 messages/tool_calls 双计)
/// - tools[] → `tools`(工具定义)
pub fn compute_context_breakdown(messages: &[Value], tools: &[Value]) -> ContextBreakdown {
    let mut acc = Acc::default();

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                let slot = if is_developer_block(&message_text(msg)) {
                    &mut acc.developer
                } else {
                    &mut acc.system_prompt
                };
                slot.add(count_value(msg));
            }
            "user" => acc.messages.add(count_value(msg)),
            "assistant" => {
                // reasoning_content 单独计入 reasoning;message 主体(去掉 reasoning_content
                // 后)按是否含 tool_calls 归 tool_calls / messages,避免双计。
                let reasoning_text = msg
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !reasoning_text.is_empty() {
                    acc.reasoning.add(count_tokens(reasoning_text));
                }
                let mut body = msg.clone();
                if let Some(obj) = body.as_object_mut() {
                    obj.remove("reasoning_content");
                }
                let slot = if has_tool_calls(&body) {
                    &mut acc.tool_calls
                } else {
                    &mut acc.messages
                };
                slot.add(count_value(&body));
            }
            "tool" => acc.tool_calls.add(count_value(msg)),
            // 未知 role 兜底进 messages(不丢)。
            _ => acc.messages.add(count_value(msg)),
        }
    }

    for tool in tools {
        acc.tools.add(count_value(tool));
    }

    // 注:seed 数组顺序是 tokens 相等时排序的 tiebreak,保持稳定。
    let mut categories: Vec<BreakdownCategory> = [
        (keys::SYSTEM_PROMPT, acc.system_prompt),
        (keys::DEVELOPER, acc.developer),
        (keys::MESSAGES, acc.messages),
        (keys::TOOL_CALLS, acc.tool_calls),
        (keys::REASONING, acc.reasoning),
        (keys::TOOLS, acc.tools),
    ]
    .into_iter()
    .filter(|(_, b)| b.tokens > 0)
    .map(|(key, b)| BreakdownCategory {
        key,
        tokens: b.tokens,
        items: b.items,
    })
    .collect();

    categories.sort_by_key(|c| std::cmp::Reverse(c.tokens));
    let total_tokens = categories.iter().map(|c| c.tokens).sum();

    ContextBreakdown {
        total_tokens,
        categories,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 结构真实的合成 messages(覆盖各 role + tag + tool_calls + reasoning),
    /// 不含真实对话内容(隐私 + 不把抓包 token 进 repo);真机校验在本地另跑。
    fn sample_messages() -> Vec<Value> {
        vec![
            json!({ "role": "system", "content": "You are Codex." }), // base → system_prompt
            json!({ "role": "developer", "content": "<permissions instructions> sandbox=workspace-write <environment_context> ..." }), // → developer
            json!({ "role": "user", "content": "帮我改个 bug" }), // → messages
            json!({ "role": "assistant", "content": "", "reasoning_content": "先看代码再改", "tool_calls": [{"id":"c1","type":"function","function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}}] }), // tool_calls + reasoning
            json!({ "role": "tool", "tool_call_id": "c1", "content": "file1.rs\nfile2.rs" }), // → tool_calls
            json!({ "role": "assistant", "content": "改好了" }), // → messages
        ]
    }

    fn sample_tools() -> Vec<Value> {
        vec![
            json!({ "type": "function", "function": { "name": "shell", "description": "run", "parameters": {} } }),
            json!({ "type": "function", "function": { "name": "apply_patch", "description": "patch", "parameters": {} } }),
        ]
    }

    #[test]
    fn classifies_each_source_into_expected_bucket() {
        let bd = compute_context_breakdown(&sample_messages(), &sample_tools());
        let by_key: std::collections::HashMap<_, _> =
            bd.categories.iter().map(|c| (c.key, c)).collect();

        // system_prompt:1 条 base instructions
        assert_eq!(by_key.get(keys::SYSTEM_PROMPT).unwrap().items, 1);
        // developer:1 条带 <permissions instructions>
        assert_eq!(by_key.get(keys::DEVELOPER).unwrap().items, 1);
        // messages:user + 纯文本 assistant = 2 条
        assert_eq!(by_key.get(keys::MESSAGES).unwrap().items, 2);
        // tool_calls:带 tool_calls 的 assistant + role=tool = 2 条
        assert_eq!(by_key.get(keys::TOOL_CALLS).unwrap().items, 2);
        // reasoning:reasoning_content 抽出 = 1 条
        assert_eq!(by_key.get(keys::REASONING).unwrap().items, 1);
        // tools:2 个工具定义
        assert_eq!(by_key.get(keys::TOOLS).unwrap().items, 2);
    }

    #[test]
    fn total_equals_sum_and_sorted_desc() {
        let bd = compute_context_breakdown(&sample_messages(), &sample_tools());
        let sum: u64 = bd.categories.iter().map(|c| c.tokens).sum();
        assert_eq!(bd.total_tokens, sum);
        assert!(bd.total_tokens > 0);
        // 降序
        for w in bd.categories.windows(2) {
            assert!(w[0].tokens >= w[1].tokens);
        }
    }

    #[test]
    fn empty_inputs_yield_empty_breakdown() {
        let bd = compute_context_breakdown(&[], &[]);
        assert_eq!(bd.total_tokens, 0);
        assert!(bd.categories.is_empty());
    }

    #[test]
    fn reasoning_not_double_counted_in_tool_calls() {
        // 带 reasoning_content + tool_calls 的 assistant:reasoning 进 reasoning 桶,
        // 主体进 tool_calls 桶,两者不重叠。
        let msgs = vec![json!({
            "role": "assistant",
            "content": "",
            "reasoning_content": "AAAAAAAA",
            "tool_calls": [{"id":"c1","type":"function","function":{"name":"x","arguments":"{}"}}]
        })];
        let bd = compute_context_breakdown(&msgs, &[]);
        let by_key: std::collections::HashMap<_, _> =
            bd.categories.iter().map(|c| (c.key, c)).collect();
        assert!(by_key.contains_key(keys::REASONING));
        assert!(by_key.contains_key(keys::TOOL_CALLS));
        // tool_calls 主体不含 reasoning_content 文本(已 remove)
        assert!(!by_key.contains_key(keys::MESSAGES));
    }
}
