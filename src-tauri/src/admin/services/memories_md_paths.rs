//! 用户自定义 Memories MD 路径列表持久化 + 项目根 / 子目录分类.
//!
//! 跟 `agents_md_paths.rs` 完全对称,差异仅:
//! - 持久化文件名:`codex-memories-paths.json`(独立 store)
//! - history 文件前缀:`memories-<hash>.json`
//! - `list_all_entries` 返回 Codex 固定 memory 索引 + 用户主动添加的项目 MEMORY.md

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use super::path_guard;

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
    #[cfg(test)]
    if let Some(home) = test_home_override() {
        return Some(home);
    }
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

#[cfg(test)]
fn test_home_override() -> Option<PathBuf> {
    TEST_HOME.with(|home| home.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
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
        let project_name = parent.file_name().map(|s| s.to_string_lossy().into_owned());
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
    let project_name = parent.file_name().map(|s| s.to_string_lossy().into_owned());
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

/// 返回 dropdown 列表:`~/.codex/memories/MEMORY.md`(主索引)+ `memory_summary.md`(摘要)
/// 加用户主动添加的项目 MEMORY.md。
/// 固定两项是 codex memories 唯二会被 AI 模型直接读取的 user-editable 文件。
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
        let path = path_guard::validate_memories_path(&path)?;
        entries.push(MemoriesPathEntry {
            category: PathCategory::ProjectRoot,
            hash: path_hash(&path),
            path: path.to_string_lossy().into_owned(),
            project_name: Some(label.to_string()),
            subdir_path: None,
        });
    }
    let store = load_store()?;
    for p in &store.memories {
        let raw_path = PathBuf::from(p);
        let Ok(path) = path_guard::validate_memories_path(&raw_path) else {
            continue;
        };
        let (category, project_name, subdir_path) = classify_path_full(&path);
        entries.push(MemoriesPathEntry {
            category,
            hash: path_hash(&path),
            path: path.to_string_lossy().into_owned(),
            project_name,
            subdir_path,
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
    let path = if path.is_dir() {
        path_guard::validate_memories_path(&path.join("MEMORY.md"))?
            .parent()
            .ok_or_else(|| format!("path has no parent: {}", path.display()))?
            .to_path_buf()
    } else {
        path_guard::validate_memories_path(&path)?
    };
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
    let target = path_guard::validate_memories_path(&target)?;
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
    store.memories.retain(|p| {
        let raw_path = PathBuf::from(p);
        if path_hash(&raw_path) == hash {
            return false;
        }
        match path_guard::validate_memories_path(&raw_path) {
            Ok(path) => path_hash(&path) != hash,
            Err(_) => true,
        }
    });
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
            return path_guard::validate_memories_path(&path);
        }
    }
    let store = load_store()?;
    for p in &store.memories {
        let raw_path = PathBuf::from(p);
        if path_hash(&raw_path) == hash {
            return path_guard::validate_memories_path(&raw_path);
        }
        if let Ok(path) = path_guard::validate_memories_path(&raw_path) {
            if path_hash(&path) == hash {
                return Ok(path);
            }
        }
    }
    Err(format!("path hash not found: {hash}"))
}

#[cfg(test)]
fn resolve_path_by_raw_hash_for_test(path: &Path) -> Result<PathBuf, String> {
    let hash = path_hash(path);
    resolve_path_by_hash(&hash)
}

#[cfg(test)]
fn resolve_path_by_canonical_hash_for_test(path: &Path) -> Result<PathBuf, String> {
    let path = path_guard::validate_memories_path(path)?;
    let hash = path_hash(&path);
    resolve_path_by_hash(&hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(label: &str) -> PathBuf {
        let mut rand_buf = [0u8; 6];
        let _ = getrandom::getrandom(&mut rand_buf);
        let rand_hex: String = rand_buf.iter().map(|b| format!("{b:02x}")).collect();
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = root.join(format!("cas-memories-paths-{label}-{rand_hex}"));
        fs::create_dir_all(&dir).unwrap();
        // macOS /tmp is a symlink to /private/tmp — canonicalize so expected
        // paths match the guard's canonicalize() output (no-op on Linux CI).
        fs::canonicalize(&dir).unwrap_or(dir)
    }

    fn with_test_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        TEST_HOME.with(|slot| *slot.borrow_mut() = Some(home.to_path_buf()));
        let out = path_guard::with_test_home(home, f);
        TEST_HOME.with(|slot| *slot.borrow_mut() = None);
        out
    }

    #[test]
    fn add_directory_falls_back_to_safe_memory_md() {
        let home = tmp_home("fallback");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();

        with_test_home(&home, || {
            let entry = add_path(project.to_str().unwrap()).unwrap();
            assert_eq!(
                entry.path,
                project.join("MEMORY.md").to_string_lossy().into_owned()
            );
            let entries = list_all_entries().unwrap();
            assert!(entries.iter().any(|e| e.hash == entry.hash));
            assert_eq!(
                resolve_path_by_hash(&entry.hash).unwrap(),
                project.join("MEMORY.md")
            );
            assert_eq!(
                resolve_path_by_raw_hash_for_test(&project.join("MEMORY.md")).unwrap(),
                project.join("MEMORY.md")
            );
            assert_eq!(
                resolve_path_by_canonical_hash_for_test(&project.join("MEMORY.md")).unwrap(),
                project.join("MEMORY.md")
            );
        });
    }

    #[test]
    fn resolve_rejects_unsafe_legacy_memory_path() {
        let home = tmp_home("legacy");
        let path = home.join(".ssh").join("MEMORY.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "memory").unwrap();

        with_test_home(&home, || {
            save_store(&MemoriesMdPathsStore {
                memories: vec![path.to_string_lossy().into_owned()],
            })
            .unwrap();
            let hash = path_hash(&path);
            let err = resolve_path_by_hash(&hash).unwrap_err();
            assert!(err.contains("sensitive directory"));
        });
    }
}
