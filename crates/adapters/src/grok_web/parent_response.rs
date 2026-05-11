//! `parentResponseId` DAG 多轮锚定 in-memory tracker。
//!
//! ## 协议背景
//!
//! grok.com Web 后端不接受 `messages: [...]` 数组,只接受单 `message` 字段。
//! 多轮上下文走 **DAG 锚定**:每次请求带 `parent_response_id`(上一轮 modelResponse
//! 的 UUID),后端自己拉历史。
//!
//! 实测(2026-05-11 SuperGrok)三轮 DAG 严格成立:
//!
//! ```text
//! prev_model → R1.user → R1.model → R2.user → R2.model → R3.user → R3.model
//!              b2fd...   9f82...    cbf7...   16f8...    f81c...   e501...
//! user.parentResponseId  = 上一轮 model.responseId(交替)
//! model.parentResponseId = 同轮 user.responseId
//! ```
//!
//! ## 本 tracker 职责
//!
//! Codex APP 通过 OpenAI Responses API 的 `previous_response_id` 表达多轮关系。
//! 本 tracker:
//!
//! - 接 Codex APP `previous_response_id`(我们自己发出的 Responses ID)
//! - 反查对应 grok.com modelResponse 的 `responseId`(grok 本地 UUID)
//! - 让下次请求把那个 UUID 当 `parent_response_id` 传给 grok.com
//!
//! ## 失败回退
//!
//! - tracker miss 时,**不传** `parent_response_id`(让 grok 开新会话,与首轮等价)
//! - 接受多轮信息"断片",比 hard fail 友好
//! - 持久化策略:R3 PoC 阶段全内存(进程重启即清),R1 后续可加磁盘备份
//!
//! ## 线程安全
//!
//! 内部用 `Mutex<HashMap>`,所有方法 `&self`,与 [`Adapter`] trait `Send + Sync` 兼容。
//! 选择 std `Mutex` 而非 `DashMap` 避免引入新 workspace 依赖;tracker 操作频率
//! 远低于 SSE 帧处理,锁竞争不是热点。
//!
//! [`Adapter`]: crate::types::Adapter

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// `(Codex Responses ID) → (grok.com responseId)` 反查表。
#[derive(Debug, Default)]
pub struct ParentResponseTracker {
    map: Mutex<HashMap<String, String>>,
}

impl ParentResponseTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录:Codex APP 暴露的 Responses ID → grok.com 后端的 modelResponse.responseId。
    pub fn record(
        &self,
        codex_response_id: impl Into<String>,
        grok_response_id: impl Into<String>,
    ) {
        if let Ok(mut g) = self.map.lock() {
            g.insert(codex_response_id.into(), grok_response_id.into());
        } else {
            // Mutex poisoned 在生产 unlikely;silently drop 比 panic 友好
            tracing::warn!("parent_response_tracker mutex poisoned; record dropped");
        }
    }

    /// 查询:给定 Codex APP `previous_response_id`,返回对应 grok responseId。
    ///
    /// 返回 `None` 时,请求构建方应**省略** `parent_response_id` 字段(开新会话语义)。
    pub fn get(&self, codex_response_id: &str) -> Option<String> {
        self.map.lock().ok()?.get(codex_response_id).cloned()
    }

    /// 容量(测试用)。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// 全局单例 tracker —— 进程级,所有 Provider 共用。
pub fn global_tracker() -> &'static ParentResponseTracker {
    static TRACKER: OnceLock<ParentResponseTracker> = OnceLock::new();
    TRACKER.get_or_init(ParentResponseTracker::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_get_roundtrip() {
        let t = ParentResponseTracker::new();
        t.record("resp_abc", "9f82a10c-grok-uuid");
        assert_eq!(t.get("resp_abc").as_deref(), Some("9f82a10c-grok-uuid"));
        assert_eq!(t.get("resp_unknown"), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn record_overwrites_existing_entry() {
        let t = ParentResponseTracker::new();
        t.record("resp_abc", "old-uuid");
        t.record("resp_abc", "new-uuid");
        assert_eq!(t.get("resp_abc").as_deref(), Some("new-uuid"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn global_tracker_is_singleton() {
        let a = global_tracker() as *const _;
        let b = global_tracker() as *const _;
        assert_eq!(a, b);
    }
}
