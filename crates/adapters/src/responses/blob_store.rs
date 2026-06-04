//! Content-addressed blob sidecar for the response session cache (MOC-142).
//!
//! ## 为什么需要
//!
//! Codex 走 stateless,每轮把**全量历史**(含早期粘进来的截图 inline base64)发
//! 回来;[`super::session::ResponseSessionCache`] 又是"每轮整存一行快照"模型 →
//! 同一张图被几十上百轮各存一份。实测:64 张唯一图被存 5500 次(平均 86×、最高
//! 471×)= 3.35GB 重复(占 `sessions.db` 63%)。
//!
//! ## 借鉴
//!
//! 思路对齐 Codex `~/.codex/generated_images/<session>/ig_<hash>.png` 与 Claude
//! Code `~/.claude/paste-cache/<hash>.txt` 的**内容寻址外部存储**:大 `data:` blob
//! 按 sha256 落成独立文件(文件名即哈希 → 天然去重),`messages_json` 里只留一个
//! 轻量引用。读回时回填成原始 `data:` 字符串,对下游字节级无感。
//!
//! ## 边界
//!
//! - 只外置 `data:` 开头且 ≥ [`BLOB_INLINE_THRESHOLD`] 的字符串(图片/文件 blob);
//!   小内容、纯文本 tool 输出**不动**(文字侧跨轮去重见 append-only followup
//!   MOC-168)。
//! - 存**原始字符串字节**(非解码二进制)→ 回填字节级精确,放弃 base64 的 ~33%
//!   膨胀;去重(86×)才是主收益,精确性 > 这点空间。
//! - `put` 失败 → 留 inline(非破坏);`inline` 时 blob 缺失 → 调用方按 row 损坏
//!   处理(当 cache miss),**不**把引用对象泄漏给模型。

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 只外置 ≥ 8KiB 的 `data:` 字符串。图片 base64 必然超过;小 data URL(光标图标
/// 之类)留 inline 避免无谓的文件碎片。
pub(crate) const BLOB_INLINE_THRESHOLD: usize = 8 * 1024;

/// `messages_json` 里 blob 引用的 sentinel key。私有键,正常对话内容不会出现;
/// `inline` 只在「对象含此键且其值带**合法 64 位 hex** `sha256`」时才当引用还原
/// —— hex 闸门同时挡掉伪造 `sha256`(路径穿越)与误把 lookalike 用户内容当引用。
/// `pub(crate)`:`session.rs` 的 GC `LIKE` 扫描复用同一常量,避免魔法串多处漂移。
pub(crate) const BLOB_REF_KEY: &str = "__cat_session_blob__";

/// 文件系统内容寻址 blob 存储。根目录通常是 `~/.codex-app-transfer/blobs/`
/// (由 `ResponseSessionCache` 从 `sessions.db` 同级推导)。
#[derive(Debug)]
pub(crate) struct BlobStore {
    root: PathBuf,
}

/// `inline` 回填失败原因。调用方(`persist_load`)据此把整行当损坏处理。
#[derive(Debug)]
pub(crate) enum InlineError {
    /// 引用存在但 blob 文件已不在(被 GC 误删 / 用户删了 blobs 目录 / db 跨机
    /// 拷贝没带 blobs)。
    Missing(String),
    /// 读 blob 文件 IO 错误。
    Io(io::Error),
}

impl std::fmt::Display for InlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InlineError::Missing(hash) => write!(f, "blob {hash} missing"),
            InlineError::Io(e) => write!(f, "blob read io error: {e}"),
        }
    }
}

/// `sweep` 结果:成功删除的 blob 数 + **失败**数(分片读不出 / 文件删不掉)。
/// 隐私清除(`clear_all_persisted` → `POST /api/sessions/clear`)据 `failed > 0`
/// 上报"没清干净"(私密图片可能残留),不能 best-effort 静默成功;启动孤儿 GC
/// 则仅观测 `failed`、不因此 abort(单个锁文件不该挡住其余回收)。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SweepStats {
    pub removed: usize,
    pub failed: usize,
}

