//! Codex App Transfer · Provider 协议适配层(Stage 3).
//!
//! 设计目标:
//! - 让 `crates/proxy` 在转发前/后,把入站协议与上游 provider 协议互转
//! - 每种 `apiFormat`(`openai_chat` / `responses` / 未来更多)对应一个
//!   `Adapter` 实现,通过 `AdapterRegistry::lookup` 按 provider 配置选用
//! - 当前内置 `OpenAiChatAdapter` 和 `ResponsesAdapter`:前者规范化
//!   OpenAI Chat 路径,后者完成 Responses API ↔ Chat Completions 的请求
//!   body 与流式响应转换。
//!
//! 流式语义:`transform_response_stream` 接收上游字节流,返回客户端字节流。
//! 对于 passthrough 适配器(`openai_chat`),返回值就是入参,实现为 0 复制 /
//! 0 缓冲。`responses` 适配器会重写 SSE 流。

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
