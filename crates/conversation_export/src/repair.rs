//! [CAT-255] 导入 / 恢复其他工具(cc-switch 等)留下的隔离会话.
//!
//! Codex Desktop 列表按当前活动 `model_provider` 过滤显示。其他工具给 Codex 写了
//! **第三方 `model_provider`**(如 `cas` / `deepseek`),而 transfer 故意不写
//! (Codex 默认 active=`openai`,所有 transfer 会话都 tag openai → 互通)。所以那些
//! 第三方 tag 的旧会话在 transfer 视图下**被隐藏**。
//!
//! 本模块提供双向就地归一:
//! - **导入**([`import_foreign_sessions`]):第三方 → `openai`,让其在 transfer 显示;
//! - **恢复**([`set_sessions_provider`] 写任意 provider):`openai` → 用户选定的
//!   provider,让其他工具重新看到该会话(导入的逆操作)。
//!
//! 可见性三要素(实测 + meta-xucong/ccswitch-codex-guide 开源项目双重验证):
//! 1. `threads.model_provider` = 当前活动 provider(transfer = `openai`)
//! 2. `threads.has_user_event = 1`(列表只显示有用户消息的会话)
//! 3. 会话 `cwd` 是 Codex 侧边栏项目(本模块管不到 —— cwd 不在项目里的会话修了也要
//!    用户把该文件夹加进侧边栏才显示)
//! 同步把 rollout 首行 `session_meta` 的 `model_provider` 一并归一,防 Codex 某次
//! backfill 又按 rollout 把 threads 回退。
//!
//! **必须在 Codex 关闭时调用**:写的是 Codex 独占的 `state_<N>.sqlite`。调用方
//! (`/api/codex-sessions/*` handler)负责先退出 Codex、写完再重启。

use crate::ExportError;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// transfer 的锚点 provider。config.toml 不写 model_provider 时 Codex 默认 active=openai。
/// **检测时只把第三方挑出来,openai / 没写的不碰**(用户硬性约束);导入时归到它。
const ANCHOR_PROVIDER: &str = "openai";

/// 不算「第三方」的 model_provider —— detect 排除它们:
/// - `openai`:锚点本身;
/// - `codex-app-transfer`:本工具自己的旧 tag(早期版本写过),不能误当其他工具的会话导入/重写。
///   (改这里要同步改下方 detect SQL 里 NOT IN 的字面量。)
const NON_FOREIGN_PROVIDERS: [&str; 2] = ["openai", "codex-app-transfer"];

/// 一条被其他工具隔离的会话(待导入)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignSession {
    pub id: String,
    pub model_provider: String,
    pub cwd: String,
    pub title: String,
    /// rollout `.jsonl` 路径(threads 行自带,可能为空)。
    pub rollout_path: String,
}

/// 写操作结果(部分成功:逐条收 repaired / failed,不整体报错)。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RepairResult {
    /// 成功写入的 session id
    pub repaired: Vec<String>,
    /// 失败列表 + 原因
    pub failed: Vec<RepairFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairFailure {
    pub session_id: String,
    pub reason: String,
}

