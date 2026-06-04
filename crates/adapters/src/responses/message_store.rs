//! Content-addressed **message** sidecar for the response session cache (MOC-168).
//!
//! ## 为什么需要
//!
//! Codex stateless 每轮回放全量历史,`ResponseSessionCache` 又是"每轮整存一行快照"
//! 模型 → 同一条消息(user / assistant / tool 文字 + 工具调用/输出)被几十上百轮各
//! 存一份。实测最近 800 行:**11 万条消息实例 → 仅 2,687 条唯一(平均 41×)**,消息
//! 级去重省 **97%**。MOC-142 的 blob 外置解决了图片侧(63%),本模块收文字/tool 侧。
//!
//! ## 借鉴 & 机制
//!
//! Codex rollout 是 per-session append-only、每个 event 只落一次。我们 stateless 无
//! `previous_response_id` 串联,改用**内容寻址**达到同样"每条唯一消息只存一份":
//! `messages_json` 数组里**每条消息**按 sha256 存进 `message_contents(hash, json)`
//! 表(`INSERT OR IGNORE` 天然去重),数组只留轻量引用 `{"__cat_msg__":"<hash>"}`;
//! 读回时按 hash 回填整条。跟 MOC-142 范式同构,只是把粒度从"大 `data:` 串"下沉到
//! "每条消息",存储从文件系统换成同 db 的 SQLite 表(消息多而小,表优于上万碎文件)。
//!
//! ## 与 blob 的组合(两级)
//!
//! 含图消息在 [`externalize`] **之前**已由 blob 层把 `data:` 图换成 blob 引用,所以
//! 这里存进 `message_contents` 的是"带 blob 引用的消息";读回时 [`inline`] 先回填
//! 消息(含 blob 引用),再由 blob 层回填图。save 顺序 blob→msg,load 顺序 msg→blob。
//!
//! ## 边界
//!
//! - `INSERT` 失败的单条消息**留 inline**(非破坏);[`inline`] 时 hash 缺失/损坏 →
//!   调用方按 row cache-miss 处理(**不删行**、不把引用泄漏给模型)。
//! - 只把对象 `{"__cat_msg__":"<64 hex>"}` 当引用(hex 闸门挡 lookalike 用户内容)。
//! - 表是 additive、**不 bump SCHEMA_VERSION**:旧整存行(完整消息)照读、新行存引用。

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// `messages_json` 里消息引用的 sentinel key(区别于 blob 的 `__cat_session_blob__`)。
pub(crate) const MSG_REF_KEY: &str = "__cat_msg__";

/// `inline` 回填失败原因。调用方据此把整行当 cache-miss(非破坏,不删行)。
#[derive(Debug)]
pub(crate) enum MsgInlineError {
    /// 引用存在但 `message_contents` 无此 hash(被 GC 误删 / db 损坏)。
    Missing(String),
    /// 查 `message_contents` 的 sqlite 错误。
    Db(rusqlite::Error),
    /// `message_contents.json` 本身 parse 失败(磁盘损坏 / 手工改库)。
    Corrupt(String),
}

impl std::fmt::Display for MsgInlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MsgInlineError::Missing(h) => write!(f, "message {h} missing"),
            MsgInlineError::Db(e) => write!(f, "message lookup db error: {e}"),
            MsgInlineError::Corrupt(h) => write!(f, "message {h} json corrupt"),
        }
    }
}

/// 建 `message_contents` 表(idempotent,additive)。在 `init_db` 里**无条件**调用,
/// 让既有 db(只有 `response_sessions`)也补上这张表,不依赖 schema 重建。
pub(crate) fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_contents (\
             hash TEXT PRIMARY KEY, \
             json TEXT NOT NULL\
         );",
    )
}

/// 把数组里**每条消息**外置成引用:序列化 → sha256 → `INSERT OR IGNORE`(去重)→
/// 数组元素替换成 `{"__cat_msg__":hash}`。单条 `INSERT` 失败就留 inline(非破坏)。
/// 返回外置了几条。**只处理顶层数组元素**,不递归进消息内部(内部 `data:` 图已由
/// blob 层在更早一步外置)。
pub(crate) fn externalize(conn: &Connection, messages: &mut [Value]) -> usize {
    let mut n = 0;
    for m in messages.iter_mut() {
        if as_msg_ref(m).is_some() {
            continue; // 已是引用(save 路径不该出现,稳妥跳过)
        }
        let s = match serde_json::to_string(m) {
            Ok(s) => s,
            Err(_) => continue, // 编码不了就留 inline
        };
        let hash = sha256_hex(s.as_bytes());
        match conn.execute(
            "INSERT OR IGNORE INTO message_contents (hash, json) VALUES (?1, ?2)",
            params![hash, s],
        ) {
            Ok(_) => {
                *m = make_msg_ref(&hash);
                n += 1;
            }
            Err(e) => warn(
                "SESSIONS_MSG_PUT_FAILED",
                format!("message INSERT failed, leaving inline: {e}"),
            ),
        }
    }
    n
}

