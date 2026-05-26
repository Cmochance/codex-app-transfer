//! 把 [`NormalizedSession`] 渲染成 Markdown / JSON / 把多个 session 打 zip.

use crate::redact::redact_secrets;
use crate::types::{ExportOptions, NormalizedSession, TurnItem};
use crate::ExportError;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// Markdown 单 session 导出。
pub fn export_markdown(session: &NormalizedSession, opts: &ExportOptions) -> String {
    let mut out = String::with_capacity(4096);

    if let Some(meta) = &session.meta {
        let title = meta.title.as_deref().unwrap_or("");
        let display_title = if !title.is_empty() {
            title.to_string()
        } else {
            format!("Session {}", short_id(&meta.id))
        };
        out.push_str(&format!("# {display_title}\n\n"));
        out.push_str(&format!(
            "- **Session ID**: `{}`\n",
            apply_redact(opts, &meta.id)
        ));
        out.push_str(&format!(
            "- **创建时间**: {}\n",
            meta.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        ));
        out.push_str(&format!("- **项目**: `{}`\n", meta.cwd.display()));
        if !meta.originator.is_empty() {
            out.push_str(&format!("- **来源**: {}\n", meta.originator));
        }
        if !meta.cli_version.is_empty() {
            out.push_str(&format!("- **Codex CLI**: {}\n", meta.cli_version));
        }
        if !meta.model_provider.is_empty() {
            out.push_str(&format!("- **Provider**: {}\n", meta.model_provider));
        }
        out.push('\n');
        out.push_str("---\n\n");
    }

    for (i, turn) in session.turns.iter().enumerate() {
        out.push_str(&format!("## Turn {}\n\n", i + 1));
        for item in &turn.items {
            render_turn_item_md(item, opts, &mut out);
        }
    }

    out
}

fn render_turn_item_md(item: &TurnItem, opts: &ExportOptions, out: &mut String) {
    match item {
        TurnItem::User { text, .. } => {
            out.push_str("**用户**:\n\n");
            out.push_str(&apply_redact(opts, text));
            out.push_str("\n\n");
        }
        TurnItem::Assistant { text, .. } => {
            out.push_str("**助手**:\n\n");
            out.push_str(&apply_redact(opts, text));
            out.push_str("\n\n");
        }
        TurnItem::Reasoning { text, .. } if opts.include_reasoning => {
            out.push_str("<details><summary>Reasoning</summary>\n\n");
            out.push_str(&apply_redact(opts, text));
            out.push_str("\n\n</details>\n\n");
        }
        TurnItem::Reasoning { .. } => {}
        TurnItem::ToolCall {
            name, arguments, ..
        } if opts.include_tool_calls => {
            let label = truncate(arguments, 120);
            out.push_str(&format!(
                "<details><summary>🔧 `{name}` — {label}</summary>\n\n```\n{}\n```\n\n</details>\n\n",
                apply_redact(opts, arguments)
            ));
        }
        TurnItem::ToolCall { .. } => {}
        TurnItem::ToolOutput { output, .. } if opts.include_tool_calls => {
            let truncated = output.chars().count() > opts.tool_output_max_chars;
            let body = if truncated {
                let mut s: String = output.chars().take(opts.tool_output_max_chars).collect();
                s.push_str(&format!(
                    "\n... [truncated {} more chars]",
                    output.chars().count() - opts.tool_output_max_chars
                ));
                s
            } else {
                output.clone()
            };
            out.push_str(&format!(
                "<details><summary>↳ output</summary>\n\n```\n{}\n```\n\n</details>\n\n",
                apply_redact(opts, &body)
            ));
        }
        TurnItem::ToolOutput { .. } => {}
        TurnItem::Compacted { summary, .. } => {
            out.push_str("> 📦 **[Autocompact 切点]** 之前若干轮被压缩为下方 summary:\n>\n");
            for line in summary.lines() {
                out.push_str(&format!("> {}\n", apply_redact(opts, line)));
            }
            out.push('\n');
        }
        TurnItem::System { role, text, .. } if opts.include_system_prompts => {
            out.push_str(&format!("<details><summary>[{role}]</summary>\n\n"));
            out.push_str(&apply_redact(opts, text));
            out.push_str("\n\n</details>\n\n");
        }
        TurnItem::System { .. } => {}
    }
}

