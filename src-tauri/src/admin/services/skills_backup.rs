//! Codex CLI Skills 目录 (`~/.codex/skills/`) 的 file-level snapshot backup / restore.
//!
//! 跟 ManagedBlock trait 不同 — Skills 不是单文件受管块模式, 是**整目录树**备份恢复
//! (借鉴 borawong/AiMaMi:src-tauri/src/core/skills.rs 的 file-snapshot 设计)。
//!
//! ## 设计选择
//! - **shell out `tar`** 而非 Rust tar crate: macOS / Linux native, Windows 10+ 自带
//!   bsdtar — 3 平台都 work, 不引入新 crate (减 dep / binary size)
//! - 备份目标: `~/.codex-app-transfer/skills-backups/<UTC-timestamp>.tar.gz`
//! - restore: 解压覆盖 `~/.codex/skills/` (覆盖前没 backup, 用户自己责任 — 跟
//!   AGENTS.md 受管块不同, skills 是用户手维护, app 只提供 backup/restore 工具)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum SkillsBackupError {
    Io(String),
    TarFailed(String),
    InvalidBackup(String),
}

impl std::fmt::Display for SkillsBackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillsBackupError::Io(e) => write!(f, "skills IO failed: {e}"),
            SkillsBackupError::TarFailed(e) => write!(f, "tar command failed: {e}"),
            SkillsBackupError::InvalidBackup(e) => write!(f, "invalid backup: {e}"),
        }
    }
}

impl std::error::Error for SkillsBackupError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntry {
    pub name: String,
    pub has_skill_md: bool,
    pub files_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub filename: String,
    pub size_bytes: u64,
    pub created_unix: u64,
}

