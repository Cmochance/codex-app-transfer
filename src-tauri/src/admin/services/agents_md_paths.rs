//! 用户自定义 AGENTS.md 路径列表持久化 + 项目根 / 子目录分类.
//!
//! 持久化文件:`~/.codex-app-transfer/codex-doc-paths.json`
//! 形态:`{ "agents": ["~/myproj/AGENTS.md", "~/myproj/src/AGENTS.md", ...] }`
//!
//! 全局 `~/.codex/AGENTS.md` **不写入此 list**,API 调用方负责拼回 dropdown
//! 首条(避免持久化默认全局再被 delete 还能找回)。
//!
//! 路径分类(项目根 / 子目录 / 全局)走纯文件系统检测:
//! - `~/.codex/AGENTS.md` → "global"
//! - `path.parent()` 有 `.git/` dir → "project-root"
//! - 沿父目录上溯找到 `.git/` 但不是直接 parent → "subdir"
//! - 找不到 `.git/`(用户没 git) → "project-root"(默认归类)
//!
//! 独立 history:每条 path 的 history 写到 `~/.codex-app-transfer/managed-history/agents-<hash>.json`,
//! hash = SHA-256(path) 前 16 字符.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use super::path_guard;

const PATHS_STORE_FILE: &str = "codex-doc-paths.json";

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct AgentsMdPathsStore {
    /// 用户添加的自定义 AGENTS.md 绝对路径列表(全局 `~/.codex/AGENTS.md` 不在内)。
    #[serde(default)]
    pub agents: Vec<String>,
}

/// 路径分类标签 — 给前端 dropdown 显示用。
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PathCategory {
    Global,
    ProjectRoot,
    Subdir,
}

impl PathCategory {
    /// i18n key suffix — 前端用 `codex.agentsPath.{global|projectRoot|subdir}`
    pub fn label_key_suffix(&self) -> &'static str {
        match self {
            PathCategory::Global => "global",
            PathCategory::ProjectRoot => "projectRoot",
            PathCategory::Subdir => "subdir",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AgentsPathEntry {
    /// 绝对路径(已 expanduser)
    pub path: String,
    /// 路径分类
    pub category: PathCategory,
    /// SHA-256 前 16 字符,用于独立 history 文件名
    pub hash: String,
    /// 项目名(项目根 / 子目录两种 category 才有)— 项目根 dir 的 file_name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// 子目录路径(仅 Subdir category 才有)— 项目根 → AGENTS.md 父目录的相对路径
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

/// `~/.codex-app-transfer/codex-doc-paths.json`
fn store_file_path() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex-app-transfer").join(PATHS_STORE_FILE))
}

/// `~/.codex/AGENTS.md`(全局,固定首条)
pub fn global_agents_path() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex").join("AGENTS.md"))
}

pub fn validated_global_agents_path() -> Result<PathBuf, String> {
    path_guard::validate_agents_path(&global_agents_path()?)
}

/// `~/.codex-app-transfer/managed-history/agents-<hash>.json`
pub fn history_file_for(hash: &str) -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home
        .join(".codex-app-transfer")
        .join("managed-history")
        .join(format!("agents-{hash}.json")))
}

/// 计算 path 的 SHA-256 前 16 字符 — 独立 history 文件名 + dropdown stable id
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

/// 把 path 分类成 global / project-root / subdir + 找到 git root → 项目名 +(子目录时)子目录路径。
///
/// 纯文件系统检测,**不读 AGENTS.md 内容**:
/// - `~/.codex/AGENTS.md` → (Global, None, None)
/// - `<dir>/AGENTS.md`, `<dir>/.git/` exists → (ProjectRoot, Some(dir basename), None)
/// - `<root>/.../<sub>/AGENTS.md`, 上溯找到 `<root>/.git/` → (Subdir, Some(root basename), Some(<sub> 相对 root 路径))
/// - 上溯找不到 `.git/` → (ProjectRoot, Some(path.parent basename), None) — 用户没用 git 时 fallback
pub fn classify_path_full(path: &Path) -> (PathCategory, Option<String>, Option<String>) {
    if let Ok(global) = global_agents_path() {
        if path == global {
            return (PathCategory::Global, None, None);
        }
    }
    let Some(parent) = path.parent() else {
        return (PathCategory::ProjectRoot, None, None);
    };
    // case 1: 父目录就是项目根(直接含 .git)
    if parent.join(".git").exists() {
        let project_name = parent.file_name().map(|s| s.to_string_lossy().into_owned());
        return (PathCategory::ProjectRoot, project_name, None);
    }
    // case 2: 沿父目录上溯找 `.git/`,找到 → subdir + 项目名 + 子目录相对路径
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
    // case 3: 找不到 `.git/` — 用户没用 git,用 path.parent 的 file_name 当 fallback project name
    let project_name = parent.file_name().map(|s| s.to_string_lossy().into_owned());
    (PathCategory::ProjectRoot, project_name, None)
}

