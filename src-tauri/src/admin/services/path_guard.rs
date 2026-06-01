//! Shared path policy for admin-managed Codex markdown files.
//!
//! The admin API can read/write these files by path hash, so both path add and
//! hash resolution must re-check the same invariant.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedDocKind {
    Agents,
    Memories,
    Skills,
}

fn resolve_home() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(home) = test_home_override() {
        return Ok(home);
    }
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME / USERPROFILE not set".to_owned())
}

#[cfg(test)]
fn test_home_override() -> Option<PathBuf> {
    TEST_HOME.with(|home| home.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    TEST_HOME.with(|slot| *slot.borrow_mut() = Some(home.to_path_buf()));
    let out = f();
    TEST_HOME.with(|slot| *slot.borrow_mut() = None);
    out
}

fn canonicalize_for_policy(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|e| format!("canonicalize path: {e}"));
    }
    let mut missing = Vec::new();
    let mut cur = path;
    while !cur.exists() {
        let name = cur
            .file_name()
            .ok_or_else(|| format!("path has no existing ancestor: {}", path.display()))?;
        missing.push(name.to_os_string());
        let parent = cur
            .parent()
            .ok_or_else(|| format!("path has no existing ancestor: {}", path.display()))?;
        if parent == cur {
            return Err(format!("path has no existing ancestor: {}", path.display()));
        }
        cur = parent;
    }
    let mut canonical = fs::canonicalize(cur).map_err(|e| format!("canonicalize ancestor: {e}"))?;
    for part in missing.iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}

fn canonical_home(home: &Path) -> PathBuf {
    fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf())
}

fn path_starts_with(path: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        let path_components: Vec<String> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();
        let base_components: Vec<String> = base
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();
        path_components.len() >= base_components.len()
            && path_components[..base_components.len()] == base_components[..]
    }
    #[cfg(not(windows))]
    {
        path.starts_with(base)
    }
}

fn file_name_eq(path: &Path, expected: &str) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

fn has_expected_filename(path: &Path, kind: ManagedDocKind) -> bool {
    match kind {
        ManagedDocKind::Agents => file_name_eq(path, "AGENTS.md"),
        ManagedDocKind::Memories => {
            file_name_eq(path, "MEMORY.md") || file_name_eq(path, "memory_summary.md")
        }
        ManagedDocKind::Skills => file_name_eq(path, "SKILL.md"),
    }
}

fn has_sensitive_component(path: &Path) -> Option<String> {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "windows"
                | "system32"
                | "program files"
                | "program files (x86)"
                | "programdata"
                | "appdata"
                | ".ssh"
                | ".gnupg"
                | ".aws"
                | ".azure"
                | ".kube"
        ) {
            return Some(name);
        }
    }
    None
}

fn has_sensitive_filename(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.contains("credential")
        || name.contains("secret")
        || name.contains("token")
        || name.contains("password")
        || name.contains("passwd")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
}

