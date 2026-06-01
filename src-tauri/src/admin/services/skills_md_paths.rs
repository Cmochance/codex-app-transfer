//! Skills 路径列表 — 扫 `~/.codex/skills/<name>/SKILL.md`,无 add/remove。
//! 不区分 doc/bundle(都是 SKILL.md 编辑,kind 无用);codex 没有静态 skill 索引文件。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use super::path_guard;

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
        let Ok(skill_md) = path_guard::validate_skill_path(&skill_md, &root) else {
            continue;
        };
        let dir = skill_md
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.to_path_buf());
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
            return path_guard::validate_skill_path(&PathBuf::from(entry.path), &skills_root()?);
        }
    }
    Err(format!("skill hash not found: {hash}"))
}

pub fn resolve_dir_by_hash(hash: &str) -> Result<PathBuf, String> {
    for entry in list_all_entries()? {
        if entry.hash == hash {
            let skill_path =
                path_guard::validate_skill_path(&PathBuf::from(entry.path), &skills_root()?)?;
            return skill_path
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| format!("skill path has no parent: {}", skill_path.display()));
        }
    }
    Err(format!("skill hash not found: {hash}"))
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
        let dir = root.join(format!("cas-skills-md-paths-{label}-{rand_hex}"));
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
    fn resolve_skill_hash_stays_under_skills_root() {
        let home = tmp_home("resolve");
        let skill = home
            .join(".codex")
            .join("skills")
            .join("alpha")
            .join("SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "skill").unwrap();

        with_test_home(&home, || {
            let hash = path_hash(&skill);
            assert_eq!(resolve_path_by_hash(&hash).unwrap(), skill);
            assert_eq!(
                resolve_dir_by_hash(&hash).unwrap(),
                skill.parent().unwrap().to_path_buf()
            );
        });
    }

    #[test]
    fn list_skips_sensitive_skill_paths() {
        let home = tmp_home("sensitive");
        let skill = home
            .join(".codex")
            .join("skills")
            .join("AppData")
            .join("SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "skill").unwrap();

        with_test_home(&home, || {
            let entries = list_all_entries().unwrap();
            assert!(entries.is_empty());
        });
    }
}
