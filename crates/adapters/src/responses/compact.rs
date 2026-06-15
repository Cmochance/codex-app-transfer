//! 本地实现 Codex autocompact 两套协议(V1 / V2 双轨)。
//!
//! ## V1:私有 `/responses/compact` 端点
//!
//! 旧版 Codex CLI 在累计 token 超过 `model_auto_compact_token_limit` 时会调
//! `POST /responses/compact`,期望后端做"上下文压缩"——把整段对话历史摘要成
//! 一段简短的纯文本 summary,用 `{"output":[{"type":"compaction",
//! "encrypted_content":"<SUMMARY_PREFIX>\n<text>"}]}` 形态回写。
//!
//! ## V2:[MOC-198] remote compaction v2
//!
//! 新版 Codex(启用 `remote_compaction_v2`)不再调私有端点,改发**普通流式
//! `/responses`** 请求并在 `input` 末尾追加 `{"type":"compaction_trigger"}`
//! 标记 item,期待响应流中**恰好一个** `type=compaction` output item(否则
//! 报 `remote compaction v2 expected exactly one compaction output item`)。
//! 响应必须是 SSE 流:`response.created` → `response.output_item.done`
//! (单 `type=compaction` item)→ `response.completed`。
//!
//! ## 公共实现说明
//!
//! 这是 OpenAI 官方 Responses API 的私有扩展,**所有第三方 OpenAI-compatible
//! provider(MiMo / Kimi / DeepSeek / MiniMax / 智谱 / 百炼)都不支持**——
//! 透传必 404,litellm 也只对 openai provider 实现透传。
//!
//! 本模块在代理层本地实现两套路径:把 `CompactionInput`(V1 body /
//! V2 剥掉 `compaction_trigger` 后的 body)重组成普通 `/chat/completions`
//! 请求(注入 SUMMARIZATION_PROMPT),拿到上游 chat completion 响应后提取
//! summary 文本,V1 包装成非流式 JSON compact 响应;V2 包装成 SSE 流。
//!
//! ## 协议来源
//!
//! 通过 `openai/codex` 公开源码反查(Apache-2 license,标注引用):
//! - 请求结构 `CompactionInput`:`codex-rs/codex-api/src/common.rs`
//! - 响应结构 `CompactHistoryResponse { output: Vec<ResponseItem> }` +
//!   `ResponseItem::Compaction { encrypted_content: String }`:
//!   `codex-rs/codex-api/src/endpoint/compact.rs` + `codex-rs/protocol/src/models.rs:882`
//! - SUMMARY_PREFIX / SUMMARIZATION_PROMPT 文本:
//!   `codex-rs/core/templates/compact/summary_prefix.md`、
//!   `codex-rs/core/templates/compact/prompt.md`
//! - `encrypted_content` 字段名是历史包袱,**实际是明文** `format!("{PREFIX}\n{summary}")`
//!   (`codex-rs/core/src/compact.rs:262`)。

use bytes::Bytes;
use codex_app_transfer_registry::{compact_disable_thinking_wire, Provider};
use futures_util::stream::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Value};

use crate::core::routes;
use crate::types::{AdapterError, ByteStream, ResponsePlan};

use super::request::responses_body_to_chat_body_for_provider;

/// **#219 fix prompt rewrite**:从 v2.0.12 的 9-section + few-shot example
/// 长 prompt(~3300 chars)换回上游 Codex CLI 的短指令风格(~460 chars),
/// 仅补两条 Claude Code 关键锚定 bullet(All User Messages verbatim +
/// Next Step verbatim quote)。最终 ~800 chars,真机 DeepSeek v4-pro 实测:
/// 相对 9-section 长版本快 ~40% / 省 ~48% token,但保留下一轮任务锚定能力。
///
/// ## 借鉴出处
///
/// - 基础结构:`openai/codex` 仓库 `codex-rs/core/templates/compact/prompt.md`
///   (Apache-2,460 字符,适配 GPT-5 强指令遵循)。
/// - "All user messages verbatim" + "Next Step verbatim quote" 两条 bullet
///   措辞:Piebald-AI/claude-code-system-prompts 反编译公开版本
///   `agent-prompt-conversation-summarization.md` 的第 6 / 9 段。
///
/// ## 为什么换回短 prompt
///
/// v2.0.12 加 9-section schema + few-shot example 的初衷是"用结构强约束让
/// 弱指令模型必填字段",但反直觉的是:
///
/// 1. **DeepSeek v4-pro 真机测试**:9-section 长 prompt 与短 prompt 都正常
///    产出 summary,无模板/example 回显。issue #219 阶段三那次"模板回显"
///    可能是 sampling 偶发或上下文极致超长触发的退化,常规 case 不复现。
/// 2. **长 prompt 反而拖慢**:同一对话历史,长 prompt 94s / 4254 tokens,
///    短 prompt 44s / 1699 tokens,混合版 57s / 2198 tokens。
/// 3. **业界共识**:long prompts 稀释模型注意力,chunk-and-merge 比单轮
///    长 prompt 更可靠。
///
/// ## 保留的关键能力(借鉴 Claude Code 第 6/9 段)
///
/// 1. **All User Messages verbatim 列表**:防丢用户中途意图变化 —
///    任何长对话中,用户的修正 / 反馈 / 换需求是最易被压缩掉的信息。
/// 2. **Next Step + 最近用户原话 verbatim quote**:防任务漂移 —
///    下一轮模型读到原话引用即知道"我接续到哪里",不靠总结模型的推断。
///
/// ## 不保留的字段(相对 v2.0.12)
///
/// `<analysis>` / `<summary>` 二段输出、9-section 强 schema、few-shot
/// example 全部移除。模型可自由选择 markdown / 段落 / 列表组织答案,
/// `extract_summary_section` 在无 `<summary>` tag 时直接 raw fallback
/// (本来就是容错路径,现在成为常规路径)。
const COMPACT_SUMMARIZATION_PROMPT_EN: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue
- **All user messages so far, verbatim or near-verbatim, in chronological order** — this preserves intent shifts that get lost otherwise
- **Next Step** — the immediate next action aligned with the user's most recent explicit request. Include a **verbatim direct quote** from the most recent user message showing exactly where you left off; this prevents task drift.

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.";

/// COMPACT 总结提示词中文版(#262)。
///
/// **翻译原则**:
/// - 跟英文版**逐条对应** — 不漏 emphasis(**verbatim** / **All user messages**)
/// - 技术词保英文:`LLM` / `Next Step`(英文章节名,跟英文版结构对齐)
/// - **不**翻译 [`COMPACT_SUMMARY_PREFIX`] — Codex CLI 用 `startswith` 识别该前缀,
///   字面英文不能动。此处仅翻译要模型 **写** summary 的 prompt(输入侧)
const COMPACT_SUMMARIZATION_PROMPT_ZH: &str = "你正在执行 CONTEXT CHECKPOINT COMPACTION(上下文检查点压缩)。为下一个接手任务的 LLM 写一份交接总结。

包含:
- 当前进度和已做出的关键决策
- 重要 context、约束、或 user 偏好
- 还有什么待办(清晰的下一步)
- 继续任务所需的关键数据、示例、引用
- **截至目前的所有 user message,按时间顺序逐字或近似逐字保留** —— 这能保留其它方式会丢失的 intent 演变
- **Next Step** —— 跟 user 最近一次显式请求对齐的下一个动作。包含从 user 最近一条 message 中**逐字引用**的直接 quote,标明你停在了哪里;这能防止任务漂移。

精简、结构化,聚焦于帮助下一个 LLM 无缝接续工作。";

/// 按当前 user 语言偏好选 compact summarization prompt(#262)。
fn compact_summarization_prompt_for_current_language() -> &'static str {
    use crate::core::language::{current_language, Language};
    match current_language() {
        Language::Chinese => COMPACT_SUMMARIZATION_PROMPT_ZH,
        Language::English => COMPACT_SUMMARIZATION_PROMPT_EN,
    }
}

/// 抄自 `openai/codex` 仓库 `codex-rs/core/templates/compact/summary_prefix.md` (Apache-2).
/// Codex CLI 反序列化 compact 响应后,通过 `is_summary_message`(`startswith(PREFIX)`)
/// 识别这段文本是 compaction summary 并接管历史回放。**前缀必须保持字面一致**。
pub(crate) const COMPACT_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// `COMPACT_USER_MESSAGE_MAX_TOKENS` from `codex-rs/core/src/compact.rs:48`.
const COMPACT_MAX_OUTPUT_TOKENS: u32 = 20_000;

/// Compact must reserve room for the summarization prompt and the generated
/// summary. This is a byte budget over the final Chat `messages` array, applied
/// after Responses-to-Chat conversion because that is the real upstream shape.
const COMPACT_CHAT_MESSAGES_MAX_BYTES: usize = 120 * 1024;
const COMPACT_OMISSION_NOTICE_MAX_CHARS: usize = 8_000;
const COMPACT_SINGLE_MESSAGE_MAX_CHARS: usize = 8_000;
const COMPACT_TOOL_ARGUMENTS_MAX_CHARS: usize = 3_000;
const COMPACT_EXCERPT_HEAD_CHARS: usize = 1_800;
const COMPACT_EXCERPT_TAIL_CHARS: usize = 1_000;

/// 收上游 chat completions 响应的最大字节数,防止异常 provider 把我们打挂内存。
/// 32 MB 远超合理 chat completion 响应大小(typical 几十 KB)。
const MAX_UPSTREAM_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// 判断入站 path 是否是 `/responses/compact`(含可选 `/v1/`、`/openai/v1/` 前缀)。
pub(crate) fn is_compact_path(path: &str) -> bool {
    routes::is_exact_responses_compact_path(path)
}

/// compact 请求类别。
///
/// [MOC-198] Codex 启用 `remote_compaction_v2` 后,autocompact 不再调 V1 私有
/// 端点,改发**普通流式 `/responses`** 请求并在 input 末尾追加
/// `{"type":"compaction_trigger"}` 标记(上游 `codex-rs/core/src/compact_remote_v2.rs`
/// `input.push(ResponseItem::CompactionTrigger)`),期待响应流中**恰好一个**
/// `type=compaction` output item,否则报
/// `remote compaction v2 expected exactly one compaction output item`。
/// 两版并存:旧版 Codex 仍走 V1,检测必须双轨。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactKind {
    /// 旧私有端点 `POST /responses/compact`(非流式 JSON 响应)。
    V1,
    /// remote compaction v2:`POST /responses`(stream)+ `compaction_trigger`
    /// input item(SSE 响应,单 compaction output item)。
    V2,
}

/// 统一检测入口:V1 按 path,V2 按「普通 /responses + body 含 compaction_trigger」。
///
/// V2 检测做两段:先廉价字节子串扫(`"compaction_trigger"` 不在 body 里直接
/// 返 None,避免为每个普通请求多做一次完整 JSON parse —— compact 触发点在
/// 80% 上下文,body 可达 MB 级);命中再 parse 确认它真是 input 数组里的 item
/// type(防历史消息文本里恰好出现这个词的误判)。
pub(crate) fn detect_compact(client_path: &str, body: &[u8]) -> Option<CompactKind> {
    if is_compact_path(client_path) {
        return Some(CompactKind::V1);
    }
    if !routes::is_local_responses_route(client_path) {
        return None;
    }
    const NEEDLE: &[u8] = b"\"compaction_trigger\"";
    if !body.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
        return None;
    }
    let Some(parsed) = serde_json::from_slice::<Value>(body).ok() else {
        // 不可达级防御(proxy 已整段缓冲,Codex serde 不会发非法 JSON):留痕
        // 防未来 wire 形态变化让 V2 静默退化成普通对话(silent-failure review)。
        tracing::debug!(
            "[compact-v2] body 含 compaction_trigger 字节但 JSON parse 失败,按普通请求处理"
        );
        return None;
    };
    let has_trigger = parsed
        .get("input")
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter()
                .any(|it| it.get("type").and_then(|t| t.as_str()) == Some("compaction_trigger"))
        })
        .unwrap_or(false);
    if !has_trigger {
        // 字节命中但确认失败:多半是历史文本里出现该词(正常),也可能是上游
        // 协议改了 trigger 位置/形态(异常)。debug 留痕供后者排查。
        tracing::debug!(
            "[compact-v2] body 含 compaction_trigger 字节但非 input item type,按普通请求处理"
        );
        return None;
    }
    Some(CompactKind::V2)
}

