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

pub mod anthropic_messages;
/// `core` 内部模块,**仅** `language` 子模块对外暴露(#262 让 src-tauri 同步
/// user 语言到 adapters 全局)。其它子模块 (`events` / `input` / `routes`)
/// 仍是 crate-private — language 的 pub-mod 表现在 [`core::language`] level。
pub mod core;
pub mod gemini_cli;
pub mod gemini_native;
pub mod grok_web;
pub(crate) mod mapper;
pub mod openai_chat;
pub mod passthrough;
pub mod registry;
pub mod responses;
pub mod types;

pub use anthropic_messages::AnthropicMessagesAdapter;
pub use gemini_cli::GeminiCliAdapter;
pub use gemini_native::GeminiNativeAdapter;
pub use grok_web::GrokWebAdapter;
pub use openai_chat::OpenAiChatAdapter;
pub use passthrough::ResponsesPassthroughAdapter;
pub use registry::AdapterRegistry;
pub use responses::{
    convert_chat_to_responses_stream, responses_body_to_chat_body,
    responses_body_to_chat_body_for_provider, ChatToResponsesConverter, ResponsesAdapter,
};
pub use types::{Adapter, AdapterError, ByteStream, RequestPlan, ResponsePlan};

/// 同 tool_type 前 [`DROP_TOOL_WARN_LIMIT`] 次 warn,之后静默(但 counter 仍累加)。
///
/// **Why first-N (not first-1)**: MOC-32 PR-1 复盘 — 原 `HashSet` first-1 dedup
/// 让 Codex 0.130+ 引入的 `tool_search` drop **每 session 只 warn 一次**,bug
/// 长期被 noise / log rotation 掩盖,无人察觉 → MCP 工具暴露失败问题积压 1+ 月才
/// 被诊断出来。改 first-3 让 maintainer 有 3 次机会在 log scroll 时看到 + 配合
/// `dropped_tool_counters()` 累计 counter 让前端 UI 实时展示 drop 频率。
///
/// **Why 不无限 warn**: Codex CLI 多轮对话每轮重发完整 tools 列表,无限 warn
/// 30 分钟攒几十行,真有问题时仍埋没。first-N 是噪音 / 可见性折中。
///
/// 进程重启 counter 归零(重启常跟版本升级 / 新 Codex CLI 行为相关,值得再看)。
///
/// 借鉴 `7as0nch/mimo2codex` `reqToChat.ts:158-172` 的 `warnOnce` 思路,本次
/// 改造改成 first-N + counter(原方案纯 dedup)。
pub const DROP_TOOL_WARN_LIMIT: u32 = 3;

pub fn warn_once_drop_tool(tool_type: &str) {
    let counters = drop_tool_counters();
    let Ok(mut guard) = counters.lock() else {
        // poisoned mutex 在生产 unlikely;直接 warn 不重 dedup,避免 panic
        tracing::warn!(tool_type = %tool_type, "dropping unsupported responses tool type");
        return;
    };
    let count = guard.entry(tool_type.to_owned()).or_insert(0);
    *count += 1;
    let n = *count;
    if n <= DROP_TOOL_WARN_LIMIT {
        if n == DROP_TOOL_WARN_LIMIT {
            tracing::warn!(
                tool_type = %tool_type,
                warned_times = n,
                limit = DROP_TOOL_WARN_LIMIT,
                "dropping unsupported responses tool type (further drops will be silenced; use dropped_tool_counters() to inspect totals)",
            );
        } else {
            tracing::warn!(tool_type = %tool_type, warned_times = n, "dropping unsupported responses tool type");
        }
    }
}

