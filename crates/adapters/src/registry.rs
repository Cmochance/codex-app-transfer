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

    /// Selects the adapter for a local Codex request.
    ///
    /// The provider's `apiFormat` describes the upstream protocol, while Codex
    /// still enters this proxy through local Responses routes. v1.x handled
    /// `/responses` locally first, then converted to the provider protocol. Keep
    /// that routing rule here so OpenAI Chat providers do not receive
    /// `/responses` directly.
    pub fn lookup_for_request(&self, api_format: &str, client_path: &str) -> Arc<dyn Adapter> {
        let normalized = api_format.trim().to_ascii_lowercase().replace('-', "_");
        if matches!(
            normalized.as_str(),
            "openai" | "openai_chat" | "chat_completions"
        ) && is_local_responses_route(client_path)
        {
            return self.responses.clone();
        }
        self.lookup(api_format)
    }
}

pub fn is_local_responses_route(client_path: &str) -> bool {
    let path = client_path.split('?').next().unwrap_or(client_path);
    let normalized = normalize_local_responses_path(path);
    let normalized = normalized.as_str();
    // `/responses` / `/messages` 是 OpenAI Codex CLI 本地入站路径;
    // `/responses/compact` 等 `/responses/*` 子路径(以及 `/messages/*`)
    // 是 OpenAI Responses API 的私有扩展,第三方 provider 都不实现,**必须**
    // 走 ResponsesAdapter 在本地处理而不是透传到 openai_chat 上游(否则
    // OpenaiChatAdapter 会把 `/responses/compact` 直接转给 chat-completions
    // 上游 base_url,触发 404)。
    normalized == "/responses"
        || normalized.starts_with("/responses/")
        || normalized == "/messages"
        || normalized.starts_with("/messages/")
}

fn normalize_local_responses_path(path: &str) -> String {
    let path = path.strip_prefix("/openai").unwrap_or(path);
    if path == "/claude/v1/messages" {
        return "/messages".to_owned();
    }
    if let Some(stripped) = path.strip_prefix("/v1") {
        return if stripped.is_empty() {
            "/".to_owned()
        } else {
            stripped.to_owned()
        };
    }
    path.to_owned()
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

    #[test]
    fn openai_chat_local_responses_routes_use_responses_adapter() {
        let r = AdapterRegistry::with_builtins();
        for path in [
            "/responses",
            "/responses?stream=1",
            "/v1/responses",
            "/openai/v1/responses",
            "/v1/messages",
            "/claude/v1/messages",
        ] {
            assert_eq!(
                r.lookup_for_request("openai_chat", path).name(),
                "responses",
                "{path} must be treated as a local Codex Responses route"
            );
        }
        assert_eq!(
            r.lookup_for_request("openai_chat", "/v1/chat/completions")
                .name(),
            "openai_chat"
        );
    }

    #[test]
    fn responses_compact_subpath_routes_to_responses_adapter() {
        // 关键回归 (2026-05-07):/responses/compact 必须命中 ResponsesAdapter,
        // 让它在本地实现 compact 端点,而不是被 OpenaiChatAdapter 直接透传到
        // 上游 chat-completions provider(那一定 404,因为这是 OpenAI 私有
        // 扩展,第三方都没实现)。
        let r = AdapterRegistry::with_builtins();
        for path in [
            "/responses/compact",
            "/responses/compact?foo=1",
            "/v1/responses/compact",
            "/openai/v1/responses/compact",
        ] {
            assert_eq!(
                r.lookup_for_request("openai_chat", path).name(),
                "responses",
                "{path} 必须走 ResponsesAdapter 本地处理(不能透传成 OpenaiChat)"
            );
        }
    }

    #[test]
    fn responses_routes_match_does_not_trigger_on_unrelated_prefixes() {
        // 防回归:`/responses_alt`、`/responsesfake` 不应被误判成 local 路由
        let r = AdapterRegistry::with_builtins();
        for path in ["/responses_alt", "/v1/responsesfake", "/v1/messagessuffix"] {
            assert_eq!(
                r.lookup_for_request("openai_chat", path).name(),
                "openai_chat",
                "{path} 不应误判为 Codex 本地 Responses/Messages 路由"
            );
        }
    }
}
