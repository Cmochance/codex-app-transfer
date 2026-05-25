//! 读写 Codex Desktop 主进程的 `~/.codex/.codex-global-state.json` 文件。
//!
//! 该文件由 Codex Desktop 的 Electron 主进程用 jotai `atomWithStorage` +
//! electron-store backend 持久化,结构:
//!
//! ```jsonc
//! {
//!   "electron-saved-workspace-roots": [...],
//!   "electron-persisted-atom-state": {
//!     "local-conversation-status-section-visible": true,
//!     ...其它 atom...
//!   },
//!   ...其它顶层字段...
//! }
//! ```
//!
//! 本模块提供 **保守的最小侵入式** 修改 API:只改 `electron-persisted-atom-state`
//! 下指定 atom key,其它字段(workspace roots / prompt history / window bounds 等
//! Codex 自己维护的状态)**原样保留** —— 不读 / 不动 / 不重排序。
//!
//! **重要 race**:Codex Desktop **运行时**改这个文件会被它内存里的 atom 在下次
//! 写入时**覆盖**。所以调用必须发生在 Codex Desktop **启动前**(典型 caller:
//! `apply_provider` 在 user 切 provider 时,跟 `desktop` 模块"先 apply 再启动
//! Codex" 的时序天然配合)。

use std::path::Path;

use serde_json::{Map, Value};

use crate::CodexError;

/// 上下文圆环 + tokens/s 显示开关。Codex Desktop `/status` slash command
/// 切换的就是这个 key,默认 `false`(升级后 user 看不到圆环的真因,见 #258)。
pub const STATUS_SECTION_VISIBLE_KEY: &str = "local-conversation-status-section-visible";

/// `electron-persisted-atom-state` 顶层字段名。
const ATOM_STATE_KEY: &str = "electron-persisted-atom-state";

/// 读 `~/.codex/.codex-global-state.json` 并返回某 atom key 的当前值。
///
/// 严格区分"无值"vs"读不到"(silent-failure-hunter CRITICAL 提示):
/// - 文件 ENOENT(不存在)→ `Ok(None)`(合法"无值"语义)
/// - 其它 IO 错误(EACCES / EIO / FUSE 超时 / 中途读崩)→ `Err(CodexError::Io)`
/// - JSON 解析失败 → `Err(CodexError::Other)`(不要 silently 视作"无值",caller
///   snapshot 路径会据此决定不写 manifest pre_value,避免 restore 时 strip 掉
///   user 原有但因读错被 mask 的 atom)
/// - 文件正常但顶层不是 object,或没 `electron-persisted-atom-state` 段,或段
///   存在但 atom key 缺失 → `Ok(None)`(合法"无该字段"语义)
/// - atom key 存在 → `Ok(Some(value))`
pub fn read_atom(path: &Path, atom_key: &str) -> Result<Option<Value>, CodexError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CodexError::Io(e)),
    };
    let root: Value = serde_json::from_str(&text).map_err(|e| {
        CodexError::Other(format!(
            "{} is not valid JSON: {e} — cannot read atom safely",
            path.display()
        ))
    })?;
    Ok(root
        .get(ATOM_STATE_KEY)
        .and_then(|state| state.get(atom_key))
        .cloned())
}

