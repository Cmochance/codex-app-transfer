//! [MOC-234] responses 1:1 passthrough 的**只读会话观测镜像**。
//!
//! 原生 Responses 上游自管 `previous_response_id` session,wire 上续轮 `input` 只有增量
//! 当前轮。要给 context 面板算「完整上下文 by-source 明细」,proxy 必须自己镜像会话历史。
//! 本 store 按 turn 记录每轮的 Responses item(本轮 input + 本轮 output),用 `response_id`
//! → `prev_id` 链可重建任意 tip 之前的全历史。
//!
//! ## 严格只读 / 不接管(MOC-234 约束)
//! - **绝不回注请求**:仅供 [`crate::responses::compute_context_breakdown_responses`] 旁路
//!   计算 + session 观测读;转发字节一字不改。
//! - **独立于 chat 形 `ResponseSessionCache`**:那个存 chat messages、写入侧耦合
//!   tool_call_cache / artifact_store;本 store 存**原始 Responses item**,形状不同,
//!   混用会被 chat 路径 `build_messages_with_history` 读坏。
//! - 仅在 `breakdown_enabled()`(面板开)时由 mapper 写入,默认关零开销。
//!
//! 纯内存(无持久化):breakdown 结果本身已按 conv_id 落盘(复用 MOC-232),重启后新轮
//! 重建即可;会话镜像无需跨重启。TTL + 总上限防无界增长。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 跨所有对话的 turn 总上限(= 存的 `response_id` 数)。超限按最旧 inserted 顶出。
const MAX_TURNS: usize = 4096;
/// turn 记录 TTL:2h 没被触达即视为过期(陈旧会话,删了不影响——续轮会重建链头)。
const TTL: Duration = Duration::from_secs(2 * 3600);
/// 单次 `assemble_history` 沿 `prev_id` 链最多回溯的 turn 数(防异常超长链 / 环导致卡顿)。
const MAX_CHAIN_DEPTH: usize = 2048;

/// 一轮的观测记录:本轮拼进上下文的 Responses item(input + output)+ 上一轮 id。
struct TurnRecord {
    inserted: Instant,
    items: Vec<Value>,
    prev_id: Option<String>,
}

#[derive(Default)]
struct Inner {
    turns: HashMap<String, TurnRecord>,
}

/// 只读会话观测镜像(见模块 doc)。
pub struct PassthroughObserveStore {
    inner: Mutex<Inner>,
}

impl Default for PassthroughObserveStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PassthroughObserveStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// 记录一轮:`response_id` = 本轮上游分配的响应 id(链头),`items` = 本轮 input + output
    /// 的 Responses item,`prev_id` = 本轮请求的 `previous_response_id`(无则 None)。
    /// 空 `response_id` 直接丢(无法作链 key)。
    pub fn record_turn(&self, response_id: &str, prev_id: Option<String>, items: Vec<Value>) {
        if response_id.is_empty() {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return; // 锁中毒:观测是 best-effort,绝不 panic 影响转发
        };
        evict_expired(&mut inner);
        if inner.turns.len() >= MAX_TURNS && !inner.turns.contains_key(response_id) {
            evict_oldest(&mut inner);
        }
        inner.turns.insert(
            response_id.to_owned(),
            TurnRecord {
                inserted: Instant::now(),
                items,
                prev_id,
            },
        );
    }

    /// 沿 `tip_id` 的 `prev_id` 链回溯,收集**全历史** Responses item。
    ///
    /// 返回顺序为「最新轮 → 最旧轮」(breakdown 按类计 token,与顺序无关,故不额外反转)。
    /// 用 visited 集合防环;`MAX_CHAIN_DEPTH` 防异常超长链。链中缺环(如 proxy 重启后
    /// 半途接手的会话)即止,返回已收集部分(降级但不出错)。命中的 turn 会刷新其
    /// inserted 时间(LRU 保活:活跃会话的历史不被 TTL 误删)。
    pub fn assemble_history(&self, tip_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(mut inner) = self.inner.lock() else {
            return out;
        };
        let now = Instant::now();
        let mut visited: HashSet<String> = HashSet::new();
        let mut cursor = Some(tip_id.to_owned());
        let mut depth = 0;
        while let Some(id) = cursor {
            if depth >= MAX_CHAIN_DEPTH || !visited.insert(id.clone()) {
                break;
            }
            depth += 1;
            let Some(rec) = inner.turns.get_mut(&id) else {
                break;
            };
            rec.inserted = now; // 保活:活跃会话沿链命中的每轮都续期
            out.extend(rec.items.iter().cloned());
            cursor = rec.prev_id.clone();
        }
        out
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|i| i.turns.len()).unwrap_or(0)
    }
}