/// [MOC-198] 从 V2 请求 body 的 input 数组里剥掉 `compaction_trigger` 标记 item。
/// 它是纯标记(空 item),进历史展开会被当未知 type 产生占位噪音;剥掉后剩余
/// body 即与 V1 的 `CompactionInput` 同形(model + input 历史),可直接复用
/// [`build_compact_chat_request`]。
pub(crate) fn strip_compaction_trigger(body: &[u8]) -> Result<Vec<u8>, AdapterError> {
    let mut parsed: Value = serde_json::from_slice(body)
        .map_err(|e| AdapterError::BadRequest(format!("compact v2 body 不是合法 JSON: {e}")))?;
    let mut stripped = 0usize;
    if let Some(arr) = parsed.get_mut("input").and_then(|i| i.as_array_mut()) {
        let before = arr.len();
        arr.retain(|it| it.get("type").and_then(|t| t.as_str()) != Some("compaction_trigger"));
        stripped = before - arr.len();
    }
    // 不变量:caller 只在 detect_compact 判 V2 后调用(同一字节 parse 两次结果
    // 一致),必剥掉 ≥1 个。violated = 有人绕过 detect 直接调,trigger 会静默
    // 流入历史展开(silent-failure review 防御项)。
    if stripped == 0 {
        return Err(AdapterError::Internal(
            "compact v2 strip 不变量违反:body 中无 compaction_trigger item(caller 未经 detect_compact?)".into(),
        ));
    }
    serde_json::to_vec(&parsed)
        .map_err(|e| AdapterError::Internal(format!("compact v2 body re-serialize: {e}")))
}

/// 把 Codex CLI 的 `CompactionInput` JSON 改写成上游 `/chat/completions` 请求体。
///
/// 策略(v2.0.12 调整):
/// - **注入 `COMPACT_SUMMARIZATION_PROMPT` 作为最后一条 user message**(append
///   到 input 数组末尾),而不是 instructions/system。原因:
///   * 第三方 provider 对 user 服从度普遍 > system,structured prompt 更被尊重
///   * 避免 system prompt cache 截断 / 去重(部分 provider 把超长 system 截短)
///   * 对齐 Codex CLI 自家做法(`compact.rs::build_compact_request` 把 prompt
///     当 `UserInput::Text` 注入)
/// - 保留 `input` 数组(原对话历史),交给现有 `responses_body_to_chat_body_for_provider`
///   做 ResponseItem → ChatMessage 转换、merge consecutive、tool call repair、vision 剥离等
/// - `stream = false`(上游回完整 chat completion JSON,不是 SSE)
/// - 丢弃 `instructions`(摘要任务不应受原任务 system prompt 影响)
/// - 保留 `tools`(`ensure_thinking_tool_call_reasoning` 的 `has_tool_loop`
///   检测需要,且第三方 provider 看到 tools 字段不会 400)
pub(crate) fn build_compact_chat_request(
    body_bytes: &[u8],
    provider: &Provider,
) -> Result<Vec<u8>, AdapterError> {
    let parsed: Value = serde_json::from_slice(body_bytes)
        .map_err(|e| AdapterError::BadRequest(format!("compact body 不是合法 JSON: {e}")))?;
    let model = parsed.get("model").cloned().unwrap_or(Value::Null);
    let raw_input = parsed.get("input").cloned();

    // A2:把 SUMMARIZATION_PROMPT 作为最后一条 user message append 到 input。
    // 必须**先 normalize input 为 array**才能可靠 append —— `extract_input_items`
    // (`responses/request.rs:376`)接受 Null / String / Object / Array 多种形式,
    // 实际客户端 body 也可能是 string/object(非典型但合法)。如果只 match
    // array 路径,non-array input 时会**完全丢失 prompt**,上游收到无 summary
    // 指令的请求,返回任意 chat 内容而不是 summary —— PR #71 codex review 报
    // 的 P2 隐患(2026-05-08)。
    let mut input_array: Vec<Value> = match raw_input {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr,
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "type": "message",
                    "role": "user",
                    "content": s,
                })]
            }
        }
        Some(obj @ Value::Object(_)) => {
            // 已是 single item object(可能是带 type 的 input item,也可能是
            // {role,content} 形式),直接当 array[0]
            vec![obj]
        }
        Some(other) => {
            // bool / number 等非典型形式,toString 包成 user message 兜底
            vec![json!({
                "type": "message",
                "role": "user",
                "content": other.to_string(),
            })]
        }
    };
    // MOC-11: compact 摘要任务只要历史的「结论」不要「过程」,剥掉历史
    // `type=reasoning` items 省 input budget(仿 Claude Code 主动管理 stale
    // thinking)。剥离后:
    // - 真实 reasoning 不会被 `build_messages_from_input` 烤进 assistant 消息的
    //   `reasoning_content`(那条回灌发生在 `request.rs` 的
    //   `extract_input_items` 消费处:reasoning item → 下一条 assistant 的
    //   reasoning_content),input 字节随之下降;
    // - 协议兼容性不受影响:下方仍透传 `reasoning` 字段,
    //   `ensure_thinking_tool_call_reasoning` 会给带 tool_calls 的 assistant
    //   消息补 `" "` 占位,Kimi / DeepSeek 等强制 reasoning_content 的上游不 400。
    // 必须在 pre-conversion(filter input items)而非 post-conversion 做:转换后
    // `ensure_thinking_tool_call_reasoning` 已把占位塞进 tool_call 消息,届时再
    // strip `reasoning_content` 要么连占位一起删(重新 400)、要么对 agentic 会话
    // 最常见的 tool_call 消息无效。pre-conversion 让 ensure 自然补占位,最干净。
    // 多段 interleaved thinking(MOC-203,envelope 可能含多个 reasoning item)
    // 一并覆盖:retain 删全部 type=reasoning,不依赖数量。V2 compact
    // (MOC-198,strip_compaction_trigger 后复用本函数)亦自动覆盖。
    input_array.retain(|item| item.get("type").and_then(|t| t.as_str()) != Some("reasoning"));
    input_array.push(json!({
        "type": "message",
        "role": "user",
        "content": compact_summarization_prompt_for_current_language(),
    }));
    let input = Value::Array(input_array);

    let mut synthetic_responses_body = json!({
        "model": model,
        "input": input,
        "stream": false,
        "max_output_tokens": COMPACT_MAX_OUTPUT_TOKENS,
    });

    // 透传原 CompactionInput 里的 thinking-相关字段。
    // 关键:`responses_body_to_chat_body_for_provider` 内部的
    // `ensure_thinking_tool_call_reasoning` 通过 `body.get("reasoning")` 判断
    // 是否启用 thinking,只在 reasoning 字段存在时才给 history 里的
    // assistant tool_call message 补 reasoning_content。如果不透传,Kimi /
    // DeepSeek 等 thinking 默认开的上游会 400 报
    // "thinking is enabled but reasoning_content is missing in assistant
    // tool call message"。
    if let Some(reasoning) = parsed.get("reasoning") {
        synthetic_responses_body["reasoning"] = reasoning.clone();
    }
    if let Some(tools) = parsed.get("tools") {
        // 工具定义需要透传(含 ensure_thinking_tool_call_reasoning 路径
        // 的 has_tool_loop 检测,以及万一上游借 tool 信息提取上下文)。
        synthetic_responses_body["tools"] = tools.clone();
    }

    // MOC-190: compact 转换不保留最新 tool 全文(压缩历史)。set→convert→reset(即使 Err 也 reset)。
    super::request::set_compact_no_keep_recent(true);
    let conversion =
        responses_body_to_chat_body_for_provider(&synthetic_responses_body, Some(provider));
    super::request::set_compact_no_keep_recent(false);
    let chat_body = conversion?;
    let chat_body = enforce_compact_chat_message_budget(chat_body);
    let chat_body = inject_compact_disable_thinking_if_supported(chat_body);
    serde_json::to_vec(&chat_body)
        .map_err(|e| AdapterError::Internal(format!("re-serialize compact body: {e}")))
}

/// 按 chat body 的 `model` 字段查 `compact_thinking_policy` 注册表,命中即注入
/// 对应 wire(派 A `thinking.type=disabled` / 派 B `enable_thinking=false`)。
///
/// 注册表覆盖范围、入表四证、不入表的故意决策见
/// `codex_app_transfer_registry::compact_thinking_policy` 模块顶部文档。
/// 本函数只做 "查表 + 注入" 两步,**不在此处** inline 任何 provider / model 判定 —
/// 加新模型走"加 registry entry + 加 registry 单测"路径,无需改本文件。
fn inject_compact_disable_thinking_if_supported(mut chat_body: Value) -> Value {
    let model_id = chat_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    if let Some(wire) = compact_disable_thinking_wire(&model_id) {
        wire.inject(&mut chat_body);
    }
    chat_body
}

fn enforce_compact_chat_message_budget(mut chat_body: Value) -> Value {
    let Some(messages) = chat_body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return chat_body;
    };
    let original_bytes = serialized_messages_len(messages);
    if original_bytes <= COMPACT_CHAT_MESSAGES_MAX_BYTES {
        return chat_body;
    }
    let Some(prompt_message) = messages.pop() else {
        return chat_body;
    };
    let original_message_count = messages.len() + 1;
    let groups = group_chat_messages(std::mem::take(messages));
    let prompt_bytes = serialized_messages_len(&[prompt_message.clone()]);
    let history_budget = COMPACT_CHAT_MESSAGES_MAX_BYTES
        .saturating_sub(prompt_bytes)
        .saturating_sub(COMPACT_OMISSION_NOTICE_MAX_CHARS + 512);

    let mut retained_rev: Vec<Vec<Value>> = Vec::new();
    let mut retained_bytes = 0usize;
    let mut split_at = groups.len();

    for idx in (0..groups.len()).rev() {
        let compacted = compact_group_for_budget(groups[idx].clone());
        let group_bytes = serialized_messages_len(&compacted);
        if retained_bytes + group_bytes > history_budget && !retained_rev.is_empty() {
            split_at = idx + 1;
            break;
        }
        retained_bytes = retained_bytes.saturating_add(group_bytes);
        retained_rev.push(compacted);
        split_at = idx;
    }

    retained_rev.reverse();
    let mut retained_groups = retained_rev;
    let mut new_messages: Vec<Value> = Vec::new();
    if original_bytes > COMPACT_CHAT_MESSAGES_MAX_BYTES {
        new_messages.push(build_compact_omission_notice(
            &groups[..split_at],
            original_message_count,
            original_bytes,
        ));
    }
    for group in &retained_groups {
        new_messages.extend(group.iter().cloned());
    }
    new_messages.push(prompt_message.clone());

    while serialized_messages_len(&new_messages) > COMPACT_CHAT_MESSAGES_MAX_BYTES
        && !retained_groups.is_empty()
    {
        retained_groups.remove(0);
        let omitted_count = groups.len().saturating_sub(retained_groups.len());
        new_messages.clear();
        new_messages.push(build_compact_omission_notice(
            &groups[..omitted_count],
            original_message_count,
            original_bytes,
        ));
        for group in &retained_groups {
            new_messages.extend(group.iter().cloned());
        }
        new_messages.push(prompt_message.clone());
    }

    if serialized_messages_len(&new_messages) > COMPACT_CHAT_MESSAGES_MAX_BYTES {
        new_messages.clear();
        new_messages.push(build_compact_omission_notice(
            &groups,
            original_message_count,
            original_bytes,
        ));
        new_messages.push(prompt_message);
    }

    *messages = new_messages;
    chat_body
}

