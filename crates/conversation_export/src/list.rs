//! 扫描 `~/.codex/sessions/**/*.jsonl` + `~/.codex/archived_sessions/*.jsonl`
//! → [`SessionMeta`] 列表(轻量,不读 full body)。

use crate::types::{RolloutKind, SessionMeta};
use crate::ExportError;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// session_index.jsonl 一行的解析结构。Codex Desktop 写,字段:
/// - `id`: session uuid
/// - `thread_name`: 用户给会话起的标题(或 Desktop 自动生成的)
/// - `updated_at`: 索引最近一次写入
#[derive(Debug, Deserialize)]
struct SessionIndexLine {
    id: String,
    #[serde(default)]
    thread_name: Option<String>,
}

/// 读 `~/.codex/session_index.jsonl`,返回 `{ session_id → thread_name }`。
/// 文件不存在 / parse 失败 → 返回空 map(降级,不阻塞 list)。
pub fn read_session_index_titles(codex_home: &Path) -> HashMap<String, String> {
    let path = codex_home.join("session_index.jsonl");
    let mut out = HashMap::new();
    let Ok(file) = std::fs::File::open(&path) else {
        return out;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<SessionIndexLine>(trimmed) {
            if let Some(name) = parsed.thread_name.filter(|s| !s.trim().is_empty()) {
                out.insert(parsed.id, name);
            }
        }
    }
    out
}

/// 扫两个目录下所有 rollout 文件,merge title,按 last_modified 倒序返回。
pub fn list_sessions(codex_home: &Path) -> Result<Vec<SessionMeta>, ExportError> {
    let titles = read_session_index_titles(codex_home);
    let mut out: Vec<SessionMeta> = Vec::new();

    let sessions_dir = codex_home.join("sessions");
    collect_rollouts_recursively(&sessions_dir, RolloutKind::Active, &mut out, &titles);

    let archived_dir = codex_home.join("archived_sessions");
    collect_rollouts_recursively(&archived_dir, RolloutKind::Archived, &mut out, &titles);

    out.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Ok(out)
}

fn collect_rollouts_recursively(
    dir: &Path,
    kind: RolloutKind,
    out: &mut Vec<SessionMeta>,
    titles: &HashMap<String, String>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts_recursively(&path, kind, out, titles);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(meta) = read_meta_from_rollout(&path, kind, titles) {
            out.push(meta);
        }
    }
}

/// 流式读 rollout 头部 + 扫 user_message 计数 turn_count。**不读全文**,只读直到
/// 拿到 session_meta + 一次性 count user_message 行数(用 grep-style 字节扫描)。
fn read_meta_from_rollout(
    path: &Path,
    kind: RolloutKind,
    titles: &HashMap<String, String>,
) -> Option<SessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let last_modified: DateTime<Utc> = file
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);

    let mut reader = BufReader::new(file);

    // 第一行必须是 session_meta(Codex CLI 行为),否则跳过该文件
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).ok()? == 0 {
        return None;
    }
    let session_meta: SessionMetaLine = serde_json::from_str(first_line.trim()).ok()?;
    if session_meta.r#type != "session_meta" {
        return None;
    }
    let payload = session_meta.payload?;

    // 继续扫剩余行数 user_message 次数(粗略 turn 估算)
    let mut turn_count = 0usize;
    for line in reader.lines().map_while(Result::ok) {
        // 廉价 substr 判定避免每行 full JSON parse(rollout 一行就到 60KB 时
        // parse 太贵)
        if line.contains("\"type\":\"user_message\"") {
            turn_count += 1;
        }
    }

    let title = titles.get(&payload.id).cloned();

    Some(SessionMeta {
        id: payload.id,
        path: path.to_path_buf(),
        kind,
        created_at: payload.timestamp,
        last_modified,
        cwd: PathBuf::from(payload.cwd),
        originator: payload.originator.unwrap_or_default(),
        cli_version: payload.cli_version.unwrap_or_default(),
        model_provider: payload.model_provider.unwrap_or_default(),
        turn_count,
        title,
    })
}

