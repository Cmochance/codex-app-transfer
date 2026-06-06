//! 统一诊断流量 store(MOC-169)。
//!
//! forward-trace(以及后续 MCP-trace)的每条记录走同一个进程内 store,提供**三个 sink**:
//! 1. **ring**:`Mutex<VecDeque<Arc<TraceEntry>>>`(cap [`RING_CAP`]),超额淘汰最旧 —— 供
//!    独立端口 viewer 的 `GET /api/traces` 拉历史(后续增量)。
//! 2. **broadcast**:`tokio::sync::broadcast::Sender<Arc<TraceEntry>>`(cap [`BROADCAST_CAP`])
//!    —— 供 viewer 的 SSE `/api/stream` 实时推送(后续增量);无订阅者时 send 静默 no-op。
//! 3. **jsonl**:按天 append 到 `~/.codex-app-transfer/forward-trace/<YYYYMMDD>.jsonl`
//!    —— 离线 `jq`(沿用 MOC-89 的落盘格式、保留期、首写 trim)。
//!
//! `Arc<TraceEntry>` 让三个 sink 共享一份分配(body 可能很大)。**默认关**:调用方
//! (`diagnostics::write_forward_trace_jsonl`)先用 [`crate::diagnostics::forward_trace_enabled`]
//! gate,关时根本不会构造 entry、不会 push,故 store 零开销。

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::Local;
use codex_app_transfer_registry::config_dir;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::diagnostics::{trim_old_files, FORWARD_TRACE_KEEP_DAYS};

/// ring 最多保留的记录条数(超额淘汰最旧)。
const RING_CAP: usize = 500;
/// broadcast 通道容量(慢订阅者落后超此值会收到 `Lagged`,viewer 据此 resync)。
const BROADCAST_CAP: usize = 256;

/// 一条记录的类别。`value` 里也带 `trace_kind` 字符串(给 jsonl / 前端),这个 enum 是
/// Rust 侧的类型化判别,供 viewer 按类别过滤(forward / mcp / cat_webfetch)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    Forward,
    Mcp,
    /// cat-webfetch MCP 子进程内部链路 (MOC-181): 请求 / 抓取后端 + 升级链 / 选块 / 摘要
    /// prompt + 响应 / 返回 Codex 结果。子进程经 viewer `POST /api/ingest` 反向上报。
    CatWebfetch,
    /// chatgpt backend passthrough (MOC-125): relay 模式下 Codex 的账号/插件/wham/远程控制
    /// 请求经 proxy 透传 chatgpt.com,记 inbound/outbound/response(header 用 cookie 友好脱敏、
    /// 保留 set-cookie 的 Domain/Path 等属性)定位 enroll/server 404 死循环等会话连续性问题。
    ChatgptBackend,
}

/// store 里的一条记录。`value` 是**已脱敏**的完整 JSON 对象(forward-trace 即
/// `diagnostics::build_forward_trace_value` 的产物,含 `seq`/`trace_kind`/`inbound`/…),
/// 直接用于 jsonl 行 / SSE `data:` / `/api/traces` 响应。
#[derive(Debug, Clone, Serialize)]
pub struct TraceEntry {
    pub seq: u64,
    pub kind: TraceKind,
    /// 已脱敏的完整记录对象(序列化后即一条 jsonl 行)。
    pub value: Value,
}

/// 统一 trace store(ring + broadcast + jsonl)。
pub struct TraceStore {
    ring: Mutex<VecDeque<Arc<TraceEntry>>>,
    tx: broadcast::Sender<Arc<TraceEntry>>,
    /// 进程内是否已 trim 过旧 jsonl(只在首次落盘时 readdir 清理一次,避免每请求 readdir)。
    trimmed: AtomicBool,
}

impl TraceStore {
    fn new() -> Self {
        let (tx, _rx0) = broadcast::channel(BROADCAST_CAP);
        Self {
            ring: Mutex::new(VecDeque::with_capacity(RING_CAP)),
            tx,
            trimmed: AtomicBool::new(false),
        }
    }

    /// 推入一条记录:写 ring(超额淘汰最旧)→ broadcast(无订阅者静默忽略)→ append jsonl。
    /// 返回 jsonl 文件路径(写盘失败 / 无 home 返 `None`)—— 供调用方判定「开了诊断却写不出」
    /// 并 WARN。ring / broadcast 是 best-effort,不影响返回值。
    pub fn push(&self, kind: TraceKind, seq: u64, value: Value) -> Option<PathBuf> {
        let entry = Arc::new(TraceEntry { seq, kind, value });

        if let Ok(mut ring) = self.ring.lock() {
            while ring.len() >= RING_CAP {
                ring.pop_front();
            }
            ring.push_back(Arc::clone(&entry));
        }

        // 无订阅者时 send 返回 Err(0 receivers),静默忽略 —— 默认没人订阅。
        let _ = self.tx.send(Arc::clone(&entry));

        self.append_jsonl(&entry)
    }

