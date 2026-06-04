//! Responses API conversation session cache —— **双层**(2026-05-08+):
//!
//! - **L1 in-memory hot cache**(`HashMap` + LRU + TTL):热路径零 IO,跟 v1.x
//!   行为一致(1000 条 × 60 分钟 LRU,可 `with_capacity` 调整)。
//! - **L2 sqlite write-through**(默认 `~/.codex-app-transfer/sessions.db`,**持久
//!   化、不过期** MOC-170):**进程重启不丢历史**。Codex CLI 用旧
//!   `previous_response_id` 续轮时,L1 miss → 从 L2 查回 → 升温到 L1。
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
//! - **持久化(MOC-170)**:L2 不再设 TTL —— 内容寻址去重(MOC-142 图 + MOC-168
//!   消息)后体积极小,留存全部历史的磁盘成本可忽略,移除 30 天过期换"老会话永
//!   远续得上"。`evict_expired_persisted` 机制保留(短 TTL 仍可触发,供测试 / 未
//!   来可配置 retention),只是默认不在启动时调用。
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
use super::message_store::{self, MsgInlineError, MSG_REF_KEY};

/// 当前 sqlite schema 版本。改 schema 时把这个值 +1,旧 db 会被自动备份重建。
const SCHEMA_VERSION: i64 = 1;

