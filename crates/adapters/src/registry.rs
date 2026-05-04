//! AdapterRegistry —— 按 `provider.api_format` 字符串查找 adapter 实例.
//!
//! 当前内置:
//! - `openai_chat` → `OpenAiChatAdapter`
//!
//! Stage 3.2 起会注册 `responses` adapter,以及为部分 provider 注册其
//! 专用 workaround adapter(如 deepseek 的 reasoning_content 字段处理)。

use std::sync::Arc;

use crate::openai_chat::OpenAiChatAdapter;
use crate::responses::ResponsesAdapter;
use crate::types::Adapter;

#[derive(Clone)]
pub struct AdapterRegistry {
    openai_chat: Arc<dyn Adapter>,
    responses: Arc<dyn Adapter>,
}

impl AdapterRegistry {
    pub fn with_builtins() -> Self {
        Self {
            openai_chat: Arc::new(OpenAiChatAdapter),
            responses: Arc::new(ResponsesAdapter),
        }
    }

    /// 按 `apiFormat` 字符串(已小写化)查 adapter。
    /// 与 `backend/api_adapters.py::normalize_api_format` 行为对齐:
    /// - `openai` / `openai_chat` / `chat_completions` → openai_chat
    /// - `responses` / `openai_responses` → responses
    /// - **`anthropic` / `claude` / `messages`**:Python 历史配置兼容值,在源
    ///   码里被归一为 `responses`(并非 Anthropic Messages 协议入站,详见
    ///   docs/migration-plan.md 修订日志 2026-05-04 关于此项的说明)
    /// - 未知值 fallback 到 `responses`(与 Python 默认 `responses` 一致)
    pub fn lookup(&self, api_format: &str) -> Arc<dyn Adapter> {
        let normalized = api_format.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "openai" | "openai_chat" | "chat_completions" => self.openai_chat.clone(),
            "responses" | "openai_responses" | "anthropic" | "claude" | "messages" => {
                self.responses.clone()
            }
            "" => self.responses.clone(), // Python 默认值
            _ => self.responses.clone(),
        }
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_openai_chat_aliases() {
        let r = AdapterRegistry::with_builtins();
        for v in ["openai", "openai_chat", "Chat-Completions", "OPENAI_CHAT"] {
            assert_eq!(
                r.lookup(v).name(),
                "openai_chat",
                "alias {v} 应解析到 openai_chat"
            );
        }
    }

    #[test]
    fn lookup_responses_aliases() {
        let r = AdapterRegistry::with_builtins();
        for v in [
            "responses",
            "openai_responses",
            "Openai-Responses",
            "anthropic",
            "claude",
            "messages",
        ] {
            assert_eq!(r.lookup(v).name(), "responses", "{v} 应解析到 responses");
        }
    }

    #[test]
    fn lookup_empty_or_unknown_falls_back_to_responses() {
        let r = AdapterRegistry::with_builtins();
        assert_eq!(r.lookup("").name(), "responses");
        assert_eq!(r.lookup("unknown_format").name(), "responses");
    }
}
