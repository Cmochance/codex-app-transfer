//! Codex 配置文件"受管块"管理 — 借鉴 borawong/AiMaMi:src-tauri/src/core/custom_instructions.rs.
//!
//! ## 设计目标
//!
//! 让 Codex App Transfer 在用户的 Codex 配置文件(`~/.codex/AGENTS.md` /
//! `config.toml` MCP 段 / `skills/*/SKILL.md`)中维护"受管块":
//!
//! - **物理隔离**: 用注释 marker 把 app 受管区跟用户手写区切开,app 永远不动用户区
//! - **可预览**: apply 前先 preview 算 diff,user 确认后才写盘
//! - **可回滚**: 每次 apply 把旧 managed 段存进 history,后续可 rollback
//! - **可清除**: 删 marker + content 还原成 app 介入前
//!
//! ## Marker 规范(本项目)
//!
//! ```markdown
//! <!-- cas:managed:agents:v1:start -->
//! <app 受管内容,可被 apply/rollback>
//! <!-- cas:managed:agents:v1:end -->
//! ```
//!
//! - `cas:` 项目 prefix (避免跟 AiMaMi `AIMAMI_*` 等其他工具 marker 冲突)
//! - `managed:` 模式标识
//! - `<block-type>:` agents / mcp / skills 等
//! - `v1:` 版本号 (为将来 marker schema 升级留余地)
//! - `start` / `end` 边界
//!
//! ## 实施 scope(本 PR 第一刀)
//!
//! 只实现 Markdown 变种 (`MarkdownManagedBlock`),覆盖 `~/.codex/AGENTS.md`。
//! TOML 变种(MCP servers) + file-level snapshot (Skills) 留 stacked PR
//! (#25 P3-P5)。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// History 环形缓冲上限 — 借鉴 AiMaMi HISTORY_LIMIT=10
pub const HISTORY_LIMIT: usize = 10;

/// 解析 / apply 失败原因.
#[derive(Debug)]
pub enum ManagedBlockError {
    /// 目标 / history 文件读写失败 (权限 / 不存在 / disk 满 / etc.)
    Io(String),
    /// Marker 配对错误(只有 start 没有 end,或交叉嵌套)
    MalformedMarker(String),
    /// History rollback 索引越界 / 文件损坏
    HistoryAccess(String),
    /// JSON 解析 / 序列化失败 (history snapshot 文件 schema 损坏)
    Serialization(String),
}

impl std::fmt::Display for ManagedBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagedBlockError::Io(e) => write!(f, "managed-block IO failed: {e}"),
            ManagedBlockError::MalformedMarker(e) => {
                write!(f, "managed-block marker malformed: {e}")
            }
            ManagedBlockError::HistoryAccess(e) => {
                write!(f, "managed-block history access failed: {e}")
            }
            ManagedBlockError::Serialization(e) => {
                write!(f, "managed-block serialization failed: {e}")
            }
        }
    }
}

impl std::error::Error for ManagedBlockError {}

/// 单条 history snapshot:apply 前的旧 managed 段 + timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 旧 managed 段内容 (start/end marker 之间的 raw text, 不含 marker 自身)
    pub managed_content: String,
    /// Unix epoch 秒
    pub timestamp: u64,
    /// 应用本次 apply 的新内容 (供 UI 展示 "从 X 改成 Y")
    pub applied_content: String,
}

/// 文件解析后的三段结构.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    /// marker start 之前的用户手写区
    pub before_user: String,
    /// marker 之间的 app 受管区(不含 marker)— None 表示 marker 还没插入
    pub managed: Option<String>,
    /// marker end 之后的用户手写区
    pub after_user: String,
}

impl ParsedFile {
    /// 序列化回完整文件内容
    pub fn render(&self, start_marker: &str, end_marker: &str) -> String {
        match &self.managed {
            Some(managed) => {
                let mut buf = String::with_capacity(
                    self.before_user.len()
                        + self.after_user.len()
                        + managed.len()
                        + start_marker.len()
                        + end_marker.len()
                        + 16,
                );
                buf.push_str(&self.before_user);
                if !self.before_user.is_empty() && !self.before_user.ends_with('\n') {
                    buf.push('\n');
                }
                buf.push_str(start_marker);
                buf.push('\n');
                buf.push_str(managed);
                if !managed.is_empty() && !managed.ends_with('\n') {
                    buf.push('\n');
                }
                buf.push_str(end_marker);
                if !self.after_user.is_empty() && !self.after_user.starts_with('\n') {
                    buf.push('\n');
                }
                buf.push_str(&self.after_user);
                buf
            }
            None => {
                // 无 marker:全文都是用户区(before_user 跟 after_user 合一)
                format!("{}{}", self.before_user, self.after_user)
            }
        }
    }
}

