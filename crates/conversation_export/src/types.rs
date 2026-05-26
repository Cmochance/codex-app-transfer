//! 对话导出公开类型.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// rollout 文件物理位置:active(sessions/) vs archived。前端 list 上加 chip
/// 让用户分辨,active 通常是 Codex 还在用的 session(可能还在被 append)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RolloutKind {
    Active,
    Archived,
}

/// list 视图的轻量 session 摘要 — 从 rollout 头几行 + session_index 合并出来,
/// **不**包含完整 turn 内容(用 [`crate::parse_session`] 拉详情)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub path: PathBuf,
    pub kind: RolloutKind,
    /// session_meta.timestamp(开 session 时间)
    pub created_at: DateTime<Utc>,
    /// rollout 文件 mtime,反映"最近一次写入";live session 的 created_at + mtime
    /// 间距是活跃时长。
    pub last_modified: DateTime<Utc>,
    pub cwd: PathBuf,
    pub originator: String,
    pub cli_version: String,
    pub model_provider: String,
    /// 粗略估计的 turn 数(`event_msg/user_message` 出现次数)— list 用,
    /// 不 streaming 全文,只够给用户一个量感
    pub turn_count: usize,
    /// 优先 session_index.jsonl 的 thread_name,否则 None(前端兜底 user_message 首段)
    pub title: Option<String>,
}

/// 解析后归一化的一通对话,按 turn 切分。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedSession {
    pub meta: Option<SessionMeta>,
    pub turns: Vec<Turn>,
    /// 解析时遇到的非致命 warning(行 parse 失败 / 未知 type 等),给前端 / 日志查
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub turn_index: usize,
    /// 本 turn 内事件按 timestamp 顺序;每条 item 是 user / assistant / reasoning /
    /// tool_call / tool_output / compacted_summary 之一
    pub items: Vec<TurnItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TurnItem {
    User {
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
    Assistant {
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
    /// reasoning summary text(`response_item/reasoning.summary[].text`)
    Reasoning {
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
    /// function_call / custom_tool_call
    ToolCall {
        name: String,
        arguments: String,
        call_id: Option<String>,
        timestamp: Option<DateTime<Utc>>,
    },
    /// function_call_output / custom_tool_call_output / exec_command_end output
    ToolOutput {
        call_id: Option<String>,
        output: String,
        truncated: bool,
        timestamp: Option<DateTime<Utc>>,
    },
    /// autocompact 摘要切点
    Compacted {
        summary: String,
        timestamp: Option<DateTime<Utc>>,
    },
    /// developer / system message(默认隐藏,带 `include_system_prompts` 才导出)
    System {
        role: String,
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Markdown,
    Json,
    Jsonl,
}

/// 导出选项 — 前端 dialog 收 + 持久化到 settings.
///
/// **devin #272 review fix**:`#[serde(default)]` 加到 struct 级,partial
/// payload(如 `{"options": {}}` 或缺字段)时缺的字段走 `Default` impl,
/// 而不是 422 反序列化失败。前端 / 老客户端给不全也能跑。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExportOptions {
    /// 是否包含 reasoning summary 块
    pub include_reasoning: bool,
    /// 是否包含 tool calls + outputs(默认 true)
    pub include_tool_calls: bool,
    /// tool output 单条最大字符数,超过截断(默认 2048)
    pub tool_output_max_chars: usize,
    /// 是否包含 system / developer 角色消息(默认 false,太冗长)
    pub include_system_prompts: bool,
    /// 是否 redact `sk-…` / `cas_…` / JWT / Bearer 等密钥模式(默认 true)
    pub redact_secrets: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_reasoning: false,
            include_tool_calls: true,
            tool_output_max_chars: 2048,
            include_system_prompts: false,
            redact_secrets: true,
        }
    }
}