fn serialized_messages_len(messages: &[Value]) -> usize {
    serde_json::to_vec(messages)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

fn group_chat_messages(messages: Vec<Value>) -> Vec<Vec<Value>> {
    let mut groups = Vec::new();
    let mut idx = 0usize;
    while idx < messages.len() {
        let mut group = vec![messages[idx].clone()];
        let is_assistant_tool_call = messages[idx].get("role").and_then(|v| v.as_str())
            == Some("assistant")
            && messages[idx]
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .is_some_and(|calls| !calls.is_empty());
        idx += 1;
        if is_assistant_tool_call {
            while idx < messages.len()
                && messages[idx].get("role").and_then(|v| v.as_str()) == Some("tool")
            {
                group.push(messages[idx].clone());
                idx += 1;
            }
        }
        groups.push(group);
    }
    groups
}

fn compact_group_for_budget(group: Vec<Value>) -> Vec<Value> {
    group.into_iter().map(compact_message_for_budget).collect()
}

fn compact_message_for_budget(mut message: Value) -> Value {
    if serialized_messages_len(&[message.clone()]) <= COMPACT_SINGLE_MESSAGE_MAX_CHARS {
        return message;
    }

    if let Some(calls) = message.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
        for call in calls {
            if let Some(args) = call
                .pointer_mut("/function/arguments")
                .and_then(|v| v.as_str().map(ToOwned::to_owned))
            {
                if args.chars().count() > COMPACT_TOOL_ARGUMENTS_MAX_CHARS {
                    call["function"]["arguments"] = Value::String(shortened_tool_arguments(
                        &args,
                        COMPACT_TOOL_ARGUMENTS_MAX_CHARS,
                    ));
                }
            }
        }
    }

    if serialized_messages_len(&[message.clone()]) <= COMPACT_SINGLE_MESSAGE_MAX_CHARS {
        return message;
    }

    let role = message
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("message")
        .to_owned();
    let text = message_text(&message);
    if let Some(obj) = message.as_object_mut() {
        obj.insert(
            "content".to_owned(),
            Value::String(shortened_text(
                &format!("{role} message shortened for compact input"),
                &text,
                COMPACT_SINGLE_MESSAGE_MAX_CHARS,
            )),
        );
    }
    message
}

fn build_compact_omission_notice(
    omitted_groups: &[Vec<Value>],
    original_message_count: usize,
    original_bytes: usize,
) -> Value {
    let omitted_messages: usize = omitted_groups.iter().map(Vec::len).sum();
    let omitted_bytes: usize = omitted_groups
        .iter()
        .map(|group| serialized_messages_len(group))
        .sum();
    let mut notice = String::new();
    notice.push_str("[Compact input budget applied]\n");
    notice.push_str(
        "Older conversation blocks were omitted or shortened from this compact request so the compact request itself stays below the upstream context limit. Newest blocks and the summarization instructions were preserved.\n",
    );
    notice.push_str(&format!(
        "Original messages: {original_message_count}. Omitted messages: {omitted_messages}. Original chat messages JSON bytes: {original_bytes}. Omitted JSON bytes: {omitted_bytes}.\n"
    ));

    let user_excerpts = omitted_user_excerpts(omitted_groups, 12);
    if !user_excerpts.is_empty() {
        notice.push_str("Omitted user-message excerpts:\n");
        for excerpt in user_excerpts {
            notice.push_str("- ");
            notice.push_str(&excerpt);
            notice.push('\n');
        }
    }

    if notice.chars().count() > COMPACT_OMISSION_NOTICE_MAX_CHARS {
        notice = take_first_chars(&notice, COMPACT_OMISSION_NOTICE_MAX_CHARS);
        notice.push_str("\n[Omission notice truncated to compact budget.]");
    }

    json!({
        "role": "user",
        "content": notice,
    })
}

fn omitted_user_excerpts(groups: &[Vec<Value>], max: usize) -> Vec<String> {
    let mut excerpts = Vec::new();
    for message in groups.iter().flatten() {
        if message.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let text = message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        excerpts.push(short_excerpt(&text, 500));
        if excerpts.len() >= max {
            break;
        }
    }
    excerpts
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            if out.is_empty() {
                serde_json::to_string(parts).unwrap_or_default()
            } else {
                out
            }
        }
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => serde_json::to_string(message).unwrap_or_default(),
    }
}

fn shortened_text(label: &str, text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let head = take_first_chars(text, COMPACT_EXCERPT_HEAD_CHARS.min(max_chars / 2));
    let tail = take_last_chars(text, COMPACT_EXCERPT_TAIL_CHARS.min(max_chars / 3));
    format!(
        "[{label}]\nOriginal size: {} chars.\n--- Begin head excerpt ---\n{}\n--- End head excerpt ---\n--- Begin tail excerpt ---\n{}\n--- End tail excerpt ---\n[Omitted middle content from compact request.]",
        text.chars().count(),
        head,
        tail
    )
}

/// 把超长的 `tool_call.function.arguments` 截断到 compact 预算内,**同时保持
/// 结果仍是合法 JSON 字符串**。
///
/// OpenAI chat completions 协议要求 `function.arguments` 是合法 JSON 字符串。
/// 旧实现直接塞 [`shortened_text`] 的人类可读说明(`[... shortened ...]\n---
/// Begin head excerpt ---`),它不是 JSON,严格校验的上游(如 MiniMax)会返回
/// `400 invalid params, invalid function arguments json string`(issue #356)。
/// 这里把说明包进一个合法 JSON object,既省 token 又不违反协议。
fn shortened_tool_arguments(arguments: &str, max_chars: usize) -> String {
    let note = shortened_text(
        "Tool call arguments shortened for compact input",
        arguments,
        max_chars,
    );
    json!({ "_compacted_arguments": note }).to_string()
}

fn short_excerpt(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        normalized
    } else {
        let mut excerpt = take_first_chars(&normalized, max_chars);
        excerpt.push_str("...");
        excerpt
    }
}

fn take_first_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn take_last_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars().rev().take(max).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

/// 把上游 `/chat/completions` 的非流式 JSON 响应包装成 Codex CLI 期待的
/// compact response。
///
/// 当上游返回非 2xx 时,把它的 status + body 透传给客户端(让 Codex CLI
/// 拿到上游真实错误而不是被我们包成"假成功")。
pub(crate) fn build_compact_response_plan(
    upstream_status: StatusCode,
    mut upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
) -> Result<ResponsePlan, AdapterError> {
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    upstream_headers.remove(http::header::CONTENT_LENGTH);
    upstream_headers.remove(http::header::TRANSFER_ENCODING);

    let stream_with_logic = Box::pin(futures_util::stream::once(async move {
        match collect_and_wrap_compact_body(upstream_status, upstream_stream).await {
            Ok(body) => Ok::<Bytes, std::io::Error>(Bytes::from(body)),
            Err(e) => {
                // fix #219: 当 compact summary 质量校验失败时,返回结构化
                // 错误 JSON body(模拟 OpenAI 错误格式),让 Codex CLI 感知
                // compact 失败并保留原上下文,而非收到流中断。
                let error_body = json!({
                    "error": {
                        "message": e.to_string(),
                        "type": "compact_error",
                        "code": "compact_failed",
                    }
                });
                let bytes =
                    serde_json::to_vec(&error_body).unwrap_or_else(|_| e.to_string().into_bytes());
                Ok(Bytes::from(bytes))
            }
        }
    }));

    Ok(ResponsePlan {
        status: if upstream_status.is_success() {
            StatusCode::OK
        } else {
            upstream_status
        },
        headers: upstream_headers,
        stream: stream_with_logic,
    })
}

async fn collect_and_wrap_compact_body(
    upstream_status: StatusCode,
    mut upstream_stream: ByteStream,
) -> Result<Vec<u8>, AdapterError> {
    let mut buf = Vec::new();
    while let Some(chunk) = upstream_stream.next().await {
        let bytes = chunk.map_err(|e| AdapterError::Internal(format!("upstream io: {e}")))?;
        if buf.len() + bytes.len() > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(AdapterError::Internal(format!(
                "compact upstream response > {MAX_UPSTREAM_RESPONSE_BYTES} bytes"
            )));
        }
        buf.extend_from_slice(&bytes);
    }

    if !upstream_status.is_success() {
        // 上游错误:body 可能是 HTML/JSON/纯文本,无脑透传给客户端
        // (Codex CLI 收到非 2xx 会显示原始 body)。
        return Ok(buf);
    }

    let parsed: Value = serde_json::from_slice(&buf).map_err(|e| {
        let preview: String = String::from_utf8_lossy(&buf).chars().take(500).collect();
        AdapterError::Internal(format!(
            "compact upstream non-JSON response: {e}; first 500 chars: {preview}"
        ))
    })?;
    let raw = extract_compact_summary_text(&parsed).ok_or_else(|| {
        let preview: String = serde_json::to_string(&parsed)
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        AdapterError::Internal(format!(
            "compact upstream missing summary text (tried chat choices[0].message.content + \
             gemini candidates[0].content.parts[].text); first 300 chars: {preview}"
        ))
    })?;

    compact_response_body_from_summary_text(&raw)
}