/// 受管块通用 trait — Markdown / TOML / file-snapshot 三个变种实现。
pub trait ManagedBlock {
    /// 受管块类型标识(供 marker 拼接,如 `agents` / `mcp`)
    fn block_type(&self) -> &'static str;

    /// 目标文件路径(如 `~/.codex/AGENTS.md`)
    fn target_path(&self) -> &Path;

    /// History 文件路径(如 `~/.codex-app-transfer/managed-history/agents.json`)
    fn history_path(&self) -> &Path;

    /// marker version,默认 v1
    fn marker_version(&self) -> &'static str {
        "v1"
    }

    fn start_marker(&self) -> String {
        format!(
            "<!-- cas:managed:{}:{}:start -->",
            self.block_type(),
            self.marker_version()
        )
    }

    fn end_marker(&self) -> String {
        format!(
            "<!-- cas:managed:{}:{}:end -->",
            self.block_type(),
            self.marker_version()
        )
    }

    /// 读 + 解析当前文件,返三段结构
    fn parse(&self) -> Result<ParsedFile, ManagedBlockError> {
        let path = self.target_path();
        let content = if path.exists() {
            fs::read_to_string(path)
                .map_err(|e| ManagedBlockError::Io(format!("read {}: {e}", path.display())))?
        } else {
            String::new()
        };
        parse_markdown_managed_block(&content, &self.start_marker(), &self.end_marker())
    }

    /// 算"如果 apply new_content 文件会变成什么样" —— 只返新文件内容,不写盘
    fn preview(&self, new_content: &str) -> Result<String, ManagedBlockError> {
        let mut parsed = self.parse()?;
        parsed.managed = Some(new_content.to_string());
        Ok(parsed.render(&self.start_marker(), &self.end_marker()))
    }

    /// 真写盘:把 new_content 写进 managed 段 + 旧 managed 段进 history snapshot.
    ///
    /// 写顺序: **history 先 + target 后** + target 失败回滚 history。
    ///   - P1 fix (codex-bot #206): read_history 用 `?` propagate (history 损坏返
    ///     Err 而非静默丢历史 — 否则下次 apply 替换整个 history 链, rollback
    ///     恢复点全丢)
    ///   - P2 fix (codex-bot #206): atomicity — target write 失败时 pop 刚加的
    ///     history entry + best-effort 重写 history, 防止"history 多 1 条但 target
    ///     没变" 不一致 state
    fn apply(&self, new_content: &str) -> Result<(), ManagedBlockError> {
        let parsed = self.parse()?;
        let old_managed = parsed.managed.clone().unwrap_or_default();

        // P1: propagate history corruption
        let mut history = self.read_history()?;
        let entry = HistoryEntry {
            managed_content: old_managed,
            applied_content: new_content.to_string(),
            timestamp: now_unix(),
        };
        history.push(entry);
        if history.len() > HISTORY_LIMIT {
            let drop_n = history.len() - HISTORY_LIMIT;
            history.drain(0..drop_n);
        }
        // 先 write history (有 history 但 target 没动 = 可恢复; 反过来不行)
        self.write_history(&history)?;

        // 后 write target
        let new_file = {
            let mut updated = parsed;
            updated.managed = Some(new_content.to_string());
            updated.render(&self.start_marker(), &self.end_marker())
        };
        let path = self.target_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ManagedBlockError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        if let Err(e) = fs::write(path, &new_file) {
            // P2: target 失败 → pop 刚加的 history entry + best-effort 重写
            history.pop();
            if let Err(rb_err) = self.write_history(&history) {
                // history rollback 也失败 — 记 warn, history 多 1 条 entry 不影响
                // 后续 rollback 操作 (managed_content 是旧值, applied_content 是
                // 应用过的新值, rollback 到这个 entry 等于 noop)
                tracing::warn!(
                    "apply rollback failed: target write err + history pop write err: target_err={e} history_err={rb_err}"
                );
            }
            return Err(ManagedBlockError::Io(format!(
                "write {}: {e}",
                path.display()
            )));
        }
        Ok(())
    }

    /// 从 history index 还原 managed 段 (`index` 0 = 最旧,len-1 = 最新)
    fn rollback(&self, index: usize) -> Result<(), ManagedBlockError> {
        // P1: propagate history corruption
        let history = self.read_history()?;
        let entry = history.get(index).ok_or_else(|| {
            ManagedBlockError::HistoryAccess(format!(
                "history index {index} out of bounds (len={})",
                history.len()
            ))
        })?;
        let parsed = self.parse()?;

        // 先 staging 新 history entry (still in memory)
        let mut new_history = history.clone();
        new_history.push(HistoryEntry {
            managed_content: parsed.managed.clone().unwrap_or_default(),
            applied_content: entry.managed_content.clone(),
            timestamp: now_unix(),
        });
        if new_history.len() > HISTORY_LIMIT {
            let drop_n = new_history.len() - HISTORY_LIMIT;
            new_history.drain(0..drop_n);
        }
        // 先 write history
        self.write_history(&new_history)?;

        // 后 write target — fail 时 pop history (P2)
        let new_file = {
            let mut updated = parsed.clone();
            updated.managed = Some(entry.managed_content.clone());
            updated.render(&self.start_marker(), &self.end_marker())
        };
        let path = self.target_path();
        if let Err(e) = fs::write(path, &new_file) {
            let mut rolled = new_history;
            rolled.pop();
            if let Err(rb_err) = self.write_history(&rolled) {
                tracing::warn!(
                    "rollback target write failed + history pop write failed: target_err={e} history_err={rb_err}"
                );
            }
            return Err(ManagedBlockError::Io(format!(
                "write {}: {e}",
                path.display()
            )));
        }
        Ok(())
    }

    /// 删 marker + managed 段,还原成 app 介入前
    fn clear(&self) -> Result<(), ManagedBlockError> {
        let mut parsed = self.parse()?;
        let old_managed = parsed.managed.clone().unwrap_or_default();
        if parsed.managed.is_none() {
            // 本来就没 marker,啥也不做(也不推 history)
            return Ok(());
        }

        // P1: propagate history corruption
        let mut history = self.read_history()?;
        history.push(HistoryEntry {
            managed_content: old_managed,
            applied_content: String::new(),
            timestamp: now_unix(),
        });
        if history.len() > HISTORY_LIMIT {
            let drop_n = history.len() - HISTORY_LIMIT;
            history.drain(0..drop_n);
        }
        // 先 write history
        self.write_history(&history)?;

        // 后 write target — fail 时 pop history (P2)
        parsed.managed = None;
        let new_file = parsed.render(&self.start_marker(), &self.end_marker());
        let path = self.target_path();
        if let Err(e) = fs::write(path, &new_file) {
            history.pop();
            if let Err(rb_err) = self.write_history(&history) {
                tracing::warn!(
                    "clear target write failed + history pop write failed: target_err={e} history_err={rb_err}"
                );
            }
            return Err(ManagedBlockError::Io(format!(
                "write {}: {e}",
                path.display()
            )));
        }
        Ok(())
    }

    /// 读 history 文件
    fn read_history(&self) -> Result<Vec<HistoryEntry>, ManagedBlockError> {
        let path = self.history_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut file = fs::File::open(path)
            .map_err(|e| ManagedBlockError::Io(format!("read {}: {e}", path.display())))?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .map_err(|e| ManagedBlockError::Io(format!("read {}: {e}", path.display())))?;
        serde_json::from_str(&buf).map_err(|e| ManagedBlockError::Serialization(e.to_string()))
    }

    /// 写 history 文件 (atomic-ish: 写 tmp + rename)
    fn write_history(&self, history: &[HistoryEntry]) -> Result<(), ManagedBlockError> {
        let path = self.history_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ManagedBlockError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        let json = serde_json::to_string_pretty(history)
            .map_err(|e| ManagedBlockError::Serialization(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)
            .map_err(|e| ManagedBlockError::Io(format!("create {}: {e}", tmp.display())))?;
        file.write_all(json.as_bytes())
            .map_err(|e| ManagedBlockError::Io(format!("write {}: {e}", tmp.display())))?;
        file.flush()
            .map_err(|e| ManagedBlockError::Io(format!("flush {}: {e}", tmp.display())))?;
        fs::rename(&tmp, path)
            .map_err(|e| ManagedBlockError::Io(format!("rename {}: {e}", path.display())))?;
        Ok(())
    }

    /// JSON 化当前状态 (供 HTTP handler 返回)
    fn status_json(&self) -> Result<Value, ManagedBlockError> {
        let parsed = self.parse()?;
        let history = self.read_history().unwrap_or_default();
        Ok(json!({
            "blockType": self.block_type(),
            "targetPath": self.target_path().display().to_string(),
            "hasManaged": parsed.managed.is_some(),
            "managedContent": parsed.managed.clone().unwrap_or_default(),
            "beforeUserBytes": parsed.before_user.len(),
            "afterUserBytes": parsed.after_user.len(),
            "historyCount": history.len(),
            "lastApply": history.last().map(|e| e.timestamp),
            "markerVersion": self.marker_version(),
        }))
    }
}