/// 仅返回 category(向后兼容 + 测试用)
pub fn classify_path(path: &Path) -> PathCategory {
    classify_path_full(path).0
}

/// 读 path store(不存在 → 空 store)
pub fn load_store() -> Result<AgentsMdPathsStore, String> {
    let file = store_file_path()?;
    if !file.exists() {
        return Ok(AgentsMdPathsStore::default());
    }
    let raw = fs::read_to_string(&file).map_err(|e| format!("read paths store: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse paths store: {e}"))
}

/// 写 path store(原子写)
pub fn save_store(store: &AgentsMdPathsStore) -> Result<(), String> {
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

/// 返回完整 dropdown 列表:**file-existence 自动检测**。
///
/// - 全局 `~/.codex/AGENTS.md` 仅在文件实际存在时才放入 list(跨平台:HOME 解析 →
///   `.codex/AGENTS.md` exists check)。**不预设**,**不假设**用户本机一定有。
/// - 用户添加的自定义路径同样要 file existence check —— 文件被外部删除后下次
///   list 自动剔除(不报错,silent skip),前端 UI 反映"该路径已不存在"。
///
/// 结果为空 → 前端应提示用户手动添加,而不是把不存在的路径默认填进 dropdown。
pub fn list_all_entries() -> Result<Vec<AgentsPathEntry>, String> {
    let mut entries: Vec<AgentsPathEntry> = Vec::new();
    if let Ok(global) = global_agents_path() {
        if global.exists() {
            let Ok(global) = path_guard::validate_agents_path(&global) else {
                return Ok(entries);
            };
            entries.push(AgentsPathEntry {
                path: global.to_string_lossy().into_owned(),
                category: PathCategory::Global,
                hash: path_hash(&global),
                project_name: None,
                subdir_path: None,
            });
        }
    }
    let store = load_store()?;
    for p in &store.agents {
        let path = PathBuf::from(p);
        if !path.exists() {
            continue;
        }
        let Ok(path) = path_guard::validate_agents_path(&path) else {
            continue;
        };
        let (category, project_name, subdir_path) = classify_path_full(&path);
        entries.push(AgentsPathEntry {
            category,
            hash: path_hash(&path),
            path: path.to_string_lossy().into_owned(),
            project_name,
            subdir_path,
        });
    }
    Ok(entries)
}

/// 添加自定义路径。**不允许重复添加** + **不允许添加全局路径**(全局已固定首条)。
pub fn add_path(raw_path: &str) -> Result<AgentsPathEntry, String> {
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err(format!("path must be absolute: {raw_path}"));
    }
    if !path.exists() {
        return Err(format!("file not found: {raw_path}"));
    }
    let path = path_guard::validate_agents_path(&path)?;
    let global = validated_global_agents_path()?;
    if path == global {
        return Err(format!(
            "global ~/.codex/AGENTS.md already shown by default; do not add explicitly"
        ));
    }
    let mut store = load_store()?;
    let normalized = path.to_string_lossy().into_owned();
    if store.agents.iter().any(|p| p == &normalized) {
        return Err(format!("path already added: {raw_path}"));
    }
    store.agents.push(normalized.clone());
    save_store(&store)?;
    let (category, project_name, subdir_path) = classify_path_full(&path);
    Ok(AgentsPathEntry {
        category,
        hash: path_hash(&path),
        path: normalized,
        project_name,
        subdir_path,
    })
}

