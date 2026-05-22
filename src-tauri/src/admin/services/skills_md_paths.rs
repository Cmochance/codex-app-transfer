//! Skills 路径列表 — 扫 `~/.codex/skills/<name>/SKILL.md`,无 add/remove。
//! 不区分 doc/bundle(都是 SKILL.md 编辑,kind 无用);codex 没有静态 skill 索引文件。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    /// SKILL.md 绝对路径
    pub path: String,
    /// SHA-256 前 16 字符(独立 history)
    pub hash: String,
    /// Skill 名(dir basename)
    pub name: String,
    /// Skill 所在目录(打开文件夹用)
    pub dir: String,
}

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

pub fn skills_root() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex").join("skills"))
}

pub fn history_file_for(hash: &str) -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home
        .join(".codex-app-transfer")
        .join("managed-history")
        .join(format!("skill-{hash}.json")))
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

/// 扫 `~/.codex/skills/<name>/` 全部 skill。dir 内有 SKILL.md 才入选。
pub fn list_all_entries() -> Result<Vec<SkillEntry>, String> {
    let root = skills_root()?;
    let mut entries: Vec<SkillEntry> = Vec::new();
    let Ok(read) = fs::read_dir(&root) else {
        return Ok(entries);
    };
    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let skill_md = dir.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        entries.push(SkillEntry {
            path: skill_md.to_string_lossy().into_owned(),
            hash: path_hash(&skill_md),
            name,
            dir: dir.to_string_lossy().into_owned(),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

pub fn resolve_path_by_hash(hash: &str) -> Result<PathBuf, String> {
    for entry in list_all_entries()? {
        if entry.hash == hash {
            return Ok(PathBuf::from(entry.path));
        }
    }
    Err(format!("skill hash not found: {hash}"))
}

pub fn resolve_dir_by_hash(hash: &str) -> Result<PathBuf, String> {
    for entry in list_all_entries()? {
        if entry.hash == hash {
            return Ok(PathBuf::from(entry.dir));
        }
    }
    Err(format!("skill hash not found: {hash}"))
}