/// JSON 导出 — 给后续工具链消费,结构对齐 NormalizedSession + apply 同样的
/// redact / truncate / include 开关。
pub fn export_json(session: &NormalizedSession, opts: &ExportOptions) -> Value {
    let turns: Vec<Value> = session
        .turns
        .iter()
        .enumerate()
        .map(|(i, turn)| {
            let items: Vec<Value> = turn
                .items
                .iter()
                .filter_map(|item| json_for_item(item, opts))
                .collect();
            json!({ "turnIndex": i + 1, "items": items })
        })
        .collect();
    json!({
        "session": session.meta.as_ref().map(|m| json!({
            "id": apply_redact_owned(opts, &m.id),
            "title": m.title,
            "createdAt": m.created_at,
            "cwd": m.cwd,
            "originator": m.originator,
            "cliVersion": m.cli_version,
            "modelProvider": m.model_provider,
            "kind": match m.kind {
                crate::types::RolloutKind::Active => "active",
                crate::types::RolloutKind::Archived => "archived",
            },
        })),
        "turns": turns,
        "warnings": session.warnings,
    })
}

fn json_for_item(item: &TurnItem, opts: &ExportOptions) -> Option<Value> {
    match item {
        TurnItem::User { text, timestamp } => Some(json!({
            "type": "user", "text": apply_redact_owned(opts, text), "timestamp": timestamp,
        })),
        TurnItem::Assistant { text, timestamp } => Some(json!({
            "type": "assistant", "text": apply_redact_owned(opts, text), "timestamp": timestamp,
        })),
        TurnItem::Reasoning { text, timestamp } => {
            if !opts.include_reasoning {
                return None;
            }
            Some(json!({
                "type": "reasoning", "text": apply_redact_owned(opts, text), "timestamp": timestamp,
            }))
        }
        TurnItem::ToolCall {
            name,
            arguments,
            call_id,
            timestamp,
        } => {
            if !opts.include_tool_calls {
                return None;
            }
            Some(json!({
                "type": "toolCall",
                "name": name,
                "arguments": apply_redact_owned(opts, arguments),
                "callId": call_id,
                "timestamp": timestamp,
            }))
        }
        TurnItem::ToolOutput {
            call_id,
            output,
            timestamp,
            ..
        } => {
            if !opts.include_tool_calls {
                return None;
            }
            let count = output.chars().count();
            let (body, truncated) = if count > opts.tool_output_max_chars {
                let s: String = output.chars().take(opts.tool_output_max_chars).collect();
                (s, true)
            } else {
                (output.clone(), false)
            };
            Some(json!({
                "type": "toolOutput",
                "callId": call_id,
                "output": apply_redact_owned(opts, &body),
                "truncated": truncated,
                "timestamp": timestamp,
            }))
        }
        TurnItem::Compacted { summary, timestamp } => Some(json!({
            "type": "compacted",
            "summary": apply_redact_owned(opts, summary),
            "timestamp": timestamp,
        })),
        TurnItem::System {
            role,
            text,
            timestamp,
        } => {
            if !opts.include_system_prompts {
                return None;
            }
            Some(json!({
                "type": "system",
                "role": role,
                "text": apply_redact_owned(opts, text),
                "timestamp": timestamp,
            }))
        }
    }
}

/// 多 session 打包成 zip:caller 给 `(name, bytes)` 迭代器,写到 writer。
///
/// MVP 限制:不流式压缩单个超大文件(zip crate API 不便),所有文件先在内存里
/// 准备好。对单 session export 而言,几百 KB 到几 MB 量级在桌面环境无压力。
pub fn write_bulk_zip<W: Write + std::io::Seek>(
    writer: &mut W,
    entries: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> Result<(), ExportError> {
    let mut zw = zip::ZipWriter::new(writer);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        zw.start_file(name, opts)?;
        zw.write_all(&bytes)?;
    }
    zw.finish()?;
    Ok(())
}

fn apply_redact<'a>(opts: &ExportOptions, text: &'a str) -> std::borrow::Cow<'a, str> {
    if opts.redact_secrets {
        redact_secrets(text)
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

fn apply_redact_owned(opts: &ExportOptions, text: &str) -> String {
    apply_redact(opts, text).into_owned()
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.replace('\n', " ")
    } else {
        let prefix: String = s.chars().take(max_chars).collect();
        format!("{}…", prefix.replace('\n', " "))
    }
}