/// `tool_type -> 累计 drop 次数` snapshot — 前端 UI / 反馈 bundle 可用此累计值
/// 检查是否有 silently dropped 工具(避免 MOC-32 类静默 bug 再藏 N 月)。
///
/// **不是 dedup 状态**(`warn_once_drop_tool` 内部用同一 HashMap,后续看 first-N
/// 是否还有名额),是 **总 drop 计数**(每次 `convert_responses_tool_to_chat_tool`
/// 命中 unknown type 都 +1,不受 warn 抑制影响)。
pub fn dropped_tool_counters_snapshot() -> std::collections::HashMap<String, u32> {
    drop_tool_counters()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

fn drop_tool_counters() -> &'static std::sync::Mutex<std::collections::HashMap<String, u32>> {
    static COUNTERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u32>>> =
        std::sync::OnceLock::new();
    COUNTERS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 测试用 reset hook — production 永远不调,仅 `#[cfg(test)]` caller 可见。
#[cfg(test)]
pub(crate) fn reset_dropped_tool_counters() {
    if let Ok(mut g) = drop_tool_counters().lock() {
        g.clear();
    }
}

#[cfg(test)]
mod warn_once_tests {
    //! `warn_once_drop_tool` first-N + counter 行为测试。
    //!
    //! 用 unique tool_type per test 避免并发跑时 global counter race
    //! (cargo test 默认 thread-pool 并发,共享 OnceLock counter)。
    use super::*;

    #[test]
    fn counter_increments_per_call() {
        let t = "test_a_counter_increments";
        let before = dropped_tool_counters_snapshot()
            .get(t)
            .copied()
            .unwrap_or(0);
        warn_once_drop_tool(t);
        warn_once_drop_tool(t);
        let snap = dropped_tool_counters_snapshot();
        assert_eq!(snap.get(t).copied(), Some(before + 2));
    }

    #[test]
    fn snapshot_clones_state_not_view() {
        let t = "test_b_snapshot_clone";
        warn_once_drop_tool(t);
        let s1 = dropped_tool_counters_snapshot();
        warn_once_drop_tool(t);
        let s2 = dropped_tool_counters_snapshot();
        // s1 应该是 snapshot 那一刻的拷贝,不被后续调用 mutate
        assert!(s1.get(t).copied().unwrap_or(0) < s2.get(t).copied().unwrap_or(0));
    }

    #[test]
    fn counter_distinguishes_types() {
        let t1 = "test_c_distinct_type_1";
        let t2 = "test_c_distinct_type_2";
        let b1 = dropped_tool_counters_snapshot()
            .get(t1)
            .copied()
            .unwrap_or(0);
        let b2 = dropped_tool_counters_snapshot()
            .get(t2)
            .copied()
            .unwrap_or(0);
        warn_once_drop_tool(t1);
        warn_once_drop_tool(t2);
        warn_once_drop_tool(t1);
        let snap = dropped_tool_counters_snapshot();
        assert_eq!(snap.get(t1).copied(), Some(b1 + 2));
        assert_eq!(snap.get(t2).copied(), Some(b2 + 1));
    }

    #[test]
    fn counter_keeps_incrementing_past_warn_limit() {
        // 验证 warn 静默后 counter 仍累加(防 MOC-32 类 silently藏 bug)
        let t = "test_d_past_limit";
        let before = dropped_tool_counters_snapshot()
            .get(t)
            .copied()
            .unwrap_or(0);
        for _ in 0..(DROP_TOOL_WARN_LIMIT + 5) {
            warn_once_drop_tool(t);
        }
        let snap = dropped_tool_counters_snapshot();
        assert_eq!(
            snap.get(t).copied(),
            Some(before + DROP_TOOL_WARN_LIMIT + 5)
        );
    }

    // 注:不测 `reset_dropped_tool_counters()` — 它会清掉 global counter,跟
    // 其他并发跑的 test race。reset 函数是 trivial 的 `g.clear()`,留作 future
    // test 想 isolated state 时用。
}

/// 本进程已自动禁用 web_search 的 provider id 集合 — 4xx fallback 路径调用
/// `disable_web_search_for(provider_id)` 加入,`convert_web_search_tool`
/// 调用 `is_web_search_disabled_for` 命中即 drop。
///
/// **设计语义**(对齐用户决策"A+B 双层"):
/// - **A**:Provider 配置 `request_options.web_search_enabled` 默认 false,
///   只有用户显式标 true 才会发 web_search 工具上去
/// - **B**:上游真的拒了(MiMo plugin 没开 / token plan 套餐不支持 / 其他)
///   后,proxy 自动加入此 cache,本进程后续 turn 立即 drop。下次启动
///   cache 重置(用户去 UI 关 web_search_enabled = false 才是持久关闭)。
///
/// **本提交不做**:① transparent retry without web_search(用户视角第一次
/// 请求失败,需要重新提问下一个 turn 才 work);② 写回 config.json 持久化
/// (应用重启后用户配置仍是 enabled=true,需要再失败一次)。这两项留 follow-up。
fn web_search_disabled_set() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

pub fn disable_web_search_for(provider_id: &str) {
    if let Ok(mut guard) = web_search_disabled_set().lock() {
        if guard.insert(provider_id.to_owned()) {
            tracing::warn!(
                provider_id = %provider_id,
                "auto-disabling web_search after upstream rejection (likely Web Search Plugin not activated upstream)"
            );
        }
    }
}

pub fn is_web_search_disabled_for(provider_id: &str) -> bool {
    web_search_disabled_set()
        .lock()
        .map(|s| s.contains(provider_id))
        .unwrap_or(false)
}

/// 把入站 `/v1/foo?bar` 规范化为 `/foo?bar`;若开头不是 `/v1/` 则原样返回。
///
/// 用于把 Codex CLI 入站 Responses/Chat 路径的 `/v1` 前缀剥离 —
/// `provider.base_url` 通常已带 `/v1`(如 `https://api.openai.com/v1`),不剥
/// 则会拼出 `…/v1/v1/...`(Stage 3.1 实测 OpenAI 兼容上游 404 / 405)。
///
/// `OpenAiChatAdapter` 与 `ResponsesPassthroughAdapter` 共用此规则。
pub(crate) fn normalize_v1_prefix(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    if let Some(stripped) = path.strip_prefix("/v1/") {
        format!("/{stripped}")
    } else if path == "/v1" {
        "/".to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod normalize_v1_prefix_tests {
    use super::normalize_v1_prefix;

    #[test]
    fn strips_v1_chat() {
        assert_eq!(
            normalize_v1_prefix("/v1/chat/completions"),
            "/chat/completions"
        );
    }

    #[test]
    fn strips_v1_responses() {
        assert_eq!(normalize_v1_prefix("/v1/responses"), "/responses");
    }

    #[test]
    fn preserves_query() {
        assert_eq!(
            normalize_v1_prefix("/v1/responses?stream=true"),
            "/responses?stream=true"
        );
    }

    #[test]
    fn passthrough_when_no_v1() {
        assert_eq!(normalize_v1_prefix("/responses"), "/responses");
    }

    #[test]
    fn lone_v1_becomes_root() {
        assert_eq!(normalize_v1_prefix("/v1"), "/");
    }

    #[test]
    fn empty_becomes_root() {
        assert_eq!(normalize_v1_prefix(""), "/");
    }
}