/// 从上游 compact 响应里抽 summary 文本 —— **wire 无关**,兼容三种上游形状:
/// 1. OpenAI chat-completions:`choices[0].message.content`
/// 2. Gemini `generateContent`(Google AI Studio):`candidates[0].content.parts[*].text` 拼接
/// 3. Cloud Code / Antigravity:gemini 响应外裹 `{"response": {...}}`,先剥 `response` 再按 (2) 抽
///
/// MOC-92:此前只认 chat 形状,导致 Gemini 系(gemini_native / cloud_code)compact
/// 全部解析失败(antigravity 还因 cloud_code 未实现 compact 路由而更早炸)。
fn extract_compact_summary_text(parsed: &Value) -> Option<String> {
    // chat-completions
    if let Some(s) = parsed
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
    {
        return Some(s.to_owned());
    }
    // cloud_code/antigravity 把 gemini 响应裹在 `response` 里;gemini_native 则是直出。
    let root = parsed.get("response").unwrap_or(parsed);
    if let Some(parts) = root
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
    {
        // 正常情况:摘要在 content 通道(非 thought),优先取它 —— 维持原意
        // (排除 thought 避免思维链污染摘要;gemini_native/response.rs 也把
        // part.thought 路由 reasoning 而非 content)。content 非空就用它,**不论
        // thought 多长**:模型思考再多,结论通道才是摘要,不能因 thought 更长就反取。
        let join_parts = |want_thought: bool| -> String {
            parts
                .iter()
                .filter(|p| {
                    (p.get("thought").and_then(|t| t.as_bool()) == Some(true)) == want_thought
                })
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect()
        };
        let content_text = join_parts(false);
        if !content_text.trim().is_empty() {
            return Some(content_text);
        }
        // 失败模式(MOC-243):部分 Antigravity 模型思考档焊死在 model id(如
        // gemini-3-flash-agent = "Gemini 3.5 Flash (High)",必然 High thinking、
        // 无法运行时关闭),会把整段摘要写进 thought 通道、content 通道**空** →
        // 只取非-thought 会让 compact 拿不到摘要(missing summary text),逼 Codex
        // 回退本地 inline compact。content 空时退回 thought:compact 的"思考"本身
        // 就是对历史的浓缩,远优于整个失败。下游 quality 校验(min 800)仍把关。
        //
        // 但**仅当生成正常收尾**(finishReason == STOP)才退回 thought:若因
        // MAX_TOKENS / SAFETY 等被截断(chatgpt-codex-connector P2),thought 是
        // 不完整推理残片而非摘要,接受它会把对话历史替换成残片 —— 比 compact 失败
        // 更糟。非 STOP → 不 fallback,返回 None 让 compact 优雅失败(Codex 保留
        // 历史 / 回退本地)。与 gemini_native/response.rs 的「非 STOP = incomplete」
        // 判定一致。
        let finished_ok = root
            .pointer("/candidates/0/finishReason")
            .and_then(|v| v.as_str())
            == Some("STOP");
        if finished_ok {
            let thought_text = join_parts(true);
            if !thought_text.trim().is_empty() {
                return Some(thought_text);
            }
        }
    }
    // anthropic messages 非流式:content[].{type:"text",text}(V2 路径 anthropic
    // 复用共享包装;V1 anthropic 有自己的 collect_and_wrap,不经此函数)
    if let Some(parts) = root.get("content").and_then(|v| v.as_array()) {
        let text: String = parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// 共享(V1/V2):raw 模型输出 → 质量校验 → `encrypted_content` 文本。
fn compact_encrypted_content_from_raw(raw: &str) -> Result<String, AdapterError> {
    // B1:抽 `<summary>...</summary>` tag 内容。新短 prompt 不要求此格式,
    // raw fallback(无 tag 时返回原文)是常规路径;tag 解析保留作向后容错。
    let summary = extract_summary_section(raw).trim().to_owned();

    // B2 (fix #219): 校验 summary 输出质量。第三方模型(DeepSeek 等)可能:
    // - 输出过短无信息量
    // - 输出整段格式说明而非实际 summary
    // 校验失败时返回错误,让 Codex CLI 保留原上下文不压缩(优于注入无效摘要)。
    if let Err(reason) = validate_compact_summary_quality(&summary) {
        return Err(AdapterError::Internal(format!(
            "compact summary quality check failed: {reason}. \
             The model did not produce a valid context summary. \
             Raw output length: {} chars, summary length: {} chars.",
            raw.chars().count(),
            summary.chars().count(),
        )));
    }

    Ok(format!("{COMPACT_SUMMARY_PREFIX}\n{summary}"))
}

pub(crate) fn compact_response_body_from_summary_text(raw: &str) -> Result<Vec<u8>, AdapterError> {
    let encrypted_content = compact_encrypted_content_from_raw(raw)?;
    let compact_response = json!({
        "output": [{
            "type": "compaction",
            "encrypted_content": encrypted_content,
        }]
    });
    serde_json::to_vec(&compact_response)
        .map_err(|e| AdapterError::Internal(format!("serialize compact response: {e}")))
}

// ─── [MOC-198] remote compaction v2:流式 SSE 包装 ───────────────────────

/// V2:把上游非流式摘要响应包装成 Codex remote compaction v2 期待的 Responses
/// SSE 流:`response.created` → `response.output_item.done`(单 `type=compaction`
/// item)→ `response.completed`。摘要重组/提取/质量校验与 V1 完全共享。
///
/// 失败路径(上游非 2xx / 摘要质量不过)emit `response.failed`(HTTP 仍 200,
/// 对齐 MOC-103:Codex 对裸 HTTP 错误 + JSON body 会卡 Thinking),错误 code 经
/// [`crate::codex_retry_code`] 白名单语义:质量失败 → `invalid_prompt`(永久,
/// Codex 感知后回退本地 inline compact);上游瞬时错误保留可重试语义。
/// 绝不把普通对话响应(reasoning+message)发回去 —— 那正是修复前
/// 「expected exactly one compaction output item, got 0 from 2」的来源。
pub(crate) fn build_compact_v2_response_plan(
    upstream_status: StatusCode,
    mut upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
) -> Result<ResponsePlan, AdapterError> {
    upstream_headers.remove(http::header::CONTENT_LENGTH);
    upstream_headers.remove(http::header::CONTENT_ENCODING);
    upstream_headers.remove(http::header::TRANSFER_ENCODING);
    upstream_headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );

    // 两段流(edge-case review):先立即 emit `response.created`,再 await 上游
    // 摘要 —— 摘要可能跑数十秒(慢第三方 + 大上下文),单段流在此期间 0 字节
    // 输出会逼近 Codex 默认 ~300s idle timeout;V1 端点靠 Codex 的 compact 专属
    // 4x timeout 宽限,V2 走普通 /responses 没有这个优待。
    let id = compact_v2_response_id();
    // sequence_number(chatgpt-codex P1 / devin):全仓 SSE 不变量(converter.rs
    // `every_sse_event_has_monotonically_increasing_sequence_number`)要求每个
    // event 带单调递增 sequence_number,strict client 据此排序。两段流约定:head
    // 固定发 created(seq 0),tail 从 seq 1 续(success:1,2 / failed:1)。复用
    // core::events::emit_sse_event 统一注入,不另造轮子。
    let mut head_buf = Vec::new();
    let mut seq = 0u64;
    crate::core::events::emit_sse_event(
        &mut head_buf,
        &mut seq,
        "response.created",
        json!({
            "type": "response.created",
            "response": {"id": id, "object": "response", "status": "in_progress", "output": []},
        }),
    );
    let head =
        futures_util::stream::once(
            async move { Ok::<Bytes, std::io::Error>(Bytes::from(head_buf)) },
        );
    let tail = futures_util::stream::once(async move {
        let sse = match collect_compact_summary_for_v2(upstream_status, upstream_stream).await {
            Ok((encrypted_content, usage)) => {
                compact_v2_success_tail(&id, &encrypted_content, usage)
            }
            Err((code, kind, msg)) => {
                // [silent-failure review] 失败必留主进程日志:Retryable code 会让
                // Codex 静默重发(上限 2 次),重试窗口内仅靠 SSE 错误用户不可见。
                tracing::warn!(
                    code,
                    upstream_error_kind = kind,
                    "[compact-v2] 失败,emit response.failed: {msg}"
                );
                compact_v2_failed_tail(&id, code, kind, &msg)
            }
        };
        Ok::<Bytes, std::io::Error>(Bytes::from(sse))
    });
    let stream_with_logic = Box::pin(head.chain(tail));

    Ok(ResponsePlan {
        status: StatusCode::OK,
        headers: upstream_headers,
        stream: stream_with_logic,
    })
}

/// 收上游响应 → (encrypted_content, usage)。
/// Err 携带 (最终 error code, 原始分类 kind, message):code 已按 MOC-79/103
/// 白名单语义预映射(quality 失败必须 `invalid_prompt`,不能走 `codex_retry_code`
/// 的 kind 映射 —— 它不认识 quality 类 kind 会落 Retryable);kind 进
/// `error.upstream_error_kind` 供诊断(对齐 chat/grok/gemini 失败流惯例)。
async fn collect_compact_summary_for_v2(
    upstream_status: StatusCode,
    mut upstream_stream: ByteStream,
) -> Result<(String, Value), (&'static str, &'static str, String)> {
    let mut buf = Vec::new();
    while let Some(chunk) = upstream_stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                return Err((
                    "server_error",
                    "upstream_io",
                    format!("compact v2 upstream io: {e}"),
                ))
            }
        };
        if buf.len() + bytes.len() > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err((
                "server_error",
                "oversize",
                format!("compact v2 upstream response > {MAX_UPSTREAM_RESPONSE_BYTES} bytes"),
            ));
        }
        buf.extend_from_slice(&bytes);
    }

    if !upstream_status.is_success() {
        let preview: String = String::from_utf8_lossy(&buf).chars().take(300).collect();
        // 上游 HTTP 错误按瞬时/永久映射:401/403/400 永久 surface,余者保留可重试。
        // 429 用 `rate_limit_exceeded` 而非仓库他处的 `rate_limited`:上游 codex
        // 对该字面 code 有 retry-after 解析优待(api_bridge 的 else 分支
        // try_parse_retry_after),`rate_limited` 则只是普通 Retryable。
        let (code, kind): (&'static str, &'static str) = match upstream_status.as_u16() {
            400 => ("invalid_prompt", "http_400"),
            401 => ("invalid_prompt", "http_401"),
            403 => ("invalid_prompt", "http_403"),
            429 => ("rate_limit_exceeded", "http_429"),
            _ => ("server_error", "http_5xx_or_other"),
        };
        return Err((
            code,
            kind,
            format!("compact v2 upstream {upstream_status}: {preview}"),
        ));
    }

    let parsed: Value = serde_json::from_slice(&buf).map_err(|e| {
        let preview: String = String::from_utf8_lossy(&buf).chars().take(300).collect();
        (
            "server_error",
            "non_json",
            format!("compact v2 upstream non-JSON: {e}; first 300 chars: {preview}"),
        )
    })?;
    let raw = extract_compact_summary_text(&parsed).ok_or_else(|| {
        let preview: String = serde_json::to_string(&parsed)
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        (
            "server_error",
            "missing_summary",
            format!("compact v2 upstream missing summary text; first 300 chars: {preview}"),
        )
    })?;
    let encrypted_content = compact_encrypted_content_from_raw(&raw)
        // 质量校验失败 = 重试同请求大概率同败 → 永久语义(invalid_prompt →
        // Codex InvalidRequest 非重试,error.rs::is_retryable=false,随后回退
        // 本地 inline compact —— 上游源码坐实,非推测)
        .map_err(|e| ("invalid_prompt", "quality_check_failed", e.to_string()))?;
    Ok((encrypted_content, extract_compact_usage(&parsed)))
}

/// wire 无关地从上游响应抽 token usage,映射成 Responses usage 形态。
/// 抽不到一律 0(Codex 只做记账,不影响 compact 语义)。
fn extract_compact_usage(parsed: &Value) -> Value {
    let root = parsed.get("response").unwrap_or(parsed);
    // chat: usage.prompt_tokens/completion_tokens;anthropic: usage.input_tokens/output_tokens
    let usage = root.get("usage");
    let mut input = usage
        .and_then(|u| u.get("prompt_tokens").or_else(|| u.get("input_tokens")))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut output = usage
        .and_then(|u| {
            u.get("completion_tokens")
                .or_else(|| u.get("output_tokens"))
        })
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // gemini / cloud_code(剥 response 后): usageMetadata.promptTokenCount/candidatesTokenCount
    if input == 0 && output == 0 {
        if let Some(meta) = root.get("usageMetadata") {
            input = meta
                .get("promptTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            output = meta
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
    }
    if input == 0 && output == 0 {
        // 全 0 兜底安全(Codex 只记账,下一真实 turn 即矫正),但留痕防未来
        // provider 换 usage 字段名后永远默默报 0(silent-failure review)。
        tracing::debug!("[compact-v2] 上游响应未抽到 usage,记账按 0 上报");
    }
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": input + output,
    })
}

fn compact_v2_response_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("resp_compact_v2_{millis}")
}

/// 成功流尾段(created 已由两段流头部先发):output_item.done(compaction)→
/// completed(output 含同一 item;Codex 只对 OutputItemDone 计数 compaction、
/// completed.output 仅取 usage —— 上游 collect_compaction_output 坐实,不双计)。
fn compact_v2_success_tail(id: &str, encrypted_content: &str, usage: Value) -> Vec<u8> {
    let item = json!({"type": "compaction", "encrypted_content": encrypted_content});
    let mut out = Vec::new();
    let mut seq = 1u64; // head 已用 seq 0(created)
    crate::core::events::emit_sse_event(
        &mut out,
        &mut seq,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": item,
        }),
    );
    crate::core::events::emit_sse_event(
        &mut out,
        &mut seq,
        "response.completed",
        json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "object": "response",
                "status": "completed",
                "output": [item],
                "usage": usage,
            },
        }),
    );
    out
}

/// 失败流尾段:failed 事件。`code` 已按白名单语义预映射(见
/// [`collect_compact_summary_for_v2`] doc,不经 `codex_retry_code` —— 它不认识
/// quality 类 kind);`upstream_error_kind` 保留原始分类供诊断。帧结构单源在
/// [`crate::core::failure_stream::emit_response_failed_frame`](MOC-118,
/// 对齐 chat/grok/gemini 失败流惯例)。
fn compact_v2_failed_tail(id: &str, code: &str, kind: &str, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = 1u64; // head 已用 seq 0(created)
    crate::core::failure_stream::emit_response_failed_frame(
        &mut out, &mut seq, id, code, kind, message,
    );
    out
}

