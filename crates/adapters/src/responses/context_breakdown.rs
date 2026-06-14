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

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

use codex_app_transfer_registry::config_dir;
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

    finalize_acc(acc)
}

/// 把累加器收成最终明细(各分类降序、过滤空类、汇总总数)。chat 与 responses 两条
/// 计算路径共用,保证两者口径(分类 key / 排序 / 空类过滤)完全一致。
fn finalize_acc(acc: Acc) -> ContextBreakdown {
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

/// [MOC-234] **responses 原生**上下文明细计算(1:1 passthrough 路径专用)。
///
/// 与 [`compute_context_breakdown`](chat 形)同口径(同 o200k tokenizer、同分类 key、同
/// finalize),但直接吃 **Responses 形 item**,**不经 `responses_body_to_chat_body_*` 转换**
/// —— 那条路径会引入 namespace 展平 / tool_call_cache / artifact_store / helper-prompt 注入等
/// 本项目资产,违反 MOC-234「responses 路径不接管」约束。本函数纯只读测量,绝不改动任何转发字节。
///
/// 入参:
/// - `instructions`:Responses 请求顶层 `instructions`(Codex 的 base/system 指令),
///   按内容标签归 `system_prompt` / `developer`。
/// - `items`:已由观测累积器拼好的「全历史 + 本轮」Responses input/output item 列表
///   (`message` / `function_call*` / `custom_tool_call*` / `local_shell_call*` / `reasoning` …)。
/// - `tools`:Responses 请求顶层 `tools`(含 namespace 包装、apply_patch 的 lark grammar 等,
///   原样计入 `tools` 分类)。
pub fn compute_context_breakdown_responses(
    instructions: Option<&str>,
    items: &[Value],
    tools: &[Value],
) -> ContextBreakdown {
    let mut acc = Acc::default();

    if let Some(text) = instructions.filter(|s| !s.is_empty()) {
        let slot = if is_developer_block(text) {
            &mut acc.developer
        } else {
            &mut acc.system_prompt
        };
        slot.add(count_tokens(text));
    }

    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        // tool 往返:function_call / function_call_output / custom_tool_call(_output) /
        // local_shell_call(_output) 等,统一归 tool_calls(与 chat 口径一致)。
        if item_type.starts_with("function_call")
            || item_type.starts_with("custom_tool_call")
            || item_type.starts_with("local_shell_call")
            || item_type == "tool_call"
            || item_type == "tool_result"
        {
            acc.tool_calls.add(count_value(item));
            continue;
        }
        match item_type {
            "reasoning" => acc.reasoning.add(count_value(item)),
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("");
                match role {
                    "system" | "developer" => {
                        let slot = if is_developer_block(&message_text(item)) {
                            &mut acc.developer
                        } else {
                            &mut acc.system_prompt
                        };
                        slot.add(count_value(item));
                    }
                    // user / assistant 文本消息 → messages(assistant 工具调用是独立的
                    // function_call item,已在上面归 tool_calls,不在此处)。
                    _ => acc.messages.add(count_value(item)),
                }
            }
            // 未知 / 其它 item(item_reference 等)兜底进 messages,不丢(与 chat 口径一致)。
            _ => acc.messages.add(count_value(item)),
        }
    }

    for tool in tools {
        acc.tools.add(count_value(tool));
    }

    finalize_acc(acc)
}

// ───────────────── [MOC-231/232] 按对话持久 store + 异步搬离关键路径 ─────────────────
//
// [MOC-231] 明细按**对话 uuid** 落盘 `~/.codex-app-transfer/context-breakdown/<uuid>.json`:
// `uuid` = Codex 请求的 `prompt_cache_key`(== rollout 文件名 == renderer fiber 的
// conversationId)。producer 是本模块(adapter prepare_request 路径),consumer 是 quota
// injector daemon 按**活动会话 uuid** 读盘。磁盘持久 → 重启即用;小 JSON → 读取快;按
// uuid 隔离 → 切对话不串。
//
// [MOC-232] store 从 `proxy::telemetry` 迁来 + 计算改 `spawn_blocking` 后台跑:原先在
// `prepare_request` 同步算(面板开时 404k 上下文 ~1s 卡 TTFB),现搬离转发关键路径;
// compute 与 persist 同处 adapters,数据流最短(不再经 adapter_metadata 透传 ~1.5MB)。
// persist 用原子 rename,同对话并发后台任务最新者胜出(面板不卡旧轮)。

/// 明细持久目录:`<config_dir>/context-breakdown/`。
fn context_breakdown_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("context-breakdown"))
}

