//! 用户自定义 Memories MD 路径列表持久化 + 项目根 / 子目录分类.
//!
//! 跟 `agents_md_paths.rs` 完全对称,差异仅:
//! - 持久化文件名:`codex-memories-paths.json`(独立 store)
//! - history 文件前缀:`memories-<hash>.json`
//! - **不放全局首条**(用户明示"memories 只在项目中管理")— `list_all_entries`
//!   返回的全是用户主动添加的路径,没默认全局位置

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const PATHS_STORE_FILE: &str = "codex-memories-paths.json";

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct MemoriesMdPathsStore {
    #[serde(default)]
    pub memories: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PathCategory {
    ProjectRoot,
    Subdir,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MemoriesPathEntry {
    pub path: String,
    pub category: PathCategory,
    pub hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir_path: Option<String>,
}

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

fn store_file_path() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex-app-transfer").join(PATHS_STORE_FILE))
}

pub fn history_file_for(hash: &str) -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home
        .join(".codex-app-transfer")
        .join("managed-history")
        .join(format!("memories-{hash}.json")))
}

pub fn path_hash(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let result = hasher.finalize();
    let mut s = String::with_capacity(16);
    for b in &result[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 把 path 分类成 project-root / subdir(无 global 概念 — memories 只项目内管理)
pub fn classify_path_full(path: &Path) -> (PathCategory, Option<String>, Option<String>) {
    let Some(parent) = path.parent() else {
        return (PathCategory::ProjectRoot, None, None);
    };
    if parent.join(".git").exists() {
        let project_name = parent
            .file_name()
            .map(|s| s.to_string_lossy().into_owned());
        return (PathCategory::ProjectRoot, project_name, None);
    }
    let mut cur = parent.parent();
    while let Some(p) = cur {
        if p.join(".git").exists() {
            let project_name = p.file_name().map(|s| s.to_string_lossy().into_owned());
            let subdir_path = parent
                .strip_prefix(p)
                .ok()
                .map(|rel| rel.to_string_lossy().into_owned());
            return (PathCategory::Subdir, project_name, subdir_path);
        }
        cur = p.parent();
    }
    let project_name = parent
        .file_name()
        .map(|s| s.to_string_lossy().into_owned());
    (PathCategory::ProjectRoot, project_name, None)
}

pub fn load_store() -> Result<MemoriesMdPathsStore, String> {
    let file = store_file_path()?;
    if !file.exists() {
        return Ok(MemoriesMdPathsStore::default());
    }
    let raw = fs::read_to_string(&file).map_err(|e| format!("read paths store: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse paths store: {e}"))
}

pub fn save_store(store: &MemoriesMdPathsStore) -> Result<(), String> {
    let file = store_file_path()?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir store parent: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(store).map_err(|e| format!("serialize store: {e}"))?;
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &file).map_err(|e| format!("rename tmp: {e}"))?;
    Ok(())
}

/// 返回 dropdown 固定列表:`~/.codex/memories/MEMORY.md`(主索引)+ `memory_summary.md`(摘要)。
/// 这是 codex memories 唯二会被 AI 模型直接读取的 user-editable 文件,无 add/remove。
/// `project_name` 字段填文件 basename 作为前端 chip 显示标签。
pub fn list_all_entries() -> Result<Vec<MemoriesPathEntry>, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    let memories_dir = home.join(".codex").join("memories");
    let mut entries: Vec<MemoriesPathEntry> = Vec::new();
    for (filename, label) in [
        ("MEMORY.md", "MEMORY.md"),
        ("memory_summary.md", "memory_summary.md"),
    ] {
        let path = memories_dir.join(filename);
        entries.push(MemoriesPathEntry {
            category: PathCategory::ProjectRoot,
            hash: path_hash(&path),
            path: path.to_string_lossy().into_owned(),
            project_name: Some(label.to_string()),
            subdir_path: None,
        });
    }
    Ok(entries)
}

/// 接受**目录或文件**绝对路径。如果是目录,按优先级自动检索 MEMORY.md 索引位置:
/// 1. `<dir>/.codex/memories/MEMORY.md`(完整 Codex memories 约定)
/// 2. `<dir>/MEMORY.md`(项目根直接放)
/// 3. `<dir>/.codex/MEMORY.md`(简化版)
///
/// 都不存在 → 默认 `<dir>/MEMORY.md`,首次 Apply 时自动创建。
/// 如果传入的是文件,直接用。
pub fn add_path(raw_path: &str) -> Result<MemoriesPathEntry, String> {
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err(format!("path must be absolute: {raw_path}"));
    }
    if !path.exists() {
        return Err(format!("path not found: {raw_path}"));
    }
    // 目录 → 自动检索 / fallback
    let target = if path.is_dir() {
        let candidates = [
            path.join(".codex").join("memories").join("MEMORY.md"),
            path.join("MEMORY.md"),
            path.join(".codex").join("MEMORY.md"),
        ];
        candidates
            .iter()
            .find(|p| p.exists())
            .cloned()
            .unwrap_or_else(|| candidates[1].clone())
    } else {
        path
    };
    let mut store = load_store()?;
    let normalized = target.to_string_lossy().into_owned();
    if store.memories.iter().any(|p| p == &normalized) {
        return Err(format!("path already added: {normalized}"));
    }
    store.memories.push(normalized.clone());
    save_store(&store)?;
    let (category, project_name, subdir_path) = classify_path_full(&target);
    Ok(MemoriesPathEntry {
        category,
        hash: path_hash(&target),
        path: normalized,
        project_name,
        subdir_path,
    })
}

pub fn remove_by_hash(hash: &str) -> Result<bool, String> {
    let mut store = load_store()?;
    let before = store.memories.len();
    store
        .memories
        .retain(|p| path_hash(&PathBuf::from(p)) != hash);
    let removed = store.memories.len() != before;
    if removed {
        save_store(&store)?;
    }
    Ok(removed)
}

pub fn resolve_path_by_hash(hash: &str) -> Result<PathBuf, String> {
    // 固定 2 路径
    for entry in list_all_entries()? {
        let path = PathBuf::from(&entry.path);
        if entry.hash == hash {
            return Ok(path);
        }
    }
    Err(format!("path hash not found: {hash}"))
}