/// session_meta JSONL 行的结构(只取要用的字段,unknown 忽略)。
#[derive(Debug, Deserialize)]
struct SessionMetaLine {
    r#type: String,
    payload: Option<SessionMetaPayload>,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    timestamp: DateTime<Utc>,
    cwd: String,
    #[serde(default)]
    originator: Option<String>,
    #[serde(default)]
    cli_version: Option<String>,
    #[serde(default)]
    model_provider: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn write_session_meta_line(id: &str, ts: &str, cwd: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"{id}","timestamp":"{ts}","cwd":"{cwd}","originator":"Codex Desktop","cli_version":"0.130","model_provider":"openai"}}}}"#
        )
    }

    #[test]
    fn list_sessions_returns_active_and_archived_sorted_by_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path();

        let active = codex_home.join("sessions/2026/05/26/rollout-A.jsonl");
        write_jsonl(
            &active,
            &[
                &write_session_meta_line("id-active-1", "2026-05-26T10:00:00Z", "/cwd/a"),
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"again"}}"#,
            ],
        );

        let archived = codex_home.join("archived_sessions/rollout-B.jsonl");
        write_jsonl(
            &archived,
            &[&write_session_meta_line(
                "id-archived-1",
                "2026-05-20T10:00:00Z",
                "/cwd/b",
            )],
        );

        let sessions = list_sessions(codex_home).unwrap();
        assert_eq!(sessions.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let active = by_id.get("id-active-1").expect("active session 必返回");
        assert_eq!(active.kind, RolloutKind::Active);
        assert_eq!(active.turn_count, 2, "user_message 出现 2 次");
        let arch = by_id.get("id-archived-1").expect("archived session 必返回");
        assert_eq!(arch.kind, RolloutKind::Archived);
    }

    #[test]
    fn list_sessions_merges_title_from_session_index() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path();

        write_jsonl(
            &codex_home.join("session_index.jsonl"),
            &[
                r#"{"id":"id-1","thread_name":"分析数据","updated_at":"2026-05-26T00:00:00Z"}"#,
                r#"{"id":"id-2","thread_name":"","updated_at":"2026-05-26T00:00:00Z"}"#,
            ],
        );
        write_jsonl(
            &codex_home.join("sessions/2026/05/26/rollout-A.jsonl"),
            &[&write_session_meta_line(
                "id-1",
                "2026-05-26T10:00:00Z",
                "/p",
            )],
        );
        write_jsonl(
            &codex_home.join("sessions/2026/05/26/rollout-B.jsonl"),
            &[&write_session_meta_line(
                "id-2",
                "2026-05-26T11:00:00Z",
                "/p",
            )],
        );

        let sessions = list_sessions(codex_home).unwrap();
        let s1 = sessions.iter().find(|s| s.id == "id-1").unwrap();
        let s2 = sessions.iter().find(|s| s.id == "id-2").unwrap();
        assert_eq!(s1.title.as_deref(), Some("分析数据"));
        assert_eq!(s2.title, None, "空 thread_name 不应被当 title");
    }

    #[test]
    fn list_sessions_skips_non_jsonl_files() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path();
        // 放个非 jsonl 文件,目录里也放隐藏目录
        std::fs::create_dir_all(codex_home.join("sessions/2026")).unwrap();
        std::fs::write(codex_home.join("sessions/2026/.DS_Store"), b"junk").unwrap();
        std::fs::write(codex_home.join("sessions/2026/notes.txt"), b"hello").unwrap();
        write_jsonl(
            &codex_home.join("sessions/2026/05/rollout-A.jsonl"),
            &[&write_session_meta_line(
                "id-1",
                "2026-05-26T10:00:00Z",
                "/p",
            )],
        );

        let sessions = list_sessions(codex_home).unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn list_sessions_skips_rollouts_missing_session_meta_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path();
        // 首行不是 session_meta(脏数据),应跳过不 panic
        write_jsonl(
            &codex_home.join("sessions/rollout-bad.jsonl"),
            &[r#"{"type":"event_msg","payload":{"type":"task_started"}}"#],
        );
        let sessions = list_sessions(codex_home).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn read_session_index_titles_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let map = read_session_index_titles(tmp.path());
        assert!(map.is_empty());
    }
}
