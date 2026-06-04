//! Responses API conversation session cache —— **双层**(2026-05-08+):
//!
//! - **L1 in-memory hot cache**(`HashMap` + LRU + TTL):热路径零 IO,跟 v1.x
//!   行为一致(1000 条 × 60 分钟 LRU,可 `with_capacity` 调整)。
//! - **L2 sqlite write-through**(默认 `~/.codex-app-transfer/sessions.db`,30
//!   天 TTL):**进程重启不丢历史**。Codex CLI 用旧 `previous_response_id` 续轮
//!   时,L1 miss → 从 L2 查回 → 升温到 L1。
//!
//! 写入双写;读取先 L1 后 L2。L2 任何 IO 错误 → log warning 后退到纯 L1,代理
//! 仍能对外服务(只是丢"重启不丢历史"这个新增能力)。
//!
//! ## 数据治理
//!
//! - **Schema version**:启动时检查 `sessions_meta` 表里的 `schema_version`,
//!   不匹配 / 表不存在 → 备份现 db 为 `sessions.db.bak.<unix-ts>` 然后重建空
//!   db(用户丢历史一次,但能正常运行;重启场景跟 PR 1 修的 cache miss 相同 →
//!   返回 OpenAI SDK 兼容 400)。**当前 schema_version = 1**。
//! - **TTL** 30 天:启动时 `DELETE WHERE last_access_unix < now - 30d` 清过期。
//! - **隐私**:db 文件包含完整对话历史(messages JSON),用户可:(a) admin
//!   endpoint `POST /api/sessions/clear` 全清;(b) 直接删 db 文件。README 在
//!   隐私小节里写明。
//! - **并发**:rusqlite `Connection` 不是 Sync,用 `Mutex` 包。WAL + synchronous
//!   NORMAL 提升单写多读性能;sqlite 自身保证原子性,不需要额外 transaction。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::blob_store::{BlobStore, InlineError, BLOB_REF_KEY};

/// 当前 sqlite schema 版本。改 schema 时把这个值 +1,旧 db 会被自动备份重建。
const SCHEMA_VERSION: i64 = 1;

/// 默认 L2(sqlite)TTL — 30 天,跟 OpenAI Responses API 服务端 30 天 retention
/// 对齐(`docs/guides/conversation-state`)。
const DEFAULT_PERSISTED_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// L1 默认尺寸 / TTL,保留 v1.x 行为(1000 条 × 60 分钟 LRU)。
const DEFAULT_L1_SIZE: usize = 1000;
const DEFAULT_L1_TTL: Duration = Duration::from_secs(3600);

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
    /// L1 内存层 LRU 容量上限。
    max_size: usize,
    /// L1 内存层 TTL。
    ttl: Duration,
    /// L2 sqlite TTL(从 last_access_unix 起算)。仅在 db 启用时生效。
    persisted_ttl: Duration,
    inner: Mutex<SessionCacheInner>,
    /// L2 sqlite 持久化层。`Some` = 启用 write-through;`None` = 纯内存
    /// (单元测试 / db 启动失败 fallback)。
    db: Mutex<Option<Connection>>,
    /// MOC-142 内容寻址 blob 外置层(根 = `sessions.db` 同级 `blobs/`)。`Some` =
    /// L2 写盘前把大 `data:` 图片抽成 sha256 引用、读回时回填;`None` = 纯内存
    /// (无盘 → 无外置)。L1 内存层始终存完整 inline,不经 blob。
    blobs: Option<BlobStore>,
}

impl ResponseSessionCache {
    /// 纯内存 cache(测试 / fallback 用),不持久化。
    pub fn new(max_size: usize, ttl: Duration) -> Self {
        Self {
            max_size: max_size.max(1),
            ttl,
            persisted_ttl: DEFAULT_PERSISTED_TTL,
            inner: Mutex::new(SessionCacheInner {
                entries: HashMap::new(),
            }),
            db: Mutex::new(None),
            blobs: None,
        }
    }

    /// 内存 + sqlite 双层。`db_path` 通常是 `~/.codex-app-transfer/sessions.db`。
    /// 任何 sqlite 初始化错误(权限 / IO / schema 不匹配)→ 回退到纯内存,**不**
    /// 让代理启动失败;并把错误 log 到调用方传入的 `on_error`。
    ///
    /// `persisted_ttl` 控制 L2 过期清理(默认 30 天)。
    pub fn with_db_path(
        max_size: usize,
        ttl: Duration,
        persisted_ttl: Duration,
        db_path: &Path,
    ) -> (Self, Option<String>) {
        let cache = Self {
            max_size: max_size.max(1),
            ttl,
            persisted_ttl,
            inner: Mutex::new(SessionCacheInner {
                entries: HashMap::new(),
            }),
            db: Mutex::new(None),
            // blobs 根 = sessions.db 同级 blobs/;db 无父目录(罕见)→ None。
            blobs: db_path.parent().map(|p| BlobStore::new(p.join("blobs"))),
        };
        let warn = match init_db(db_path, persisted_ttl) {
            Ok(conn) => {
                *cache.db.lock().expect("session cache db mutex poisoned") = Some(conn);
                None
            }
            Err(e) => Some(format!(
                "sessions.db init failed at {}: {e} — falling back to in-memory only",
                db_path.display()
            )),
        };
        (cache, warn)
    }