/// 删除 TTL 过期的 turn(在 record 前调,均摊清理)。
fn evict_expired(inner: &mut Inner) {
    let now = Instant::now();
    inner
        .turns
        .retain(|_, rec| now.duration_since(rec.inserted) < TTL);
}

/// 顶出最旧 inserted 的一条(到达 MAX_TURNS 时调)。
fn evict_oldest(inner: &mut Inner) {
    if let Some(oldest) = inner
        .turns
        .iter()
        .min_by_key(|(_, rec)| rec.inserted)
        .map(|(k, _)| k.clone())
    {
        inner.turns.remove(&oldest);
    }
}

/// 进程级全局观测镜像(mapper 写、breakdown assemble 读)。
pub fn global_passthrough_observe_store() -> &'static PassthroughObserveStore {
    static STORE: OnceLock<PassthroughObserveStore> = OnceLock::new();
    STORE.get_or_init(PassthroughObserveStore::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(text: &str) -> Value {
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})
    }

    #[test]
    fn assemble_walks_prev_id_chain_full_history() {
        let s = PassthroughObserveStore::new();
        // turn1: 无 prev;turn2 → turn1;turn3 → turn2
        s.record_turn("r1", None, vec![item("t1in"), item("t1out")]);
        s.record_turn("r2", Some("r1".into()), vec![item("t2in"), item("t2out")]);
        s.record_turn("r3", Some("r2".into()), vec![item("t3in")]);

        let hist = s.assemble_history("r3");
        // 3 轮共 2+2+1 = 5 个 item
        assert_eq!(hist.len(), 5, "应沿链拼出全历史 5 个 item");
    }

    #[test]
    fn assemble_missing_tip_returns_empty() {
        let s = PassthroughObserveStore::new();
        assert!(s.assemble_history("nope").is_empty());
    }

    #[test]
    fn assemble_broken_chain_returns_collected_prefix() {
        // r2 → r1,但 r1 不在 store(重启后半途接手)→ 只收到 r2 自己的 item
        let s = PassthroughObserveStore::new();
        s.record_turn("r2", Some("r1".into()), vec![item("t2in"), item("t2out")]);
        assert_eq!(s.assemble_history("r2").len(), 2);
    }

    #[test]
    fn assemble_tolerates_cycle() {
        // 异常:r1 ↔ r2 互指,visited 防环必须能终止
        let s = PassthroughObserveStore::new();
        s.record_turn("r1", Some("r2".into()), vec![item("a")]);
        s.record_turn("r2", Some("r1".into()), vec![item("b")]);
        let hist = s.assemble_history("r1");
        assert_eq!(hist.len(), 2, "环也只各收一次");
    }

    #[test]
    fn empty_response_id_is_dropped() {
        let s = PassthroughObserveStore::new();
        s.record_turn("", None, vec![item("x")]);
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn records_independent_turns_without_collision() {
        let s = PassthroughObserveStore::new();
        for i in 0..10 {
            s.record_turn(&format!("r{i}"), None, vec![item("x")]);
        }
        assert_eq!(s.len(), 10);
    }

    #[test]
    fn cap_evicts_when_over_max_turns() {
        // 真正灌过 MAX_TURNS,验证 evict_oldest 把总量封顶(reviewer:原测试只插 10 条、
        // 没触达 cap,名不副实)。
        let s = PassthroughObserveStore::new();
        for i in 0..(super::MAX_TURNS + 5) {
            s.record_turn(&format!("r{i}"), None, vec![item("x")]);
        }
        assert_eq!(
            s.len(),
            super::MAX_TURNS,
            "超过 MAX_TURNS 必须顶出最旧、总量封顶"
        );
    }
}