    /// 订阅实时流(viewer SSE 用)。慢订阅者落后超 [`BROADCAST_CAP`] 会收到 `Lagged(n)`。
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<TraceEntry>> {
        self.tx.subscribe()
    }

    /// ring 历史快照(oldest→newest),最多 `limit` 条(viewer `GET /api/traces` 用)。
    pub fn recent(&self, limit: usize) -> Vec<Arc<TraceEntry>> {
        let Ok(ring) = self.ring.lock() else {
            return Vec::new();
        };
        let skip = ring.len().saturating_sub(limit);
        ring.iter().skip(skip).cloned().collect()
    }

    /// 清空 ring(viewer `POST /api/clear` 用)。不动已落盘的 jsonl。
    pub fn clear(&self) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.clear();
        }
    }

    /// append 一行 jsonl 到当天文件;首次落盘顺带 trim 超 [`FORWARD_TRACE_KEEP_DAYS`] 天的旧文件。
    fn append_jsonl(&self, entry: &TraceEntry) -> Option<PathBuf> {
        let dir = config_dir()?.join("forward-trace");
        fs::create_dir_all(&dir).ok()?;
        if !self.trimmed.swap(true, Ordering::Relaxed) {
            trim_old_files(&dir, FORWARD_TRACE_KEEP_DAYS, "jsonl");
        }
        // 每次取当天文件名(不缓存 → 进程跨天自动分文件)。
        let path = dir.join(format!("{}.jsonl", Local::now().format("%Y%m%d")));
        let mut line = serde_json::to_vec(&entry.value).ok()?;
        line.push(b'\n');
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_path())
            .ok()?;
        f.write_all(&line).ok()?;
        Some(path)
    }
}

/// 进程级全局 store(`OnceLock` 懒初始化)。
pub fn trace_store() -> &'static TraceStore {
    static STORE: OnceLock<TraceStore> = OnceLock::new();
    STORE.get_or_init(TraceStore::new)
}

/// 全 store **共享**的单调序号(forward-trace 与 MCP-trace 共用),保证 viewer 里所有记录
/// 的 `seq` 全局唯一——viewer 按 `seq` 做行主键,forward/mcp 各自计数会撞键选错行。
pub fn next_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry_value(seq: u64) -> Value {
        json!({"trace_kind": "forward_protocol", "seq": seq, "inbound": {"method": "POST"}})
    }

    #[test]
    fn ring_evicts_oldest_beyond_cap() {
        let store = TraceStore::new();
        for i in 0..(RING_CAP as u64 + 10) {
            // 不落盘断言这里(append_jsonl 依赖 config_dir),只验 ring 行为
            let entry = Arc::new(TraceEntry {
                seq: i,
                kind: TraceKind::Forward,
                value: entry_value(i),
            });
            let mut ring = store.ring.lock().unwrap();
            while ring.len() >= RING_CAP {
                ring.pop_front();
            }
            ring.push_back(entry);
        }
        let recent = store.recent(RING_CAP + 100);
        assert_eq!(recent.len(), RING_CAP, "ring 应被 cap 在 {RING_CAP}");
        // 最旧的应被淘汰:现存最小 seq = 总数 - RING_CAP = 10
        assert_eq!(recent.first().unwrap().seq, 10);
        assert_eq!(recent.last().unwrap().seq, RING_CAP as u64 + 9);
    }

    #[tokio::test]
    async fn subscriber_receives_pushed_entry() {
        let store = TraceStore::new();
        let mut rx = store.subscribe();
        // push 经 broadcast(jsonl 会因测试无写权限/无 home 影响返回值,但 broadcast 仍发)
        let _ = store.tx.send(Arc::new(TraceEntry {
            seq: 7,
            kind: TraceKind::Forward,
            value: entry_value(7),
        }));
        let got = rx.recv().await.expect("应收到广播");
        assert_eq!(got.seq, 7);
        assert_eq!(got.kind, TraceKind::Forward);
    }

    #[test]
    fn recent_returns_tail_in_order() {
        let store = TraceStore::new();
        {
            let mut ring = store.ring.lock().unwrap();
            for i in 0..5 {
                ring.push_back(Arc::new(TraceEntry {
                    seq: i,
                    kind: TraceKind::Forward,
                    value: entry_value(i),
                }));
            }
        }
        let last3 = store.recent(3);
        assert_eq!(
            last3.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        store.clear();
        assert!(store.recent(10).is_empty());
    }
}