/// 默认 L2(sqlite)retention — **持久化、不过期**(MOC-170)。
///
/// 历史上是 30 天 TTL(对齐 OpenAI 服务端 retention)。MOC-142(图片 blob 外置)+
/// MOC-168(消息内容寻址去重)后 L2 体积已极小(实测 11 万消息实例去重到 ~2,687
/// 条唯一、省 97%,图片再走 blob 省 63%),按内容寻址存跨会话天然合并,留存全部
/// 历史的磁盘成本可忽略;30 天 TTL 反而带来"老会话续不上"的体验损失。故改持久化。
///
/// 实现上用一个 ~100 年的哨兵当"永不过期",**复用现有 TTL 比较逻辑零改动**——
/// `persist_load` 的 cutoff、`evict_expired_persisted` 在这个值下自然全 no-op
/// (cutoff = now - 100y < 0,任何 row 的 last_access 都 > cutoff),不必给读路径
/// 到处加 `if persistent` 分支。短 TTL(测试 / 未来可配置 retention)仍可显式传入。
const DEFAULT_PERSISTED_TTL: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

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
    /// `persisted_ttl` 控制 L2 过期清理。默认 [`DEFAULT_PERSISTED_TTL`] 是 ~100 年持久
    /// 化哨兵(MOC-170,不过期);测试 / 未来可配置 retention 可显式传短 TTL。
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

    /// **彻底清除**:L1 内存 + L2 sqlite 表 + blob。给 admin endpoint
    /// `POST /api/sessions/clear` 用。返回清掉的 L2 行数(L1 总是清空)。
    ///
    /// **db 不可用(`db=None`)时返回 `Err` 且不动 blob**(数据完整性 BLOCKER 修):`db=None`
    /// 可能只是**瞬时**(db 文件数据完好、blob 仍被磁盘 row 有效引用,本进程只是没打开 db,
    /// 如启动撞锁 / IO 抖动)。旧实现在此仍无条件 `store.sweep(空集)` 删光所有 blob,却没删
    /// (删不到)磁盘 db 行 → 留下指向缺失文件的悬挂 blob 引用、丢失含图历史(真机 5.5G 库
    /// 迁移后点清除踩此坑:blob 全删、response_sessions/message_contents 保留)。改为隐私
    /// 清除在 db 不可达时**整体失败而非部分成功**,blob 原样保留,用户**重启应用**(db 在
    /// `with_db_path` 一次性 init、运行期不自愈重连,故"db 恢复"= 重启 app)后重试即可完整
    /// 清除;"db 永久坏 + blobs/ 残私密图"的极端场景留给用户手动删 blobs/。
    pub fn clear_all_persisted(&self) -> Result<usize, String> {
        self.clear();
        // **全程持 db 锁**(从这里到 fn 末尾):DELETE 与 blob sweep 之间**不放锁**,杜绝
        // 并发 `persist_save` 在两者之间 acquire 锁、externalize 新图、insert 新 row —— 否则
        // 紧随其后的空-live sweep 会把那张刚写的 blob 当孤儿删,留下指向缺失 blob 的悬挂行
        // → 下次 L2 load 该 response 必 miss(codex-connector P2 并发竞态)。被阻塞的 save
        // 会排到 clear 之后执行,其 blob 不在本轮 sweep 范围,安全。
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            // db=None:不删 blob(见上方 doc)。返回 Err 让 endpoint 返 500,用户重试。
            let detail = "sessions.db unavailable (in-memory fallback); persisted history NOT \
                          cleared and blobs left intact to avoid orphaning rows that may still \
                          exist on disk — retry once the db is reachable"
                .to_owned();
            log_db_warning("SESSIONS_DB_CLEAR_DB_UNAVAILABLE", detail.clone());
            return Err(detail);
        };
        // db 可用:DELETE 两表 → 行全清 → 所有 blob 成孤儿 → 下面 sweep 删所有(一致、彻底)。
        let deleted = conn
            .execute("DELETE FROM response_sessions", [])
            .map_err(|e| {
                let detail = format!("clear failed: {e}");
                log_db_warning("SESSIONS_DB_CLEAR_FAILED", detail.clone());
                detail
            })?;
        // MOC-168:消息内容表同 db,隐私清除需连带全清。失败上报(返 Err → 500)。
        conn.execute("DELETE FROM message_contents", [])
            .map_err(|e| {
                let detail = format!("clear message_contents failed: {e}");
                log_db_warning("SESSIONS_MSG_CLEAR_FAILED", detail.clone());
                detail
            })?;
        // MOC-142:行全清 → 所有 blob 成孤儿,一并清掉("彻底清除"语义)。**隐私清除**端点:
        // blob 没删干净必须**上报**(返 Err → handler 500),不 best-effort 静默成功让私密图
        // 残留 blobs/(codex-connector P1)。此处 db 行已清空,sweep(空集)删所有 blob 是安全的
        // (无任何 row 引用它们),与上面 db=None 不删 blob 的关键区别在此。
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
            // MOC-168:消息被外置后,blob 引用主要嵌在 `message_contents.json` 里(行只剩
            // `__cat_msg__` 引用,扫行扫不到 blob 引用)。**必须**把 message_contents 里的
            // blob 引用也收进 live,否则 blob GC 会把仍被引用的图当孤儿删(图片历史丢)。
            // 旧 inline 行的 blob 引用仍在行内 → 两处都扫、union。同样 fail-safe abort。
            let mut stmt2 = conn
                .prepare("SELECT json FROM message_contents WHERE json LIKE ?1")
                .map_err(|e| format!("sessions.db blob-ref(msg) scan prepare failed: {e}"))?;
            let rows2 = stmt2
                .query_map(params![like_pattern], |r| r.get::<_, String>(0))
                .map_err(|e| format!("sessions.db blob-ref(msg) scan failed: {e}"))?;
            for row in rows2 {
                let row = row.map_err(|e| {
                    format!("sessions.db blob-ref(msg) read failed, abort sweep (no delete): {e}")
                })?;
                let msg: Value = serde_json::from_str(&row).map_err(|e| {
                    format!("sessions.db blob-ref(msg) parse failed, abort sweep (no delete): {e}")
                })?;
                BlobStore::collect_hashes(&msg, &mut live);
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

    /// MOC-168 启动 GC:删掉 `message_contents` 里不再被任何 row 引用的孤儿消息
    /// (过期 row 删后留下的悬挂消息)。无 db → `Ok(0)`。跟 blob sweep 同构:只扫含
    /// `__cat_msg__` 引用的 row;mark 不完整即 **abort**(本轮不删任何消息),宁漏回收
    /// 勿误删活消息。仅启动调一次。
    pub fn sweep_orphan_messages(&self) -> Result<usize, String> {
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            return Ok(0);
        };
        let mut live: HashSet<String> = HashSet::new();
        {
            let like_pattern = format!("%{MSG_REF_KEY}%");
            let mut stmt = conn
                .prepare("SELECT messages_json FROM response_sessions WHERE messages_json LIKE ?1")
                .map_err(|e| format!("sessions.db msg-ref scan prepare failed: {e}"))?;
            let rows = stmt
                .query_map(params![like_pattern], |r| r.get::<_, String>(0))
                .map_err(|e| format!("sessions.db msg-ref scan failed: {e}"))?;
            for row in rows {
                let row = row.map_err(|e| {
                    format!("sessions.db msg-ref row read failed, abort sweep (no delete): {e}")
                })?;
                let messages: Vec<Value> = serde_json::from_str(&row).map_err(|e| {
                    format!("sessions.db msg-ref row parse failed, abort sweep (no delete): {e}")
                })?;
                message_store::collect_hashes(&messages, &mut live);
            }
        }
        sweep_message_contents(conn, &live)
            .map_err(|e| format!("sessions.db message sweep failed: {e}"))
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

    /// MOC-170 存量一次性迁移:把 MOC-142/168 上线**之前**写入的旧行(完整 inline
    /// base64 图 + 逐轮重复消息整存)就地 reformat 成两级内容寻址引用,回收历史膨胀
    /// (实测某真机库 5.5GB)。**幂等**:完成置 `sessions_meta['content_addr_migrated']`
    /// 标志,重复调用早返 `Ok(0)`;`db=None`(纯内存)同样 `Ok(0)`。
    ///
    /// ## 设计
    ///
    /// - **分批 + rowid 游标**:每批 `MIGRATE_BATCH` 行、一个事务,批间释放 db 锁让
    ///   正常 serving(save/load)交错,不长期独占。游标 `rowid > cursor` 单调前进
    ///   →(a)已 reformat 的行下批被 `NOT LIKE` 跳过;(b)**parse 失败跳过的坏行
    ///   也被游标越过,不会被反复取到死循环**;(c)迁移期间用户新写的行(更大 rowid)
    ///   若已是引用格式被 `NOT LIKE` 过滤,旧格式则顺带迁移。
    /// - **只迁旧行**:`messages_json NOT LIKE '%__cat_msg__%'` 跳过已是引用形态的行。
    /// - **非破坏**:每行 parse→externalize→UPDATE 同一行;单行 parse/encode 失败仅
    ///   跳过(留 legacy,下次重迁),不中断整体。blob 写盘在事务**外**(FS),事务
    ///   rollback 只留孤儿 blob(GC 清),行保持 legacy → 幂等可重迁。
    /// - **收尾 VACUUM**:reformat 后大量 page 进 freelist,VACUUM 压实物理文件(临时
    ///   空间 ≈ 压缩后体积,非原始大小)。VACUUM 失败非致命(逻辑去重已生效)。
    ///
    /// 返回迁移的行数。
    pub fn migrate_existing_rows(&self) -> Result<usize, String> {
        // 幂等:已迁移直接早返(也挡掉纯内存 db=None)。
        {
            let mut guard = self.db.lock().expect("session cache db mutex poisoned");
            let Some(conn) = guard.as_mut() else {
                return Ok(0);
            };
            if migration_done(conn).map_err(|e| format!("read migration flag failed: {e}"))? {
                // MOC-171:已迁移 → 检查 VACUUM 是否本版本 pending("0")需补跑(解耦重试);
                // legacy(MOC-170)库无此标记 → ensure_vacuumed 内跳过,不无谓 VACUUM(P2)。
                ensure_vacuumed(conn).map_err(|e| format!("vacuum retry failed: {e}"))?;
                return Ok(0);
            }
        }

        let like = format!("%{MSG_REF_KEY}%");
        let mut cursor: i64 = 0;
        let mut total = 0usize;
        loop {
            // 每批独立 acquire db 锁 + 事务;批末 drop 锁让 serving 交错。
            let mut guard = self.db.lock().expect("session cache db mutex poisoned");
            let Some(conn) = guard.as_mut() else {
                return Ok(total); // db 中途没了(几乎不可能)→ 返已迁数
            };
            let batch: Vec<(i64, String, String)> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT rowid, response_id, messages_json FROM response_sessions \
                         WHERE rowid > ?1 AND messages_json NOT LIKE ?2 \
                         ORDER BY rowid LIMIT ?3",
                    )
                    .map_err(|e| format!("migrate prepare failed: {e}"))?;
                let rows = stmt
                    .query_map(params![cursor, like, MIGRATE_BATCH], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(|e| format!("migrate query failed: {e}"))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| format!("migrate collect failed: {e}"))?
            };
            if batch.is_empty() {
                break;
            }

            let blobs = self.blobs.as_ref();
            let tx = conn
                .transaction()
                .map_err(|e| format!("migrate tx begin failed: {e}"))?;
            for (rowid, response_id, json) in &batch {
                cursor = (*rowid).max(cursor); // 游标单调前进,坏行也越过(防死循环)
                let mut messages: Vec<Value> = match serde_json::from_str(json) {
                    Ok(m) => m,
                    Err(e) => {
                        log_db_warning(
                            "SESSIONS_DB_MIGRATE_ROW_SKIP",
                            format!("response_id={response_id} parse failed, leave legacy: {e}"),
                        );
                        continue;
                    }
                };
                externalize_for_storage(&tx, blobs, &mut messages);
                let reformatted = match serde_json::to_string(&messages) {
                    Ok(s) => s,
                    Err(e) => {
                        log_db_warning(
                            "SESSIONS_DB_MIGRATE_ROW_SKIP",
                            format!(
                                "response_id={response_id} re-encode failed, leave legacy: {e}"
                            ),
                        );
                        continue;
                    }
                };
                tx.execute(
                    "UPDATE response_sessions SET messages_json = ?1 WHERE rowid = ?2",
                    params![reformatted, rowid],
                )
                .map_err(|e| format!("migrate update failed: {e}"))?;
                total += 1;
            }
            tx.commit()
                .map_err(|e| format!("migrate commit failed: {e}"))?;
            drop(guard); // 显式:批间释放 db 锁
            tracing::debug!(
                error_id = "SESSIONS_DB_MIGRATE_PROGRESS",
                cursor,
                total,
                "sessions.db 存量迁移进度"
            );
        }

        // 全部迁移完:置迁移完成标志 + VACUUM 压实。MOC-171:VACUUM 与 migrated flag 解耦
        // —— migrated 先置(逻辑去重已 commit 生效,标记完成避免大表每次启动全表重扫),
        // `ensure_vacuumed` 内 VACUUM 成功才另置 vacuumed flag;失败仅 warn,下次启动经早
        // 返分支的 ensure_vacuumed 重试,直到成功(不再"VACUUM 失败 = 永久放弃回收")。
        let mut guard = self.db.lock().expect("session cache db mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            return Ok(total);
        };
        // MOC-171:**先**标记 VACUUM pending("0"),**再**置 migrated —— 顺序关键(codex-connector
        // P2):若 migrated 先,则 migrated 已置但 pending 写失败/crash 时,下次启动早返见
        // vacuumed 缺失会误判 legacy 跳过 VACUUM → 新迁移库丢失重试保证。先置 pending 后,
        // 最坏是"pending 置了 migrated 没置"→ 下次 migration_done=false 重迁、收尾重置,VACUUM
        // 不丢;由此"migrated=1 且 vacuumed 缺失"只可能是 legacy(MOC-170)库,跳过它才安全。
        meta_set(conn, VACUUM_FLAG_KEY, "0")
            .map_err(|e| format!("set vacuum pending failed: {e}"))?;
        set_migration_done(conn).map_err(|e| format!("set migration flag failed: {e}"))?;
        ensure_vacuumed(conn).map_err(|e| format!("vacuum failed: {e}"))?;
        Ok(total)
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
        // MOC-142 → MOC-168 两级外置(顺序 blob→msg):①每条消息里的大 `data:` 图 → blob
        // 引用(FS sidecar,写盘在事务外:内容寻址、孤儿可 GC、缺失走非破坏 cache-miss);
        // ②每条消息整体(已含 blob 引用)→ `message_contents` 引用(同 db 去重)。任一步单
        // 条失败留 inline(非破坏);L1 内存层始终存完整 inline。
        // **MOC-171**:②的 INSERT 与下面 `response_sessions` UPSERT 包进**同一事务**(与
        // `migrate_existing_rows` 对称)。否则各自 autocommit:UPSERT 失败时 message_contents
        // 已写却无行引用 → 瞬时孤儿(GC 会清但两写路径语义不一致)。tx rollback 时一起回滚。
        let mut slim: Vec<Value> = messages.to_vec();
        if let Some(store) = self.blobs.as_ref() {
            for m in &mut slim {
                store.externalize(m); // blob 外置写 FS,事务外
            }
        }
        let now = unix_now();
        let tx = conn.transaction()?;
        message_store::externalize(&tx, &mut slim); // message_contents INSERT,事务内
        let json = match serde_json::to_string(&slim) {
            Ok(s) => s,
            Err(e) => {
                // **silent-failure H1**:编码失败 warn + 跳过本次写入保留 L2 原 row(L1 已
                // save,本轮正常)。tx 未 commit,drop 即 rollback,刚 INSERT 的
                // message_contents 一起回滚、不留孤儿。
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
        tx.execute(
            "INSERT INTO response_sessions \
             (response_id, messages_json, created_unix, last_access_unix, access_count) \
             VALUES (?1, ?2, ?3, ?3, 0) \
             ON CONFLICT(response_id) DO UPDATE SET \
                 messages_json = excluded.messages_json, \
                 last_access_unix = excluded.last_access_unix",
            params![response_id, json, now],
        )?;
        tx.commit()?;
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
                // MOC-168:先把消息引用回填成整条消息(内部可能仍是 blob 引用)。任一引用
                // 缺失/损坏/db 错 → 本行无法完整回填,**非破坏**当 cache-miss 返回(不删行、
                // 不把引用对象泄漏给模型);留待 TTL 自然淘汰。
                if let Err(e) = message_store::inline(conn, &mut messages) {
                    let error_id = match e {
                        MsgInlineError::Missing(_) => "SESSIONS_DB_MSG_MISSING",
                        MsgInlineError::Corrupt(_) => "SESSIONS_DB_MSG_CORRUPT",
                        MsgInlineError::Db(_) => "SESSIONS_DB_MSG_DB_ERROR",
                    };
                    log_db_warning(
                        error_id,
                        format!(
                            "message inline failed for response_id={response_id}, \
                             serving cache-miss without delete: {e}"
                        ),
                    );
                    return Ok(None);
                }
                // MOC-142:再把 blob 引用回填成原始 `data:` 字符串。任一引用的 blob
                // 缺失/IO 错 → 同样**非破坏**当 cache miss(不删行、不泄漏引用)。纯内存
                // 模式(blobs=None)直接返回。
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

/// MOC-170 存量迁移每批行数。每批一个事务,批间释放 db 锁让正常 serving 交错。
const MIGRATE_BATCH: i64 = 200;

/// `sessions_meta` 里标记"存量内容寻址迁移已完成"的 key(幂等 guard)。
const MIGRATION_FLAG_KEY: &str = "content_addr_migrated";

/// `sessions_meta` 里 VACUUM 物理压实状态 key(MOC-171)。**三态**:`"1"`=已成功、
/// `"0"`=本版本迁移收尾标记的 pending(待跑/重试)、**缺失**=上一版本(MOC-170)迁移的
/// legacy 库(VACUUM 态未知,不打扰)。与 `MIGRATION_FLAG_KEY` 解耦让 migrated 先置(避免
/// 大表每次启动全表重扫),只 `"0"` 触发(重)跑 VACUUM —— 见 [`ensure_vacuumed`]。
const VACUUM_FLAG_KEY: &str = "content_addr_vacuumed";

/// 两级外置(MOC-142 blob → MOC-168 message,顺序固定):把一组消息**就地**转成
/// 入库瘦身形态 —— ①每条消息里的大 `data:` 图换成 blob 引用(FS sidecar);②每条
/// 消息整体(已含 blob 引用)换成 `message_contents` 引用(同 db 去重)。
/// `blobs=None`(纯内存 fallback / 无盘)跳过 blob 级、只做 message 级。任一单条失败
/// 留 inline(非破坏)。`persist_save`(write-through)与 `migrate_existing_rows`
/// (存量迁移)共用,保证两条写路径产出**完全一致**的引用形态。
///
/// `conn` 可传 `&Connection` 或 `&Transaction`(后者经 `Deref` 强转)。
fn externalize_for_storage(conn: &Connection, blobs: Option<&BlobStore>, messages: &mut [Value]) {
    if let Some(store) = blobs {
        for m in messages.iter_mut() {
            store.externalize(m);
        }
    }
    message_store::externalize(conn, messages);
}

/// 读 `sessions_meta` 里某个 key 的原始值(缺失 = `None`)。
fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM sessions_meta WHERE key = ?1",
        params![key],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// 置 `sessions_meta` 里某个 key 的值。
fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO sessions_meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

/// 读"存量迁移已完成"标志(MOC-170 幂等 guard)。
fn migration_done(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(meta_get(conn, MIGRATION_FLAG_KEY)?.as_deref() == Some("1"))
}

/// 置"存量迁移已完成"标志。
fn set_migration_done(conn: &Connection) -> rusqlite::Result<()> {
    meta_set(conn, MIGRATION_FLAG_KEY, "1")
}

/// MOC-171:迁移收尾 VACUUM 物理压实(可断点重试)。vacuumed [`VACUUM_FLAG_KEY`] **三态**:
/// - `"1"` = 已成功 → no-op;
/// - `"0"` = 本版本迁移收尾标记的 **pending**(尝试过/待跑)→ (重)跑;
/// - **缺失** = 上一版本(MOC-170)迁移的 legacy 库,VACUUM 态未知 → **不打扰**(保持
///   MOC-170 "不重试" 行为)。
///
/// 关键(codex-connector P2):只有明确写过 `"0"` 才(重)跑。否则 MOC-170 迁移过的升级
/// 用户库(`migrated=1` 但从无 vacuumed flag)会被早返分支每次启动无谓 VACUUM,大库
/// (尤其 MOC-170 时 VACUUM 已失败的 5.5G 库)磁盘不足时每次启动重试、持 db mutex 卡
/// serving。VACUUM 成功 → `"1"`;失败 → 留 `"0"` 下次重试。**必须在无活动事务时调**
/// (迁移收尾 / 早返分支的 `conn` 均无活动 tx)。
fn ensure_vacuumed(conn: &Connection) -> rusqlite::Result<()> {
    if meta_get(conn, VACUUM_FLAG_KEY)?.as_deref() != Some("0") {
        return Ok(()); // "1"=已成功 / 缺失=legacy(不打扰) → 不跑
    }
    match conn.execute_batch("VACUUM") {
        Ok(()) => meta_set(conn, VACUUM_FLAG_KEY, "1")?,
        Err(e) => log_db_warning(
            "SESSIONS_DB_MIGRATE_VACUUM_FAILED",
            format!(
                "VACUUM failed (physical space not reclaimed); logical dedup already \
                 live, will retry on next startup: {e}"
            ),
        ),
    }
    Ok(())
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
        // MOC-168:消息内容表 additive,无条件建(不进 SCHEMA_VERSION)。
        message_store::ensure_table(&conn)?;
        return Ok(conn);
    }
    create_schema_if_missing(&conn)?;
    // MOC-168:既有 db(只有 response_sessions)也补建 message_contents 表。
    message_store::ensure_table(&conn)?;
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
/// MOC-168 消息外置层(`message_contents` 同 db,故无独立备份/sweep-IO 问题):
/// - `SESSIONS_MSG_PUT_FAILED` — 消息 INSERT 失败,该消息留 inline(非破坏)
/// - `SESSIONS_DB_MSG_MISSING` / `SESSIONS_DB_MSG_CORRUPT` / `SESSIONS_DB_MSG_DB_ERROR` —
///   inline 时消息引用缺失 / json 损坏 / 查库错 → 本行**非破坏**当 cache-miss(不删行)
/// - `SESSIONS_MSG_CLEAR_FAILED` — 隐私清除删 `message_contents` 失败,已 Err 上报(500)
/// - `SESSIONS_MSG_SWEEP_FAILED` — 启动 GC mark 不完整 → abort,本轮不删任何消息
///
/// MOC-170 存量迁移(后台一次性 reformat 旧 inline 行 → 内容寻址引用):
/// - `SESSIONS_DB_MIGRATION_DONE` — 迁移完成(info 级,带 `migrated` 行数)
/// - `SESSIONS_DB_MIGRATION_NOOP` — 无需迁移(已迁移 / 无旧行 / 纯内存;debug 级)
/// - `SESSIONS_DB_MIGRATE_ROW_SKIP` — 单行 parse/encode 失败,跳过留 legacy(下次重迁)
/// - `SESSIONS_DB_MIGRATE_VACUUM_FAILED` — VACUUM 物理压实失败(逻辑去重已生效、非致命;
///   MOC-171:不置 vacuumed flag,下次启动经 `ensure_vacuumed` 重试直到成功)
/// - `SESSIONS_DB_MIGRATION_FAILED` — 迁移整体失败(未置完成标志,下次启动重试)
/// - `SESSIONS_DB_MIGRATION_PANIC` — 迁移线程 panic 被 catch_unwind 捕获(未置标志,下次重试)
/// - `SESSIONS_DB_MIGRATION_SPAWN_FAILED` — 迁移线程 spawn 失败(线程资源耗尽,罕见)
/// - `SESSIONS_DB_MIGRATE_PROGRESS` — 分批迁移进度(debug 级,带 `cursor` / `total`)
///
/// `error_id` 必须用 `&'static str`(literal)以保跨版本稳定;`detail` 给人类
/// 阅读的上下文(path / error message),不进 metric label。
///
/// **deferred suggestion**(type-design-analyzer):升级为 `enum SessionDbErrorId`
/// 获取 compile-time enforcement。本 PR 暂用 literal 跟 codebase 其他
/// `tracing::warn!(error_id=...)` 用法一致(`tool_call_cache.rs` / `request.rs`
/// 同模式),若未来 error_id 数量翻倍或出现拼写漂移再升级。
/// MOC-168:删 `message_contents` 里不在 `live` 集合的孤儿消息(事务批量,startup GC)。
fn sweep_message_contents(
    conn: &mut Connection,
    live: &HashSet<String>,
) -> rusqlite::Result<usize> {
    let all: Vec<String> = {
        let mut stmt = conn.prepare("SELECT hash FROM message_contents")?;
        let it = stmt.query_map([], |r| r.get::<_, String>(0))?;
        it.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let orphans: Vec<&String> = all.iter().filter(|h| !live.contains(*h)).collect();
    if orphans.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    {
        let mut del = tx.prepare("DELETE FROM message_contents WHERE hash = ?1")?;
        for h in &orphans {
            del.execute(params![h])?;
        }
    }
    tx.commit()?;
    Ok(orphans.len())
}

fn log_db_warning(error_id: &'static str, detail: String) {
    tracing::warn!(error_id, detail = %detail, "sessions.db");
    // 兼容老路径:Tauri proxy log file 收 stderr,保留 eprintln 兜底防 tracing
    // subscriber 未初始化(早期启动期 / unit test)时丢日志。
    eprintln!("warning: [{error_id}] {detail}");
}

/// info 级结构化日志(同 `log_db_warning` 的 stable `error_id` + eprintln 兜底约定,
/// 仅级别为 info)。用于迁移完成等正常里程碑。
fn log_db_info(error_id: &'static str, detail: String) {
    tracing::info!(error_id, detail = %detail, "sessions.db");
    eprintln!("info: [{error_id}] {detail}");
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
                // MOC-170:不再启动时按 TTL 清 L2(已改持久化,默认 100 年哨兵不
                // 过期)。`evict_expired_persisted` 机制保留供测试 / 未来可配置
                // retention,只是不在此自动调用。孤儿 GC(下面两步)仍保留:历史
                // 遗留(旧 30 天 TTL 删过的 row)/ 手动 evict / clear 都可能留孤儿。
                // MOC-168:**先**清悬挂消息(孤儿),**再**清 blob —— 这样 blob GC 扫
                // message_contents 时只看到活消息,过期图能一次清掉(否则孤儿 blob 要延
                // 一个重启周期才回收;codex-connector P2)。两步均失败仅 log。
                if let Err(e) = cache.sweep_orphan_messages() {
                    log_db_warning("SESSIONS_MSG_SWEEP_FAILED", e);
                }
                if let Err(e) = cache.sweep_orphan_blobs() {
                    log_db_warning("SESSIONS_BLOB_SWEEP_FAILED", e);
                }
                cache
            }
            None => ResponseSessionCache::new(DEFAULT_L1_SIZE, DEFAULT_L1_TTL),
        }
    })
}

