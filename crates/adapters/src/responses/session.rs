//! Responses API conversation session cache.
//!
//! This is the Rust port of the old Python `ResponseSessionCache`: an in-memory,
//! TTL-bound cache used to restore Chat Completions history when Codex sends a
//! `previous_response_id` on the next Responses API request.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Debug, Clone)]
struct SessionEntry {
    messages: Vec<Value>,
    ts: Instant,
    access_count: u64,
}

#[derive(Debug)]
struct SessionCacheInner {
    entries: HashMap<String, SessionEntry>,
}

#[derive(Debug)]
pub struct ResponseSessionCache {
    max_size: usize,
    ttl: Duration,
    inner: Mutex<SessionCacheInner>,
}

impl ResponseSessionCache {
    pub fn new(max_size: usize, ttl: Duration) -> Self {
        Self {
            max_size: max_size.max(1),
            ttl,
            inner: Mutex::new(SessionCacheInner {
                entries: HashMap::new(),
            }),
        }
    }

    pub fn save(&self, response_id: &str, messages: Vec<Value>) {
        if response_id.trim().is_empty() {
            return;
        }

        let mut inner = self.inner.lock().expect("session cache mutex poisoned");
        self.evict_expired_locked(&mut inner);
        if inner.entries.len() >= self.max_size && !inner.entries.contains_key(response_id) {
            self.evict_oldest_locked(&mut inner);
        }
        inner.entries.insert(
            response_id.to_owned(),
            SessionEntry {
                messages,
                ts: Instant::now(),
                access_count: 0,
            },
        );
    }

    pub fn get(&self, response_id: &str) -> Option<Vec<Value>> {
        if response_id.trim().is_empty() {
            return None;
        }

        let mut inner = self.inner.lock().expect("session cache mutex poisoned");
        let expired = inner
            .entries
            .get(response_id)
            .map(|entry| entry.ts.elapsed() > self.ttl)
            .unwrap_or(false);
        if expired {
            inner.entries.remove(response_id);
            return None;
        }
        let entry = inner.entries.get_mut(response_id)?;
        entry.access_count += 1;
        Some(entry.messages.clone())
    }

    pub fn build_messages_with_history(
        &self,
        previous_response_id: &str,
        current_messages: &[Value],
    ) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(history) = self.get(previous_response_id) {
            out.extend(history);
        }
        out.extend(current_messages.iter().cloned());
        out
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("session cache mutex poisoned")
            .entries
            .clear();
    }

    fn evict_expired_locked(&self, inner: &mut SessionCacheInner) {
        let ttl = self.ttl;
        inner.entries.retain(|_, entry| entry.ts.elapsed() <= ttl);
    }

    fn evict_oldest_locked(&self, inner: &mut SessionCacheInner) {
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

pub fn global_response_session_cache() -> &'static ResponseSessionCache {
    static CACHE: OnceLock<ResponseSessionCache> = OnceLock::new();
    CACHE.get_or_init(|| ResponseSessionCache::new(1000, Duration::from_secs(3600)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_restores_history_before_current_messages() {
        let cache = ResponseSessionCache::new(2, Duration::from_secs(60));
        cache.save(
            "resp_1",
            vec![
                json!({"role": "user", "content": "first"}),
                json!({"role": "assistant", "content": "answer"}),
            ],
        );

        let merged = cache
            .build_messages_with_history("resp_1", &[json!({"role": "user", "content": "next"})]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["content"], "first");
        assert_eq!(merged[2]["content"], "next");
    }

    #[test]
    fn cache_evicts_least_used_oldest_entry() {
        let cache = ResponseSessionCache::new(2, Duration::from_secs(60));
        cache.save("resp_1", vec![json!({"role": "user", "content": "one"})]);
        cache.save("resp_2", vec![json!({"role": "user", "content": "two"})]);
        assert!(cache.get("resp_2").is_some());
        cache.save("resp_3", vec![json!({"role": "user", "content": "three"})]);

        assert!(cache.get("resp_1").is_none());
        assert!(cache.get("resp_2").is_some());
        assert!(cache.get("resp_3").is_some());
    }
}