/// Markdown 文件的受管块实现 (适用 AGENTS.md 等). marker 走 HTML 注释 `<!-- ... -->`.
pub struct MarkdownManagedBlock {
    pub block_type: &'static str,
    pub target: PathBuf,
    pub history: PathBuf,
}

impl ManagedBlock for MarkdownManagedBlock {
    fn block_type(&self) -> &'static str {
        self.block_type
    }
    fn target_path(&self) -> &Path {
        &self.target
    }
    fn history_path(&self) -> &Path {
        &self.history
    }
}

/// TOML 文件的受管块实现 (适用 `config.toml` MCP servers 段). marker 走行注释 `# ...`.
///
/// **schema 假设**: 受管块内是 TOML 合法片段 (典型: 多个 `[mcp_servers.<name>]` table),
/// app 自动给受管 server name 加 `cas-` 或 `app-` prefix 避免跟用户区 server 冲突。
/// `toml_edit` 解析时受管块在 marker 之间,作为 raw text 处理 (不参与 TOML AST),
/// render 写回时保留原顺序 + 注释 — 用户区的 TOML 字段顺序跟注释**永远不被 app 触碰**。
pub struct TomlManagedBlock {
    pub block_type: &'static str,
    pub target: PathBuf,
    pub history: PathBuf,
}