/// 按 hash 删自定义路径(全局路径删不掉 — 它根本不在 store)
pub fn remove_by_hash(hash: &str) -> Result<bool, String> {
    let mut store = load_store()?;
    let before = store.agents.len();
    store.agents.retain(|p| {
        let raw_path = PathBuf::from(p);
        if path_hash(&raw_path) == hash {
            return false;
        }
        match path_guard::validate_agents_path(&raw_path) {
            Ok(path) => path_hash(&path) != hash,
            Err(_) => true,
        }
    });
    let removed = store.agents.len() != before;
    if removed {
        save_store(&store)?;
    }
    Ok(removed)
}

/// 根据 hash 找到 path(用于 6 endpoints 从 query 拿到具体文件)
pub fn resolve_path_by_hash(hash: &str) -> Result<PathBuf, String> {
    let global = validated_global_agents_path()?;
    if path_hash(&global) == hash {
        return Ok(global);
    }
    let store = load_store()?;
    for p in &store.agents {
        let raw_path = PathBuf::from(p);
        if path_hash(&raw_path) == hash {
            return path_guard::validate_agents_path(&raw_path);
        }
        if let Ok(path) = path_guard::validate_agents_path(&raw_path) {
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
    let path = path_guard::validate_agents_path(path)?;
    let hash = path_hash(&path);
    resolve_path_by_hash(&hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_home(label: &str) -> PathBuf {
        let mut rand_buf = [0u8; 6];
        let _ = getrandom::getrandom(&mut rand_buf);
        let rand_hex: String = rand_buf.iter().map(|b| format!("{b:02x}")).collect();
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = root.join(format!("cas-agents-paths-{label}-{rand_hex}"));
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
    fn classify_global_path() {
        let global = global_agents_path().expect("HOME set");
        assert_eq!(classify_path(&global), PathCategory::Global);
    }

    #[test]
    fn classify_non_git_project_root_default() {
        let p = PathBuf::from("/tmp/no-git-here/AGENTS.md");
        let _ = std::fs::create_dir_all("/tmp/no-git-here");
        assert_eq!(classify_path(&p), PathCategory::ProjectRoot);
    }

    #[test]
    fn path_hash_stable() {
        let p1 = PathBuf::from("/Users/foo/myproj/AGENTS.md");
        let p2 = PathBuf::from("/Users/foo/myproj/AGENTS.md");
        assert_eq!(path_hash(&p1), path_hash(&p2));
        assert_eq!(path_hash(&p1).len(), 16);
    }

    #[test]
    fn add_path_accepts_safe_agents_under_home() {
        let home = tmp_home("safe");
        let path = home.join("project").join("AGENTS.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "rules").unwrap();

        with_test_home(&home, || {
            let entry = add_path(path.to_str().unwrap()).unwrap();
            assert_eq!(entry.path, path.to_string_lossy().into_owned());
            assert_eq!(resolve_path_by_hash(&entry.hash).unwrap(), path);
            assert_eq!(resolve_path_by_raw_hash_for_test(&path).unwrap(), path);
            assert_eq!(
                resolve_path_by_canonical_hash_for_test(&path).unwrap(),
                path
            );
        });
    }

    #[test]
    fn add_path_rejects_sensitive_agents_path() {
        let home = tmp_home("sensitive");
        let path = home.join(".ssh").join("AGENTS.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "rules").unwrap();

        with_test_home(&home, || {
            let err = add_path(path.to_str().unwrap()).unwrap_err();
            assert!(err.contains("sensitive directory"));
        });
    }

    #[test]
    fn resolve_rejects_unsafe_legacy_store_path() {
        let home = tmp_home("legacy");
        let path = home.join(".ssh").join("AGENTS.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "rules").unwrap();

        with_test_home(&home, || {
            save_store(&AgentsMdPathsStore {
                agents: vec![path.to_string_lossy().into_owned()],
            })
            .unwrap();
            let hash = path_hash(&path);
            let err = resolve_path_by_hash(&hash).unwrap_err();
            assert!(err.contains("sensitive directory"));
        });
    }
}