    pub fn save(&self, response_id: &str, messages: Vec<Value>) {
        if response_id.trim().is_empty() {
            return;
        }

        // L1 写入(同步,纳秒级)
        {
            let mut inner = self.inner.lock().expect("session cache mutex poisoned");
            self.evict_expired_locked(&mut inner);
            if inner.entries.len() >= self.max_size && !inner.entries.contains_key(response_id) {
                self.evict_oldest_locked(&mut inner);
            }
            inner.entries.insert(
                response_id.to_owned(),
                SessionEntry {
                    messages: messages.clone(),
                    ts: Instant::now(),
                    access_count: 0,
                },
            );
        }

        // L2 sqlite write-through(毫秒级)。失败仅 log,不影响 L1。
        if let Err(e) = self.persist_save(response_id, &messages) {
            log_db_warning(
                "SESSIONS_DB_SAVE_FAILED",
                format!("save response_id={response_id} failed: {e}"),
            );
        }
    }

    pub fn get(&self, response_id: &str) -> Option<Vec<Value>> {
        if response_id.trim().is_empty() {
            return None;
        }

        // L1 先查
        {
            let mut inner = self.inner.lock().expect("session cache mutex poisoned");
            let expired = inner
                .entries
                .get(response_id)
                .map(|entry| entry.ts.elapsed() > self.ttl)
                .unwrap_or(false);
            if expired {
                inner.entries.remove(response_id);
            } else if let Some(entry) = inner.entries.get_mut(response_id) {
                entry.access_count += 1;
                return Some(entry.messages.clone());
            }
        }

        // L1 miss → L2 sqlite 查;命中则升温到 L1
        match self.persist_load(response_id) {
            Ok(Some(messages)) => {
                let mut inner = self.inner.lock().expect("session cache mutex poisoned");
                if inner.entries.len() >= self.max_size {
                    self.evict_oldest_locked(&mut inner);
                }
                inner.entries.insert(
                    response_id.to_owned(),
                    SessionEntry {
                        messages: messages.clone(),
                        ts: Instant::now(),
                        access_count: 1,
                    },
                );
                Some(messages)
            }
            Ok(None) => None,
            Err(e) => {
                log_db_warning(
                    "SESSIONS_DB_LOAD_FAILED",
                    format!("load response_id={response_id} failed: {e}"),
                );
                None
            }
        }
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

    /// 仅清 L1 内存(老语义,保留供已有调用)。
    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("session cache mutex poisoned")
            .entries
            .clear();
    }

    /// **彻底清除**:L1 内存 + L2 sqlite 表。给 admin endpoint
    /// `POST /api/sessions/clear` 用。返回清掉的 L2 行数(L1 总是清空)。
    pub fn clear_all_persisted(&self) -> Result<usize, String> {
        self.clear();
        // **全程持 db 锁**(从这里到 fn 末尾):DELETE 与 blob sweep 之间**不放锁**,杜绝
        // 并发 `persist_save` 在两者之间 acquire 锁、externalize 新图、insert 新 row —— 否则
        // 紧随其后的空-live sweep 会把那张刚写的 blob 当孤儿删,留下指向缺失 blob 的悬挂行
        // → 下次 L2 load 该 response 必 miss(codex-connector P2 并发竞态)。被阻塞的 save
        // 会排到 clear 之后执行,其 blob 不在本轮 sweep 范围,安全。db=None(纯内存)时
        // persist_save 早返、无并发 blob 写入,持不持锁都安全。
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let deleted = match guard.as_mut() {
            Some(conn) => conn
                .execute("DELETE FROM response_sessions", [])
                .map_err(|e| {
                    let detail = format!("clear failed: {e}");
                    log_db_warning("SESSIONS_DB_CLEAR_FAILED", detail.clone());
                    detail
                })?,
            // db 不可用(sqlite init 失败 → 纯内存 fallback):L2 行数 0,但 `blobs/` 可能仍有
            // **上次成功运行**外置的私密图(`with_db_path` 无条件按 db_path 同级建 blobs 层)。
            // **不能**早返成功跳过 sweep —— 必须继续往下清 blob(codex-connector P2)。
            None => 0,
        };
        // MOC-142:行全清 → 所有 blob 成孤儿,一并清掉("彻底清除"语义)。这是**隐私清除**
        // 端点(POST /api/sessions/clear):blob 没删干净必须**上报**(返 Err → handler 500),
        // 不能 best-effort 静默成功让私密图片残留在 blobs/(codex-connector P1)。
        if let Some(store) = self.blobs.as_ref() {
            let stats = store.sweep(&HashSet::new()).map_err(|e| {
                let detail = format!("blob store clear failed (private images may remain): {e}");
                log_db_warning("SESSIONS_BLOB_CLEAR_INCOMPLETE", detail.clone());
                detail
            })?;
            if stats.failed > 0 {
                let detail = format!(
                    "session rows cleared but {} blob file(s) could not be removed from \
                     blobs/ (private images may remain)",
                    stats.failed
                );
                log_db_warning("SESSIONS_BLOB_CLEAR_INCOMPLETE", detail.clone());
                return Err(detail);
            }
        }
        drop(guard); // 显式:sweep 全程持锁,到此才放
        Ok(deleted)
    }