/// 把某 atom key 设为给定值。**保留**文件里所有其它字段。
///
/// 写入语义:
/// - 文件不存在 → 创建,只写 `{ "electron-persisted-atom-state": { atom_key: value } }`
/// - 文件存在但非合法 JSON / 顶层非 object → 返 [`CodexError::Other`],**不破坏**原文件
/// - 文件存在但没 `electron-persisted-atom-state` 段 → 新建该段
/// - 段存在 → 只修改 `atom_key`,其它 atom 原样保留
///
/// 原子写:走 `<path>.tmp` → `rename` 避免中途崩溃留半文件。
pub fn write_atom(path: &Path, atom_key: &str, value: Value) -> Result<(), CodexError> {
    let mut root = read_root_or_empty(path)?;

    // 取或建 electron-persisted-atom-state 子 object
    let state_entry = root
        .entry(ATOM_STATE_KEY.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(state_obj) = state_entry.as_object_mut() else {
        return Err(CodexError::Other(format!(
            "{ATOM_STATE_KEY} exists but is not an object in {}",
            path.display()
        )));
    };

    state_obj.insert(atom_key.to_string(), value);

    write_atomic(path, &Value::Object(root))
}

/// 把指定 atom key 从 `electron-persisted-atom-state` 段中删除。
///
/// 用于 restore 路径 —— 把 user 原本没有此字段的状态退回(snapshot 拍到 None 时)。
/// 边界条件:
/// - 文件 ENOENT → no-op(原本就没文件,无需 strip)
/// - 其它 IO 错误 → `Err`(caller 决定是否 best-effort 吞)
/// - JSON 解析失败 → no-op + `tracing::warn!`(不能安全 strip 损坏文件,但要留 audit
///   trail 让 user 看到"残留 atom 未清"信号 — 见 silent-failure-hunter MEDIUM)
/// - 段 / atom key 缺失 → no-op(idempotent)
pub fn remove_atom(path: &Path, atom_key: &str) -> Result<(), CodexError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CodexError::Io(e)),
    };
    let mut root_value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "codex_integration::electron_state",
                path = %path.display(),
                error = %e,
                atom = atom_key,
                "remove_atom skipped: file is not valid JSON; cannot safely strip key. \
                 User may have stale atom — manually fix the file or recreate it.",
            );
            return Ok(());
        }
    };
    let Some(root) = root_value.as_object_mut() else {
        return Ok(());
    };
    let Some(state_obj) = root.get_mut(ATOM_STATE_KEY).and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if state_obj.remove(atom_key).is_none() {
        return Ok(());
    }
    write_atomic(path, &root_value)
}