impl ManagedBlock for TomlManagedBlock {
    fn block_type(&self) -> &'static str {
        self.block_type
    }
    fn target_path(&self) -> &Path {
        &self.target
    }
    fn history_path(&self) -> &Path {
        &self.history
    }
    fn start_marker(&self) -> String {
        // TOML 用 `#` 行注释作 marker (HTML 注释在 TOML parser 看是非法字符)
        format!(
            "# cas:managed:{}:{}:start",
            self.block_type(),
            self.marker_version()
        )
    }
    fn end_marker(&self) -> String {
        format!(
            "# cas:managed:{}:{}:end",
            self.block_type(),
            self.marker_version()
        )
    }
}

/// 解析 Markdown / 通用 text 文件,找 start/end marker 切三段.
///
/// **多 marker pair 检测**: 只接受一对 start/end (出现 ≥2 个 start 报
/// MalformedMarker,防止 app 误吞用户中间的另一段)。
pub fn parse_markdown_managed_block(
    content: &str,
    start_marker: &str,
    end_marker: &str,
) -> Result<ParsedFile, ManagedBlockError> {
    let start_count = content.matches(start_marker).count();
    let end_count = content.matches(end_marker).count();
    if start_count == 0 && end_count == 0 {
        return Ok(ParsedFile {
            before_user: content.to_string(),
            managed: None,
            after_user: String::new(),
        });
    }
    if start_count != 1 || end_count != 1 {
        return Err(ManagedBlockError::MalformedMarker(format!(
            "expected exactly 1 start + 1 end marker, found start={start_count} end={end_count}"
        )));
    }
    let start_idx = content.find(start_marker).unwrap();
    let end_idx = content.find(end_marker).unwrap();
    if end_idx < start_idx {
        return Err(ManagedBlockError::MalformedMarker(
            "end marker appears before start marker".to_string(),
        ));
    }
    let before = &content[..start_idx];
    // skip start_marker + 后续换行
    let mut managed_start = start_idx + start_marker.len();
    if content.as_bytes().get(managed_start) == Some(&b'\n') {
        managed_start += 1;
    }
    let mut managed_end = end_idx;
    if managed_end > 0 && content.as_bytes().get(managed_end - 1) == Some(&b'\n') {
        managed_end -= 1;
    }
    let managed = if managed_end > managed_start {
        content[managed_start..managed_end].to_string()
    } else {
        String::new()
    };
    let mut after_start = end_idx + end_marker.len();
    if content.as_bytes().get(after_start) == Some(&b'\n') {
        after_start += 1;
    }
    let after = if after_start <= content.len() {
        content[after_start..].to_string()
    } else {
        String::new()
    };
    Ok(ParsedFile {
        before_user: before.to_string(),
        managed: Some(managed),
        after_user: after,
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_paths() -> (PathBuf, PathBuf) {
        // 并行 test 跑同 second 时, 仅靠 (pid, unix) 会撞 dir, 加 random 8 bytes 区分
        let mut rand_buf = [0u8; 8];
        let _ = getrandom::getrandom(&mut rand_buf);
        let rand_hex: String = rand_buf.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!(
            "cas-managed-block-test-{}-{}-{}",
            std::process::id(),
            now_unix(),
            rand_hex,
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (dir.join("AGENTS.md"), dir.join("history.json"))
    }

    fn block(target: &PathBuf, history: &PathBuf) -> MarkdownManagedBlock {
        MarkdownManagedBlock {
            block_type: "agents",
            target: target.clone(),
            history: history.clone(),
        }
    }

    #[test]
    fn parse_no_marker_treats_full_content_as_user_region() {
        let parsed = parse_markdown_managed_block(
            "## User written\n\nhello world\n",
            "<!-- cas:managed:agents:v1:start -->",
            "<!-- cas:managed:agents:v1:end -->",
        )
        .unwrap();
        assert!(parsed.managed.is_none());
        assert_eq!(parsed.before_user, "## User written\n\nhello world\n");
        assert!(parsed.after_user.is_empty());
    }

    #[test]
    fn parse_with_marker_splits_three_regions() {
        let content = "user before\n<!-- cas:managed:agents:v1:start -->\napp content here\n<!-- cas:managed:agents:v1:end -->\nuser after\n";
        let parsed = parse_markdown_managed_block(
            content,
            "<!-- cas:managed:agents:v1:start -->",
            "<!-- cas:managed:agents:v1:end -->",
        )
        .unwrap();
        assert_eq!(parsed.before_user, "user before\n");
        assert_eq!(parsed.managed.as_deref(), Some("app content here"));
        assert_eq!(parsed.after_user, "user after\n");
    }

    #[test]
    fn parse_rejects_multiple_marker_pairs() {
        let content = "<!-- cas:managed:agents:v1:start -->\nA\n<!-- cas:managed:agents:v1:end -->\n<!-- cas:managed:agents:v1:start -->\nB\n<!-- cas:managed:agents:v1:end -->\n";
        let err = parse_markdown_managed_block(
            content,
            "<!-- cas:managed:agents:v1:start -->",
            "<!-- cas:managed:agents:v1:end -->",
        )
        .unwrap_err();
        assert!(matches!(err, ManagedBlockError::MalformedMarker(_)));
    }

    #[test]
    fn parse_rejects_end_before_start() {
        let content = "<!-- cas:managed:agents:v1:end -->\n<!-- cas:managed:agents:v1:start -->\n";
        let err = parse_markdown_managed_block(
            content,
            "<!-- cas:managed:agents:v1:start -->",
            "<!-- cas:managed:agents:v1:end -->",
        )
        .unwrap_err();
        assert!(matches!(err, ManagedBlockError::MalformedMarker(_)));
    }

    #[test]
    fn apply_writes_marker_and_pushes_history() {
        let (target, history) = tmp_paths();
        fs::write(&target, "## User\n\noriginal\n").unwrap();
        let b = block(&target, &history);

        b.apply("# managed v1\n- item 1\n- item 2").unwrap();
        let after = fs::read_to_string(&target).unwrap();
        assert!(after.contains("<!-- cas:managed:agents:v1:start -->"));
        assert!(after.contains("# managed v1"));
        assert!(after.contains("<!-- cas:managed:agents:v1:end -->"));
        assert!(after.starts_with("## User\n\noriginal\n"));

        let hist = b.read_history().unwrap();
        assert_eq!(hist.len(), 1, "first apply should push 1 history entry");
        assert_eq!(hist[0].managed_content, "", "old managed was empty");
        assert_eq!(hist[0].applied_content, "# managed v1\n- item 1\n- item 2");

        let _ = fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn preview_does_not_touch_disk() {
        let (target, history) = tmp_paths();
        fs::write(&target, "user\n").unwrap();
        let b = block(&target, &history);
        let preview = b.preview("new content").unwrap();
        assert!(preview.contains("new content"));
        // 文件本身未改
        assert_eq!(fs::read_to_string(&target).unwrap(), "user\n");
        // history 文件未生成
        assert!(!history.exists());
        let _ = fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn rollback_restores_previous_managed_content() {
        let (target, history) = tmp_paths();
        fs::write(&target, "user\n").unwrap();
        let b = block(&target, &history);
        b.apply("v1").unwrap();
        b.apply("v2").unwrap();
        // 状态: file managed = v2, history = [v0→v1, v1→v2]
        let hist = b.read_history().unwrap();
        assert_eq!(hist.len(), 2);
        // rollback 到 index 0 (v0→v1 entry 的 managed_content = "" 是 v0 状态)
        b.rollback(0).unwrap();
        let parsed = b.parse().unwrap();
        assert_eq!(parsed.managed.as_deref(), Some(""));
        let _ = fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn clear_removes_marker_and_pushes_history() {
        let (target, history) = tmp_paths();
        fs::write(&target, "user\n").unwrap();
        let b = block(&target, &history);
        b.apply("managed").unwrap();
        b.clear().unwrap();
        let parsed = b.parse().unwrap();
        assert!(parsed.managed.is_none(), "clear should remove marker");
        let after = fs::read_to_string(&target).unwrap();
        assert!(!after.contains("cas:managed:agents:v1:start"));
        let hist = b.read_history().unwrap();
        // apply + clear = 2 entries
        assert_eq!(hist.len(), 2);
        let _ = fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn history_ring_buffer_caps_at_limit() {
        let (target, history) = tmp_paths();
        fs::write(&target, "user\n").unwrap();
        let b = block(&target, &history);
        for i in 0..(HISTORY_LIMIT + 5) {
            b.apply(&format!("v{i}")).unwrap();
        }
        let hist = b.read_history().unwrap();
        assert_eq!(
            hist.len(),
            HISTORY_LIMIT,
            "history must cap at HISTORY_LIMIT"
        );
        // 最老的 v0..v4 被丢, 留 v5..v14
        assert_eq!(hist[0].applied_content, "v5");
        assert_eq!(hist.last().unwrap().applied_content, "v14");
        let _ = fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn status_json_emits_useful_fields() {
        let (target, history) = tmp_paths();
        fs::write(&target, "user\n").unwrap();
        let b = block(&target, &history);
        let status = b.status_json().unwrap();
        assert_eq!(status["blockType"], "agents");
        assert_eq!(status["hasManaged"], false);
        assert_eq!(status["historyCount"], 0);

        b.apply("content").unwrap();
        let status = b.status_json().unwrap();
        assert_eq!(status["hasManaged"], true);
        assert_eq!(status["managedContent"], "content");
        assert_eq!(status["historyCount"], 1);
        let _ = fs::remove_dir_all(target.parent().unwrap());
    }

    #[test]
    fn render_keeps_user_regions_intact_around_managed() {
        let content = "before\n<!-- cas:managed:agents:v1:start -->\nold\n<!-- cas:managed:agents:v1:end -->\nafter\n";
        let mut parsed = parse_markdown_managed_block(
            content,
            "<!-- cas:managed:agents:v1:start -->",
            "<!-- cas:managed:agents:v1:end -->",
        )
        .unwrap();
        parsed.managed = Some("NEW".to_string());
        let rendered = parsed.render(
            "<!-- cas:managed:agents:v1:start -->",
            "<!-- cas:managed:agents:v1:end -->",
        );
        assert!(rendered.contains("before\n"));
        assert!(rendered.contains("NEW"));
        assert!(rendered.contains("after\n"));
        assert!(!rendered.contains("\nold\n"));
    }
}
