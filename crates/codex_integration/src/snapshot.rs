//! 首次 apply 前的快照机制.
//!
//! 1. apply 前调一次 [`snapshot_codex_state`]:把当前 `config.toml` 与
//!    `auth.json` 整文件复制到 `~/.codex-app-transfer/codex-snapshot/`,并写
//!    一份 `manifest.json` 记录"这两个文件原本存不存在"。
//! 2. 已经有快照时,**不重复**(同会话多次 apply 不会污染最初备份)。
//! 3. restore 时基于快照精确还原我们改过的几个 key,**不动**用户在我们运行
//!    期间手加的内容。

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::paths::CodexPaths;
use crate::toml_sync::write_atomic;
use crate::CodexError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotManifest {
    pub snapshot_at: String,
    pub config_existed: bool,
    pub auth_existed: bool,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotStatus {
    pub has_snapshot: bool,
    pub snapshot_at: Option<String>,
    pub config_existed: bool,
    pub auth_existed: bool,
    pub app_version: Option<String>,
}

/// 是否有未还原的快照。
pub fn has_snapshot(paths: &CodexPaths) -> bool {
    paths.snapshot_manifest.exists()
}

/// 供 UI 展示用的快照状态(不含敏感字段)。
pub fn get_snapshot_status(paths: &CodexPaths) -> SnapshotStatus {
    if !has_snapshot(paths) {
        return SnapshotStatus {
            has_snapshot: false,
            snapshot_at: None,
            config_existed: false,
            auth_existed: false,
            app_version: None,
        };
    }
    let manifest = read_manifest(paths).unwrap_or(SnapshotManifest {
        snapshot_at: String::new(),
        config_existed: false,
        auth_existed: false,
        app_version: String::new(),
    });
    SnapshotStatus {
        has_snapshot: true,
        snapshot_at: Some(manifest.snapshot_at),
        config_existed: manifest.config_existed,
        auth_existed: manifest.auth_existed,
        app_version: Some(manifest.app_version),
    }
}

/// 首次 apply 前调用。已存在快照则直接返回当前 manifest。
pub fn snapshot_codex_state(
    paths: &CodexPaths,
    app_version: &str,
) -> Result<SnapshotManifest, CodexError> {
    if has_snapshot(paths) {
        return read_manifest(paths);
    }
    std::fs::create_dir_all(&paths.snapshot_dir)?;

    let config_existed = paths.config_toml.exists();
    let auth_existed = paths.auth_json.exists();

    if config_existed {
        std::fs::copy(&paths.config_toml, &paths.snapshot_config)?;
    }
    if auth_existed {
        std::fs::copy(&paths.auth_json, &paths.snapshot_auth)?;
        // 快照里的 auth 也要 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &paths.snapshot_auth,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }

    let manifest = SnapshotManifest {
        snapshot_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        config_existed,
        auth_existed,
        app_version: app_version.to_owned(),
    };
    write_manifest(paths, &manifest)?;
    Ok(manifest)
}

/// 删除整个快照目录(restore 完成后的清理)。
pub fn drop_snapshot(paths: &CodexPaths) -> Result<(), CodexError> {
    if paths.snapshot_dir.exists() {
        std::fs::remove_dir_all(&paths.snapshot_dir)?;
    }
    Ok(())
}

pub(crate) fn read_manifest(paths: &CodexPaths) -> Result<SnapshotManifest, CodexError> {
    let s = std::fs::read_to_string(&paths.snapshot_manifest)?;
    Ok(serde_json::from_str(&s)?)
}

fn write_manifest(paths: &CodexPaths, manifest: &SnapshotManifest) -> Result<(), CodexError> {
    let mut s = serde_json::to_string_pretty(manifest)?;
    s.push('\n');
    write_atomic(&paths.snapshot_manifest, &s)?;
    Ok(())
}

/// 读取快照中的 config.toml 内容(不存在时返回空)。
pub(crate) fn read_snapshot_config(paths: &CodexPaths) -> Option<String> {
    std::fs::read_to_string(&paths.snapshot_config).ok()
}

/// 读取快照中的 auth.json(不存在时返回空对象)。
pub(crate) fn read_snapshot_auth(paths: &CodexPaths) -> serde_json::Value {
    let _ = paths;
    let txt = std::fs::read_to_string(&paths.snapshot_auth).ok();
    match txt {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
        _ => serde_json::Value::Object(Default::default()),
    }
}

/// 解析快照 config.toml 中某个 root key 的原始字面量(包含引号等)。
/// 返回 `None` 表示快照里**没有**这个 key,`Some(literal)` 表示快照里此 key
/// 的字面量(可能包含两侧引号、整数无引号等);该字面量可直接喂回
/// [`crate::toml_sync::sync_root_value`]。
pub(crate) fn snapshot_toml_value_literal(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let stripped = line.trim_start();
        if !stripped.starts_with(key) {
            continue;
        }
        let after = &stripped[key.len()..];
        // 必须是 `key=...` 或 `key <空白> ...=...` 形式
        let mut rest = after.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        rest = rest[1..].trim_start();
        // 去掉行末注释(`# ...`)
        if let Some(idx) = rest.find('#') {
            rest = rest[..idx].trim_end();
        }
        let trimmed = rest.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        return Some(trimmed.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_with_tmp() -> (tempfile::TempDir, CodexPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(tmp.path());
        (tmp, paths)
    }

    #[test]
    fn snapshot_when_neither_file_exists() {
        let (_t, paths) = paths_with_tmp();
        let m = snapshot_codex_state(&paths, "v2.0.0-stage2.5").unwrap();
        assert!(!m.config_existed);
        assert!(!m.auth_existed);
        assert!(has_snapshot(&paths));
        assert!(!paths.snapshot_config.exists());
        assert!(!paths.snapshot_auth.exists());
    }

    #[test]
    fn snapshot_copies_existing_files() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"existing\"\n").unwrap();
        std::fs::write(&paths.auth_json, "{\"OPENAI_API_KEY\":\"existing\"}\n").unwrap();
        let m = snapshot_codex_state(&paths, "v").unwrap();
        assert!(m.config_existed);
        assert!(m.auth_existed);
        assert_eq!(
            std::fs::read_to_string(&paths.snapshot_config).unwrap(),
            "openai_base_url = \"existing\"\n"
        );
    }

    #[test]
    fn snapshot_is_idempotent() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "old\n").unwrap();
        snapshot_codex_state(&paths, "v").unwrap();
        // 改了 config.toml,再 snapshot 一次 —— 不应覆盖原始备份
        std::fs::write(&paths.config_toml, "new\n").unwrap();
        snapshot_codex_state(&paths, "v").unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.snapshot_config).unwrap(),
            "old\n",
            "首次快照后再次调用必须保留原始备份"
        );
    }

    #[test]
    fn drop_snapshot_clears_dir() {
        let (_t, paths) = paths_with_tmp();
        snapshot_codex_state(&paths, "v").unwrap();
        assert!(has_snapshot(&paths));
        drop_snapshot(&paths).unwrap();
        assert!(!has_snapshot(&paths));
    }

    #[test]
    fn snapshot_toml_value_literal_extracts() {
        let s = "# c\nopenai_base_url = \"http://x\"\nfoo = 1\n";
        assert_eq!(
            snapshot_toml_value_literal(s, "openai_base_url"),
            Some("\"http://x\"".to_owned())
        );
        assert_eq!(snapshot_toml_value_literal(s, "foo"), Some("1".to_owned()));
        assert_eq!(snapshot_toml_value_literal(s, "missing"), None);
    }

    #[test]
    fn snapshot_toml_value_literal_strips_inline_comment() {
        let s = "openai_base_url = \"http://x\" # comment\n";
        assert_eq!(
            snapshot_toml_value_literal(s, "openai_base_url"),
            Some("\"http://x\"".to_owned())
        );
    }
}