/// 校验 conversation_id 是规范 uuid(防路径穿越:只允许 hex + 连字符、长度 36)。
fn is_safe_conversation_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// temp 文件名后缀计数器(同进程内唯一,避免并发后台任务的 temp 互撞)。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 按对话 uuid 持久化明细(best-effort:失败不影响计算/转发)。
///
/// [MOC-232] **原子写**:先写唯一 temp 再 `rename` 覆盖目标(同目录 rename 原子)。同一对话
/// 的并发后台任务(web_search retry / 大上下文后跟快速 auto-turn)各写各的 temp、各自 rename,
/// 最后 rename 的胜出(面板取最新);避免非原子 `fs::write` 被 consumer 读到半截 JSON,也无需
/// 丢弃新快照的 in-flight 去重(那会让面板卡在上一轮,code review P2)。
fn persist_context_breakdown(conversation_id: &str, breakdown: &Value) {
    if !is_safe_conversation_id(conversation_id) {
        return;
    }
    let Some(dir) = context_breakdown_dir() else {
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        tracing::debug!(error = %e, "context_breakdown 持久化建目录失败");
        return;
    }
    let bytes = match serde_json::to_vec(breakdown) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(error = %e, "context_breakdown 序列化失败");
            return;
        }
    };
    let path = dir.join(format!("{conversation_id}.json"));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        "{conversation_id}.{}.{seq}.tmp",
        std::process::id()
    ));
    if let Err(e) = fs::write(&tmp, &bytes) {
        tracing::debug!(error = %e, "context_breakdown 写 temp 失败");
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        tracing::debug!(error = %e, path = %path.display(), "context_breakdown rename 失败");
        let _ = fs::remove_file(&tmp);
    }
}

/// 按对话 uuid 读最近持久化的明细(quota injector daemon 每 tick 按活动会话读)。
pub fn load_context_breakdown(conversation_id: &str) -> Option<Value> {
    if !is_safe_conversation_id(conversation_id) {
        return None;
    }
    let path = context_breakdown_dir()?.join(format!("{conversation_id}.json"));
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// [MOC-231] GC `context-breakdown/` 下 mtime 超 `max_age` 的明细文件。best-effort,
/// 启动时跑一次(陈旧对话的明细本就过时,删了下次有请求会重建)。
pub fn gc_context_breakdown(max_age: Duration) {
    let Some(dir) = context_breakdown_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return; // 目录还不存在 = 没持久化过,正常
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mt| now.duration_since(mt).ok())
            .is_some_and(|age| age > max_age);
        if too_old {
            let _ = fs::remove_file(&path);
        }
    }
}

/// [MOC-232] 把 o200k 逐 item tokenize + 持久化搬到 `spawn_blocking` 后台跑,
/// **不阻塞转发关键路径**(实测面板开时 404k 上下文同步算 ~1s)。
///
/// - conv_id 非规范 uuid → 直接返回(省掉无谓 tokenize,反正 persist 也会拦)。
/// - 无 tokio runtime(单测 / 非 async 调用)→ 直接返回,不计算、不 panic。
/// - fire-and-forget:丢弃 JoinHandle,算完原子落盘,面板下一 tick 读到(consumer 是磁盘
///   轮询、与本计算无同步握手,"晚到一拍"无副作用)。同对话多个后台任务(retry / 快速
///   auto-turn)不做丢弃新快照的去重 —— 各自算各自原子 rename,最新者胜出,面板始终是最新轮
///   (见 [`persist_context_breakdown`] 的原子写;code review P2)。
pub fn spawn_compute_and_persist(messages: Vec<Value>, tools: Vec<Value>, conv_id: String) {
    if !is_safe_conversation_id(&conv_id) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn_blocking(move || {
        let breakdown = compute_context_breakdown(&messages, &tools);
        if let Ok(v) = serde_json::to_value(&breakdown) {
            persist_context_breakdown(&conv_id, &v);
        }
    });
}

