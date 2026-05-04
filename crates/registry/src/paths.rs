//! 配置目录解析 —— 等价于 `backend/config.py` 中的常量.

use std::path::PathBuf;

const CONFIG_DIR_NAME: &str = ".codex-app-transfer";
const CONFIG_FILE_NAME: &str = "config.json";
const LIBRARY_DIR_NAME: &str = "configLibrary";
const BACKUPS_DIR_NAME: &str = "backups";

/// 解析当前用户的 home 目录;走 `$HOME` 然后 `$USERPROFILE`(Windows).
pub fn resolve_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    None
}

pub fn config_dir() -> Option<PathBuf> {
    resolve_home().map(|h| h.join(CONFIG_DIR_NAME))
}

pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

pub fn library_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join(LIBRARY_DIR_NAME))
}

pub fn backups_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join(BACKUPS_DIR_NAME))
}