    /// 启动时清 L2 过期 entry(`last_access_unix < now - persisted_ttl`)。
    pub fn evict_expired_persisted(&self) -> Result<usize, String> {
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            return Ok(0);
        };
        let cutoff = unix_now().saturating_sub(self.persisted_ttl.as_secs() as i64);
        conn.execute(
            "DELETE FROM response_sessions WHERE last_access_unix <= ?1",
            params![cutoff],
        )
        .map_err(|e| format!("sessions.db evict expired failed: {e}"))
    }

    /// MOC-142 启动 GC:删掉不再被任何 row 引用的孤儿 blob 文件(`evict_expired_persisted`
    /// 删过期 row 后会留下悬挂 blob)。无 blob 层 → `Ok(0)`。
    ///
    /// 只扫含引用 sentinel 的 row(`LIKE`),**不**解析存量纯 inline 大行(老格式
    /// / base64 直存),避免在用户既有大库上启动时把 GB 级 messages_json 全 parse。
    ///
    /// **fail-safe**:任一 row 读不出 / parse 不了 → 整体 `Err`(本轮不删任何 blob),
    /// 宁可漏回收孤儿也绝不误删活 blob(mark 不完整就 sweep = 历史丢)。
    ///
    /// **回收时机**:仅 `global_response_session_cache()` 初始化时调一次 → 孤儿在长跑
    /// 进程内累积到下次**重启**才清。桌面 app 常重启,可接受;暂不做进程内周期 GC。
    pub fn sweep_orphan_blobs(&self) -> Result<usize, String> {
        let Some(store) = self.blobs.as_ref() else {
            return Ok(0);
        };
        let mut live: HashSet<String> = HashSet::new();
        {
            let mut guard = self.db.lock().expect("session cache db mutex poisoned");
            let Some(conn) = guard.as_mut() else {
                return Ok(0);
            };
            // LIKE 模式由 BLOB_REF_KEY 单一来源构造(避免魔法串多处漂移)。`_` 在 LIKE
            // 里是单字符通配 → 只会**多**匹配,是安全超集;真实 hash 由 collect_hashes
            // 精确 JSON parse 取,多扫几行不影响正确性。
            let like_pattern = format!("%{BLOB_REF_KEY}%");
            let mut stmt = conn
                .prepare("SELECT messages_json FROM response_sessions WHERE messages_json LIKE ?1")
                .map_err(|e| format!("sessions.db blob-ref scan prepare failed: {e}"))?;
            let rows = stmt
                .query_map(params![like_pattern], |r| r.get::<_, String>(0))
                .map_err(|e| format!("sessions.db blob-ref scan failed: {e}"))?;
            for row in rows {
                // **fail-safe**(silent-failure BLOCKER):mark 不完整就 sweep = 可能把活
                // blob 当孤儿删 → 历史永久丢。任一行读不出 / parse 不了就整体 **abort**
                // (返 Err,本轮一个 blob 都不删,非破坏),孤儿留到下次干净启动再回收。
                let row = row.map_err(|e| {
                    format!("sessions.db blob-ref row read failed, abort sweep (no delete): {e}")
                })?;
                let messages: Vec<Value> = serde_json::from_str(&row).map_err(|e| {
                    format!("sessions.db blob-ref row parse failed, abort sweep (no delete): {e}")
                })?;
                for m in &messages {
                    BlobStore::collect_hashes(m, &mut live);
                }
            }
        }
        let stats = store
            .sweep(&live)
            .map_err(|e| format!("sessions.db blob sweep failed: {e}"))?;
        if stats.failed > 0 {
            log_db_warning(
                "SESSIONS_BLOB_SWEEP_PARTIAL",
                format!(
                    "startup GC: removed {} orphan blob(s), {} failed \
                     (best-effort, retry next GC)",
                    stats.removed, stats.failed
                ),
            );
        }
        Ok(stats.removed)
    }

    /// fix(#210 P2): Proxy 停止前把 L1 内存中所有活跃 entry 重新写入 L2。
    ///
    /// 正常情况下 L2 已通过 write-through 保持一致，但瞬时 IO 错误可能导致部分
    /// entry 仅存在于 L1。此方法在进程 shutdown 前做最后一次批量 flush，尽力
    /// 补齐可能的 L2 漏洞，减少重启后 `previous_response_id` cache miss。
    ///
    /// 返回 `(attempted, failed)` 分别表示尝试写入的条目数和失败条数。
    pub fn flush_to_persistent(&self) -> (usize, usize) {
        let entries: Vec<(String, Vec<Value>)> = {
            let inner = self.inner.lock().expect("session cache mutex poisoned");
            inner
                .entries
                .iter()
                .filter(|(_, entry)| entry.ts.elapsed() <= self.ttl)
                .map(|(k, v)| (k.clone(), v.messages.clone()))
                .collect()
        };
        let total = entries.len();
        let mut failed = 0usize;
        for (response_id, messages) in &entries {
            if let Err(e) = self.persist_save(response_id, messages) {
                log_db_warning(
                    "SESSIONS_DB_FLUSH_FAILED",
                    format!("flush response_id={response_id} failed: {e}"),
                );
                failed += 1;
            }
        }
        (total, failed)
    }

    fn persist_save(&self, response_id: &str, messages: &[Value]) -> rusqlite::Result<()> {
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            return Ok(());
        };
        // **silent-failure H1 修**:旧实现 `unwrap_or_else(|_| "[]")` 让编码失败
        // 静默覆盖原本有效的 row(`ON CONFLICT DO UPDATE`)→ 后续 get 返空历史 →
        // 用户视角是 silent context loss。新实现编码失败 warn + **跳过本次写入**
        // 让 L2 原 row 保留(L1 内存层已 save,本轮请求仍正常)。
        // MOC-142:写盘前把大 `data:` 图片外置成 sha256 引用(内容寻址去重),只让
        // L2 存引用而非逐轮重复整张 base64。单个 blob 外置失败留 inline(非破坏);
        // L1 内存层已存完整 inline,不受影响。纯内存模式(blobs=None)行为不变。
        let encoded = match self.blobs.as_ref() {
            Some(store) => {
                let mut slim: Vec<Value> = messages.to_vec();
                for m in &mut slim {
                    store.externalize(m);
                }
                serde_json::to_string(&slim)
            }
            None => serde_json::to_string(messages),
        };
        let json = match encoded {
            Ok(s) => s,
            Err(e) => {
                log_db_warning(
                    "SESSIONS_DB_SAVE_ENCODE_FAILED",
                    format!(
                        "json encode failed for response_id={response_id}, \
                         skip L2 write to preserve any prior row: {e}"
                    ),
                );
                return Ok(());
            }
        };
        let now = unix_now();
        conn.execute(
            "INSERT INTO response_sessions \
             (response_id, messages_json, created_unix, last_access_unix, access_count) \
             VALUES (?1, ?2, ?3, ?3, 0) \
             ON CONFLICT(response_id) DO UPDATE SET \
                 messages_json = excluded.messages_json, \
                 last_access_unix = excluded.last_access_unix",
            params![response_id, json, now],
        )?;
        Ok(())
    }

    fn persist_load(&self, response_id: &str) -> rusqlite::Result<Option<Vec<Value>>> {
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            return Ok(None);
        };
        let cutoff = unix_now().saturating_sub(self.persisted_ttl.as_secs() as i64);
        // 同时检查 last_access_unix 防越过 TTL 的脏读
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT messages_json, last_access_unix FROM response_sessions \
                 WHERE response_id = ?1 AND last_access_unix > ?2",
                params![response_id, cutoff],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((json, _last)) = row else {
            return Ok(None);
        };
        // 命中后更新 last_access_unix(顺手 +access_count 便于将来观测)
        // **silent-failure H2 修**:旧 `let _ = ...` 静默 UPDATE 失败 → row 的
        // `last_access_unix` 永远不前进 → 下次 `evict_expired_persisted` 把活会话
        // 当过期删 → 用户视角 history 凭空消失。新实现 warn 不 panic,let read
        // 路径仍 return 数据。
        let now = unix_now();
        if let Err(e) = conn.execute(
            "UPDATE response_sessions SET last_access_unix = ?1, access_count = access_count + 1 \
             WHERE response_id = ?2",
            params![now, response_id],
        ) {
            log_db_warning(
                "SESSIONS_DB_TOUCH_FAILED",
                format!(
                    "last_access_unix UPDATE failed for response_id={response_id} \
                     (read served from L2,but TTL refresh skipped — row 可能被 evict): {e}"
                ),
            );
        }
        match serde_json::from_str::<Vec<Value>>(&json) {
            Ok(mut messages) => {
                // MOC-142:把 blob 引用回填成原始 `data:` 字符串。任一引用的 blob
                // 缺失/IO 错 → 整行不可用,删行当 cache miss(绝不把引用对象泄漏
                // 给模型)。纯内存模式(blobs=None)直接返回原 messages。
                if let Some(store) = self.blobs.as_ref() {
                    for m in &mut messages {
                        if let Err(e) = store.inline(m) {
                            // blob 缺失/IO → 本行无法完整回填。**非破坏**:当 cache miss
                            // 返回,绝不删行(撞用户硬规则)、绝不把引用对象泄漏给模型;
                            // 留待 blob 恢复(IO 瞬时)或 TTL 自然淘汰(永久缺失)。
                            let error_id = match e {
                                InlineError::Missing(_) => "SESSIONS_DB_BLOB_MISSING",
                                InlineError::Io(_) => "SESSIONS_DB_BLOB_IO",
                            };
                            log_db_warning(
                                error_id,
                                format!(
                                    "blob inline failed for response_id={response_id}, \
                                     serving cache-miss without delete: {e}"
                                ),
                            );
                            return Ok(None);
                        }
                    }
                }
                Ok(Some(messages))
            }
            Err(parse_err) => {
                // 历史格式损坏 → 删除该行,当 miss 处理。
                // **F6 follow-up**(code-reviewer IMPORTANT #2 修):旧实现 silent
                // delete,operator 无从观测数据损坏率。新实现 emit warn 让磁盘
                // corruption / schema drift / 手工编辑 db 等异常路径有信号。
                let delete_result = conn.execute(
                    "DELETE FROM response_sessions WHERE response_id = ?1",
                    params![response_id],
                );
                log_db_warning(
                    "SESSIONS_DB_ROW_CORRUPT",
                    format!(
                        "messages_json parse failed for response_id={response_id} \
                         (delete_ok={}): {parse_err}",
                        delete_result.is_ok()
                    ),
                );
                Ok(None)
            }
        }
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|e| {
            // **silent-failure M4 修**:SystemTime pre-epoch(系统时钟漂移到
            // 1970 前)极罕见但会让 TTL evict 全 no-op + 新 row created_unix=0,
            // 默默丢失观测;此处 warn 一次保留信号。
            log_db_warning(
                "SESSIONS_DB_CLOCK_PRE_EPOCH",
                format!("SystemTime before UNIX_EPOCH: {e} — falling back to 0"),
            );
            0
        })
}