fn validate_doc_path_with_home(
    path: &Path,
    home: &Path,
    kind: ManagedDocKind,
) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("path must be absolute: {}", path.display()));
    }
    if !has_expected_filename(path, kind) {
        return Err(format!(
            "unexpected managed file name for {kind:?}: {}",
            path.display()
        ));
    }

    let canonical = canonicalize_for_policy(path)?;
    // Re-check the filename on the *canonical* path, not just the raw one: a
    // symlink named AGENTS.md can point at e.g. ~/.codex/auth.json, which would
    // pass the raw-name gate above but resolve to a different file. Re-checking
    // here closes that symlink/TOCTOU redirect and keeps both gates consistent.
    if !has_expected_filename(&canonical, kind) {
        return Err(format!(
            "resolved path has an unexpected file name for {kind:?}: {}",
            canonical.display()
        ));
    }
    let home = canonical_home(home);
    if !path_starts_with(&canonical, &home) {
        return Err(format!(
            "path must stay under the current user's home directory: {}",
            canonical.display()
        ));
    }
    if let Some(component) = has_sensitive_component(&canonical) {
        return Err(format!(
            "path contains a sensitive directory component ({component}): {}",
            canonical.display()
        ));
    }
    if has_sensitive_filename(&canonical) {
        return Err(format!(
            "path looks like a credential or key file: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

pub fn validate_agents_path(path: &Path) -> Result<PathBuf, String> {
    let home = resolve_home()?;
    validate_doc_path_with_home(path, &home, ManagedDocKind::Agents)
}

pub fn validate_memories_path(path: &Path) -> Result<PathBuf, String> {
    let home = resolve_home()?;
    validate_doc_path_with_home(path, &home, ManagedDocKind::Memories)
}

pub fn validate_skill_path(path: &Path, skills_root: &Path) -> Result<PathBuf, String> {
    let canonical = validate_doc_path_with_home(path, &resolve_home()?, ManagedDocKind::Skills)?;
    let root = canonicalize_for_policy(skills_root)?;
    if !path_starts_with(&canonical, &root) {
        return Err(format!(
            "skill path must stay under {}: {}",
            root.display(),
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
pub(crate) fn validate_agents_path_with_home_for_test(
    path: &Path,
    home: &Path,
) -> Result<PathBuf, String> {
    validate_doc_path_with_home(path, home, ManagedDocKind::Agents)
}

#[cfg(test)]
pub(crate) fn validate_memories_path_with_home_for_test(
    path: &Path,
    home: &Path,
) -> Result<PathBuf, String> {
    validate_doc_path_with_home(path, home, ManagedDocKind::Memories)
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
        let dir = root.join(format!("cas-path-guard-{label}-{rand_hex}"));
        fs::create_dir_all(&dir).unwrap();
        // macOS /tmp is a symlink to /private/tmp — canonicalize so expected
        // paths match the guard's canonicalize() output (no-op on Linux CI).
        fs::canonicalize(&dir).unwrap_or(dir)
    }

    #[test]
    fn allows_agents_md_under_home_project() {
        let home = tmp_home("agents-ok");
        let path = home.join("project").join("AGENTS.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "rules").unwrap();

        let validated = validate_agents_path_with_home_for_test(&path, &home).unwrap();
        assert!(validated.ends_with("AGENTS.md"));
    }

    #[test]
    fn allows_agents_md_directly_under_home() {
        let home = tmp_home("agents-home");
        let path = home.join("AGENTS.md");
        fs::write(&path, "rules").unwrap();

        let validated = validate_agents_path_with_home_for_test(&path, &home).unwrap();
        assert_eq!(validated, path);
    }

    #[test]
    fn rejects_sensitive_home_directories() {
        let home = tmp_home("sensitive-dir");
        let path = home.join("AppData").join("project").join("AGENTS.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "rules").unwrap();

        let err = validate_agents_path_with_home_for_test(&path, &home).unwrap_err();
        assert!(err.contains("sensitive directory"));
    }

    #[test]
    fn rejects_non_agents_filename() {
        let home = tmp_home("wrong-name");
        let path = home.join("project").join("README.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "notes").unwrap();

        let err = validate_agents_path_with_home_for_test(&path, &home).unwrap_err();
        assert!(err.contains("unexpected managed file name"));
    }

    #[test]
    fn rejects_paths_outside_home() {
        let home = tmp_home("outside-home");
        let outside = tmp_home("outside-target").join("AGENTS.md");
        fs::write(&outside, "rules").unwrap();

        let err = validate_agents_path_with_home_for_test(&outside, &home).unwrap_err();
        assert!(err.contains("home directory"));
    }

    #[test]
    fn allows_memory_fallback_target_under_safe_directory() {
        let home = tmp_home("memory-fallback");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        let target = project.join("MEMORY.md");

        let validated = validate_memories_path_with_home_for_test(&target, &home).unwrap();
        assert!(validated.ends_with("MEMORY.md"));
    }

    // A symlink named AGENTS.md that resolves to a different filename (e.g.
    // ~/.codex/auth.json) passes the raw-name gate but must be rejected by the
    // canonical-name re-check — this is the symlink/TOCTOU redirect.
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_redirecting_to_other_filename() {
        let home = tmp_home("symlink-redirect");
        let target = home.join(".codex").join("auth.json");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "secret").unwrap();

        let link = home.join("project").join("AGENTS.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = validate_agents_path_with_home_for_test(&link, &home).unwrap_err();
        assert!(err.contains("unexpected file name"), "got: {err}");
    }
}
