//! 流式解析单个 rollout JSONL → [`NormalizedSession`].
//!
//! Turn 切分规则:`event_msg/task_started` 之间为一 turn(首个 task_started
//! 之前的 `developer` / `system` 头部 item 归到 turn 0)。
//!
//! 容错:
//! - 行 JSON parse 失败 → warning + 跳过(不中断整个解析)
//! - 未知 type → 静默丢(不报 warning,避免 Codex 后续加新 type 时刷屏)
//! - 文件被截断(live session 尾部行未 flush 完整)→ 容忍最后一行 parse 失败

use crate::types::{NormalizedSession, RolloutKind, SessionMeta, Turn, TurnItem};
use crate::ExportError;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct EventLine {
    #[serde(default)]
    timestamp: Option<DateTime<Utc>>,
    r#type: String,
    payload: Option<Value>,
}

/// 解析一个 rollout JSONL 文件 → [`NormalizedSession`]。
pub fn parse_session(path: &Path) -> Result<NormalizedSession, ExportError> {
    let file = std::fs::File::open(path)?;
    let last_modified: DateTime<Utc> = file
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);
    let kind = if path.to_string_lossy().contains("archived_sessions") {
        RolloutKind::Archived
    } else {
        RolloutKind::Active
    };

    let mut session = NormalizedSession::default();
    let mut current_turn = Turn {
        turn_index: 0,
        items: Vec::new(),
    };
    let mut turn_seen = false;

    for (lineno, raw) in BufReader::new(file).lines().enumerate() {
        let line = match raw {
            Ok(s) => s,
            Err(e) => {
                session
                    .warnings
                    .push(format!("line {} read failed: {e}", lineno + 1));
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event: EventLine = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(e) => {
                session
                    .warnings
                    .push(format!("line {} json parse failed: {e}", lineno + 1));
                continue;
            }
        };

        match event.r#type.as_str() {
            "session_meta" => {
                if let Some(meta) = parse_session_meta(event.payload, path, kind, last_modified) {
                    session.meta = Some(meta);
                }
            }
            "event_msg" => {
                handle_event_msg(
                    event.payload,
                    event.timestamp,
                    &mut session,
                    &mut current_turn,
                    &mut turn_seen,
                );
            }
            "response_item" => {
                handle_response_item(event.payload, event.timestamp, &mut current_turn);
            }
            "compacted" => {
                if let Some(text) = event
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("message"))
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        event
                            .payload
                            .as_ref()
                            .and_then(|p| p.get("summary"))
                            .and_then(|v| v.as_str())
                    })
                {
                    current_turn.items.push(TurnItem::Compacted {
                        summary: text.to_string(),
                        timestamp: event.timestamp,
                    });
                } else {
                    current_turn.items.push(TurnItem::Compacted {
                        summary: String::new(),
                        timestamp: event.timestamp,
                    });
                }
            }
            "turn_context" => {
                // turn_context 含 model / approval / sandbox 等配置,不进 export
            }
            _ => {
                // 未知 type 静默丢
            }
        }
    }

    if !current_turn.items.is_empty() {
        session.turns.push(current_turn);
    }
    Ok(session)
}