/// 找 Codex 当前的 state DB:`~/.codex/state_<N>.sqlite`,N 是迁移版本号。
/// 取最高 N(当前版本)——**不写死 state_5**,Codex 升级会 bump N。
fn find_state_db(codex_home: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(codex_home).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 只认 state_<N>.sqlite 本体,排除 -wal / -shm 旁文件
        if let Some(n) = name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
            .and_then(|s| s.parse::<u32>().ok())
        {
            if best.as_ref().map(|(bn, _)| n > *bn).unwrap_or(true) {
                best = Some((n, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 扫 `threads` 表挑出 model_provider 是「明确第三方」的会话:非空、非 null、非 openai
/// (大小写无关)。**只读**打开 state DB,Codex 运行时也安全。无 state DB 时回空列表。
pub fn detect_foreign_sessions(codex_home: &Path) -> Result<Vec<ForeignSession>, ExportError> {
    let Some(db) = find_state_db(codex_home) else {
        return Ok(Vec::new());
    };
    debug_assert_eq!(NON_FOREIGN_PROVIDERS, ["openai", "codex-app-transfer"]);
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // `NON_FOREIGN_PROVIDERS` 排除自有/锚点 tag;`COALESCE(archived,0)` 防 archived 为 NULL 时
    // `archived = 0` 在 SQL 里判 false 而漏掉(第三方工具写的行可能没设该列)。
    let mut stmt = conn.prepare(
        "SELECT id, model_provider, COALESCE(cwd,''), COALESCE(title,''), COALESCE(rollout_path,'') \
         FROM threads \
         WHERE model_provider IS NOT NULL AND model_provider != '' \
           AND LOWER(model_provider) NOT IN ('openai', 'codex-app-transfer') \
           AND COALESCE(archived, 0) = 0",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ForeignSession {
            id: r.get(0)?,
            model_provider: r.get(1)?,
            cwd: r.get(2)?,
            title: r.get(3)?,
            rollout_path: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 就地改写 rollout 首行 `session_meta` 的 `model_provider`(+ 可选补 `thread_source=user`)。
/// 原子写(同目录 `.part` + rename),只动第一行、其余字节 1:1 保留。
fn rewrite_rollout_provider(
    path: &Path,
    target_provider: &str,
    ensure_thread_source_user: bool,
) -> Result<(), ExportError> {
    let content = std::fs::read_to_string(path)?;
    let mut split = content.splitn(2, '\n');
    let first = split.next().unwrap_or("");
    let rest = split.next(); // 剩余全部(含中间换行);None = 原文件只有一行、无尾换行

    let mut meta: serde_json::Value = serde_json::from_str(first)?;
    let payload = meta
        .get_mut("payload")
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| ExportError::NotFound(format!("{}: 首行非 session_meta", path.display())))?;
    payload.insert(
        "model_provider".into(),
        serde_json::Value::String(target_provider.to_owned()),
    );
    if ensure_thread_source_user
        && payload
            .get("thread_source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        payload.insert(
            "thread_source".into(),
            serde_json::Value::String("user".into()),
        );
    }

    let mut out = serde_json::to_string(&meta)?;
    if let Some(rest) = rest {
        out.push('\n');
        out.push_str(rest);
    }

    // 原子写:同目录 `<file>.part` → rename(同 fs 原子,避免半截文件)
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, out.as_bytes())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// 把给定 session 的 `model_provider` 就地改成 `target_provider`(threads 行 + rollout 首行)。
///
/// **必须在 Codex 关闭时调用**(写 Codex 独占的 state DB)。两个方向共用:
/// - 导入:`target_provider="openai"`、`mark_visible=true`(顺带点亮 `has_user_event` /
///   `thread_source` 让其在 transfer 列表显示);
/// - 恢复:`target_provider=<用户选的第三方>`、`mark_visible=false`(只改 provider,
///   让其他工具重新可见,不动可见性字段)。
///
/// 每条:① threads 行 UPDATE(找不到 id → failed);② rollout 首行同步(best-effort,
/// 失败只 warn,threads 已生效)。逐条收 repaired / failed。
pub fn set_sessions_provider(
    codex_home: &Path,
    session_ids: &[String],
    target_provider: &str,
    mark_visible: bool,
) -> Result<RepairResult, ExportError> {
    let mut result = RepairResult::default();
    if session_ids.is_empty() {
        return Ok(result);
    }
    let Some(db) = find_state_db(codex_home) else {
        return Ok(result);
    };
    let conn = Connection::open(&db)?;

    // 导入额外点亮可见性。`has_user_event` 仅有 user 消息时点亮(空会话不强行点亮,对齐上游
    // repair 脚本);`thread_source` 仅原本为空时补 'user'(已有值不覆盖 —— 减少对来源元数据的
    // 破坏性改写,让会话回到其他工具时元数据更接近原样)。
    let sql = if mark_visible {
        "UPDATE threads SET model_provider = ?1, \
         thread_source = CASE WHEN thread_source IS NULL OR thread_source = '' \
           THEN 'user' ELSE thread_source END, \
         has_user_event = CASE \
           WHEN first_user_message IS NOT NULL AND length(trim(first_user_message)) > 0 \
           THEN 1 ELSE has_user_event END \
         WHERE id = ?2"
    } else {
        "UPDATE threads SET model_provider = ?1 WHERE id = ?2"
    };

    for id in session_ids {
        // 先拿 rollout_path(顺带验证 id 存在)。真 sqlite 错(锁/IO)逐条收 failed、**不 `?`
        // 中断整批**(否则前面已 UPDATE+提交的会话被静默改了却报整体失败)。
        let rollout_path = match conn
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
        {
            Ok(Some(rp)) => rp, // 行存在,rp = rollout_path(可能 NULL/空)
            Ok(None) => {
                result.failed.push(RepairFailure {
                    session_id: id.clone(),
                    reason: "threads 表无此会话".into(),
                });
                continue;
            }
            Err(e) => {
                result.failed.push(RepairFailure {
                    session_id: id.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };

        match conn.execute(sql, rusqlite::params![target_provider, id]) {
            Ok(_) => {
                if let Some(path) = rollout_path.filter(|p| !p.is_empty()) {
                    if let Err(e) =
                        rewrite_rollout_provider(Path::new(&path), target_provider, mark_visible)
                    {
                        tracing::warn!("[CAT-255] rollout meta rewrite failed for {id}: {e}");
                    }
                }
                result.repaired.push(id.clone());
            }
            Err(e) => result.failed.push(RepairFailure {
                session_id: id.clone(),
                reason: e.to_string(),
            }),
        }
    }
    Ok(result)
}

/// 导入便捷封装:检测所有第三方会话 → 全部归一成 `openai` + 点亮可见。
/// 返回结果(空 = 没有可导入的会话)。**必须在 Codex 关闭时调用。**
pub fn import_foreign_sessions(codex_home: &Path) -> Result<RepairResult, ExportError> {
    let ids: Vec<String> = detect_foreign_sessions(codex_home)?
        .into_iter()
        .map(|s| s.id)
        .collect();
    set_sessions_provider(codex_home, &ids, ANCHOR_PROVIDER, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::fs;

    fn make_state_db(dir: &Path, n: u32) -> PathBuf {
        let p = dir.join(format!("state_{n}.sqlite"));
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (\
                id TEXT PRIMARY KEY, model_provider TEXT, cwd TEXT, title TEXT, \
                rollout_path TEXT, has_user_event INTEGER DEFAULT 0, archived INTEGER DEFAULT 0, \
                first_user_message TEXT, thread_source TEXT);",
        )
        .unwrap();
        p
    }

    #[test]
    fn find_state_db_picks_highest_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state_3.sqlite"), b"").unwrap();
        fs::write(dir.path().join("state_5.sqlite"), b"").unwrap();
        fs::write(dir.path().join("state_5.sqlite-wal"), b"").unwrap();
        fs::write(dir.path().join("other.sqlite"), b"").unwrap();
        assert!(find_state_db(dir.path())
            .unwrap()
            .ends_with("state_5.sqlite"));
    }

    #[test]
    fn rewrite_rollout_only_touches_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("rollout.jsonl");
        fs::write(
            &f,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"model_provider\":\"cas\"}}\n{\"a\":1}\n{\"b\":2}",
        )
        .unwrap();
        rewrite_rollout_provider(&f, "openai", true).unwrap();
        let out = fs::read_to_string(&f).unwrap();
        let mut lines = out.lines();
        let meta: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(meta["payload"]["model_provider"], "openai");
        assert_eq!(meta["payload"]["thread_source"], "user");
        // 内容行 1:1 保留
        assert_eq!(lines.next().unwrap(), "{\"a\":1}");
        assert_eq!(lines.next().unwrap(), "{\"b\":2}");
    }

    #[test]
    fn detect_and_set_provider_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_state_db(dir.path(), 5);
        let conn = Connection::open(&db).unwrap();
        // a=第三方(检出);b=openai 锚点(不碰);c=空(不碰);d=codex-app-transfer 自有 tag
        // (不碰,#1);e=第三方但 archived 为 NULL(COALESCE 后仍检出,防漏)
        conn.execute(
            "INSERT INTO threads (id, model_provider, cwd, title, rollout_path, archived, first_user_message) VALUES \
               ('a','cas','/p','t','',0,'hello'), \
               ('b','openai','/p','t','',0,'hi'), \
               ('c','','/p','t','',0,'x'), \
               ('d','codex-app-transfer','/p','t','',0,'own'), \
               ('e','deepseek','/p','t','',NULL,'nullarch')",
            [],
        )
        .unwrap();

        let mut foreign = detect_foreign_sessions(dir.path()).unwrap();
        foreign.sort_by(|x, y| x.id.cmp(&y.id));
        let ids: Vec<&str> = foreign.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a", "e"],
            "只检出第三方(排除 openai/空/codex-app-transfer)"
        );

        // 导入:cas → openai + 点亮可见
        let r = set_sessions_provider(dir.path(), &["a".into()], "openai", true).unwrap();
        assert_eq!(r.repaired, vec!["a".to_string()]);
        assert!(r.failed.is_empty());
        let hue: i64 = conn
            .query_row("SELECT has_user_event FROM threads WHERE id='a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hue, 1, "有 first_user_message 应点亮 has_user_event");
    }

    #[test]
    fn set_provider_missing_id_collected_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        make_state_db(dir.path(), 5);
        let r = set_sessions_provider(dir.path(), &["nope".into()], "cas", false).unwrap();
        assert!(r.repaired.is_empty());
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].session_id, "nope");
    }
}