/// 把引用还原成整条消息。任一引用缺失/损坏/db 错 → 整体返 `Err`(调用方按 row
/// cache-miss 处理,绝不把引用对象泄漏给模型)。非引用元素(旧整存格式 / inline
/// 兜底)原样保留。返回还原了几条。
///
/// **契约**:返 `Err` 时 `messages` 可能已被**部分**回填 —— 调用方必须整行丢弃。
pub(crate) fn inline(conn: &Connection, messages: &mut [Value]) -> Result<usize, MsgInlineError> {
    let mut n = 0;
    for m in messages.iter_mut() {
        let Some(hash) = as_msg_ref(m) else {
            continue;
        };
        let hash = hash.to_owned();
        let json: Option<String> = conn
            .query_row(
                "SELECT json FROM message_contents WHERE hash = ?1",
                params![hash],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(MsgInlineError::Db)?;
        let Some(json) = json else {
            return Err(MsgInlineError::Missing(hash));
        };
        let val: Value =
            serde_json::from_str(&json).map_err(|_| MsgInlineError::Corrupt(hash.clone()))?;
        *m = val;
        n += 1;
    }
    Ok(n)
}

/// 收集数组里所有消息引用的 hash(GC mark 用)。
pub(crate) fn collect_hashes(messages: &[Value], out: &mut HashSet<String>) {
    for m in messages {
        if let Some(hash) = as_msg_ref(m) {
            out.insert(hash.to_owned());
        }
    }
}

fn as_msg_ref(value: &Value) -> Option<&str> {
    let obj = value.as_object()?;
    // **必须是 `make_msg_ref` 造的精确形态**:对象**仅** `__cat_msg__` 一个键、值是合法
    // 64 位 hex。不能只查"含此键" —— 否则一条真实消息若恰好带名为 `__cat_msg__` 的
    // 元数据字段(且值像 hex),会被误判成引用 → cache-miss 或(hash 撞库)回填成无关
    // 消息、损坏历史(codex-connector P2)。畸形/伪造的当普通内容原样穿过。
    if obj.len() != 1 {
        return None;
    }
    let hash = obj.get(MSG_REF_KEY)?.as_str()?;
    is_sha256_hex(hash).then_some(hash)
}

fn make_msg_ref(hash: &str) -> Value {
    let mut obj = serde_json::Map::with_capacity(1);
    obj.insert(MSG_REF_KEY.to_owned(), Value::String(hash.to_owned()));
    Value::Object(obj)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn warn(error_id: &'static str, detail: String) {
    tracing::warn!(error_id, detail = %detail, "sessions message store");
    eprintln!("warning: [{error_id}] {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_table(&c).unwrap();
        c
    }

    fn msgs() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "you are helpful"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ]
    }

    #[test]
    fn externalize_then_inline_round_trips() {
        let c = conn();
        let original = msgs();
        let mut v = original.clone();
        assert_eq!(externalize(&c, &mut v), 3);
        // 已变引用形态:不再含原文
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains(MSG_REF_KEY) && !s.contains("you are helpful"));
        assert_eq!(inline(&c, &mut v).unwrap(), 3);
        assert_eq!(v, original, "回填后必须字节级等于原始");
    }

    #[test]
    fn identical_messages_dedupe_to_one_row() {
        let c = conn();
        // 模拟逐轮快照:turn1 = [a,b],turn2 = [a,b,c](共享 a,b)
        let mut t1 = vec![msgs()[0].clone(), msgs()[1].clone()];
        let mut t2 = msgs();
        externalize(&c, &mut t1);
        externalize(&c, &mut t2);
        // 共 3 条唯一(a,b,c),即便出现在 5 个位置
        let cnt: i64 = c
            .query_row("SELECT COUNT(*) FROM message_contents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 3, "共享消息只应存一份");
    }

    #[test]
    fn missing_message_reports_error_not_leak() {
        let c = conn();
        let mut v = msgs();
        externalize(&c, &mut v);
        // 删掉 message_contents 模拟缺失
        c.execute("DELETE FROM message_contents", []).unwrap();
        match inline(&c, &mut v) {
            Err(MsgInlineError::Missing(_)) => {}
            other => panic!("缺失应报 Missing,实际 {other:?}"),
        }
    }

    #[test]
    fn crafted_non_hex_ref_is_not_treated_as_ref() {
        let c = conn();
        // 伪造引用:hash 非 hex → 当普通内容,原样穿过,不查库
        let mut v = vec![json!({"__cat_msg__": "../../../../etc/passwd"})];
        let before = v.clone();
        assert_eq!(inline(&c, &mut v).unwrap(), 0);
        assert_eq!(v, before);
        let mut set = HashSet::new();
        collect_hashes(&v, &mut set);
        assert!(set.is_empty());
    }

    #[test]
    fn collect_hashes_gathers_refs() {
        let c = conn();
        let mut v = msgs();
        externalize(&c, &mut v);
        let mut set = HashSet::new();
        collect_hashes(&v, &mut set);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn old_inline_messages_pass_through_inline() {
        // 旧整存格式(完整消息、无引用)→ inline 原样保留(向后兼容)。
        let c = conn();
        let mut v = msgs();
        let before = v.clone();
        assert_eq!(inline(&c, &mut v).unwrap(), 0, "无引用 → 不还原任何");
        assert_eq!(v, before);
    }

    #[test]
    fn message_with_cat_msg_metadata_field_is_not_a_ref() {
        // codex-connector P2:真实消息恰好带名为 __cat_msg__ 的字段(值像 hex),对象不止
        // 1 键 → **不能**当引用。必须正常 externalize(按内容存)+ inline 字节级还原。
        let c = conn();
        let hexish = "a".repeat(64);
        let mut v = vec![json!({"role": "user", "content": "hi", "__cat_msg__": hexish})];
        let original = v.clone();
        assert_eq!(
            externalize(&c, &mut v),
            1,
            "带 __cat_msg__ 元数据字段的真实消息应被正常外置,而非误当引用跳过"
        );
        assert_eq!(inline(&c, &mut v).unwrap(), 1);
        assert_eq!(v, original, "回填后字节级还原(含 __cat_msg__ 元数据字段)");
    }
}