impl BlobStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 写入一个 blob,返回其 sha256 hex。已存在则跳过写(去重)。
    fn put(&self, data: &str) -> io::Result<String> {
        let hash = sha256_hex(data.as_bytes());
        let final_path = self.path_for(&hash);
        if final_path.exists() {
            return Ok(hash);
        }
        let Some(shard) = final_path.parent() else {
            return Err(io::Error::other("blob path has no parent"));
        };
        fs::create_dir_all(shard)?;
        // 原子落盘:同目录 temp + rename(同一文件系统内 rename 原子)。temp 名带
        // nanos+seq 防同 hash 并发 put 撞 temp;两个并发 put 同内容 → rename 幂等。
        let tmp = shard.join(tmp_name(&hash));
        fs::write(&tmp, data.as_bytes())?;
        match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(hash),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // 并发 put 可能已把 final 建好 → 视作成功
                if final_path.exists() {
                    Ok(hash)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// 读一个 blob;不存在 → `Ok(None)`。
    fn get(&self, hash: &str) -> io::Result<Option<String>> {
        // 防线(BLOCKER:path traversal):hash 只能是 64 位 hex,否则绝不碰文件系统
        // —— `hash` 可能来自 messages_json 里伪造的 `sha256`(如 `../../etc/passwd`)。
        if !is_sha256_hex(hash) {
            return Ok(None);
        }
        match fs::read_to_string(self.path_for(hash)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        // git 式两位分片,避免单目录上万文件。
        let shard = hash.get(0..2).unwrap_or("00");
        self.root.join(shard).join(hash)
    }

    /// 递归把 value 里超阈值的 `data:` 字符串外置成引用。best-effort:单个 `put`
    /// 失败就留 inline(非破坏)。返回外置了几个。
    pub(crate) fn externalize(&self, value: &mut Value) -> usize {
        match value {
            Value::String(s) if is_blob_candidate(s) => match self.put(s) {
                Ok(hash) => {
                    let bytes = s.len();
                    *value = make_blob_ref(hash, bytes);
                    1
                }
                Err(e) => {
                    warn(
                        "SESSIONS_BLOB_PUT_FAILED",
                        format!("put blob failed, leaving inline: {e}"),
                    );
                    0
                }
            },
            Value::Array(arr) => arr.iter_mut().map(|v| self.externalize(v)).sum(),
            Value::Object(map) => map.iter_mut().map(|(_, v)| self.externalize(v)).sum(),
            _ => 0,
        }
    }

    /// 递归把引用还原成原始 `data:` 字符串。任一引用的 blob 缺失/IO 错 → 整体
    /// 返 `Err`(调用方按 row 损坏处理),不把引用对象泄漏给模型。返回还原了几个
    /// (计数仅供测试 / 观测,生产调用方忽略)。
    ///
    /// **契约**:返 `Err` 时 `value` 可能已被**部分**回填(失败 message 之前的引用
    /// 已展开)—— 调用方必须**整行丢弃**,绝不可使用半回填的 `value`。
    pub(crate) fn inline(&self, value: &mut Value) -> Result<usize, InlineError> {
        if let Some(hash) = as_blob_ref(value) {
            let hash = hash.to_owned();
            return match self.get(&hash).map_err(InlineError::Io)? {
                Some(data) => {
                    *value = Value::String(data);
                    Ok(1)
                }
                None => Err(InlineError::Missing(hash)),
            };
        }
        let mut n = 0;
        match value {
            Value::Array(arr) => {
                for v in arr.iter_mut() {
                    n += self.inline(v)?;
                }
            }
            Value::Object(map) => {
                for (_, v) in map.iter_mut() {
                    n += self.inline(v)?;
                }
            }
            _ => {}
        }
        Ok(n)
    }

    /// mark-sweep:删掉所有不在 `live` 集合里的 blob 文件(及 `.tmp.` 残留)。返回
    /// [`SweepStats`](删除数 + 失败数)。blobs 根不存在 → `Ok(default)`。分片读不出
    /// / blob 删不掉计入 `failed`(供隐私清除路径上报),但不中断其余回收。
    pub(crate) fn sweep(&self, live: &HashSet<String>) -> io::Result<SweepStats> {
        let mut stats = SweepStats::default();
        let shards = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(stats),
            Err(e) => return Err(e),
        };
        for shard in shards {
            let shard = match shard {
                Ok(s) => s,
                Err(e) => {
                    warn(
                        "SESSIONS_BLOB_SHARD_ITER_FAILED",
                        format!("blobs 分片目录项遍历失败,跳过: {e}"),
                    );
                    stats.failed += 1;
                    continue;
                }
            };
            if !shard.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let files = match fs::read_dir(shard.path()) {
                Ok(rd) => rd,
                Err(e) => {
                    warn(
                        "SESSIONS_BLOB_SHARD_READ_FAILED",
                        format!("read_dir({:?}) 失败,本分片孤儿未回收: {e}", shard.path()),
                    );
                    stats.failed += 1;
                    continue;
                }
            };
            for f in files {
                // per-entry 读取错误也计入 `failed`:否则隐私清除可能漏查/漏删某 blob
                // 却仍报 failed==0 → clear 误报成功(codex-connector P2)。
                let f = match f {
                    Ok(f) => f,
                    Err(e) => {
                        warn(
                            "SESSIONS_BLOB_ENTRY_FAILED",
                            format!("blobs 文件项读取失败,可能漏清: {e}"),
                        );
                        stats.failed += 1;
                        continue;
                    }
                };
                let name = f.file_name();
                let Some(name) = name.to_str() else { continue };
                // 没 rename 完的 temp 残留顺手清(NotFound = 被并发清掉,正常)。**temp 文件
                // 含 `put` 在途写入的 blob 字节(就是图片内容,只是没 rename 到位)**,所以删
                // 不掉同样计入 `failed` —— 隐私清除必须把这些半写文件也算没清干净(P1)。
                if name.starts_with(".tmp.") {
                    if let Err(e) = fs::remove_file(f.path()) {
                        if e.kind() != io::ErrorKind::NotFound {
                            warn(
                                "SESSIONS_BLOB_TMP_REMOVE_FAILED",
                                format!("残留 temp {name}(含在途 blob 字节)删除失败: {e}"),
                            );
                            stats.failed += 1;
                        }
                    }
                    continue;
                }
                // 只动形如 sha256 的 blob 文件:非 blob 文件一律不碰(防误删 + 稳健)。
                if !is_sha256_hex(name) {
                    continue;
                }
                if !live.contains(name) {
                    match fs::remove_file(f.path()) {
                        Ok(()) => stats.removed += 1,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => {
                            warn(
                                "SESSIONS_BLOB_REMOVE_FAILED",
                                format!("孤儿 blob {name} 删除失败,下次 GC 重试: {e}"),
                            );
                            stats.failed += 1;
                        }
                    }
                }
            }
        }
        Ok(stats)
    }

    /// 收集 value 里所有 blob 引用的 hash(GC mark 用)。
    pub(crate) fn collect_hashes(value: &Value, out: &mut HashSet<String>) {
        if let Some(hash) = as_blob_ref(value) {
            out.insert(hash.to_owned());
            return;
        }
        match value {
            Value::Array(arr) => arr.iter().for_each(|v| Self::collect_hashes(v, out)),
            Value::Object(map) => map.values().for_each(|v| Self::collect_hashes(v, out)),
            _ => {}
        }
    }
}

fn is_blob_candidate(s: &str) -> bool {
    s.len() >= BLOB_INLINE_THRESHOLD && s.starts_with("data:")
}

/// 构造引用对象 `{ "__cat_session_blob__": { "sha256": <hex>, "bytes": <len> } }`。
/// `bytes` 仅供观测(看原始大小),`inline` 只用 `sha256`。
fn make_blob_ref(hash: String, bytes: usize) -> Value {
    let mut inner = serde_json::Map::with_capacity(2);
    inner.insert("sha256".to_owned(), Value::String(hash));
    inner.insert("bytes".to_owned(), Value::from(bytes as u64));
    let mut outer = serde_json::Map::with_capacity(1);
    outer.insert(BLOB_REF_KEY.to_owned(), Value::Object(inner));
    Value::Object(outer)
}

fn as_blob_ref(value: &Value) -> Option<&str> {
    let hash = value
        .as_object()?
        .get(BLOB_REF_KEY)?
        .as_object()?
        .get("sha256")?
        .as_str()?;
    // 只有合法 hex 才认作引用:伪造/畸形 `sha256` 视作普通内容(原样穿过、不读盘、
    // 不进 live-set),既堵路径穿越,也避免把 lookalike 用户内容误判成引用。
    is_sha256_hex(hash).then_some(hash)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// blob 文件名 / 引用 `sha256` 必须是 64 位小写 hex(`sha256_hex` 的输出形态)。
/// 校验后才用作路径分量,杜绝 messages_json 里伪造的 `sha256`(`../../etc/passwd`
/// 等)穿越 blobs 目录读任意文件;`sweep` 也据此只清形如 hash 的 blob 文件。
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn tmp_name(hash: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".tmp.{hash}.{nanos:x}.{seq:x}")
}

fn warn(error_id: &'static str, detail: String) {
    tracing::warn!(error_id, detail = %detail, "sessions blob store");
    eprintln!("warning: [{error_id}] {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs"));
        (dir, store)
    }

    fn big_data_url(seed: char) -> String {
        format!(
            "data:image/png;base64,{}",
            seed.to_string().repeat(BLOB_INLINE_THRESHOLD)
        )
    }

    #[test]
    fn externalize_then_inline_round_trips() {
        let (_d, s) = store();
        let original = json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": big_data_url('A')}]
        });
        let mut v = original.clone();
        assert_eq!(s.externalize(&mut v), 1);
        // 引用形态:序列化后不再含 data: 原文
        assert!(!serde_json::to_string(&v).unwrap().contains("data:image"));
        assert_eq!(s.inline(&mut v).unwrap(), 1);
        assert_eq!(v, original, "回填后必须字节级等于原始");
    }

    #[test]
    fn duplicate_blobs_dedupe_to_one_file() {
        let (_d, s) = store();
        let data = big_data_url('B');
        let h1 = s.put(&data).unwrap();
        let h2 = s.put(&data).unwrap();
        assert_eq!(h1, h2, "同内容必须同 hash");
        let shard = s.root.join(&h1[0..2]);
        let real_files = fs::read_dir(&shard)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| !n.starts_with(".tmp."))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(real_files, 1, "重复 put 只应落一个文件");
    }

    #[test]
    fn small_data_url_stays_inline() {
        let (_d, s) = store();
        let mut v = json!({"image_url": "data:image/png;base64,SHORT"});
        assert_eq!(s.externalize(&mut v), 0);
        assert_eq!(v["image_url"], "data:image/png;base64,SHORT");
    }

    #[test]
    fn large_non_data_string_stays_inline() {
        let (_d, s) = store();
        let big_text = "x".repeat(BLOB_INLINE_THRESHOLD * 2);
        let mut v = json!({"content": big_text.clone()});
        assert_eq!(
            s.externalize(&mut v),
            0,
            "非 data: 大文本不外置(留给 append-only)"
        );
        assert_eq!(v["content"], big_text);
    }

    #[test]
    fn missing_blob_reports_error_not_leak() {
        let (_d, s) = store();
        let mut v = json!({"image_url": big_data_url('C')});
        s.externalize(&mut v);
        let hash = as_blob_ref(&v["image_url"]).unwrap().to_owned();
        fs::remove_file(s.path_for(&hash)).unwrap();
        match s.inline(&mut v) {
            Err(InlineError::Missing(h)) => assert_eq!(h, hash),
            other => panic!("blob 缺失应报 Missing,实际 {other:?}"),
        }
    }

    #[test]
    fn sweep_removes_orphans_keeps_live() {
        let (_d, s) = store();
        let keep = s.put(&big_data_url('D')).unwrap();
        let orphan = s.put(&big_data_url('E')).unwrap();
        let mut live = HashSet::new();
        live.insert(keep.clone());
        let stats = s.sweep(&live).unwrap();
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.failed, 0);
        assert!(s.get(&keep).unwrap().is_some(), "live blob 必须保留");
        assert!(s.get(&orphan).unwrap().is_none(), "orphan blob 必须删");
    }

    #[test]
    fn collect_hashes_walks_nested() {
        let (_d, s) = store();
        let mut v = json!({"content": [{"image_url": big_data_url('F')}]});
        s.externalize(&mut v);
        let mut set = HashSet::new();
        BlobStore::collect_hashes(&v, &mut set);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn crafted_non_hex_sha256_is_not_a_blob_ref_no_traversal() {
        // 安全(BLOCKER):messages_json 里伪造的引用,sha256 是路径穿越串(非 hex)。
        // 绝不能被当引用 → 不读文件系统、不进 live-set,原样穿过。
        let (_d, s) = store();
        let mut v = json!({
            "image_url": {"__cat_session_blob__": {"sha256": "../../../../etc/passwd"}}
        });
        let before = v.clone();
        assert_eq!(
            s.inline(&mut v).unwrap(),
            0,
            "伪造 sha256 不应被当 blob 还原"
        );
        assert_eq!(v, before, "内容原样保留,绝不触发任意文件读");
        let mut set = HashSet::new();
        BlobStore::collect_hashes(&v, &mut set);
        assert!(set.is_empty(), "伪造 sha256 不应进 live-set");
    }

    #[test]
    fn sweep_ignores_non_blob_files() {
        // sweep 只清形如 sha256 的 blob 文件,绝不碰分片目录里的其它文件。
        let (_d, s) = store();
        let keep = s.put(&big_data_url('K')).unwrap();
        let shard = s.root.join(&keep[0..2]);
        let stray = shard.join("not-a-blob.txt");
        fs::write(&stray, b"keep me").unwrap();
        let mut live = HashSet::new();
        live.insert(keep.clone());
        s.sweep(&live).unwrap();
        assert!(stray.exists(), "非 blob 文件不应被 sweep 删");
        assert!(s.get(&keep).unwrap().is_some(), "live blob 仍在");
    }
}
