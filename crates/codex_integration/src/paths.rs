//! 路径解析:`~/.codex/{config.toml,auth.json}` + `~/.codex-app-transfer/codex-snapshot/`.

use std::path::{Path, PathBuf};

use crate::CodexError;

#[derive(Debug, Clone)]
pub struct CodexPaths {
    pub codex_home: PathBuf,
    pub app_home: PathBuf,
    pub config_toml: PathBuf,
    pub auth_json: PathBuf,
    pub model_catalog_json: PathBuf,
    pub snapshot_dir: PathBuf,
    pub snapshot_config: PathBuf,
    pub snapshot_auth: PathBuf,
    pub snapshot_manifest: PathBuf,
}

impl CodexPaths {
    /// 用真实用户 home 目录构造(`$HOME` / `$USERPROFILE`)。
    pub fn from_home_env() -> Result<Self, CodexError> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map_err(|_| CodexError::NoHome)?;
        Ok(Self::from_home_dir(home))
    }

    /// 显式给一个 home 目录(测试常用 tmp dir)。
    pub fn from_home_dir(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        let codex_home = home.join(".codex");
        let app_home = home.join(".codex-app-transfer");
        let snapshot_dir = app_home.join("codex-snapshot");
        Self {
            config_toml: codex_home.join("config.toml"),
            auth_json: codex_home.join("auth.json"),
            model_catalog_json: app_home.join("config.json"),
            snapshot_config: snapshot_dir.join("config.toml"),
            snapshot_auth: snapshot_dir.join("auth.json"),
            snapshot_manifest: snapshot_dir.join("manifest.json"),
            snapshot_dir,
            codex_home,
            app_home,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_home_dir_layout() {
        let p = CodexPaths::from_home_dir("/x");
        assert_eq!(p.codex_home, PathBuf::from("/x/.codex"));
        assert_eq!(p.app_home, PathBuf::from("/x/.codex-app-transfer"));
        assert_eq!(p.config_toml, PathBuf::from("/x/.codex/config.toml"));
        assert_eq!(p.auth_json, PathBuf::from("/x/.codex/auth.json"));
        assert_eq!(
            p.model_catalog_json,
            PathBuf::from("/x/.codex-app-transfer/config.json")
        );
        assert_eq!(
            p.snapshot_dir,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshot")
        );
        assert_eq!(
            p.snapshot_manifest,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshot/manifest.json")
        );
    }
}
