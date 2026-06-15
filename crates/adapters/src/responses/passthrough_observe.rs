//! [MOC-234] responses 1:1 passthrough 的**只读会话观测镜像**。
//!
//! 原生 Responses 上游自管 `previous_response_id` session,wire 上续轮 `input` 只有增量
//! 当前轮。要给 context 面板算「完整上下文 by-source 明细」,proxy 必须自己镜像会话历史。
//! 本 store 按 turn 记录每轮的 Responses item(本轮 input + 本轮 output),用 `response_id`
//! → `prev_id` 链可重建任意 tip 之前的全历史。
//!
//! ## 写入与读取(MOC-234)
//! - **写入 always-on**:由响应侧 tee 每轮记 input+output(不依赖 breakdown 面板)——
//!   既支撑 breakdown 拼全历史,也支撑 orphan-400 降级重建上下文(需要历史始终被记下)。
//!   仅 breakdown 的 o200k 逐 item 计算保持 gated(那是较重的一步)。
//! - **两类读取**:① [`crate::responses::compute_context_breakdown_responses`] 旁路只读
//!   计 token(不改转发);② [`crate::responses::rebuild_orphan_context_bytes`] 在上游报
//!   orphan-400 时**沿链重建完整上下文回注重发**——这是用户授权的 error-path 降级(偏离
//!   纯 1:1),仅该错误触发,成功路径与正常转发一律不改字节。
//! - **独立于 chat 形 `ResponseSessionCache`**:那个存 chat messages、写入侧耦合
//!   tool_call_cache / artifact_store;本 store 存**原始 Responses item**,形状不同,
//!   混用会被 chat 路径 `build_messages_with_history` 读坏。
//!
//! 纯内存(无持久化):会话镜像无需跨重启(重启后新轮重建链头;断链的旧历史拼不回属预期
//! 降级)。TTL + 总上限防无界增长。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 跨所有对话的 turn 总上限(= 存的 `response_id` 数)。超限按最旧 inserted 顶出。
const MAX_TURNS: usize = 4096;
/// turn 记录 TTL:2h 没被触达即视为过期(陈旧会话,删了不影响——续轮会重建链头)。
const TTL: Duration = Duration::from_secs(2 * 3600);
/// 单次 `assemble_chain` 沿 `prev_id` 链最多回溯的 turn 数(防异常超长链 / 环导致卡顿)。
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

    /// 沿 `tip_id` 的 `prev_id` 链回溯,拼出**时序完整历史**(最旧轮在前,轮内 item 顺序不变)。
    /// 供 breakdown 计 token(顺序无关、部分历史也可用)。**断链容忍**:链中缺环 / 环 / 超深即止,
    /// 返回已收集部分(降级但不出错)。需要「链是否完整」语义的 orphan 降级请用
    /// [`assemble_chain_complete`]。命中的 turn 刷新 inserted(LRU 保活)。
    pub fn assemble_chain(&self, tip_id: &str) -> Vec<Value> {
        self.walk_chain(tip_id).0
    }

    /// 沿链回溯并**要求链完整**:仅当回溯到真正链根(某轮 `prev_id=None`)且中途无缺环 / 无环 /
    /// 未超深时返回 `Some(时序完整历史)`,否则(tip 缺失 / 中途缺环 / 环 / 超深 → 拿到的只是
    /// 尾段不完整上下文)返回 `None`。供 orphan-400 降级:只有完整链才能安全 inline + 去
    /// `previous_response_id` 重发,否则带**缺失早期上下文**重试会让上游从错误任务续写(reviewer:
    /// 旧版只判 `is_empty`、把断链尾段当完整);拼不全则退回原 400 显示错误。
    pub fn assemble_chain_complete(&self, tip_id: &str) -> Option<Vec<Value>> {
        let (history, complete) = self.walk_chain(tip_id);
        if complete && !history.is_empty() {
            Some(history)
        } else {
            None
        }
    }

    /// 沿 `prev_id` 链回溯的公共实现,返回 `(时序完整历史, 是否回溯到真正链根)`。`complete=false`
    /// 表示中途 break(缺环 / 环 / 超深),拿到的是不完整尾段。命中的 turn 刷新 inserted(LRU 保活)。
    fn walk_chain(&self, tip_id: &str) -> (Vec<Value>, bool) {
        let Ok(mut inner) = self.inner.lock() else {
            return (Vec::new(), false);
        };
        let now = Instant::now();
        let mut visited: HashSet<String> = HashSet::new();
        let mut cursor = Some(tip_id.to_owned());
        let mut depth = 0;
        let mut complete = false;
        // 先按「最新轮 → 最旧轮」收集每轮 items,再整轮反转成时序。
        let mut turns_newest_first: Vec<Vec<Value>> = Vec::new();
        while let Some(id) = cursor {
            if depth >= MAX_CHAIN_DEPTH || !visited.insert(id.clone()) {
                break;
            }
            depth += 1;
            let Some(rec) = inner.turns.get_mut(&id) else {
                break;
            };
            rec.inserted = now; // 保活:活跃会话沿链命中的每轮都续期
            turns_newest_first.push(rec.items.clone());
            cursor = rec.prev_id.clone();
            // prev_id=None → 这轮是真正链根,链回溯完整(while 随之正常退出)。
            if cursor.is_none() {
                complete = true;
            }
        }
        let mut out = Vec::new();
        for turn_items in turns_newest_first.into_iter().rev() {
            out.extend(turn_items);
        }
        (out, complete)
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

        let hist = s.assemble_chain("r3");
        // 3 轮共 2+2+1 = 5 个 item
        assert_eq!(hist.len(), 5, "应沿链拼出全历史 5 个 item");
    }

    #[test]
    fn assemble_missing_tip_returns_empty() {
        let s = PassthroughObserveStore::new();
        assert!(s.assemble_chain("nope").is_empty());
    }

    #[test]
    fn assemble_broken_chain_returns_collected_prefix() {
        // r2 → r1,但 r1 不在 store(重启后半途接手)→ 只收到 r2 自己的 item
        let s = PassthroughObserveStore::new();
        s.record_turn("r2", Some("r1".into()), vec![item("t2in"), item("t2out")]);
        assert_eq!(s.assemble_chain("r2").len(), 2);
    }

    #[test]
    fn assemble_chain_complete_requires_full_chain_to_root() {
        let s = PassthroughObserveStore::new();
        // 完整链:r1(prev=None 真链根)← r2 → assemble_chain_complete 返 Some
        s.record_turn("r1", None, vec![item("t1")]);
        s.record_turn("r2", Some("r1".into()), vec![item("t2")]);
        assert!(
            s.assemble_chain_complete("r2").is_some(),
            "回溯到链根 → 完整"
        );
        // 断链:r9 的 prev=r8 不在 store(缺环)→ assemble_chain 仍返回尾段,但
        // assemble_chain_complete 必须 None(不能把不完整尾段当完整去 inline 重发)
        s.record_turn("r9", Some("r8".into()), vec![item("t9")]);
        assert_eq!(
            s.assemble_chain("r9").len(),
            1,
            "断链 assemble_chain 返回尾段"
        );
        assert!(
            s.assemble_chain_complete("r9").is_none(),
            "断链 → assemble_chain_complete None"
        );
        // tip 本身缺失 → None
        assert!(s.assemble_chain_complete("nope").is_none());
    }

    #[test]
    fn assemble_tolerates_cycle() {
        // 异常:r1 ↔ r2 互指,visited 防环必须能终止
        let s = PassthroughObserveStore::new();
        s.record_turn("r1", Some("r2".into()), vec![item("a")]);
        s.record_turn("r2", Some("r1".into()), vec![item("b")]);
        let hist = s.assemble_chain("r1");
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