/// 读顶层 object,文件不存在或非 object 时返空 map(后续 write 会创建)。
/// 非合法 JSON 返错误 —— 不要 silently 覆盖 user 已有的(可能损坏但可恢复的)文件。
fn read_root_or_empty(path: &Path) -> Result<Map<String, Value>, CodexError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(Map::new());
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        CodexError::Other(format!(
            "{} is not valid JSON: {e} — refusing to overwrite",
            path.display()
        ))
    })?;
    match value {
        Value::Object(m) => Ok(m),
        other => Err(CodexError::Other(format!(
            "{} top-level is not an object (found {})",
            path.display(),
            type_name(&other)
        ))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn write_atomic(path: &Path, value: &Value) -> Result<(), CodexError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string(value)
        .map_err(|e| CodexError::Other(format!("serialize global-state json: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn tmp_path() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".codex-global-state.json");
        (dir, path)
    }

    #[test]
    fn read_atom_returns_none_when_file_missing() {
        let (_t, p) = tmp_path();
        assert!(read_atom(&p, STATUS_SECTION_VISIBLE_KEY).unwrap().is_none());
    }

    /// silent-failure-hunter CRITICAL #1:corrupt JSON 必须返 Err,不能 silently
    /// 当 None — caller snapshot 路径会据此 mark capture_failed=true 防 restore 抹 user 原值。
    #[test]
    fn read_atom_propagates_error_on_corrupt_json() {
        let (_t, p) = tmp_path();
        std::fs::write(&p, "not json").unwrap();
        let err = read_atom(&p, STATUS_SECTION_VISIBLE_KEY).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not valid JSON") || msg.contains("Other"),
            "expected Other error for corrupt JSON, got: {msg}"
        );
    }

    #[test]
    fn read_atom_returns_value_when_present() {
        let (_t, p) = tmp_path();
        std::fs::write(
            &p,
            r#"{"electron-persisted-atom-state":{"local-conversation-status-section-visible":true,"other-atom":42}}"#,
        )
        .unwrap();
        assert_eq!(
            read_atom(&p, STATUS_SECTION_VISIBLE_KEY).unwrap(),
            Some(json!(true))
        );
        assert_eq!(read_atom(&p, "other-atom").unwrap(), Some(json!(42)));
        assert_eq!(read_atom(&p, "absent-atom").unwrap(), None);
    }

    #[test]
    fn write_atom_creates_file_with_minimal_shape() {
        let (_t, p) = tmp_path();
        write_atom(&p, STATUS_SECTION_VISIBLE_KEY, json!(true)).unwrap();
        let content: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            content,
            json!({"electron-persisted-atom-state":{"local-conversation-status-section-visible":true}})
        );
    }

    #[test]
    fn write_atom_preserves_all_other_top_level_and_atom_state_fields() {
        // 模拟 user 真实 .codex-global-state.json:含 workspace roots / prompt history /
        // 多个 atom 共存 —— transfer 只能动我们 target 的 atom key,其它 100% 不能动。
        let (_t, p) = tmp_path();
        let original = json!({
            "electron-saved-workspace-roots": ["/Users/me/proj1", "/Users/me/proj2"],
            "active-workspace-roots": ["/Users/me/proj1"],
            "electron-persisted-atom-state": {
                "diff-filter": "last-turn",
                "composer-auto-context-enabled": false,
                "local-conversation-status-section-visible": false,
                "agent-mode-by-host-id": {"local": "full-access"},
            },
            "electron-main-window-bounds": {"x": 31, "y": 56, "width": 1419, "height": 820},
            "thread-titles": {"titles": {}, "order": []},
        });
        std::fs::write(&p, original.to_string()).unwrap();

        write_atom(&p, STATUS_SECTION_VISIBLE_KEY, json!(true)).unwrap();

        let modified: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        // 我们 target 的字段被改
        assert_eq!(
            modified.pointer("/electron-persisted-atom-state/local-conversation-status-section-visible"),
            Some(&json!(true))
        );
        // 同段其它 atom 原样
        assert_eq!(
            modified.pointer("/electron-persisted-atom-state/diff-filter"),
            Some(&json!("last-turn"))
        );
        assert_eq!(
            modified.pointer("/electron-persisted-atom-state/composer-auto-context-enabled"),
            Some(&json!(false))
        );
        assert_eq!(
            modified.pointer("/electron-persisted-atom-state/agent-mode-by-host-id"),
            Some(&json!({"local": "full-access"}))
        );
        // 其它顶层字段原样
        assert_eq!(
            modified.get("electron-saved-workspace-roots"),
            Some(&json!(["/Users/me/proj1", "/Users/me/proj2"]))
        );
        assert_eq!(
            modified.get("electron-main-window-bounds"),
            Some(&json!({"x": 31, "y": 56, "width": 1419, "height": 820}))
        );
    }

    #[test]
    fn write_atom_refuses_to_overwrite_corrupt_json() {
        let (_t, p) = tmp_path();
        std::fs::write(&p, "this is not json {").unwrap();
        let err = write_atom(&p, STATUS_SECTION_VISIBLE_KEY, json!(true)).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not valid JSON") || msg.contains("invalid"),
            "expected refusal to overwrite, got: {msg}"
        );
        // 文件内容仍是原 corrupt 字符串(没被我们盖)
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "this is not json {");
    }

    #[test]
    fn remove_atom_strips_target_only() {
        let (_t, p) = tmp_path();
        std::fs::write(
            &p,
            r#"{"electron-persisted-atom-state":{"local-conversation-status-section-visible":true,"other-atom":42}}"#,
        )
        .unwrap();
        remove_atom(&p, STATUS_SECTION_VISIBLE_KEY).unwrap();
        let content: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            content.pointer("/electron-persisted-atom-state/local-conversation-status-section-visible"),
            None
        );
        assert_eq!(
            content.pointer("/electron-persisted-atom-state/other-atom"),
            Some(&json!(42))
        );
    }

    #[test]
    fn remove_atom_noop_when_file_missing() {
        let (_t, p) = tmp_path();
        assert!(remove_atom(&p, STATUS_SECTION_VISIBLE_KEY).is_ok());
        assert!(!p.exists());
    }
}