/// 校验 compact summary 的输出质量。
///
/// **#219 fix 后的精简策略**(2 道校验,从 v2.0.12 的 4 道砍掉 2 道):
///
/// 1. **C1 长度门槛**(800 字符):合格 summary 实测 1.4K-7K chars,800 留余量。
/// 2. **C4 通用结构信号**:summary 必须含至少 1 个 markdown header
///    (`#`, `##`, `###` 开头的行)或长度 ≥ 1500 chars 自由格式 — 防短而无结构
///    的"我不知道怎么总结"式无效输出。
///
/// 删掉的 C2 / C3(few-shot 指纹 / 模板指令回显)是为 v2.0.12 长 prompt
/// 配套的防御,新短 prompt 没 few-shot example、也没 9-section 强 schema,
/// 这两类回显模式不存在,继续校验只会误伤合法输出(如 `<analysis>` 段引用
/// 用户原话被当成模板回显)。
///
/// 返回 `Ok(())` 表示通过,`Err(reason)` 表示校验失败(附原因说明)。
///
/// **必须用 `chars().count()` 而非 `len()`**:本项目大量中文用户,中文每字符
/// UTF-8 是 3 bytes,`.len()` 用 byte 计数会让 800 byte ≈ 267 中文字符就通过,
/// 阈值实际比文档/错误消息标注的 "800 chars" 宽松 3 倍。同模块其它字符计数
/// 路径(`shortened_text` 等)已用 `chars().count()`,这里对齐。
fn validate_compact_summary_quality(summary: &str) -> Result<(), String> {
    let char_count = summary.chars().count();
    if char_count < 800 {
        return Err(format!(
            "summary too short ({char_count} chars, minimum 800)"
        ));
    }

    let has_markdown_header = summary
        .lines()
        .any(|line| matches!(line.trim_start().as_bytes(), [b'#', ..]));
    if !has_markdown_header && char_count < 1500 {
        return Err(format!(
            "summary lacks markdown headers and is short ({char_count} chars); \
             likely not a valid context summary"
        ));
    }

    Ok(())
}