/// 复制原始 rollout JSONL 一字节不动给 raw 导出。
pub fn read_raw_jsonl(path: &Path) -> Result<Vec<u8>, ExportError> {
    Ok(std::fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RolloutKind, SessionMeta, Turn};
    use chrono::Utc;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn make_session() -> NormalizedSession {
        let meta = SessionMeta {
            id: "019df883-b067-7181-b074-1fbea01434d3".into(),
            path: PathBuf::from("/tmp/rollout.jsonl"),
            kind: RolloutKind::Active,
            created_at: Utc::now(),
            last_modified: Utc::now(),
            cwd: PathBuf::from("/p"),
            originator: "Codex Desktop".into(),
            cli_version: "0.130".into(),
            model_provider: "openai".into(),
            turn_count: 1,
            title: Some("测试会话".into()),
        };
        let turn = Turn {
            turn_index: 0,
            items: vec![
                TurnItem::User {
                    text: "hello".into(),
                    timestamp: None,
                },
                TurnItem::Assistant {
                    text: "hi! my key is sk-abcd1234efgh5678 btw".into(),
                    timestamp: None,
                },
                TurnItem::Reasoning {
                    text: "thinking...".into(),
                    timestamp: None,
                },
                TurnItem::ToolCall {
                    name: "shell".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                    call_id: Some("c1".into()),
                    timestamp: None,
                },
                TurnItem::ToolOutput {
                    call_id: Some("c1".into()),
                    output: "a\nb\nc".into(),
                    truncated: false,
                    timestamp: None,
                },
            ],
        };
        NormalizedSession {
            meta: Some(meta),
            turns: vec![turn],
            warnings: vec![],
        }
    }

    #[test]
    fn markdown_export_includes_title_and_user_assistant() {
        let s = make_session();
        let md = export_markdown(&s, &ExportOptions::default());
        assert!(md.contains("# 测试会话"));
        assert!(md.contains("**用户**"));
        assert!(md.contains("**助手**"));
        assert!(md.contains("hello"));
    }

    #[test]
    fn markdown_redacts_secrets_by_default() {
        let s = make_session();
        let md = export_markdown(&s, &ExportOptions::default());
        assert!(md.contains("[REDACTED]"));
        assert!(!md.contains("sk-abcd1234efgh5678"));
    }

    #[test]
    fn markdown_hides_reasoning_by_default_and_shows_when_enabled() {
        let s = make_session();
        let mut opts = ExportOptions::default();
        opts.include_reasoning = false;
        let md = export_markdown(&s, &opts);
        assert!(!md.contains("thinking..."));

        opts.include_reasoning = true;
        let md = export_markdown(&s, &opts);
        assert!(md.contains("thinking..."));
    }

    #[test]
    fn markdown_hides_tool_calls_when_disabled() {
        let s = make_session();
        let mut opts = ExportOptions::default();
        opts.include_tool_calls = false;
        let md = export_markdown(&s, &opts);
        assert!(!md.contains("shell"));
        assert!(!md.contains("output"));
    }

    #[test]
    fn markdown_truncates_long_tool_output() {
        let mut s = make_session();
        s.turns[0].items.push(TurnItem::ToolOutput {
            call_id: Some("big".into()),
            output: "X".repeat(5000),
            truncated: false,
            timestamp: None,
        });
        let mut opts = ExportOptions::default();
        opts.tool_output_max_chars = 100;
        let md = export_markdown(&s, &opts);
        assert!(md.contains("truncated"));
    }

    #[test]
    fn json_export_round_trip() {
        let s = make_session();
        let v = export_json(&s, &ExportOptions::default());
        assert_eq!(v["turns"].as_array().unwrap().len(), 1);
        // tool call 包括
        let items = v["turns"][0]["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["type"] == "toolCall"));
    }

    #[test]
    fn bulk_zip_writes_multiple_files() {
        let mut buf = Cursor::new(Vec::<u8>::new());
        write_bulk_zip(
            &mut buf,
            [
                ("a.md".to_string(), b"hello".to_vec()),
                ("b.json".to_string(), b"{}".to_vec()),
            ],
        )
        .unwrap();
        let bytes = buf.into_inner();
        assert!(bytes.len() > 0);
        // zip 文件以 "PK" 开头
        assert_eq!(&bytes[0..2], b"PK");
    }
}
