//! Codex App Transfer · Provider 协议适配层(Stage 3).
//!
//! 设计目标:
//! - 让 `crates/proxy` 在转发前/后,把入站协议与上游 provider 协议互转
//! - 每种 `apiFormat`(`openai_chat` / `responses` / 未来更多)对应一个
//!   `Adapter` 实现,通过 `AdapterRegistry::lookup` 按 provider 配置选用
//! - **本轮(Stage 3.1)**只交付 `OpenAiChatAdapter`(覆盖现有 5 家用户
//!   provider 的 100%),Responses API ↔ Chat 互转留 Stage 3.2/3.3
//!
//! 流式语义:`transform_response_stream` 接收上游字节流,返回客户端字节流。
//! 对于 passthrough 适配器(本轮的 openai_chat),返回值就是入参,实现
//! 为 0 复制 / 0 缓冲。Stage 3.2 起的 SSE 状态机适配器会重写这条流。

pub mod openai_chat;
pub mod registry;
pub mod responses;
pub mod types;

pub use openai_chat::OpenAiChatAdapter;
pub use registry::AdapterRegistry;
pub use responses::{
    convert_chat_to_responses_stream, responses_body_to_chat_body,
    responses_body_to_chat_body_for_provider, ChatToResponsesConverter, ResponsesAdapter,
};
pub use types::{Adapter, AdapterError, ByteStream, RequestPlan, ResponsePlan};

/// 把"丢弃某个未知 Responses 工具 type"的告警在整个进程内**每个 type 只 warn 一次**。
///
/// Codex CLI 多轮对话每轮都会重发完整 tools 列表,普通 `tracing::warn!` 会让相同
/// 警告每轮触发一次,30 分钟攒几十行重复 warn,真有问题时埋没在噪音里。借鉴
/// `7as0nch/mimo2codex` `reqToChat.ts:158-172` 的 `warnOnce` 思路:全局
/// `HashSet` 记录已 warn 过的 type,后续静默 drop。
///
/// 进程重启后重置(也就是想要的行为 — 重启可能跟版本升级 / 新 Codex CLI 行为
/// 相关,值得再看一次)。
pub fn warn_once_drop_tool(tool_type: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut guard) = seen.lock() else {
        // poisoned mutex 在生产 unlikely;直接 warn 不重 dedup,避免 panic
        tracing::warn!(tool_type = %tool_type, "dropping unsupported responses tool type");
        return;
    };
    if guard.insert(tool_type.to_owned()) {
        tracing::warn!(tool_type = %tool_type, "dropping unsupported responses tool type");
    }
}