/// 初始化 sqlite db。schema 检测 + 不匹配时备份 + 重建。
fn init_db(db_path: &Path, _persisted_ttl: Duration) -> rusqlite::Result<Connection> {
    if let Some(parent) = db_path.parent() {
        // **silent-failure M1 修**:旧 `let _ = create_dir_all` 让 fs 错误被
        // 后续 `Connection::open` 的 generic sqlite error 掩盖。warn 让 operator
        // 知道根因是 fs perm / parent-is-file 还是 sqlite 真坏。
        if let Err(e) = std::fs::create_dir_all(parent) {
            log_db_warning(
                "SESSIONS_DB_PARENT_DIR_FAILED",
                format!(
                    "create_dir_all({}) failed: {e} — Connection::open 可能也会失败,\
                     真根因是 fs 不是 sqlite",
                    parent.display()
                ),
            );
        }
    }
    let conn = open_db_with_pragmas(db_path)?;
    if needs_rebuild(&conn)? {
        drop(conn);
        backup_corrupt_db(db_path);
        let conn = open_db_with_pragmas(db_path)?;
        create_schema(&conn)?;
        return Ok(conn);
    }
    create_schema_if_missing(&conn)?;
    Ok(conn)
}

fn open_db_with_pragmas(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    // WAL = 单写多读不互锁,显著提升并发读性能(代理常见用法)
    // synchronous=NORMAL = 放松 fsync 频率,在系统崩溃时可能丢最后几条但
    // 不会损坏 db。session cache 重建即可,可接受。
    // **silent-failure M2 修**:pragma 失败(read-only FS / 网络 mount)会让
    // doc-comment 的"WAL 单写多读"承诺破裂 + 并发降级,旧 `let _ =` 静默 → 没
    // 人看得到。emit warn 不 fail open(degrade 仍能跑,只是慢)。
    if let Err(e) = conn.pragma_update(None, "journal_mode", "WAL") {
        log_db_warning(
            "SESSIONS_DB_PRAGMA_FAILED",
            format!("pragma journal_mode=WAL failed: {e} — concurrent reads will block writes"),
        );
    }
    if let Err(e) = conn.pragma_update(None, "synchronous", "NORMAL") {
        log_db_warning(
            "SESSIONS_DB_PRAGMA_FAILED",
            format!("pragma synchronous=NORMAL failed: {e} — fsync frequency unchanged"),
        );
    }
    Ok(conn)
}