fn parse_session_meta(
    payload: Option<Value>,
    path: &Path,
    kind: RolloutKind,
    last_modified: DateTime<Utc>,
) -> Option<SessionMeta> {
    let p = payload?;
    let id = p.get("id")?.as_str()?.to_string();
    let cwd = p
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let created_at: DateTime<Utc> = p
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    Some(SessionMeta {
        id,
        path: path.to_path_buf(),
        kind,
        created_at,
        last_modified,
        cwd: PathBuf::from(cwd),
        originator: p
            .get("originator")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        cli_version: p
            .get("cli_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        model_provider: p
            .get("model_provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        turn_count: 0,
        title: None,
    })
}

fn handle_event_msg(
    payload: Option<Value>,
    timestamp: Option<DateTime<Utc>>,
    session: &mut NormalizedSession,
    current_turn: &mut Turn,
    turn_seen: &mut bool,
) {
    let Some(p) = payload else { return };
    let Some(sub) = p.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match sub {
        "task_started" => {
            // 切 turn:把 current_turn(若非空)flush,开新的
            if !current_turn.items.is_empty() {
                session.turns.push(std::mem::take(current_turn));
                current_turn.turn_index = session.turns.len();
            } else if *turn_seen {
                // 已经开过 turn 但内容为空(很罕见),不重复 push
            }
            *turn_seen = true;
        }
        "user_message" => {
            if let Some(text) = p.get("message").and_then(|v| v.as_str()) {
                current_turn.items.push(TurnItem::User {
                    text: text.to_string(),
                    timestamp,
                });
            }
        }
        "exec_command_end" => {
            let stdout = p
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let stderr = p.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            let exit = p
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            let call_id = p.get("call_id").and_then(|v| v.as_str()).map(String::from);
            let mut combined = stdout;
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(stderr);
            }
            if exit != 0 {
                combined.push_str(&format!("\n(exit {exit})"));
            }
            current_turn.items.push(TurnItem::ToolOutput {
                call_id,
                output: combined,
                truncated: false,
                timestamp,
            });
        }
        "patch_apply_end" => {
            let success = p.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
            let stdout = p.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let call_id = p.get("call_id").and_then(|v| v.as_str()).map(String::from);
            let output = if success {
                format!("(patch applied)\n{stdout}")
            } else {
                format!("(patch FAILED)\n{stdout}")
            };
            current_turn.items.push(TurnItem::ToolOutput {
                call_id,
                output,
                truncated: false,
                timestamp,
            });
        }
        "mcp_tool_call_end" | "web_search_end" | "image_generation_end" => {
            let result = p
                .get("result")
                .map(|v| v.to_string())
                .or_else(|| p.get("output").map(|v| v.to_string()))
                .unwrap_or_else(|| format!("({sub})"));
            current_turn.items.push(TurnItem::ToolOutput {
                call_id: p.get("call_id").and_then(|v| v.as_str()).map(String::from),
                output: result,
                truncated: false,
                timestamp,
            });
        }
        "context_compacted" => {
            if let Some(text) = p.get("summary").and_then(|v| v.as_str()) {
                current_turn.items.push(TurnItem::Compacted {
                    summary: text.to_string(),
                    timestamp,
                });
            }
        }
        // agent_message / token_count / task_complete / turn_aborted /
        // dynamic_tool_call_* 等不入 export(agent_message 是流式 delta,
        // 最终 message 由 response_item/message 给完整版)
        _ => {}
    }
}

fn handle_response_item(
    payload: Option<Value>,
    timestamp: Option<DateTime<Utc>>,
    current_turn: &mut Turn,
) {
    let Some(p) = payload else { return };
    let Some(item_type) = p.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match item_type {
        "message" => {
            let role = p
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            let text = extract_message_text(p.get("content"));
            if text.is_empty() {
                return;
            }
            match role.as_str() {
                "assistant" => current_turn
                    .items
                    .push(TurnItem::Assistant { text, timestamp }),
                "user" => current_turn.items.push(TurnItem::User { text, timestamp }),
                _ => current_turn.items.push(TurnItem::System {
                    role,
                    text,
                    timestamp,
                }),
            }
        }
        "reasoning" => {
            let text = extract_reasoning_text(p.get("summary"));
            if !text.is_empty() {
                current_turn
                    .items
                    .push(TurnItem::Reasoning { text, timestamp });
            }
        }
        "function_call" | "custom_tool_call" => {
            let name = p
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("(unnamed)")
                .to_string();
            let arguments = p
                .get("arguments")
                .or_else(|| p.get("input"))
                .map(|v| {
                    if let Some(s) = v.as_str() {
                        s.to_string()
                    } else {
                        v.to_string()
                    }
                })
                .unwrap_or_default();
            let call_id = p
                .get("call_id")
                .or_else(|| p.get("id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            current_turn.items.push(TurnItem::ToolCall {
                name,
                arguments,
                call_id,
                timestamp,
            });
        }
        "function_call_output" | "custom_tool_call_output" => {
            let output = p
                .get("output")
                .or_else(|| p.get("result"))
                .map(|v| {
                    if let Some(s) = v.as_str() {
                        s.to_string()
                    } else {
                        v.to_string()
                    }
                })
                .unwrap_or_default();
            let call_id = p
                .get("call_id")
                .or_else(|| p.get("id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            current_turn.items.push(TurnItem::ToolOutput {
                call_id,
                output,
                truncated: false,
                timestamp,
            });
        }
        "web_search_call" | "image_generation_call" => {
            let name = format!("({item_type})");
            let arguments = p
                .get("query")
                .or_else(|| p.get("prompt"))
                .map(|v| v.to_string())
                .unwrap_or_default();
            current_turn.items.push(TurnItem::ToolCall {
                name,
                arguments,
                call_id: None,
                timestamp,
            });
        }
        _ => {}
    }
}

fn extract_message_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    match c {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut parts: Vec<String> = Vec::new();
            for it in items {
                if let Some(text) = it.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        parts.push(text.to_string());
                    }
                } else if let Some(s) = it.as_str() {
                    parts.push(s.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn extract_reasoning_text(summary: Option<&Value>) -> String {
    let Some(s) = summary else {
        return String::new();
    };
    let Some(arr) = s.as_array() else {
        return s.as_str().unwrap_or("").to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    for item in arr {
        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        } else if let Some(s) = item.as_str() {
            parts.push(s.to_string());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session_jsonl(path: &Path, lines: &[&str]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn parses_basic_session_with_one_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"timestamp":"2026-05-26T10:00:00Z","type":"session_meta","payload":{"id":"sess-1","timestamp":"2026-05-26T10:00:00Z","cwd":"/p","originator":"Codex","cli_version":"0.130","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-05-26T10:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
                r#"{"timestamp":"2026-05-26T10:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
                r#"{"timestamp":"2026-05-26T10:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}}"#,
            ],
        );

        let s = parse_session(&path).unwrap();
        assert!(s.meta.is_some());
        assert_eq!(s.meta.as_ref().unwrap().id, "sess-1");
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].items.len(), 2);
        assert!(matches!(&s.turns[0].items[0], TurnItem::User { text, .. } if text == "hello"));
        assert!(
            matches!(&s.turns[0].items[1], TurnItem::Assistant { text, .. } if text == "hi there")
        );
    }

    #[test]
    fn parses_tool_call_and_output_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"c-1"}}"#,
                r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"c-1","stdout":"file1.txt","stderr":"","exit_code":0}}"#,
            ],
        );
        let s = parse_session(&path).unwrap();
        assert_eq!(s.turns.len(), 1);
        let items = &s.turns[0].items;
        assert_eq!(items.len(), 2);
        match &items[0] {
            TurnItem::ToolCall { name, call_id, .. } => {
                assert_eq!(name, "shell");
                assert_eq!(call_id.as_deref(), Some("c-1"));
            }
            _ => panic!("expected ToolCall"),
        }
        match &items[1] {
            TurnItem::ToolOutput {
                call_id, output, ..
            } => {
                assert_eq!(call_id.as_deref(), Some("c-1"));
                assert!(output.contains("file1.txt"));
            }
            _ => panic!("expected ToolOutput"),
        }
    }

    #[test]
    fn parses_reasoning_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"text":"thought A"},{"text":"thought B"}]}}"#,
            ],
        );
        let s = parse_session(&path).unwrap();
        match &s.turns[0].items[0] {
            TurnItem::Reasoning { text, .. } => {
                assert!(text.contains("thought A"));
                assert!(text.contains("thought B"));
            }
            _ => panic!("expected Reasoning"),
        }
    }

    #[test]
    fn splits_turns_on_task_started() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Q1"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"A1"}]}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"Q2"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"A2"}]}}"#,
            ],
        );
        let s = parse_session(&path).unwrap();
        assert_eq!(s.turns.len(), 2);
        assert_eq!(s.turns[0].items.len(), 2);
        assert_eq!(s.turns[1].items.len(), 2);
    }

    #[test]
    fn tolerates_malformed_lines_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}"#,
                r#"{ malformed json this line"#, // 模拟 live session 尾部断行
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
            ],
        );
        let s = parse_session(&path).unwrap();
        assert!(!s.warnings.is_empty(), "应记录 warning 不 panic");
        assert_eq!(s.turns.len(), 1);
    }

    #[test]
    fn parses_compacted_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"type":"compacted","payload":{"message":"Summary of N prior turns"}}"#,
            ],
        );
        let s = parse_session(&path).unwrap();
        match &s.turns[0].items[0] {
            TurnItem::Compacted { summary, .. } => {
                assert!(summary.contains("Summary"));
            }
            _ => panic!("expected Compacted"),
        }
    }

    #[test]
    fn drops_developer_system_messages_into_system_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        write_session_jsonl(
            &path,
            &[
                r#"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}"#,
                r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"text":"<permissions instructions>"}]}}"#,
            ],
        );
        let s = parse_session(&path).unwrap();
        match &s.turns[0].items[0] {
            TurnItem::System { role, text, .. } => {
                assert_eq!(role, "developer");
                assert!(text.contains("permissions"));
            }
            _ => panic!("expected System"),
        }
    }
}