/// 扫 `~/.codex/skills/` 列每个 skill 子目录的简要信息.
pub fn list_skills(skills_dir: &Path) -> Result<Vec<SkillEntry>, SkillsBackupError> {
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        std::fs::read_dir(skills_dir).map_err(|e| SkillsBackupError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for ent in entries.flatten() {
        if !ent.path().is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        // skip hidden + dotted
        if name.starts_with('.') {
            continue;
        }
        let has_skill_md = ent.path().join("SKILL.md").exists();
        let files_count = std::fs::read_dir(ent.path())
            .map(|it| it.flatten().count())
            .unwrap_or(0);
        out.push(SkillEntry {
            name,
            has_skill_md,
            files_count,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 把整个 `skills_dir` 打包成 `<backup_dir>/<UTC-timestamp>.tar.gz`.
/// 返回完整 backup 文件 path。
pub fn backup_skills(skills_dir: &Path, backup_dir: &Path) -> Result<PathBuf, SkillsBackupError> {
    if !skills_dir.exists() {
        return Err(SkillsBackupError::Io(format!(
            "skills dir not found: {}",
            skills_dir.display()
        )));
    }
    std::fs::create_dir_all(backup_dir).map_err(|e| SkillsBackupError::Io(e.to_string()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format!("skills-{now}.tar.gz");
    let dest = backup_dir.join(&filename);
    let status = Command::new("tar")
        .args([
            "-czf",
            &dest.display().to_string(),
            "-C",
            &skills_dir.display().to_string(),
            ".",
        ])
        .status()
        .map_err(|e| SkillsBackupError::TarFailed(format!("spawn tar: {e}")))?;
    if !status.success() {
        return Err(SkillsBackupError::TarFailed(format!(
            "tar exit status: {status}"
        )));
    }
    Ok(dest)
}

/// 列 `~/.codex-app-transfer/skills-backups/*.tar.gz` 所有备份, 按时间倒序.
pub fn list_backups(backup_dir: &Path) -> Result<Vec<BackupEntry>, SkillsBackupError> {
    if !backup_dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        std::fs::read_dir(backup_dir).map_err(|e| SkillsBackupError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let filename = ent.file_name().to_string_lossy().to_string();
        if !filename.ends_with(".tar.gz") {
            continue;
        }
        let meta = match path.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size_bytes = meta.len();
        let created_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(BackupEntry {
            filename,
            size_bytes,
            created_unix,
        });
    }
    out.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
    Ok(out)
}

/// 从指定 backup 还原到 `skills_dir`. 跟 backup 对称: `tar -xzf <backup> -C <skills>`.
///
/// **不预先备份 skills_dir 当前内容** — 用户自己责任 (建议先 backup 再 restore)。
/// 验证 backup_filename 在 backup_dir 内 (防 path traversal),且以 `.tar.gz` 结尾。
pub fn restore_backup(
    skills_dir: &Path,
    backup_dir: &Path,
    backup_filename: &str,
) -> Result<(), SkillsBackupError> {
    if backup_filename.is_empty()
        || backup_filename.contains('/')
        || backup_filename.contains('\\')
        || !backup_filename.ends_with(".tar.gz")
    {
        return Err(SkillsBackupError::InvalidBackup(format!(
            "rejected name: {backup_filename}"
        )));
    }
    let archive = backup_dir.join(backup_filename);
    if !archive.exists() {
        return Err(SkillsBackupError::InvalidBackup(format!(
            "not found: {}",
            archive.display()
        )));
    }
    std::fs::create_dir_all(skills_dir).map_err(|e| SkillsBackupError::Io(e.to_string()))?;
    let status = Command::new("tar")
        .args([
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &skills_dir.display().to_string(),
        ])
        .arg("--no-same-owner")
        .arg("--no-overwrite-dir")
        .status()
        .map_err(|e| SkillsBackupError::TarFailed(format!("spawn tar: {e}")))?;
    if !status.success() {
        return Err(SkillsBackupError::TarFailed(format!(
            "tar exit status: {status}"
        )));
    }
    // post-extract path escape check: walk skills_dir 确保没有文件逃逸到外部
    // (防御恶意 tar 含 symlink 指向 .. / 绝对路径等,虽 tar 默认拒绝绝对路径,
    // 但某些 bsdtar 版本行为不一致;做 defense-in-depth 二次校验)。
    {
        fn check_dir_escape(dir: &Path, root: &Path) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for ent in entries.flatten() {
                    let p = ent.path();
                    if let Ok(canon) = p.canonicalize() {
                        if !canon.starts_with(root) {
                            tracing::error!(
                                "skills restore: escaped path detected and cleaned: {} → {}",
                                p.display(),
                                canon.display()
                            );
                            let _ = std::fs::remove_dir_all(dir);
                            return;
                        }
                    }
                    if p.is_dir() {
                        check_dir_escape(&p, root);
                    }
                }
            }
        }
        let skills_canonical =
            skills_dir.canonicalize().unwrap_or_else(|_| skills_dir.to_path_buf());
        check_dir_escape(skills_dir, &skills_canonical);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(label: &str) -> PathBuf {
        let mut rand_buf = [0u8; 6];
        let _ = getrandom::getrandom(&mut rand_buf);
        let rand_hex: String = rand_buf.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("cas-skills-test-{label}-{rand_hex}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_skills_returns_empty_when_dir_missing() {
        let nonexistent = tmp_dir("missing").join("notexist");
        assert!(list_skills(&nonexistent).unwrap().is_empty());
    }

    #[test]
    fn list_skills_finds_subdirs_with_skill_md_presence() {
        let dir = tmp_dir("list");
        fs::create_dir_all(dir.join("alpha")).unwrap();
        fs::write(dir.join("alpha/SKILL.md"), "alpha skill").unwrap();
        fs::create_dir_all(dir.join("beta")).unwrap();
        // 不放 SKILL.md, 只放别的 file
        fs::write(dir.join("beta/notes.txt"), "notes").unwrap();
        let result = list_skills(&dir).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "alpha");
        assert!(result[0].has_skill_md);
        assert_eq!(result[1].name, "beta");
        assert!(!result[1].has_skill_md);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_skills_skips_hidden_directories() {
        let dir = tmp_dir("hidden");
        fs::create_dir_all(dir.join(".cache")).unwrap();
        fs::create_dir_all(dir.join("visible")).unwrap();
        let result = list_skills(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "visible");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_and_restore_roundtrip_preserves_content() {
        let skills = tmp_dir("skills-src");
        fs::create_dir_all(skills.join("skill-a")).unwrap();
        fs::write(skills.join("skill-a/SKILL.md"), "version 1").unwrap();
        let backup_dir = tmp_dir("backups");

        let archive = backup_skills(&skills, &backup_dir).unwrap();
        assert!(archive.exists());

        // 修改 skill 内容
        fs::write(skills.join("skill-a/SKILL.md"), "version 2 modified").unwrap();

        // restore — 跟 backup 时一致
        let archive_name = archive.file_name().unwrap().to_string_lossy().to_string();
        restore_backup(&skills, &backup_dir, &archive_name).unwrap();
        let restored = fs::read_to_string(skills.join("skill-a/SKILL.md")).unwrap();
        assert_eq!(restored, "version 1", "restore must overwrite to version 1");

        let _ = fs::remove_dir_all(&skills);
        let _ = fs::remove_dir_all(&backup_dir);
    }

    #[test]
    fn restore_rejects_path_traversal() {
        let skills = tmp_dir("skills-traversal");
        let backups = tmp_dir("backups-traversal");
        let err = restore_backup(&skills, &backups, "../escape.tar.gz").unwrap_err();
        assert!(matches!(err, SkillsBackupError::InvalidBackup(_)));
        let err = restore_backup(&skills, &backups, "foo/bar.tar.gz").unwrap_err();
        assert!(matches!(err, SkillsBackupError::InvalidBackup(_)));
        let err = restore_backup(&skills, &backups, "evil.exe").unwrap_err();
        assert!(matches!(err, SkillsBackupError::InvalidBackup(_)));
        let _ = fs::remove_dir_all(&skills);
        let _ = fs::remove_dir_all(&backups);
    }

    #[test]
    fn list_backups_orders_newest_first() {
        let dir = tmp_dir("list-backups");
        // 模拟 2 个 .tar.gz file 不同 mtime。
        // **必须跨秒 sleep**: Linux ext4 / GitHub Actions runner 文件系统 mtime
        // 精度通常秒级 (有的 fs 才纳秒), 20ms 间隔会让两个文件 mtime 相同 → sort
        // 不稳定 → test fail (2026-05-20 实测在 GitHub runner Linux 复现)。
        fs::write(dir.join("skills-100.tar.gz"), b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(dir.join("skills-200.tar.gz"), b"new").unwrap();
        // 加一个非 .tar.gz 应被忽略
        fs::write(dir.join("readme.txt"), b"info").unwrap();
        let result = list_backups(&dir).unwrap();
        assert_eq!(result.len(), 2, "tar.gz only");
        // newest first
        assert_eq!(result[0].filename, "skills-200.tar.gz");
        assert_eq!(result[1].filename, "skills-100.tar.gz");
        let _ = fs::remove_dir_all(&dir);
    }
}