/// MOC-170:后台启动存量迁移。spawn 一个**独立 std 线程**跑同步的
/// [`ResponseSessionCache::migrate_existing_rows`](SQLite 阻塞操作,不该占 tokio
/// worker)。fire-and-forget:迁移幂等(`content_addr_migrated` 标志),失败 / panic
/// 不影响 app —— 下次启动重试。
///
/// **必须从生产 app 启动调用**(`src-tauri` setup hook),**不要**塞进
/// [`global_response_session_cache`] 初始化 —— 否则集成测试调 `global().clear()` 会
/// 在**真机** `~/.codex-app-transfer/sessions.db` 上触发迁移(`sessions_db_file()`
/// 在 test 下仍返真实路径,无 cfg(test) 守卫),污染开发者本地库。
pub fn start_background_session_migration() {
    let spawned = std::thread::Builder::new()
        .name("cas-session-migrate".to_owned())
        .spawn(|| {
            // catch_unwind:迁移线程 panic(db mutex poisoned / 下游对畸形数据 panic 等)
            // 若不捕获只进 default panic hook(stderr)、无 stable error_id,telemetry /
            // Sentry 抓不到 → 违反 no-silent-failure 硬规则,且会"每次启动 panic→不置
            // flag→重试 panic"循环而可观测层寂静。捕获成 error_id 让 panic 路径也可聚合。
            let outcome = std::panic::catch_unwind(|| {
                global_response_session_cache().migrate_existing_rows()
            });
            match outcome {
                Ok(Ok(0)) => tracing::debug!(
                    error_id = "SESSIONS_DB_MIGRATION_NOOP",
                    "sessions.db 存量迁移:无需迁移(已迁移 / 无旧行 / 纯内存)"
                ),
                Ok(Ok(n)) => log_db_info(
                    "SESSIONS_DB_MIGRATION_DONE",
                    format!("存量迁移完成:{n} 行 reformat 成内容寻址引用"),
                ),
                Ok(Err(e)) => log_db_warning("SESSIONS_DB_MIGRATION_FAILED", e),
                Err(_) => log_db_warning(
                    "SESSIONS_DB_MIGRATION_PANIC",
                    "migration thread panicked (caught); 未置完成标志,下次启动重试".to_owned(),
                ),
            }
        });
    if let Err(e) = spawned {
        log_db_warning(
            "SESSIONS_DB_MIGRATION_SPAWN_FAILED",
            format!("could not spawn migration thread: {e}"),
        );
    }
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
        // MOC-168:消息整体也被外置 → 行存的是**消息引用**(`__cat_msg__`);blob 引用
        // (`__cat_session_blob__`)在 `message_contents` 里(下一级)。两级都不含 base64。
        assert!(raw.contains("__cat_msg__"), "L2 行应存消息引用,实际 {raw}");
        assert!(
            !raw.contains("__cat_session_blob__"),
            "blob 引用应在 message_contents 而非 row,实际 {raw}"
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
    fn save_dedups_repeated_messages_and_get_rehydrates() {
        // MOC-168 端到端:逐轮快照共享消息只存一份,L2 行存引用,get 回填字节级还原。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        let t1 = vec![
            json!({"role": "system", "content": "S"}),
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let mut t2 = t1.clone();
        t2.push(json!({"role": "user", "content": "u2"}));
        t2.push(json!({"role": "assistant", "content": "a2"}));
        cache.save("resp_t1", t1.clone());
        cache.save("resp_t2", t2.clone());

        // 5 条唯一消息(S,u1,a1,u2,a2),即便跨两行共出现 8 次
        let uniq: i64 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM message_contents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uniq, 5, "共享消息去重:应只 5 条唯一");

        // L2 行存引用、不含原文
        let raw: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT messages_json FROM response_sessions WHERE response_id = 'resp_t2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(raw.contains(MSG_REF_KEY), "行应存消息引用");
        assert!(!raw.contains("\"a2\""), "行不应含消息原文");

        // 清 L1 强制走 L2 + 回填,字节级还原
        cache.clear();
        assert_eq!(
            cache.get("resp_t2").expect("L2 回填"),
            t2,
            "回填字节级等于原始"
        );
        assert_eq!(cache.get("resp_t1").expect("L2 回填"), t1);
    }

    #[test]
    fn sweep_orphan_messages_removes_unreferenced() {
        // 过期 row 删后其独有消息成孤儿 → 启动 GC 清掉。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            Duration::from_secs(1),
            &path,
        );
        cache.save(
            "r_old",
            vec![json!({"role": "user", "content": "orphan-me"})],
        );
        std::thread::sleep(Duration::from_millis(1100));
        cache.evict_expired_persisted().unwrap(); // 删过期 row → 消息成孤儿
        let removed = cache.sweep_orphan_messages().unwrap();
        assert!(removed >= 1, "孤儿消息应被清,实际 {removed}");
        let cnt: i64 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM message_contents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 0, "孤儿消息应清零");
    }

    #[test]
    fn clear_all_persisted_also_removes_messages() {
        // 隐私清除连带清 message_contents。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        cache.save("r", vec![json!({"role": "user", "content": "secret"})]);
        let count = |p: &std::path::Path| -> i64 {
            Connection::open(p)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM message_contents", [], |r| r.get(0))
                .unwrap()
        };
        assert!(count(&path) >= 1, "save 后应有消息");
        cache.clear_all_persisted().unwrap();
        assert_eq!(count(&path), 0, "clear 应连带清 message_contents");
    }

    #[test]
    fn sweep_orphan_blobs_keeps_blob_referenced_via_message_store() {
        // MOC-168 回归(code-reviewer BLOCKER):消息外置后 blob 引用嵌在 message_contents
        // 里,行只剩 msg 引用。启动 blob GC 必须也扫 message_contents,否则把仍被引用的图
        // 当孤儿删 → 图片历史丢。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        let big_image = format!("data:image/png;base64,{}", "Q".repeat(20_000));
        let messages = vec![json!({
            "role": "user",
            "content": [{"type": "input_image", "image_url": big_image}],
        })];
        cache.save("resp_img2", messages.clone());

        let removed = cache.sweep_orphan_blobs().unwrap();
        assert_eq!(
            removed, 0,
            "经 message_contents 引用的活 blob 不应被删,实际 {removed}"
        );

        // 清 L1 强制走 L2,仍能两级完整回填
        cache.clear();
        assert_eq!(
            cache.get("resp_img2").expect("L2 应能两级回填"),
            messages,
            "回填后字节级等于原始"
        );
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

    // ── MOC-170 存量迁移 + 持久化测试 ─────────────────────────────────

    /// 直接 raw INSERT 一条"旧格式"行(完整 inline、无内容寻址引用),绕过 save 的
    /// 自动外置,模拟 MOC-142/168 上线前写入的存量数据。
    fn insert_legacy_row(path: &std::path::Path, id: &str, messages: &Value) {
        let conn = Connection::open(path).unwrap();
        let now = unix_now();
        conn.execute(
            "INSERT INTO response_sessions \
             (response_id, messages_json, created_unix, last_access_unix, access_count) \
             VALUES (?1, ?2, ?3, ?3, 0)",
            params![id, messages.to_string(), now],
        )
        .unwrap();
    }

    #[test]
    fn migrate_reformats_legacy_inline_rows() {
        // 存量旧行(完整 inline 大图 + 逐轮共享消息)迁移后 → 两级内容寻址引用、图
        // 去重落 blob、消息去重、get 字节级还原。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        let big = format!("data:image/png;base64,{}", "A".repeat(20_000));
        let shared = json!({"role": "system", "content": "shared-prompt"});
        let img_user = json!({
            "role": "user",
            "content": [{"type": "input_image", "image_url": big}],
        });
        // t1 = [shared, img_user];t2 = [shared, img_user, asst](共享前两条)。
        let t1 = json!([shared, img_user]);
        let t2 = json!([shared, img_user, {"role": "assistant", "content": "a"}]);
        insert_legacy_row(&path, "r1", &t1);
        insert_legacy_row(&path, "r2", &t2);

        let migrated = cache.migrate_existing_rows().unwrap();
        assert_eq!(migrated, 2, "两条旧行都应迁移");

        // 行已 reformat:不含 inline base64,改存消息引用。
        let raw1: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT messages_json FROM response_sessions WHERE response_id='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!raw1.contains("data:image"), "迁移后行不应有 inline base64");
        assert!(raw1.contains(MSG_REF_KEY), "迁移后行应存消息引用");

        // 消息去重:5 实例(shared×2, img_user×2, asst×1)→ 3 唯一。
        let uniq: i64 = Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM message_contents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uniq, 3, "共享消息去重:5 实例 → 3 唯一");

        // 大图外置到 blobs/。
        assert!(
            path.parent().unwrap().join("blobs").exists(),
            "图应外置到 blobs/"
        );

        // 清 L1 强制走 L2 两级回填,字节级还原。
        cache.clear();
        assert_eq!(
            cache.get("r1").unwrap(),
            *t1.as_array().unwrap(),
            "r1 回填字节级等于原始"
        );
        assert_eq!(
            cache.get("r2").unwrap(),
            *t2.as_array().unwrap(),
            "r2 回填字节级等于原始"
        );
    }

    #[test]
    fn migrate_is_idempotent() {
        // 迁移完成后置标志,重复调用早返 0,不重复处理。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        insert_legacy_row(&path, "r", &json!([{"role": "user", "content": "x"}]));
        assert_eq!(cache.migrate_existing_rows().unwrap(), 1, "首次迁移 1 行");
        assert_eq!(
            cache.migrate_existing_rows().unwrap(),
            0,
            "再次调用应早返 0(flag guard)"
        );
        // flag 已置 → 即便再插旧行也不迁(生产中 flag 后所有写入都走 save 外置,
        // 不会再有旧格式行;此处仅验证 guard 行为)。
        insert_legacy_row(&path, "r2", &json!([{"role": "user", "content": "y"}]));
        assert_eq!(
            cache.migrate_existing_rows().unwrap(),
            0,
            "flag 已置后直接早返,不再迁移"
        );
    }

    #[test]
    fn migrate_skips_corrupt_row_and_terminates() {
        // 坏行(messages_json 非合法 JSON)被跳过留 legacy,rowid 游标保证不死循环;
        // 好行正常迁移。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        {
            let conn = Connection::open(&path).unwrap();
            let now = unix_now();
            conn.execute(
                "INSERT INTO response_sessions \
                 (response_id, messages_json, created_unix, last_access_unix, access_count) \
                 VALUES ('bad', 'not-json{{', ?1, ?1, 0)",
                params![now],
            )
            .unwrap();
        }
        insert_legacy_row(&path, "good", &json!([{"role": "user", "content": "ok"}]));

        let migrated = cache.migrate_existing_rows().unwrap();
        assert_eq!(migrated, 1, "只迁好行,坏行跳过");
        // 坏行原样留存(legacy)。
        let bad_raw: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT messages_json FROM response_sessions WHERE response_id='bad'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad_raw, "not-json{{", "坏行不动,留 legacy");
    }

    #[test]
    fn persistent_default_ttl_never_expires_old_rows() {
        // 默认 TTL 是 ~100 年哨兵 → 即使 row 的 last_access 很旧(1970)也能 load
        // (cutoff = now - 100y < 0 < 1,任何 row 都命中)。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            1,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        cache.save(
            "r_persist",
            vec![json!({"role": "user", "content": "keep-me-forever"})],
        );
        // 手动把 last_access_unix 改成 1(1970),模拟"很久没访问的老会话"。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE response_sessions SET last_access_unix = 1 WHERE response_id = 'r_persist'",
                [],
            )
            .unwrap();
        }
        cache.clear(); // 清 L1 强制走 L2
        assert!(
            cache.get("r_persist").is_some(),
            "持久化默认:老会话不应过期"
        );
    }

    #[test]
    fn migrate_sets_vacuum_flag_on_success() {
        // MOC-171:迁移成功 → migrated + vacuumed 两个 flag 都置(VACUUM 与迁移解耦但都成功)。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        insert_legacy_row(&path, "r", &json!([{"role": "user", "content": "x"}]));
        cache.migrate_existing_rows().unwrap();
        let conn = Connection::open(&path).unwrap();
        let flag = |k: &str| -> Option<String> {
            conn.query_row(
                "SELECT value FROM sessions_meta WHERE key = ?1",
                params![k],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
        };
        assert_eq!(flag("content_addr_migrated").as_deref(), Some("1"));
        assert_eq!(
            flag("content_addr_vacuumed").as_deref(),
            Some("1"),
            "VACUUM 成功应置 vacuumed flag"
        );
    }

    #[test]
    fn vacuum_retries_when_pending_after_migration() {
        // MOC-171:模拟"迁移完成但上次 VACUUM 失败(vacuumed 缺)"→ 再调 migrate_existing_rows
        // 应经早返分支补跑 VACUUM 并置 vacuumed(不重扫表、不重迁)。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        insert_legacy_row(&path, "r", &json!([{"role": "user", "content": "x"}]));
        cache.migrate_existing_rows().unwrap();
        // 手动把 vacuumed 改回 pending("0"),模拟本版本上次 VACUUM 失败留待重试。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO sessions_meta (key, value) VALUES ('content_addr_vacuumed', '0')",
                [],
            )
            .unwrap();
        }
        // 再调:migrated=1 早返 Ok(0),ensure_vacuumed 见 "0" pending → 补跑 VACUUM。
        assert_eq!(cache.migrate_existing_rows().unwrap(), 0, "已迁移早返 0");
        let conn = Connection::open(&path).unwrap();
        let vacuumed: Option<String> = conn
            .query_row(
                "SELECT value FROM sessions_meta WHERE key = 'content_addr_vacuumed'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            vacuumed.as_deref(),
            Some("1"),
            "VACUUM pending 应在下次调用补跑并置 flag"
        );
    }

    #[test]
    fn legacy_migrated_db_not_vacuumed_on_upgrade() {
        // codex-connector P2:MOC-170 迁移过的 legacy 库(migrated=1 但 vacuumed 从无,因
        // MOC-170 没这 flag)升级 MOC-171,早返分支**不应**无谓 VACUUM —— vacuumed 保持缺失
        // (不打扰),避免升级用户已 vacuumed 大库每次启动重跑 VACUUM、失败则每次重试卡 mutex。
        let (_dir, path) = fresh_db_path();
        let (cache, _) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        // 模拟 legacy MOC-170 状态:只置 migrated,无 vacuumed(MOC-170 没这个 flag)。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO sessions_meta (key, value) VALUES ('content_addr_migrated', '1')",
                [],
            )
            .unwrap();
        }
        // 升级 MOC-171 首次启动:migrated=1 早返,ensure_vacuumed 见 vacuumed 缺失 → 不 VACUUM。
        assert_eq!(
            cache.migrate_existing_rows().unwrap(),
            0,
            "migrated=1 早返 0"
        );
        let vacuumed: Option<String> = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT value FROM sessions_meta WHERE key = 'content_addr_vacuumed'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            vacuumed, None,
            "legacy 库(无 pending 标记)不应被无谓 VACUUM,vacuumed 保持缺失"
        );
    }

    // 回归(真机 blob 删除根因):clear_all_persisted 在 db=None(db init 失败 fallback 纯
    // 内存 / 瞬时撞锁)时**不删 blob**、返回 Err —— 避免删光仍被磁盘 db 行有效引用的 blob
    // 留下悬挂引用(含图历史丢)。隐私清除在 db 不可达时失败而非部分成功,用户重试即可。
    #[cfg(unix)] // 用 chmod 000 模拟 db init 失败,仅 Unix 适用(Windows 无 PermissionsExt)
    #[test]
    fn clear_with_db_none_errors_and_preserves_blobs() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = fresh_db_path();
        let big = format!("data:image/png;base64,{}", "A".repeat(20_000));
        // 进程A:正常 db,迁移含图 → blob + message_contents + response_sessions 持久磁盘。
        {
            let (cache, w) = ResponseSessionCache::with_db_path(
                8,
                Duration::from_secs(60),
                DEFAULT_PERSISTED_TTL,
                &path,
            );
            assert!(w.is_none(), "进程A db 应正常 init");
            for i in 0..3 {
                let row = json!([{"role":"user","content":[{"type":"image_url","image_url":{"url": big.clone()}}]}]);
                insert_legacy_row(&path, &format!("r{i}"), &row);
            }
            cache.migrate_existing_rows().unwrap();
        }
        let blobdir = path.parent().unwrap().join("blobs");
        let count_blobs = |d: &std::path::Path| -> usize {
            let Ok(shards) = std::fs::read_dir(d) else {
                return 0;
            };
            shards
                .flatten()
                .filter_map(|s| std::fs::read_dir(s.path()).ok())
                .flat_map(|f| f.flatten())
                .filter(|f| {
                    f.file_name()
                        .to_str()
                        .map(|n| !n.starts_with(".tmp."))
                        .unwrap_or(false)
                })
                .count()
        };
        let count_rows = |sql: &str| -> i64 {
            Connection::open(&path)
                .unwrap()
                .query_row(sql, [], |r| r.get(0))
                .unwrap()
        };
        assert!(count_blobs(&blobdir) > 0, "迁移后应有 blob");
        let mc_before = count_rows("SELECT COUNT(*) FROM message_contents");
        let rs_before = count_rows("SELECT COUNT(*) FROM response_sessions");
        assert!(mc_before > 0 && rs_before > 0);

        // 进程B:chmod 000 让 sessions.db 打不开 → db init 失败 → db=None;blobs 层仍指真实目录。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (cache_b, warn_b) = ResponseSessionCache::with_db_path(
            8,
            Duration::from_secs(60),
            DEFAULT_PERSISTED_TTL,
            &path,
        );
        assert!(warn_b.is_some(), "db 应 init 失败 fallback 纯内存(db=None)");
        // 用户点「清除会话历史」→ POST /api/sessions/clear → clear_all_persisted。
        let result = cache_b.clear_all_persisted();

        // 恢复权限后验证:修复后 blob 应保留、db 行不变、clear 返回 Err。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let blobs_after = count_blobs(&blobdir);
        let mc_after = count_rows("SELECT COUNT(*) FROM message_contents");
        let rs_after = count_rows("SELECT COUNT(*) FROM response_sessions");

        // 修复后正确行为:db=None 时整体失败、blob 原样保留、db 行不动(无悬挂)。
        assert!(
            result.is_err(),
            "db=None 时 clear 应返回 Err(隐私清除失败而非部分成功)"
        );
        assert!(
            blobs_after > 0,
            "修复:db=None 时 blob 必须保留(不删,避免悬挂)"
        );
        assert_eq!(mc_after, mc_before, "db 行保留不变");
        assert_eq!(rs_after, rs_before, "db 行保留不变");
    }
}
