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