fn needs_rebuild(conn: &Connection) -> rusqlite::Result<bool> {
    // 表不存在 → 不算 corrupt,走 create_schema_if_missing
    let table_exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='sessions_meta'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if table_exists.is_none() {
        return Ok(false);
    }
    // 表存在 → 检查版本是否匹配
    let version: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM sessions_meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    Ok(version != Some(SCHEMA_VERSION))
}

fn create_schema_if_missing(conn: &Connection) -> rusqlite::Result<()> {
    let table_exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='response_sessions'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if table_exists.is_none() {
        create_schema(conn)?;
    }
    Ok(())
}

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS response_sessions (
            response_id TEXT PRIMARY KEY,
            messages_json TEXT NOT NULL,
            created_unix INTEGER NOT NULL,
            last_access_unix INTEGER NOT NULL,
            access_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_last_access ON response_sessions(last_access_unix);
        CREATE TABLE IF NOT EXISTS sessions_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO sessions_meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

fn backup_corrupt_db(db_path: &Path) {
    let ts = unix_now();
    let bak = PathBuf::from(format!("{}.bak.{}", db_path.display(), ts));
    // **silent-failure M3 修**:旧 `let _ = rename(...)` 静默 → rename 失败时
    // 后续会基于**原 corrupt db** 重建 schema(可能成功也可能挂);上层 warn
    // 文案仍宣称"backed up + rebuilding",撒谎给 operator。新实现区分两条
    // 路径,rename 失败时换 error_id 让 operator 看到真实状态。
    match std::fs::rename(db_path, &bak) {
        Ok(()) => {
            log_db_warning(
                "SESSIONS_DB_SCHEMA_MISMATCH",
                format!(
                    "schema mismatch at {} — backed up to {} and rebuilding empty db",
                    db_path.display(),
                    bak.display()
                ),
            );
            // MOC-142 + codex-connector P2:db 备份成 `.bak` 后,新空库的启动 sweep 会把
            // 所有 blob 当孤儿删 → `.bak` 引用的图不可恢复。连带把 `blobs/` 改名为
            // `blobs.bak.<同一 ts>/`,让备份成对自洽(也使新库的 blobs/ 为空、sweep 无可删)。
            // best-effort:失败仅 warn,不阻断重建。
            backup_blobs_dir(db_path, ts);
        }
        Err(e) => log_db_warning(
            "SESSIONS_DB_BACKUP_FAILED",
            format!(
                "schema mismatch at {} but backup rename to {} failed: {e} — \
                 重建会基于 corrupt db,可能继续异常",
                db_path.display(),
                bak.display()
            ),
        ),
    }
}

/// 把 `sessions.db` 同级的 `blobs/` 目录随 db 一起备份成 `blobs.bak.<ts>/`(与
/// `sessions.db.bak.<ts>` 配对)。仅 db 备份成功后调用;`blobs/` 不存在则 no-op。
fn backup_blobs_dir(db_path: &Path, ts: i64) {
    let Some(parent) = db_path.parent() else {
        return;
    };
    let blobs = parent.join("blobs");
    if !blobs.is_dir() {
        return;
    }
    let blobs_bak = parent.join(format!("blobs.bak.{ts}"));
    if let Err(e) = std::fs::rename(&blobs, &blobs_bak) {
        log_db_warning(
            "SESSIONS_BLOB_BACKUP_FAILED",
            format!(
                "schema mismatch: db 已备份但 blobs/ → {} rename 失败: {e} — \
                 启动 sweep 可能删掉 .bak 引用的图",
                blobs_bak.display()
            ),
        );
    }
}

/// L2 sqlite 失败 surface(F6 修):
///
/// 既走 `tracing::warn!`(带 **stable `error_id`** 字段)让 telemetry pipeline /
/// log search / 任何 tracing subscriber 都能定位 + 聚合,又**保留 `eprintln!`
/// 兜底** — proxy 启动时把 stderr 重定向到 `~/.codex-app-transfer/logs/proxy-
/// <date>.log`,即便 tracing subscriber 未配 / panic,这条日志仍能落盘。
///
/// `error_id` 是稳定 token,grep / metrics rollup 用,**不要随版本改**:
///
/// 上层失败:
/// - `SESSIONS_DB_INIT_FAILED` — sqlite 打不开 / schema 建不出来,fallback 纯内存
/// - `SESSIONS_DB_SAVE_FAILED` — write-through INSERT/UPDATE 失败(L1 未受影响)
/// - `SESSIONS_DB_LOAD_FAILED` — SELECT 失败,本次 get 返 None
/// - `SESSIONS_DB_SCHEMA_MISMATCH` — schema_version 不匹配,db 已备份重建
/// - `SESSIONS_DB_CLEAR_FAILED` — admin DELETE 失败(也通过 Result 返调用方)
/// - `SESSIONS_DB_EVICT_FAILED` — 过期清理失败(non-fatal,启动时 best-effort)
///
/// 子路径失败(silent-failure-hunter task 21 追加):
/// - `SESSIONS_DB_SAVE_ENCODE_FAILED` — `serde_json::to_string(messages)` 失败,
///   **跳过本次 L2 写入**保留原 row(防 silent context loss,H1)
/// - `SESSIONS_DB_TOUCH_FAILED` — read 命中后 `last_access_unix` UPDATE 失败,
///   row 可能被错 evict(H2)
/// - `SESSIONS_DB_ROW_CORRUPT` — `messages_json` parse 失败,该 row 已删(code-reviewer #2)
/// - `SESSIONS_DB_PARENT_DIR_FAILED` — `create_dir_all` 失败,sqlite 错误根因是 fs(M1)
/// - `SESSIONS_DB_PRAGMA_FAILED` — `journal_mode=WAL` / `synchronous=NORMAL` 失败,
///   并发 / fsync 行为退化(M2)
/// - `SESSIONS_DB_BACKUP_FAILED` — schema-mismatch 时 rename 失败,后续重建基于
///   corrupt db,可能继续异常(M3)
/// - `SESSIONS_DB_CLOCK_PRE_EPOCH` — `SystemTime::now() < UNIX_EPOCH`,TTL evict
///   退化为 no-op(M4)
///
/// MOC-142 blob 外置层(`SESSIONS_BLOB_*` 部分经 `blob_store.rs` 本地 `warn()`,同
/// `error_id` 约定):
/// - `SESSIONS_DB_BLOB_MISSING` — 引用的 blob 文件缺失,本行**非破坏**当 cache miss(不删行)
/// - `SESSIONS_DB_BLOB_IO` — 读 blob IO 错(多为瞬时),当 cache miss(不删行)
/// - `SESSIONS_BLOB_PUT_FAILED` — blob 落盘失败,该 `data:` 留 inline(非破坏)
/// - `SESSIONS_BLOB_SWEEP_FAILED` — 启动 GC mark 不完整 → abort,本轮不删任何 blob
/// - `SESSIONS_BLOB_SHARD_ITER_FAILED` / `SESSIONS_BLOB_SHARD_READ_FAILED` — sweep 遍历 /
///   读分片目录失败,该分片孤儿本轮未回收
/// - `SESSIONS_BLOB_ENTRY_FAILED` — sweep 单个文件项读取失败(计入 `failed`,防隐私
///   清除漏查某 blob 却误报成功)
/// - `SESSIONS_BLOB_REMOVE_FAILED` — 孤儿 blob 删除失败,下次 GC 重试
/// - `SESSIONS_BLOB_TMP_REMOVE_FAILED` — 残留 `.tmp.`(含在途 blob 字节)删除失败,**计入
///   `failed`** → 隐私清除据此报不完整(NotFound 静默;codex-connector P1)
/// - `SESSIONS_BLOB_SWEEP_PARTIAL` — 启动 GC 部分 blob 删失败(best-effort,下次重试)
/// - `SESSIONS_BLOB_CLEAR_INCOMPLETE` — 隐私清除(`/api/sessions/clear`)blob 没删干净,
///   **已 Err 上报**(行已删但私密图片可能残留;codex-connector P1)
/// - `SESSIONS_BLOB_BACKUP_FAILED` — schema 重建备份 db 后,`blobs/` → `blobs.bak.<ts>/`
///   rename 失败(启动 sweep 可能删掉 `.bak` 引用的图;codex-connector P2)
///
/// `error_id` 必须用 `&'static str`(literal)以保跨版本稳定;`detail` 给人类
/// 阅读的上下文(path / error message),不进 metric label。
///
/// **deferred suggestion**(type-design-analyzer):升级为 `enum SessionDbErrorId`
/// 获取 compile-time enforcement。本 PR 暂用 literal 跟 codebase 其他
/// `tracing::warn!(error_id=...)` 用法一致(`tool_call_cache.rs` / `request.rs`
/// 同模式),若未来 error_id 数量翻倍或出现拼写漂移再升级。
fn log_db_warning(error_id: &'static str, detail: String) {
    tracing::warn!(error_id, detail = %detail, "sessions.db");
    // 兼容老路径:Tauri proxy log file 收 stderr,保留 eprintln 兜底防 tracing
    // subscriber 未初始化(早期启动期 / unit test)时丢日志。
    eprintln!("warning: [{error_id}] {detail}");
}

/// 全局单例,生产代理路径用。
///
/// 启动时尝试在 `~/.codex-app-transfer/sessions.db` 打开 sqlite;成功则双层模式,
/// 失败 fallback 纯内存(纯内存模式 ≈ v2.0.10 行为,跟 PR 1 配合表现仍正常 —
/// 只是丢"重启不丢历史"这个新增能力)。
pub fn global_response_session_cache() -> &'static ResponseSessionCache {
    static CACHE: OnceLock<ResponseSessionCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let db_path = codex_app_transfer_registry::sessions_db_file();
        match db_path {
            Some(path) => {
                let (cache, warn) = ResponseSessionCache::with_db_path(
                    DEFAULT_L1_SIZE,
                    DEFAULT_L1_TTL,
                    DEFAULT_PERSISTED_TTL,
                    &path,
                );
                if let Some(msg) = warn {
                    log_db_warning("SESSIONS_DB_INIT_FAILED", msg);
                }
                // 启动时清一遍 L2 过期 row;失败仅 log
                if let Err(e) = cache.evict_expired_persisted() {
                    log_db_warning("SESSIONS_DB_EVICT_FAILED", e);
                }
                // MOC-142:过期 row 删完后清悬挂 blob(孤儿);失败仅 log。
                if let Err(e) = cache.sweep_orphan_blobs() {
                    log_db_warning("SESSIONS_BLOB_SWEEP_FAILED", e);
                }
                cache
            }
            None => ResponseSessionCache::new(DEFAULT_L1_SIZE, DEFAULT_L1_TTL),
        }
    })
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

        // 纯内存模式下,L1 LRU 淘汰 resp_1
        assert!(cache.get("resp_1").is_none());
        assert!(cache.get("resp_2").is_some());
        assert!(cache.get("resp_3").is_some());
    }

    // ── L2 sqlite 持久化测试 ──────────────────────────────────────────

    fn fresh_db_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        (dir, path)
    }

    #[test]
    fn l2_sqlite_persists_across_cache_instances() {
        // 关键回归(2026-05-08):sqlite write-through 让 cache 实例销毁后,
        // 新建实例(模拟 Tauri 重启)仍能读回历史。修复 PR 1 cache miss 根因。
        let (_dir, path) = fresh_db_path();

        let (cache_a, warn_a) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        assert!(warn_a.is_none(), "首次开 db 应当成功,实际 warn: {warn_a:?}");
        cache_a.save(
            "resp_persisted",
            vec![
                json!({"role": "user", "content": "across-restart"}),
                json!({"role": "assistant", "content": "yes I remember"}),
            ],
        );
        // 模拟"进程退出":只 drop cache_a,db 文件留在磁盘
        drop(cache_a);

        let (cache_b, warn_b) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        assert!(warn_b.is_none());
        let restored = cache_b.get("resp_persisted").expect("L2 应当恢复历史");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0]["content"], "across-restart");
        assert_eq!(restored[1]["content"], "yes I remember");
    }

    #[test]
    fn l1_miss_warms_from_l2_then_serves_l1() {
        // L1 LRU 满淘汰后,L2 仍能命中并升温回 L1。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            1, // L1 容量 1,任何新条目都会挤掉旧的
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        cache.save("resp_a", vec![json!({"role": "user", "content": "A"})]);
        cache.save("resp_b", vec![json!({"role": "user", "content": "B"})]);
        // resp_a 在 L1 已被淘汰(容量 1),但 L2 还在
        let restored = cache.get("resp_a").expect("L2 应能命中并升温到 L1");
        assert_eq!(restored[0]["content"], "A");
    }

    #[test]
    fn clear_all_persisted_wipes_both_layers() {
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        cache.save("resp_x", vec![json!({"role": "user", "content": "x"})]);
        let cleared = cache.clear_all_persisted().unwrap();
        assert!(cleared >= 1, "至少清掉 1 行,实际 {cleared}");
        assert!(cache.get("resp_x").is_none());

        // 重新开实例确认 L2 也确实清了
        drop(cache);
        let (cache2, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        assert!(cache2.get("resp_x").is_none());
    }

    #[test]
    fn schema_mismatch_backs_up_and_rebuilds() {
        // 用 schema_version=999 的"假未来 db"模拟 schema 不匹配场景:
        // - init_db 应当备份它然后重建空 db
        // - 老数据丢,但新 cache 能正常工作
        let (_dir, path) = fresh_db_path();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO sessions_meta VALUES('schema_version', '999');",
            )
            .unwrap();
        }

        let (cache, warn) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        assert!(warn.is_none(), "重建路径应成功,不报 init 失败");
        // 备份文件应存在
        let parent = path.parent().unwrap();
        let bak_count = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("sessions.db.bak.")
            })
            .count();
        assert_eq!(bak_count, 1, "应当生成一个备份文件");
        // 新 db 能正常写读
        cache.save(
            "resp_after_rebuild",
            vec![json!({"role": "user", "content": "ok"})],
        );
        let restored = cache.get("resp_after_rebuild").unwrap();
        assert_eq!(restored[0]["content"], "ok");
    }

    #[test]
    fn ttl_expired_l2_row_does_not_leak() {
        // L2 TTL 过期的 row 不能被 get 命中(即使 L1 没缓存它)
        let (_dir, path) = fresh_db_path();
        // persisted_ttl = 1s,save 后 sleep > 1s 应当 miss
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            Duration::from_secs(1),
            &path,
        );
        cache.save("resp_old", vec![json!({"role": "user", "content": "old"})]);
        std::thread::sleep(Duration::from_millis(2100));
        // L1 还可能没过期(L1 ttl 是 60s),先 clear L1 强制走 L2
        cache.clear();
        assert!(cache.get("resp_old").is_none(), "L2 TTL 过期应 miss");
    }

    #[test]
    fn evict_expired_persisted_removes_old_rows() {
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            Duration::from_secs(1),
            &path,
        );
        cache.save("resp_old", vec![json!({"role": "user", "content": "old"})]);
        cache.save(
            "resp_old2",
            vec![json!({"role": "user", "content": "old2"})],
        );
        std::thread::sleep(Duration::from_millis(2100));
        let removed = cache.evict_expired_persisted().unwrap();
        assert!(removed >= 2, "至少清两条过期,实际 {removed}");
    }

    #[test]
    fn fallback_to_in_memory_when_db_path_invalid() {
        // 故意给一个不可写路径(目录不存在 + 父级也无法创建,平台无关的方式
        // 是用一个已存在文件作为目录前缀)。退一步用 /dev/null/sub 这种:
        // *nix 上 /dev/null 是字符设备不能当目录,会失败。Windows 上换成
        // 不可达盘符。我们这里用更稳的方式:故意把 path 设成一个**已存在
        // 的文件**当 parent dir,create_dir_all 会失败。
        let dir = tempfile::tempdir().unwrap();
        let blocker_file = dir.path().join("blocker");
        std::fs::write(&blocker_file, b"x").unwrap();
        // 现在 blocker_file 是文件;尝试把 sessions.db 放在它"下面"
        let bad_path = blocker_file.join("sessions.db");

        let (cache, warn) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &bad_path,
        );
        // db 初始化必失败 → warn 应该有
        assert!(warn.is_some(), "不可达 db_path 必须 warn,得到 {warn:?}");
        // L1 仍工作
        cache.save("resp_x", vec![json!({"role": "user", "content": "x"})]);
        assert!(cache.get("resp_x").is_some());
    }

    #[test]
    fn save_externalizes_image_to_blob_and_get_rehydrates_from_l2() {
        // MOC-142 端到端:save 大图 → L2 行外置(不含 inline base64、改存 blob 引用、
        // 体积骤减、blobs 落文件);清 L1 后 get 走 L2 + 回填,字节级还原原图。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        let big_image = format!("data:image/png;base64,{}", "A".repeat(20_000));
        let messages = vec![json!({
            "role": "user",
            "content": [{"type": "input_image", "image_url": big_image}],
        })];
        cache.save("resp_img", messages.clone());

        // L2 raw row:已外置 —— 不含 inline base64、含 blob 引用、体积骤减。
        let raw: String = {
            let conn = Connection::open(&path).unwrap();
            conn.query_row(
                "SELECT messages_json FROM response_sessions WHERE response_id = 'resp_img'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(!raw.contains("data:image"), "L2 行不应再有 inline base64");
        assert!(
            raw.contains("__cat_session_blob__"),
            "L2 行应存 blob 引用,实际 {raw}"
        );
        assert!(raw.len() < 1000, "外置后行应骤减,实际 {} 字节", raw.len());

        // blobs 目录应已落文件。
        let blobs_dir = path.parent().unwrap().join("blobs");
        assert!(blobs_dir.exists(), "blobs 目录应建立");

        // 清 L1 强制走 L2 + inline 回填。
        cache.clear();
        let restored = cache.get("resp_img").expect("L2 应能读回并回填 blob");
        assert_eq!(restored, messages, "回填后必须字节级等于原始");
    }

    #[test]
    fn schema_mismatch_also_backs_up_blobs_dir() {
        // codex-connector P2:schema 重建把 db 备份成 .bak 时,必须连带把 blobs/ 改名走,
        // 否则新空库的启动 sweep 会删光 blob、让 .bak 引用的图不可恢复。
        let (_dir, path) = fresh_db_path();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO sessions_meta VALUES('schema_version', '999');",
            )
            .unwrap();
        }
        let blobs = path.parent().unwrap().join("blobs");
        std::fs::create_dir_all(blobs.join("ab")).unwrap();
        std::fs::write(blobs.join("ab").join("deadbeef"), b"img").unwrap();

        let (_cache, warn) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        assert!(warn.is_none(), "schema 重建应成功");
        assert!(!blobs.exists(), "原 blobs/ 应被改名为 blobs.bak.*");
        let bak_dirs = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.starts_with("blobs.bak."))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(bak_dirs, 1, "应生成一个 blobs.bak.* 备份目录");
    }

    #[test]
    fn clear_all_persisted_also_removes_blobs() {
        // MOC-142 隐私清除(codex-connector P1):clear 必须连带删 blobs/,否则私密
        // 图片残留;且 blob 没删干净要返 Err(此处正常路径应成功 + blob 清零)。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        let big = format!("data:image/png;base64,{}", "Z".repeat(20_000));
        cache.save("resp_clear", vec![json!({"image_url": big})]);
        let blobs_dir = path.parent().unwrap().join("blobs");

        fn count_blobs(dir: &std::path::Path) -> usize {
            let Ok(shards) = std::fs::read_dir(dir) else {
                return 0;
            };
            shards
                .flatten()
                .filter_map(|sh| std::fs::read_dir(sh.path()).ok())
                .flat_map(|files| files.flatten())
                .filter(|f| {
                    f.file_name()
                        .to_str()
                        .map(|s| !s.starts_with(".tmp."))
                        .unwrap_or(false)
                })
                .count()
        }

        assert!(count_blobs(&blobs_dir) >= 1, "save 后 blobs/ 应有文件");
        cache.clear_all_persisted().expect("正常路径 clear 应成功");
        assert_eq!(count_blobs(&blobs_dir), 0, "clear 后 blob 必须清零(隐私)");
    }
}
