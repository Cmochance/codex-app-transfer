//! Tool call definition cache —— call_id → (name, arguments)。
//!
//! Codex CLI 多轮工具调用时把 `function_call_output` 用 `call_id` 关联回上一
//! 轮的 `function_call`,但当 history 被压缩 / 用户截断时,前一条 assistant
//! 可能已经不在 messages 里。Chat Completions 上游(Kimi / DeepSeek 实测)对
//! "孤儿 tool message"零容忍,会直接 400。
//!
//! 本缓存对应改造前 Python `session_cache.py::ToolCallCache`(内嵌在
//! ResponseSessionCache 模块里)+ 改造前 `responses_adapter.py::_repair_tool_call_ids`
//! 的 path B(查 cache → 重建 tool_call → 注回前 assistant / 插占位
//! assistant),也对齐 litellm 1.84.0 `transformation.py::
//! _ensure_tool_results_have_corresponding_tool_calls`。
//!
//! 写时机:Chat → Responses 流的 `converter.rs::close_tool_call`,工具调用
//! 闭合(收齐 name + arguments)时把 `(call_id, name, args)` 写入。
//!
//! 读时机:Responses → Chat 请求侧 `request.rs::repair_tool_call_ids`,遇到
//! "tool_call_id 非空但 messages 中找不到归属 assistant" 的孤儿 tool 时,从
//! cache 命中重建 → 注回前 assistant 或插占位 assistant。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ToolCallEntry {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug)]
struct CacheEntry {
    tool_call: ToolCallEntry,
    ts: Instant,
    access_count: u64,
}

#[derive(Debug)]
struct ToolCallCacheInner {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug)]
pub struct ToolCallCache {
    max_size: usize,
    ttl: Duration,
    inner: Mutex<ToolCallCacheInner>,
}

impl ToolCallCache {
    pub fn new(max_size: usize, ttl: Duration) -> Self {
        Self {
            max_size: max_size.max(1),
            ttl,
            inner: Mutex::new(ToolCallCacheInner {
                entries: HashMap::new(),
            }),
        }
    }

    pub fn save(&self, call_id: &str, tool_call: ToolCallEntry) {
        if call_id.trim().is_empty() {
            return;
        }
        let mut inner = self.inner.lock().expect("tool call cache mutex poisoned");
        self.evict_expired_locked(&mut inner);
        if inner.entries.len() >= self.max_size && !inner.entries.contains_key(call_id) {
            self.evict_oldest_locked(&mut inner);
        }
        inner.entries.insert(
            call_id.to_owned(),
            CacheEntry {
                tool_call,
                ts: Instant::now(),
                access_count: 0,
            },
        );
    }

    pub fn get(&self, call_id: &str) -> Option<ToolCallEntry> {
        if call_id.trim().is_empty() {
            return None;
        }
        let mut inner = self.inner.lock().expect("tool call cache mutex poisoned");
        let expired = inner
            .entries
            .get(call_id)
            .map(|entry| entry.ts.elapsed() > self.ttl)
            .unwrap_or(false);
        if expired {
            inner.entries.remove(call_id);
            return None;
        }
        let entry = inner.entries.get_mut(call_id)?;
        entry.access_count += 1;
        Some(entry.tool_call.clone())
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("tool call cache mutex poisoned")
            .entries
            .clear();
    }

    fn evict_expired_locked(&self, inner: &mut ToolCallCacheInner) {
        let ttl = self.ttl;
        inner.entries.retain(|_, entry| entry.ts.elapsed() <= ttl);
    }

    fn evict_oldest_locked(&self, inner: &mut ToolCallCacheInner) {
        let Some(oldest_key) = inner
            .entries
            .iter()
            .min_by_key(|(_, entry)| (entry.access_count, entry.ts))
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        inner.entries.remove(&oldest_key);
    }
}

pub fn global_tool_call_cache() -> &'static ToolCallCache {
    static CACHE: OnceLock<ToolCallCache> = OnceLock::new();
    CACHE.get_or_init(|| ToolCallCache::new(1000, Duration::from_secs(3600)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_get_round_trip() {
        let cache = ToolCallCache::new(8, Duration::from_secs(60));
        cache.save(
            "call_a",
            ToolCallEntry {
                name: "search".into(),
                arguments: r#"{"q":"foo"}"#.into(),
            },
        );
        let entry = cache.get("call_a").expect("cache should hit");
        assert_eq!(entry.name, "search");
        assert_eq!(entry.arguments, r#"{"q":"foo"}"#);
    }

    #[test]
    fn empty_call_id_is_ignored_on_save_and_get() {
        let cache = ToolCallCache::new(8, Duration::from_secs(60));
        cache.save(
            "",
            ToolCallEntry {
                name: "noop".into(),
                arguments: String::new(),
            },
        );
        assert!(cache.get("").is_none());
    }

    #[test]
    fn lru_eviction_drops_least_used_oldest() {
        let cache = ToolCallCache::new(2, Duration::from_secs(60));
        cache.save(
            "call_1",
            ToolCallEntry {
                name: "a".into(),
                arguments: "{}".into(),
            },
        );
        cache.save(
            "call_2",
            ToolCallEntry {
                name: "b".into(),
                arguments: "{}".into(),
            },
        );
        // 给 call_2 提访问计数
        let _ = cache.get("call_2");
        // 插第三条触发淘汰,call_1(0 访问)被踢
        cache.save(
            "call_3",
            ToolCallEntry {
                name: "c".into(),
                arguments: "{}".into(),
            },
        );
        assert!(cache.get("call_1").is_none());
        assert!(cache.get("call_2").is_some());
        assert!(cache.get("call_3").is_some());
    }

    #[test]
    fn ttl_expired_entry_is_purged_on_get() {
        let cache = ToolCallCache::new(8, Duration::from_millis(1));
        cache.save(
            "call_x",
            ToolCallEntry {
                name: "search".into(),
                arguments: "{}".into(),
            },
        );
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get("call_x").is_none());
    }
}