/// [MOC-234] responses 1:1 passthrough 路径的后台 breakdown 计算 + 落盘。同
/// [`spawn_compute_and_persist`] 的异步/原子语义,但走 [`compute_context_breakdown_responses`]
/// (responses 形 item,不经 chat 转换)。`items` 由观测累积器拼好(全历史 + 本轮)。
pub fn spawn_compute_and_persist_responses(
    instructions: Option<String>,
    items: Vec<Value>,
    tools: Vec<Value>,
    conv_id: String,
) {
    if !is_safe_conversation_id(&conv_id) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn_blocking(move || {
        let breakdown =
            compute_context_breakdown_responses(instructions.as_deref(), &items, &tools);
        if let Ok(v) = serde_json::to_value(&breakdown) {
            persist_context_breakdown(&conv_id, &v);
        }
    });
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

    /// 合成 Responses 形 item(覆盖各 source),对称于 [`sample_messages`] 的 chat 形。
    fn sample_responses_items() -> Vec<Value> {
        vec![
            json!({ "type": "message", "role": "developer", "content": [{"type":"input_text","text":"<permissions instructions> sandbox=workspace-write"}] }), // → developer
            json!({ "type": "message", "role": "user", "content": [{"type":"input_text","text":"帮我改个 bug"}] }), // → messages
            json!({ "type": "reasoning", "summary": [{"type":"summary_text","text":"先看代码再改"}] }), // → reasoning
            json!({ "type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "c1" }), // → tool_calls
            json!({ "type": "function_call_output", "call_id": "c1", "output": "file1.rs\nfile2.rs" }), // → tool_calls
            json!({ "type": "message", "role": "assistant", "content": [{"type":"output_text","text":"改好了"}] }), // → messages
        ]
    }

    #[test]
    fn responses_native_classifies_each_source_into_expected_bucket() {
        // [MOC-234] responses 原生计算口径必须与 chat 版一致(同分类 key)。
        let bd = compute_context_breakdown_responses(
            Some("You are Codex."), // 顶层 instructions → system_prompt
            &sample_responses_items(),
            &sample_tools(),
        );
        let by_key: std::collections::HashMap<_, _> =
            bd.categories.iter().map(|c| (c.key, c)).collect();
        assert_eq!(
            by_key.get(keys::SYSTEM_PROMPT).unwrap().items,
            1,
            "instructions"
        );
        assert_eq!(
            by_key.get(keys::DEVELOPER).unwrap().items,
            1,
            "developer message"
        );
        assert_eq!(
            by_key.get(keys::MESSAGES).unwrap().items,
            2,
            "user + assistant"
        );
        assert_eq!(
            by_key.get(keys::TOOL_CALLS).unwrap().items,
            2,
            "function_call + function_call_output"
        );
        assert_eq!(
            by_key.get(keys::REASONING).unwrap().items,
            1,
            "reasoning item"
        );
        assert_eq!(by_key.get(keys::TOOLS).unwrap().items, 2, "tool defs");
        // 总数 = 各类之和,且降序
        assert_eq!(
            bd.total_tokens,
            bd.categories.iter().map(|c| c.tokens).sum::<u64>()
        );
        for w in bd.categories.windows(2) {
            assert!(w[0].tokens >= w[1].tokens);
        }
    }

    #[test]
    fn responses_native_empty_yields_empty() {
        let bd = compute_context_breakdown_responses(None, &[], &[]);
        assert_eq!(bd.total_tokens, 0);
        assert!(bd.categories.is_empty());
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

    #[test]
    fn is_safe_conversation_id_rejects_path_traversal_and_bad_shape() {
        // 规范 uuid(36 = 32 hex + 4 连字符)放行
        assert!(is_safe_conversation_id(
            "01234567-89ab-cdef-0123-456789abcdef"
        ));
        // 路径穿越 / 含分隔符 / 非 hex / 长度不符一律拒(防写到 context-breakdown/ 外)
        assert!(!is_safe_conversation_id("../../etc/passwd"));
        assert!(!is_safe_conversation_id(
            "01234567/89ab/cdef/0123/456789abcdef"
        )); // 36 但含 /
        assert!(!is_safe_conversation_id(
            "01234567-89ab-cdef-0123-456789abcde"
        )); // 35
        assert!(!is_safe_conversation_id(
            "zzzzzzzz-89ab-cdef-0123-456789abcdef"
        )); // 非 hex
        assert!(!is_safe_conversation_id(""));
    }

    #[test]
    fn spawn_compute_and_persist_invalid_uuid_is_noop() {
        // 非法 conv_id → is_safe 提前拦截,不 insert IN_FLIGHT、不 spawn、不落盘、不 panic。
        spawn_compute_and_persist(sample_messages(), sample_tools(), "not-a-uuid".to_owned());
    }

    #[test]
    fn spawn_compute_and_persist_no_runtime_is_noop_not_panic() {
        // 合法 uuid 但无 tokio runtime(普通 #[test])→ Handle::try_current 失败 → 直接返回,
        // 不 spawn、不落盘、不 panic。
        spawn_compute_and_persist(
            sample_messages(),
            sample_tools(),
            "01234567-89ab-cdef-0123-456789abcdef".to_owned(),
        );
    }
}
