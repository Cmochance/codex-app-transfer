//! Codex CLI rollout JSONL 对话导出(#271).
//!
//! 数据源:
//! - `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<sid>.jsonl` — active sessions
//! - `~/.codex/archived_sessions/rollout-<ts>-<sid>.jsonl` — 用户/系统归档
//! - `~/.codex/session_index.jsonl` — Codex Desktop 起的 `thread_name` 与
//!   session id 的映射,用来做 list 显示的人类可读 title
//!
//! Pipeline:
//! 1. [`list_sessions`] 扫两个目录 + 合并 session_index 拿 title
//! 2. [`parse_session`] 流式读 JSONL → [`NormalizedSession`]
//! 3. [`export_markdown`] / [`export_json`] / raw JSONL 1:1 copy
//!
//! 设计目标:
//! - **streaming**:rollout 文件可达几十 MB,所有 IO 路径都是 line-by-line
//! - **容忍部分行解析失败**:live session 尾部可能行未 flush 完整
//! - **secret redaction**:导出前替换 `sk-…` / `cas_…` / JWT / Bearer 令牌

pub mod export;
pub mod list;
pub mod parse;
pub mod redact;
pub mod trash_ops;
pub mod types;

pub use export::{export_json, export_markdown, read_raw_jsonl, write_bulk_zip};
pub use list::{list_sessions, read_session_index_titles};
pub use parse::parse_session;
pub use redact::redact_secrets;
pub use trash_ops::{move_all_sessions_to_trash, move_sessions_to_trash, TrashResult};
pub use types::{
    ExportFormat, ExportOptions, NormalizedSession, RolloutKind, SessionMeta, Turn, TurnItem,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("session not found: {0}")]
    NotFound(String),
}
