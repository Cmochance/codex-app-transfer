//! 把选中的 rollout JSONL 移到系统回收站(macOS Trash / Win Recycle Bin /
//! Linux ~/.local/share/Trash) — **不是** unlink。用户可在 Finder Trash 恢复。
//!
//! 用 `trash` crate(跨平台)。失败回 `failed` 列表附原因,success 回 `deleted`。

use crate::list::list_sessions;
use crate::ExportError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TrashResult {
    /// 成功移到 trash 的 session id
    pub deleted: Vec<String>,
    /// 失败列表 + 原因
    pub failed: Vec<TrashFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashFailure {
    pub session_id: String,
    pub path: PathBuf,
    pub reason: String,
}

/// 给定 session id 列表 → 找到对应 rollout 文件 → 整批 trash::delete。
pub fn move_sessions_to_trash(
    codex_home: &Path,
    session_ids: &[String],
) -> Result<TrashResult, ExportError> {
    let all = list_sessions(codex_home)?;
    let mut by_id = std::collections::HashMap::new();
    for s in &all {
        by_id.insert(s.id.clone(), s.path.clone());
    }

    let mut result = TrashResult::default();
    for id in session_ids {
        let Some(path) = by_id.get(id) else {
            result.failed.push(TrashFailure {
                session_id: id.clone(),
                path: PathBuf::new(),
                reason: "session not found".into(),
            });
            continue;
        };
        match trash::delete(path) {
            Ok(_) => result.deleted.push(id.clone()),
            Err(e) => result.failed.push(TrashFailure {
                session_id: id.clone(),
                path: path.clone(),
                reason: e.to_string(),
            }),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn write_session_meta(id: &str) -> String {
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-05-26T10:00:00Z","cwd":"/p"}}}}"#
        )
    }

    /// macOS sandboxing 下 `trash` 在 CI runner 上可能无法访问 ~/.Trash;
    /// 这里只验"找不到 session id 时正确报 failed"分支,不真去 trash 文件。
    #[test]
    fn returns_failed_for_unknown_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let codex_home = tmp.path();
        write_jsonl(
            &codex_home.join("sessions/rollout-A.jsonl"),
            &[&write_session_meta("real-id")],
        );
        let r = move_sessions_to_trash(codex_home, &["nonexistent-id".to_string()]).unwrap();
        assert_eq!(r.deleted.len(), 0);
        assert_eq!(r.failed.len(), 1);
        assert!(r.failed[0].reason.contains("not found"));
    }
}