/// 从模型输出中抽 `<summary>...</summary>` 段落。
///
/// **现状(#219 fix 后)**:新短 prompt 不要求 `<analysis>` + `<summary>` 二段输出,
/// 模型通常直接以 markdown / 段落形式回复,**raw fallback 是常规路径**。
///
/// `<summary>` tag 解析保留作向后容错:若未来某个 prompt 变体重新用 XML 包裹,
/// 或极少数模型自发输出 `<summary>` 标签,此分支仍可正确提取。
///
/// - 无 `<summary>` tag → 返回 raw(常规路径)
/// - 有 `<summary>` tag → 取**最后一个**出现点之后的内容(防历史遗留 prompt echo)
/// - 有 `<summary>` 无 `</summary>`(模型截断) → 返回 `<summary>` 之后所有文本
fn extract_summary_section(raw: &str) -> &str {
    // 取最后一个 <summary> 避免遗留 prompt echo 干扰。
    let Some(start) = raw.rfind("<summary>") else {
        return raw;
    };
    let after = &raw[start + "<summary>".len()..];
    if let Some(end) = after.rfind("</summary>") {
        &after[..end]
    } else {
        after
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use codex_app_transfer_registry::Provider;
    use futures_util::stream;
    use serde_json::json;

    fn make_provider() -> Provider {
        let mut p = Provider {
            id: "mimo".into(),
            name: "MiMo".into(),
            base_url: "https://example.com/v1".into(),
            auth_scheme: "bearer".into(),
            api_format: "responses".into(),
            api_key: String::new(),
            models: Default::default(),
            extra_headers: Default::default(),
            model_capabilities: Default::default(),
            request_options: Default::default(),
            is_builtin: false,
            sort_index: 0,
            extra: Default::default(),
        };
        p.models.insert("default".into(), "mimo-v2.5".into());
        p
    }

    #[test]
    fn shortened_tool_arguments_stays_valid_json() {
        // issue #356:截断超长 tool_call arguments 后必须仍是合法 JSON 字符串
        // (OpenAI chat 协议),否则严格校验的上游(MiniMax)返回
        // 400 invalid function arguments json string。
        let huge = format!("{{\"path\":\"/x\",\"content\":\"{}\"}}", "A".repeat(20_000));
        let out = shortened_tool_arguments(&huge, COMPACT_TOOL_ARGUMENTS_MAX_CHARS);
        let parsed: Value =
            serde_json::from_str(&out).expect("shortened tool arguments 必须是合法 JSON");
        assert!(
            parsed
                .get("_compacted_arguments")
                .and_then(|v| v.as_str())
                .is_some(),
            "截断说明应包在合法 JSON object 里"
        );
        assert!(
            out.chars().count() < huge.chars().count(),
            "截断后应短于原始 arguments"
        );
    }

    #[test]
    fn build_compact_chat_request_keeps_tool_arguments_valid_json_after_budget() {
        // 端到端回归(issue #356):compact messages 超预算触发裁剪、且历史里有
        // 超长 arguments 的 tool_call 时,产出的每个 tool_call arguments 都必须是
        // 合法 JSON 字符串。
        let p = make_provider();
        let huge_args = format!("{{\"content\":\"{}\"}}", "A".repeat(50_000));
        let filler = "x".repeat(200_000);
        let body = json!({
            "model": "mimo-v2.5",
            "input": [
                {"type": "message", "role": "user", "content": filler},
                {"type": "function_call", "call_id": "call_big", "name": "write", "arguments": huge_args},
                {"type": "function_call_output", "call_id": "call_big", "output": "ok"},
                {"type": "message", "role": "user", "content": "continue"}
            ],
            "tools": [{"type": "function", "name": "write"}]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        let mut checked = 0;
        let mut saw_compacted = false;
        for m in messages {
            if let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array()) {
                for call in calls {
                    let args = call["function"]["arguments"].as_str().unwrap();
                    serde_json::from_str::<Value>(args).unwrap_or_else(|e| {
                        panic!("compact 后 tool_call arguments 必须是合法 JSON: {args:?} ({e})")
                    });
                    if args.contains("_compacted_arguments") {
                        saw_compacted = true;
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "应至少校验一个 tool_call arguments");
        // 防 no-op 假绿:确认确实走了截断路径(而非 budget 逻辑变动导致 call_big
        // 整条被丢/未截断)。截断标记由 shortened_tool_arguments 注入。
        assert!(
            saw_compacted,
            "测试应实际触发 arguments 截断;若 budget 逻辑改动导致未命中,请调整 fixture 规模"
        );
    }

    #[test]
    fn is_compact_path_recognizes_v1_and_bare_forms() {
        assert!(is_compact_path("/responses/compact"));
        assert!(is_compact_path("/v1/responses/compact"));
        assert!(is_compact_path("/openai/v1/responses/compact"));
        assert!(is_compact_path("/responses/compact?foo=bar"));
        assert!(is_compact_path("/responses/compact/"));
        // 负向
        assert!(!is_compact_path("/responses"));
        assert!(!is_compact_path("/responses/compact/extra"));
        assert!(!is_compact_path("/chat/completions"));
    }

    #[test]
    fn build_compact_chat_request_passes_through_reasoning_field_for_thinking_repair() {
        // Kimi/DeepSeek 等 thinking 模式 provider 要求历史里的 assistant
        // tool_call message 必带 reasoning_content。`ensure_thinking_tool_call_reasoning`
        // 通过 body.reasoning 字段判断是否启用 thinking。compact 路径合成的
        // synthetic body **必须**透传原 reasoning,否则 thinking 模式上游
        // 会 400 "thinking is enabled but reasoning_content is missing"。
        let p = make_provider();
        let body = json!({
            "model": "kimi-for-coding",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "next"}
                ]}
            ],
            "reasoning": {"effort": "high"},
            "tools": [{"type": "function", "name": "shell"}]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        // 找到 function_call 转出来的 assistant message,必须带 reasoning_content
        let assistant_with_tool_calls = messages
            .iter()
            .find(|m| {
                m["role"] == "assistant" && m.get("tool_calls").and_then(|v| v.as_array()).is_some()
            })
            .expect("应有一条 assistant + tool_calls(从 function_call 转换而来)");
        // ensure_thinking_tool_call_reasoning 在缺真实 reasoning 时塞 " "(单空格占位)
        // 这就是 Kimi/DeepSeek 上游接受的兜底值,字段存在即可,不做非空断言。
        assert!(
            assistant_with_tool_calls
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .is_some(),
            "thinking 启用时 assistant tool_call 必须带 reasoning_content 字段(可以是单空格占位)"
        );
    }

    #[test]
    fn build_compact_chat_request_strips_history_reasoning_items() {
        // MOC-11: compact 摘要只要历史结论不要过程,历史 `type=reasoning` items
        // 的真实文本不应进入上游 chat body(省 input budget)。但带 tool_calls
        // 的 assistant 消息仍须由 ensure_thinking_tool_call_reasoning 补 " " 占位,
        // 保 Kimi/DeepSeek 等强制 reasoning_content 的上游协议兼容。
        let p = make_provider();
        let secret = "SECRET_HISTORY_REASONING_TEXT_should_be_stripped";
        let body = json!({
            "model": "kimi-for-coding",
            "input": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": secret}]},
                {"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "next"}
                ]}
            ],
            "reasoning": {"effort": "high"},
            "tools": [{"type": "function", "name": "shell"}]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        // 真实历史 reasoning 文本必须从整个上游 body 里彻底消失
        let raw = String::from_utf8(chat.clone()).unwrap();
        assert!(
            !raw.contains(secret),
            "历史 reasoning 文本不应出现在 compact chat body 里(应被剥离)"
        );
        // 带 tool_calls 的 assistant 消息仍带 reasoning_content,但只是占位(空白)
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        let assistant_with_tool_calls = messages
            .iter()
            .find(|m| {
                m["role"] == "assistant" && m.get("tool_calls").and_then(|v| v.as_array()).is_some()
            })
            .expect("应有一条 assistant + tool_calls(从 function_call 转换而来)");
        let reasoning_content = assistant_with_tool_calls
            .get("reasoning_content")
            .and_then(|v| v.as_str());
        assert!(
            reasoning_content.is_some(),
            "thinking 启用时 assistant tool_call 仍须带 reasoning_content 字段"
        );
        assert!(
            reasoning_content.unwrap().trim().is_empty(),
            "剥离后 reasoning_content 应为占位(空白),而非真实历史 reasoning 文本"
        );
    }

    #[test]
    fn build_compact_chat_request_bounds_large_tool_output_before_prompt() {
        let p = make_provider();
        let huge_line = "const minified='x';".repeat(3_000);
        let raw_output = format!(
            "Chunk ID: 44d863\n\
             Wall time: 0.1540 seconds\n\
             Process exited with code 0\n\
             Original token count: 924828\n\
             Output:\n\
             Total output lines: 18\n\n\
             /tmp/codex-asar/webview/assets/plugins-page-selectors.js:{huge_line}"
        );
        let body = json!({
            "model": "kimi-for-coding",
            "input": [
                {"type": "function_call", "call_id": "tool_large", "name": "exec_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "tool_large", "output": raw_output}
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        let tool_msg = messages
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("compact 请求中应保留 bounded tool message");
        let content = tool_msg["content"].as_str().unwrap();

        assert_eq!(tool_msg["tool_call_id"], "tool_large");
        assert!(content.contains("[Tool output stored outside model context]"));
        assert!(content.contains("Artifact ID: tool_artifact_"));
        assert!(content.contains("Original token count: 924828"));
        assert!(
            content.len() < 20_000,
            "compact 前 tool.content 应被有界化,实际长度 {}",
            content.len()
        );
        assert!(
            messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
                .is_some_and(|text| text.contains("performing a CONTEXT CHECKPOINT COMPACTION")),
            "compact summary prompt 仍应作为最后一条 user message 注入"
        );
    }

    #[test]
    fn build_compact_chat_request_prunes_chat_messages_to_compact_budget() {
        let p = make_provider();
        let old_huge = "old research detail ".repeat(10_000);
        let recent = "recent user instruction that must remain visible";
        let body = json!({
            "model": "kimi-for-coding",
            "input": [
                {"type": "message", "role": "user", "content": old_huge},
                {"type": "message", "role": "assistant", "content": "ack"},
                {"type": "message", "role": "user", "content": recent}
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        let messages_bytes = serde_json::to_vec(messages).unwrap().len();

        assert!(
            messages_bytes <= COMPACT_CHAT_MESSAGES_MAX_BYTES,
            "compact messages must be budgeted before upstream request; actual={messages_bytes}"
        );
        assert!(
            messages.iter().any(|m| {
                m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .unwrap_or("")
                        .contains("[Compact input budget applied]")
            }),
            "budget pruning must be explicit, not silent"
        );
        assert!(
            messages.iter().any(|m| {
                m["role"] == "user" && m["content"].as_str().unwrap_or("").contains(recent)
            }),
            "recent user message should be retained"
        );
        assert!(
            messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
                .is_some_and(|text| text.contains("CONTEXT CHECKPOINT")),
            "summarization prompt must remain the last message"
        );
    }

    #[test]
    fn build_compact_chat_request_keeps_tail_tool_chain_together_after_pruning() {
        let p = make_provider();
        let old_huge = "old context ".repeat(10_000);
        let body = json!({
            "model": "kimi-for-coding",
            "input": [
                {"type": "message", "role": "user", "content": old_huge},
                {"type": "function_call", "call_id": "tail_tool", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "tail_tool", "output": "short result"},
                {"type": "message", "role": "user", "content": "continue from the tool result"}
            ],
            "tools": [{"type": "function", "name": "shell"}]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();

        let assistant_idx = messages
            .iter()
            .position(|m| {
                m["role"] == "assistant"
                    && m.get("tool_calls")
                        .and_then(|v| v.as_array())
                        .is_some_and(|calls| calls.iter().any(|call| call["id"] == "tail_tool"))
            })
            .expect("tail assistant tool call should be retained");
        let tool_msg = messages
            .get(assistant_idx + 1)
            .expect("tool response should immediately follow assistant tool call");
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "tail_tool");
    }

    #[test]
    fn build_compact_chat_request_injects_prompt_as_last_user_message() {
        // v2.0.12 调整:SUMMARIZATION_PROMPT 注入成**最后一条 user message**
        // (不是 system),对齐 Codex CLI 自家做法,提升第三方 provider 服从度。
        let p = make_provider();
        let body = json!({
            "model": "mimo-v2.5",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hello"}
                ]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "world"}
                ]},
            ],
            "instructions": "ORIGINAL_PROJECT_INSTRUCTIONS",
            "tools": [{"type": "function", "name": "shell"}],
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();

        // 最后一条 message 必须是 user + 包含 SUMMARIZATION_PROMPT 关键字
        let last = messages.last().expect("non-empty messages");
        assert_eq!(last["role"], "user", "prompt 必须注入成 user message");
        let last_content = last["content"].as_str().unwrap_or_else(|| {
            // content 也可能是 array(取决于 provider 转换路径)
            last["content"]
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find_map(|b| b.get("text").and_then(|v| v.as_str()))
                })
                .unwrap_or_default()
        });
        assert!(
            last_content.contains("CONTEXT CHECKPOINT"),
            "last user message 必须含 SUMMARIZATION_PROMPT 关键字 'CONTEXT CHECKPOINT',实际:{last_content}"
        );
        assert!(
            last_content.contains("All user messages"),
            "prompt 必须含 'All user messages' bullet(下一轮模型 verbatim 锚定)"
        );
        assert!(
            last_content.contains("Next Step") && last_content.contains("verbatim direct quote"),
            "prompt 必须含 Next Step + verbatim quote bullet(防任务漂移)"
        );

        // 原 instructions **不应**进 system/任何 message(摘要任务不受原任务 system 影响)
        assert!(
            !messages.iter().any(|m| m["content"]
                .as_str()
                .unwrap_or("")
                .contains("ORIGINAL_PROJECT_INSTRUCTIONS")),
            "原 instructions 应被丢掉,不应进 messages"
        );
        // 没有 system message(prompt 改 user message 后)
        assert!(
            !messages.iter().any(|m| m["role"] == "system"),
            "compact 请求不应再产生 system message,实际 messages 角色:{:?}",
            messages
                .iter()
                .map(|m| m["role"].clone())
                .collect::<Vec<_>>()
        );
        // 历史 user / assistant 保留
        assert!(messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"].as_str().unwrap_or("").contains("hello")));
        assert!(messages
            .iter()
            .any(|m| m["role"] == "assistant"
                && m["content"].as_str().unwrap_or("").contains("world")));
        // stream 字段不带(false 在 chat body 转换里会被丢)
        assert!(parsed.get("stream").is_none() || parsed["stream"] == false);
    }

    #[test]
    fn build_compact_chat_request_injects_prompt_when_input_is_string() {
        // 关键回归(2026-05-08 codex review P2):input 不一定是 array,
        // 也可能是 string / object / null / 缺失。**所有形式都必须确保 prompt
        // 被注入**,否则上游收到无 summary 指令的请求,返回任意 chat 内容。
        let p = make_provider();
        let body = json!({
            "model": "mimo-v2.5",
            "input": "raw user prompt as plain string",
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        let last = messages.last().expect("messages 非空");
        let last_text = last["content"].as_str().unwrap_or_default();
        assert!(
            last_text.contains("CONTEXT CHECKPOINT"),
            "string input 路径下 prompt 必须仍被注入,实际 last:{last:?}"
        );
        // 原 string input 也应保留为前一条 user message
        assert!(messages.iter().any(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("raw user prompt as plain string")
        }));
    }

    #[test]
    fn build_compact_chat_request_injects_prompt_when_input_is_object() {
        // input 是单个 object item(非典型但合法),prompt 必须注入
        let p = make_provider();
        let body = json!({
            "model": "mimo-v2.5",
            "input": {"type": "message", "role": "user", "content": "single obj"},
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let chat = build_compact_chat_request(&bytes, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert!(
            last["content"]
                .as_str()
                .unwrap_or("")
                .contains("CONTEXT CHECKPOINT"),
            "object input 路径下 prompt 必须仍被注入"
        );
    }

    #[test]
    fn build_compact_chat_request_injects_prompt_when_input_is_null_or_missing() {
        let p = make_provider();
        for body in [
            json!({"model": "mimo-v2.5"}),
            json!({"model": "mimo-v2.5", "input": null}),
            json!({"model": "mimo-v2.5", "input": []}),
            json!({"model": "mimo-v2.5", "input": ""}),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            let chat = build_compact_chat_request(&bytes, &p).unwrap();
            let parsed: Value = serde_json::from_slice(&chat).unwrap();
            let messages = parsed["messages"].as_array().unwrap();
            let last = messages.last().expect("messages 必非空(prompt 至少一条)");
            assert!(
                last["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("CONTEXT CHECKPOINT"),
                "null/empty input 时 prompt 也必须注入,实际 body={body:?},last={last:?}"
            );
        }
    }

    // ── extract_summary_section ──────────────────────────────────────

    #[test]
    fn extract_summary_section_strips_analysis_and_keeps_summary() {
        let raw = "<analysis>\nblah blah meta\n</analysis>\n<summary>\nactual summary content\n</summary>";
        assert_eq!(
            extract_summary_section(raw).trim(),
            "actual summary content"
        );
    }

    #[test]
    fn extract_summary_section_handles_summary_only_no_analysis() {
        let raw = "<summary>\njust a summary\n</summary>";
        assert_eq!(extract_summary_section(raw).trim(), "just a summary");
    }

    #[test]
    fn extract_summary_section_returns_raw_when_no_tag() {
        // 模型没遵守格式 → 整段保留(总比丢好,日志会反映质量)
        let raw = "this is plain text without any tags";
        assert_eq!(extract_summary_section(raw), raw);
    }

    #[test]
    fn extract_summary_section_handles_truncated_close_tag() {
        // 模型输出超 max_tokens 被截断,只有 <summary> 没 </summary>
        let raw = "<analysis>meta</analysis><summary>\npartial summary content cut off here";
        assert_eq!(
            extract_summary_section(raw).trim(),
            "partial summary content cut off here"
        );
    }

    #[test]
    fn extract_summary_section_picks_last_when_echo_present() {
        // rfind 取最后一个 <summary>,跳过历史遗留 prompt echo 干扰。
        // 当模型 echo 旧格式 prompt 后再输出自己的 summary 时,取最后一个。
        let raw =
            "<summary>example echo content</summary>\n<summary>actual model output here</summary>";
        assert_eq!(
            extract_summary_section(raw).trim(),
            "actual model output here"
        );
    }

    #[test]
    fn extract_summary_section_single_pair_unchanged() {
        // 单对 <summary>...</summary> 行为不变
        let raw = "<analysis>meta</analysis>\n<summary>good summary content</summary>";
        assert_eq!(extract_summary_section(raw).trim(), "good summary content");
    }

    fn one_chunk_stream(bytes: Vec<u8>) -> ByteStream {
        Box::pin(stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from(bytes))
        }))
    }

    async fn collect_stream_bytes(mut s: ByteStream) -> Vec<u8> {
        let mut buf = Vec::new();
        while let Some(chunk) = s.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    /// 测试用 helper:把 caller 给的 marker 文本包成至少 850 字符 + 带 markdown
    /// header,以同时满足 `validate_compact_summary_quality` 的 C1(≥800 chars)
    /// 和 C4(至少 1 个 markdown header)门槛。
    fn long_valid_summary(marker: &str) -> String {
        let mut out = String::from("## Context Checkpoint Summary\n\n");
        out.push_str(marker);
        out.push_str("\n\n");
        let padding = "Additional handoff context preserved verbatim from prior turns to ensure the next LLM can resume without re-asking. ";
        while out.len() < 850 {
            out.push_str(padding);
        }
        out
    }

    #[tokio::test]
    async fn collect_and_wrap_extracts_summary_into_compaction_item() {
        // summary 需 >= 800 chars + markdown header 以通过质量校验(fix #219)
        let summary_content = long_valid_summary(
            "Primary Request: refactor the authentication module to support OAuth2 flows and PKCE.",
        );
        let upstream_body = serde_json::to_vec(&json!({
            "id": "chatcmpl_x",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": summary_content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        }))
        .unwrap();

        let body = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(upstream_body))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        let output = parsed["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "compaction");
        let enc = output[0]["encrypted_content"].as_str().unwrap();
        assert!(
            enc.starts_with(COMPACT_SUMMARY_PREFIX),
            "encrypted_content 必须以 SUMMARY_PREFIX 开头(Codex CLI 用它识别 summary)"
        );
        assert!(enc.contains("OAuth2 flows and PKCE"));
    }

    #[tokio::test]
    async fn collect_and_wrap_strips_analysis_keeps_only_summary_in_encrypted_content() {
        // 即便新 prompt 不再要求 `<analysis>` + `<summary>` 二段输出,模型如果
        // 自发产出二段格式时 `extract_summary_section` 仍应正确剥离 analysis
        // chain-of-thought(避免污染下一轮 history)。函数仍是 raw fallback 容错
        // 兼 tag 抽取,本测试验证 tag 抽取分支的行为。
        // 注:`<summary>` 内文本仍需 >= 800 chars + markdown header 通过质量校验。
        let summary_inner = long_valid_summary(
            "Primary Request: User requested to do Z after initially asking X. \
             Last user message verbatim: \"actually do Z\".",
        );
        let model_output = format!(
            "<analysis>\nUser asked X, I did Y, then user corrected to Z. This is detailed chain-of-thought.\n</analysis>\n<summary>\n{summary_inner}\n</summary>"
        );
        let upstream_body = serde_json::to_vec(&json!({
            "choices": [{
                "message": {"role": "assistant", "content": model_output},
                "finish_reason": "stop"
            }]
        }))
        .unwrap();

        let body = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(upstream_body))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        let enc = parsed["output"][0]["encrypted_content"].as_str().unwrap();
        assert!(enc.starts_with(COMPACT_SUMMARY_PREFIX));
        assert!(
            !enc.contains("<analysis>") && !enc.contains("</analysis>"),
            "analysis tag 不应进 encrypted_content"
        );
        assert!(
            !enc.contains("User asked X, I did Y"),
            "analysis chain-of-thought 内容不应被保留"
        );
        assert!(enc.contains("Primary Request"));
        assert!(enc.contains("\"actually do Z\""));
    }

    #[tokio::test]
    async fn collect_and_wrap_chunked_upstream_response() {
        // 上游分多 chunk 来,我们应该正确拼接后解析
        // 注:summary 需 >= 800 chars + markdown header 以通过质量校验
        let chunked_summary = long_valid_summary(
            "Primary Request: User asked to implement chunked transfer encoding support for the proxy layer.",
        );
        let upstream_body = serde_json::to_vec(&json!({
            "choices": [{"message": {"content": chunked_summary}, "finish_reason": "stop"}]
        }))
        .unwrap();
        let mid = upstream_body.len() / 2;
        let part1 = upstream_body[..mid].to_vec();
        let part2 = upstream_body[mid..].to_vec();
        let s: ByteStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from(part1)),
            Ok(Bytes::from(part2)),
        ]));
        let body = collect_and_wrap_compact_body(StatusCode::OK, s)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed["output"][0]["encrypted_content"]
            .as_str()
            .unwrap()
            .contains("chunked transfer encoding"));
    }

    #[tokio::test]
    async fn collect_and_wrap_passes_through_upstream_error_body() {
        // 上游 4xx/5xx 时直接透传 body,让 Codex CLI 看到真实错误
        let body = collect_and_wrap_compact_body(
            StatusCode::BAD_REQUEST,
            one_chunk_stream(b"<html>upstream rate limit</html>".to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(body, b"<html>upstream rate limit</html>");
    }

    #[tokio::test]
    async fn collect_and_wrap_rejects_oversized_response() {
        let huge: Vec<u8> = vec![0; MAX_UPSTREAM_RESPONSE_BYTES + 1];
        let err = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(huge))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("> "));
    }

    #[tokio::test]
    async fn collect_and_wrap_errors_on_missing_message_content() {
        let upstream_body =
            serde_json::to_vec(&json!({"choices": [{"finish_reason": "stop"}]})).unwrap();
        let err = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(upstream_body))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("missing summary text"),
            "实际错误:{err}"
        );
    }

    #[test]
    fn extract_compact_summary_text_handles_chat_gemini_and_cloudcode_shapes() {
        // MOC-92:三种上游形状都要能抽出 summary 文本。
        let long = "x".repeat(900); // 过质量校验无关,这里只验抽取
                                    // 1. chat-completions
        let chat = json!({"choices": [{"message": {"content": long}}]});
        assert_eq!(
            extract_compact_summary_text(&chat).as_deref(),
            Some(long.as_str())
        );
        // 2. gemini generateContent(Google AI Studio,直出);thought 部分必须排除
        let gemini = json!({"candidates": [{"content": {"parts": [
            {"text": "chain of thought...", "thought": true},
            {"text": "part-a "}, {"text": "part-b"}
        ]}}]});
        assert_eq!(
            extract_compact_summary_text(&gemini).as_deref(),
            Some("part-a part-b"),
            "thought 部分应被排除,不污染 summary"
        );
        // 3. cloud_code / antigravity:gemini 外裹 {"response": {...}}
        let cloud = json!({"response": {"candidates": [{"content": {"parts": [
            {"text": "wrapped summary"}
        ]}}]}});
        assert_eq!(
            extract_compact_summary_text(&cloud).as_deref(),
            Some("wrapped summary")
        );
        // 4. 都不匹配 → None
        assert_eq!(extract_compact_summary_text(&json!({"foo": "bar"})), None);
        assert_eq!(
            extract_compact_summary_text(&json!({"candidates": [{"content": {"parts": []}}]})),
            None
        );
    }

    #[test]
    fn extract_compact_summary_text_falls_back_to_thought_when_content_empty() {
        // MOC-243:gemini-3-flash-agent(思考档焊死在 id,必然 thinking)可能把整段
        // 摘要写进 thought 通道、content 通道空 → 旧逻辑只取非-thought 会返回 None
        // (missing summary text),compact 失败。content 空 + 正常收尾(STOP)时应
        // 退回 thought 文本。
        let all_thought = json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "整段摘要其实在这里...", "thought": true},
            {"text": "more summary in thought", "thought": true}
        ]}}]});
        assert_eq!(
            extract_compact_summary_text(&all_thought).as_deref(),
            Some("整段摘要其实在这里...more summary in thought"),
            "content 通道空 + STOP 时应退回 thought 文本,而非返回 None 让 compact 失败"
        );
        // cloud_code / antigravity 外裹 {"response": {...}} 变体同样生效
        let all_thought_wrapped = json!({"response": {"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "wrapped thought summary", "thought": true}
        ]}}]}});
        assert_eq!(
            extract_compact_summary_text(&all_thought_wrapped).as_deref(),
            Some("wrapped thought summary")
        );
        // 回归守卫:content 非空时**仍优先 content**,即便 thought 更长也不反取
        // (content 路径不看 finishReason —— 维持既有行为,本次只 gate thought fallback)
        let content_shorter_than_thought = json!({"candidates": [{"content": {"parts": [
            {"text": "a very long chain of thought that is much longer than the summary", "thought": true},
            {"text": "short summary"}
        ]}}]});
        assert_eq!(
            extract_compact_summary_text(&content_shorter_than_thought).as_deref(),
            Some("short summary"),
            "content 非空就用 content,不能因 thought 更长就反取(避免思维链污染)"
        );
        // content 纯空白(非空但 trim 后为空)+ 有真实 thought + STOP → 退回 thought
        // (锁住 .trim().is_empty() guard:空白不该当作有效摘要返回)
        let blank_content = json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "   "},
            {"text": "real summary lives in thought", "thought": true}
        ]}}]});
        assert_eq!(
            extract_compact_summary_text(&blank_content).as_deref(),
            Some("real summary lives in thought"),
            "content 纯空白 + STOP 时应退回 thought,而非返回空白摘要"
        );
        // [chatgpt-codex-connector P2] content 空但**生成被截断**(非 STOP,如
        // MAX_TOKENS / SAFETY)→ thought 是不完整残片,**不**退回,返回 None 让
        // compact 优雅失败,绝不把历史替换成截断的推理残片。
        for fr in ["MAX_TOKENS", "SAFETY", "RECITATION"] {
            let truncated = json!({"candidates": [{"finishReason": fr, "content": {"parts": [
                {"text": "incomplete reasoning trace cut off mid-thought...", "thought": true}
            ]}}]});
            assert_eq!(
                extract_compact_summary_text(&truncated),
                None,
                "finishReason={fr}(非 STOP)时 thought 是截断残片,不应作摘要返回"
            );
        }
        // finishReason 缺失(异常)同样不 fallback —— 要求显式 STOP
        let no_finish = json!({"candidates": [{"content": {"parts": [
            {"text": "thought without finish reason", "thought": true}
        ]}}]});
        assert_eq!(extract_compact_summary_text(&no_finish), None);
        // content 与 thought 都空 → None
        assert_eq!(
            extract_compact_summary_text(
                &json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [
                    {"text": "", "thought": true}
                ]}}]})
            ),
            None
        );
    }

    // ── validate_compact_summary_quality (fix #219) ──────────────────

    #[test]
    fn quality_check_rejects_too_short_summary() {
        assert!(validate_compact_summary_quality("short").is_err());
        assert!(validate_compact_summary_quality("").is_err());
        assert!(validate_compact_summary_quality(&"a".repeat(799)).is_err());
    }

    #[test]
    fn quality_check_counts_characters_not_bytes_for_cjk() {
        // 防 byte/chars 回归(Devin Review):中文每字符 UTF-8 是 3 bytes。
        // 300 个汉字 = 900 bytes 但只 300 字符,应该 reject(< 800 char 门槛)。
        let cjk_300 = "中".repeat(300);
        assert_eq!(cjk_300.len(), 900, "前置断言:确认 byte 长度 ≥ 800");
        assert_eq!(cjk_300.chars().count(), 300);
        let result = validate_compact_summary_quality(&cjk_300);
        assert!(
            result.is_err(),
            "300 中文字符必须被判过短(不能因 900 byte 误判通过)"
        );
        assert!(
            result.unwrap_err().contains("300 chars"),
            "错误消息必须显示字符数而非字节数"
        );
    }

    #[test]
    fn quality_check_passes_summary_with_markdown_header() {
        // C4 通用化:任何 `#` 起头的 markdown header 都算合法结构信号,
        // 不再要求严格九段 schema(已不强制九段了)
        let summary = long_valid_summary(
            "Primary Request: User wants to add dark mode toggle to settings page. \
             Next Step (verbatim): \"make sure it persists across sessions\".",
        );
        assert!(validate_compact_summary_quality(&summary).is_ok());
    }

    #[test]
    fn quality_check_passes_long_free_form_without_headers() {
        // 没有 markdown header 但实质内容超长(≥ 1500 chars)的自由格式仍应通过 —
        // 一些模型会用纯段落而不是 markdown 结构作答
        let free_form_chunk = "The user has been working on implementing a WebSocket server \
            for real-time notifications. They started by setting up the tokio runtime \
            and configuring the hyper server to handle upgrade requests. The main files \
            involved are src/ws/server.rs and src/ws/handler.rs. They encountered an \
            issue with the handshake failing due to missing Sec-WebSocket-Accept header \
            computation. This was fixed by using the sha1 crate to compute the correct \
            response hash. The user then asked to add message broadcasting to all \
            connected clients using a shared state protected by Arc<RwLock>. ";
        let free_form = format!("{free_form_chunk}{free_form_chunk}{free_form_chunk}");
        assert!(free_form.len() >= 1500);
        assert!(validate_compact_summary_quality(&free_form).is_ok());
    }

    #[test]
    fn quality_check_rejects_short_summary_without_header() {
        // ≥ 800 chars 但 < 1500 chars 且无 markdown header → 拒绝
        let s = "x".repeat(1000);
        assert!(!s.lines().any(|l| l.starts_with('#')));
        let result = validate_compact_summary_quality(&s);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("lacks markdown headers"));
    }

    #[tokio::test]
    async fn collect_and_wrap_returns_error_on_quality_failure() {
        // 当 summary 质量校验失败时,应返回错误
        let upstream_body = serde_json::to_vec(&json!({
            "choices": [{"message": {"content": "too short"}, "finish_reason": "stop"}]
        }))
        .unwrap();
        let err = collect_and_wrap_compact_body(StatusCode::OK, one_chunk_stream(upstream_body))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("quality check failed"));
    }

    // ── compact_thinking_policy 注册表接入(issue #248) ─────────────────
    //
    // 这些测试只断言 "build_compact_chat_request 末尾正确调用了 registry 注入"
    // 这一**集成点**,不重复 registry 自己的 entry-by-entry 覆盖测试
    // (那些在 `codex_app_transfer_registry::compact_thinking_policy::tests`)。
    // 加新模型走 registry 单测;本处只验"接入路径活着"。

    /// 构造一个除 model 字段外都跟 `make_provider()` 一致的 provider。
    /// 用于断言"注入决策只看 chat body 的 model 字段,不看 provider"。
    fn provider_with_model(model_id: &str) -> Provider {
        let mut p = make_provider();
        p.models.insert("default".into(), model_id.into());
        p
    }

    fn simple_compact_body(model: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": model,
            "input": [
                {"type": "message", "role": "user", "content": "hello"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn compact_injects_thinking_type_disabled_for_glm_5_1() {
        // issue #248 主修复:GLM-5.1 强制 thinking,本 PR 注入 thinking.disabled
        // 把 max_tokens 全留给 summary content。
        let p = provider_with_model("glm-5.1");
        let chat = build_compact_chat_request(&simple_compact_body("glm-5.1"), &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert_eq!(
            parsed["thinking"],
            json!({"type": "disabled"}),
            "glm-5.1 必须命中 compact_thinking_policy 派 A,chat body 含 thinking.type=disabled"
        );
    }

    #[test]
    fn compact_injects_enable_thinking_false_for_qwen3() {
        // 派 B:Qwen 3.x 用 enable_thinking=false wire,确认接入对派 B 也活着
        let p = provider_with_model("qwen3.6-plus");
        let chat = build_compact_chat_request(&simple_compact_body("qwen3.6-plus"), &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert_eq!(
            parsed["enable_thinking"],
            json!(false),
            "qwen3.6-plus 必须命中 compact_thinking_policy 派 B,chat body 含 enable_thinking=false"
        );
    }

    #[test]
    fn compact_does_not_inject_for_minimax_no_disable_wire() {
        // MiniMax M2.x 故意不入表(上游不支持 disable),compact body 必须**不含**
        // thinking / enable_thinking 字段,避免给不认识的 endpoint 发 unknown field
        let p = provider_with_model("MiniMax-M2.7");
        let chat = build_compact_chat_request(&simple_compact_body("MiniMax-M2.7"), &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert!(
            parsed.get("thinking").is_none(),
            "MiniMax 不在 compact_thinking_policy 白名单,chat body 不应有 thinking 字段"
        );
        assert!(
            parsed.get("enable_thinking").is_none(),
            "MiniMax 不在 compact_thinking_policy 白名单,chat body 不应有 enable_thinking 字段"
        );
    }

    #[test]
    fn compact_does_not_inject_for_moonshot_v1_no_thinking_mode() {
        // moonshot-v1 老 base 模型没有 thinking 模式,故意不入表
        let p = provider_with_model("moonshot-v1-32k");
        let chat = build_compact_chat_request(&simple_compact_body("moonshot-v1-32k"), &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert!(
            parsed.get("thinking").is_none(),
            "moonshot-v1 老模型无 thinking 模式,chat body 不应有 thinking 字段"
        );
        assert!(
            parsed.get("enable_thinking").is_none(),
            "moonshot-v1 老模型无 thinking 模式,chat body 不应有 enable_thinking 字段"
        );
    }

    #[test]
    fn compact_does_not_inject_for_unknown_model() {
        // 用户自定义 / 未收录的 model:保守不注入,保持 current behavior
        let p = provider_with_model("some-custom-model");
        let chat =
            build_compact_chat_request(&simple_compact_body("some-custom-model"), &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert!(
            parsed.get("thinking").is_none() && parsed.get("enable_thinking").is_none(),
            "未知 model 不应触发 compact_thinking_policy 注入"
        );
    }

    #[test]
    fn compact_does_not_inject_when_model_field_missing_or_null() {
        // 防御性:`inject_compact_disable_thinking_if_supported` 用
        // `unwrap_or("")` 兜底缺失/null model,registry 对空 string 返 None,
        // 整条链路应静默 no-op 而非 panic。
        let p = provider_with_model("glm-5.1");
        // 缺 model 字段
        let body_missing = serde_json::to_vec(&json!({
            "input": [{"type": "message", "role": "user", "content": "hello"}]
        }))
        .unwrap();
        let chat = build_compact_chat_request(&body_missing, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert!(
            parsed.get("thinking").is_none(),
            "缺 model 字段时不应注入(model 字段在 chat body 也会缺,query 不到 wire)"
        );

        // model: null
        let body_null = serde_json::to_vec(&json!({
            "model": null,
            "input": [{"type": "message", "role": "user", "content": "hello"}]
        }))
        .unwrap();
        let chat = build_compact_chat_request(&body_null, &p).unwrap();
        let parsed: Value = serde_json::from_slice(&chat).unwrap();
        assert!(parsed.get("thinking").is_none(), "model:null 时不应注入");
    }

    // ── #262: compact prompt i18n tests ──────────────────────────────

    /// Devin BUG-003 fix:跟 [`crate::core::language::TEST_I18N_LOCK`] 共用同一把
    /// 锁,跨模块 serialize 同一全局 `USER_LANGUAGE`。原版每模块独立 mutex 无法
    /// serialize cargo test 跨模块的并发,会 race。
    use crate::core::language::TEST_I18N_LOCK as LANG_TEST_LOCK;

    fn with_user_language<F: FnOnce()>(lang: &str, f: F) {
        let _guard = LANG_TEST_LOCK.lock().unwrap();
        crate::core::language::set_user_language(lang);
        f();
        crate::core::language::set_user_language("en");
    }

    fn compact_prompt_text_for_lang(lang: &str) -> String {
        let mut out = String::new();
        with_user_language(lang, || {
            out = compact_summarization_prompt_for_current_language().to_string();
        });
        out
    }

    #[test]
    fn compact_summarization_prompt_english_by_default() {
        let prompt = compact_prompt_text_for_lang("en");
        assert!(prompt.contains("CONTEXT CHECKPOINT COMPACTION"));
        assert!(prompt.contains("Be concise, structured"));
        assert!(!prompt.contains("精简、结构化"));
    }

    #[test]
    fn compact_summarization_prompt_chinese_when_language_zh() {
        let prompt = compact_prompt_text_for_lang("zh-CN");
        assert!(prompt.contains("CONTEXT CHECKPOINT COMPACTION(上下文检查点压缩)"));
        assert!(prompt.contains("精简、结构化"));
        // 关键技术词保英文 — LLM / Next Step / context 等
        for keyword in &["LLM", "Next Step", "context"] {
            assert!(
                prompt.contains(keyword),
                "ZH compact prompt must keep keyword `{keyword}` in English"
            );
        }
        // emphasis 翻译完整
        assert!(prompt.contains("**逐字引用**"));
        assert!(prompt.contains("**截至目前的所有 user message"));
    }

    /// `COMPACT_SUMMARY_PREFIX` 必须保字面英文(Codex CLI startswith 识别) —
    /// 这条 const 不该被任何 i18n 路径覆盖。防回归。
    #[test]
    fn compact_summary_prefix_stays_english_regardless_of_user_language() {
        with_user_language("zh-CN", || {
            assert!(COMPACT_SUMMARY_PREFIX.starts_with("Another language model"));
        });
        with_user_language("en", || {
            assert!(COMPACT_SUMMARY_PREFIX.starts_with("Another language model"));
        });
    }

    // ─── [MOC-198] remote compaction v2 ───────────────────────────────

    #[test]
    fn detect_compact_v1_by_path() {
        assert_eq!(
            detect_compact("/responses/compact", b"{}"),
            Some(CompactKind::V1)
        );
        assert_eq!(
            detect_compact("/v1/responses/compact", b"{}"),
            Some(CompactKind::V1)
        );
    }

    #[test]
    fn detect_compact_v2_by_trigger_item() {
        let body = br#"{"model":"gpt-5.5","stream":true,"input":[
            {"type":"message","role":"user","content":"hi"},
            {"type":"compaction_trigger"}
        ]}"#;
        assert_eq!(detect_compact("/responses", body), Some(CompactKind::V2));
        assert_eq!(detect_compact("/v1/responses", body), Some(CompactKind::V2));
    }

    #[test]
    fn detect_compact_ignores_trigger_word_inside_message_content() {
        // 历史消息文本里恰好出现 "compaction_trigger" 字样 → 字节快筛命中但
        // parse 确认它不是 input item type → 不得误判 V2
        let body = br#"{"model":"m","input":[
            {"type":"message","role":"user","content":"what is \"compaction_trigger\"?"}
        ]}"#;
        assert_eq!(detect_compact("/responses", body), None);
    }

    #[test]
    fn detect_compact_none_for_plain_responses() {
        let body = br#"{"model":"m","input":[{"type":"message","role":"user","content":"hi"}]}"#;
        assert_eq!(detect_compact("/responses", body), None);
        // 非 responses 路由不参与 v2 检测
        assert_eq!(
            detect_compact(
                "/chat/completions",
                br#"{"input":[{"type":"compaction_trigger"}]}"#
            ),
            None
        );
    }

    #[test]
    fn strip_compaction_trigger_keeps_other_items() {
        let body = br#"{"model":"m","input":[
            {"type":"message","role":"user","content":"hi"},
            {"type":"compaction_trigger"},
            {"type":"function_call_output","call_id":"c1","output":"ok"}
        ]}"#;
        let stripped = strip_compaction_trigger(body).unwrap();
        let v: Value = serde_json::from_slice(&stripped).unwrap();
        let arr = v["input"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "只剥 trigger: {v}");
        assert_eq!(arr[0]["type"], "message");
        assert_eq!(arr[1]["type"], "function_call_output");
    }

    #[tokio::test]
    async fn compact_v2_plan_wraps_chat_upstream_into_sse_with_single_compaction_item() {
        let long_summary = format!(
            "## Summary\n{}",
            "context detail line with substance. ".repeat(60)
        );
        let upstream = serde_json::to_vec(&json!({
            "choices": [{"message": {"role": "assistant", "content": long_summary}}],
            "usage": {"prompt_tokens": 1200, "completion_tokens": 340}
        }))
        .unwrap();
        let plan = build_compact_v2_response_plan(
            StatusCode::OK,
            HeaderMap::new(),
            one_chunk_stream(upstream),
        )
        .unwrap();
        assert_eq!(plan.status, StatusCode::OK);
        assert_eq!(
            plan.headers.get(http::header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let body = collect_stream_bytes(plan.stream).await;
        let out = String::from_utf8(body).unwrap();
        assert!(out.contains("event: response.created"), "{out}");
        assert!(out.contains("event: response.output_item.done"), "{out}");
        assert!(out.contains("event: response.completed"), "{out}");
        assert_eq!(
            out.matches("\"type\":\"compaction\"").count(),
            2,
            "item.done + completed.output 各一份 compaction item: {out}"
        );
        assert!(out.contains("\"input_tokens\":1200"), "{out}");
        assert!(out.contains("\"output_tokens\":340"), "{out}");
        assert!(
            !out.contains("\"type\":\"message\""),
            "不得混入普通 item: {out}"
        );
        // sequence_number 单调递增 0,1,2(全仓 SSE 不变量,chatgpt-codex P1/devin)
        assert!(
            out.contains("\"sequence_number\":0"),
            "created seq=0: {out}"
        );
        assert!(
            out.contains("\"sequence_number\":1"),
            "output_item.done seq=1: {out}"
        );
        assert!(
            out.contains("\"sequence_number\":2"),
            "completed seq=2: {out}"
        );
    }

    #[tokio::test]
    async fn compact_v2_plan_emits_failed_event_on_quality_failure() {
        // 过短 summary → 质量校验失败 → response.failed(invalid_prompt,永久,
        // Codex 回退 inline compact),HTTP 仍 200
        let upstream = serde_json::to_vec(&json!({
            "choices": [{"message": {"role": "assistant", "content": "too short"}}]
        }))
        .unwrap();
        let plan = build_compact_v2_response_plan(
            StatusCode::OK,
            HeaderMap::new(),
            one_chunk_stream(upstream),
        )
        .unwrap();
        assert_eq!(plan.status, StatusCode::OK);
        let out = String::from_utf8(collect_stream_bytes(plan.stream).await).unwrap();
        assert!(out.contains("event: response.failed"), "{out}");
        assert!(out.contains("\"code\":\"invalid_prompt\""), "{out}");
        assert!(!out.contains("response.completed"), "{out}");
        // 失败流 seq:created=0, failed=1
        assert!(
            out.contains("\"sequence_number\":0"),
            "created seq=0: {out}"
        );
        assert!(out.contains("\"sequence_number\":1"), "failed seq=1: {out}");
    }

    #[tokio::test]
    async fn compact_v2_plan_maps_upstream_429_to_retryable_failed() {
        let plan = build_compact_v2_response_plan(
            StatusCode::TOO_MANY_REQUESTS,
            HeaderMap::new(),
            one_chunk_stream(b"{\"error\":\"slow down\"}".to_vec()),
        )
        .unwrap();
        assert_eq!(plan.status, StatusCode::OK, "对齐 MOC-103:错误也走 200+SSE");
        let out = String::from_utf8(collect_stream_bytes(plan.stream).await).unwrap();
        assert!(out.contains("event: response.failed"), "{out}");
        assert!(
            out.contains("\"code\":\"rate_limit_exceeded\""),
            "瞬时错误保留可重试语义: {out}"
        );
    }

    #[test]
    fn extract_summary_handles_anthropic_content_shape() {
        let parsed = json!({
            "content": [
                {"type": "text", "text": "part one. "},
                {"type": "text", "text": "part two."}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        assert_eq!(
            extract_compact_summary_text(&parsed).as_deref(),
            Some("part one. part two.")
        );
    }

    #[test]
    fn extract_usage_covers_three_wires() {
        let chat = json!({"usage": {"prompt_tokens": 7, "completion_tokens": 3}});
        assert_eq!(extract_compact_usage(&chat)["total_tokens"], 10);
        let gemini = json!({"usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 5}});
        assert_eq!(extract_compact_usage(&gemini)["input_tokens"], 20);
        let cloud_code = json!({"response": {"usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 2}}});
        assert_eq!(extract_compact_usage(&cloud_code)["total_tokens"], 10);
        let anthropic = json!({"usage": {"input_tokens": 4, "output_tokens": 6}});
        assert_eq!(extract_compact_usage(&anthropic)["output_tokens"], 6);
    }
}
