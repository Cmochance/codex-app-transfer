//! 路径解析:`~/.codex/{config.toml,auth.json}` + 本应用 Codex 快照目录.

use std::path::{Path, PathBuf};

use crate::CodexError;

#[derive(Debug, Clone)]
pub struct CodexPaths {
    pub codex_home: PathBuf,
    pub app_home: PathBuf,
    pub config_toml: PathBuf,
    pub auth_json: PathBuf,
    pub model_catalog_json: PathBuf,
    /// Electron 主进程的持久化 atom state JSON(`~/.codex/.codex-global-state.json`)。
    ///
    /// Codex Desktop GUI 用 jotai `atomWithStorage` + electron-store 把若干 user-UI 偏好
    /// 持久化到这里,字段路径 `electron-persisted-atom-state.<atom-key>`。比如
    /// `local-conversation-status-section-visible` 控制对话页底部 context 圆环 +
    /// tokens/s 的显示(`/status` slash command 切换的就是这个 key,见 #258)。
    ///
    /// transfer 通过该路径在 Codex Desktop **启动前** ensure 某些 UI 偏好默认开启
    /// (e.g. 圆环默认显示),避免新 user / 升级 reset 后看不到关键 UI。运行时改
    /// 会被 Codex 内存 atom 覆盖,所以必须先于 Codex 启动。
    pub electron_global_state: PathBuf,
    /// Codex MCP OAuth 凭据的 file-store 落点(`~/.codex/.credentials.json`)。
    ///
    /// 当 `mcp_oauth_credentials_store = "file"` 时,Codex 把每个 MCP server 的
    /// OAuth token 写进这个单一 JSON blob(`server_name|hash` → entry,0o600)。
    /// 默认 `Auto`/`Keyring` 模式下凭据在 OS 钥匙串,此文件不存在。MOC-62 的
    /// "可移植保险箱"开关开启时强制 file 模式 + 镜像此文件。
    pub mcp_credentials: PathBuf,
    /// transfer 为 MCP 凭据维护的镜像(`~/.codex-app-transfer/mcp-credentials.json`)。
    ///
    /// 在 `~/.codex` 之外,所以 `codex switch` 的 `rsync --delete` / 误删 / 换机
    /// 都碰不到它。启动 + apply 后与 [`Self::mcp_credentials`] 并集合并(缺失则
    /// 恢复、存在则捕获),使 MCP 授权可恢复、可迁移。MOC-62。
    pub mcp_credentials_mirror: PathBuf,
    /// Legacy single-snapshot path kept for upgrade compatibility.
    pub snapshot_dir: PathBuf,
    pub snapshot_config: PathBuf,
    pub snapshot_auth: PathBuf,
    pub snapshot_manifest: PathBuf,
    pub snapshots_dir: PathBuf,
    pub active_snapshots_dir: PathBuf,
    pub recovery_snapshots_dir: PathBuf,
    /// 软删除目录 — `drop_all_snapshots` 不再物理 remove_dir_all,而是
    /// move 到 `trash/<UTC-timestamp>/`,保留 N 天后由 `gc_trash_older_than`
    /// 清理。给用户"误点 cleanup_all 还能恢复"窗口,follow-up #29 守门。
    pub trash_snapshots_dir: PathBuf,
    /// 跨平台冗余备份目录 — `snapshot_codex_state` 写完 active/ 后,
    /// 在系统级用户数据目录额外 cp 一份,防 `~/.codex-app-transfer/`
    /// 整目录被用户/卸载脚本/磁盘清理误删 → 真原始账号永久丢失。
    /// follow-up #30 守门。
    ///
    /// 路径(cfg(target_os) 决定):
    /// - macOS: `~/Library/Application Support/CodexAppTransfer/snapshot-backups/`
    /// - Windows: `%APPDATA%\CodexAppTransfer\snapshot-backups\`
    /// - Linux/BSD: `$XDG_DATA_HOME/CodexAppTransfer/snapshot-backups/`
    ///   (无 XDG_DATA_HOME 时 fallback `~/.local/share/.../`)
    pub external_backup_dir: PathBuf,
}

impl CodexPaths {
    /// 用真实用户 home 目录构造。Home 解析委派给
    /// [`codex_app_transfer_registry::paths::resolve_home`],它是 workspace
    /// 内唯一入口,统一 `HOME` → `USERPROFILE` 回退 + 空字符串视作未设(避免
    /// 此前 3 处独立实现 drift,PR #115 后续清理)。
    pub fn from_home_env() -> Result<Self, CodexError> {
        let home = codex_app_transfer_registry::paths::resolve_home().ok_or(CodexError::NoHome)?;
        Ok(Self::from_home_dir(home))
    }

    /// 显式给一个 home 目录(测试常用 tmp dir)。
    pub fn from_home_dir(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        let codex_home = home.join(".codex");
        let app_home = home.join(".codex-app-transfer");
        let snapshot_dir = app_home.join("codex-snapshot");
        let snapshots_dir = app_home.join("codex-snapshots");
        let active_snapshots_dir = snapshots_dir.join("active");
        let recovery_snapshots_dir = snapshots_dir.join("recovery");
        let trash_snapshots_dir = snapshots_dir.join("trash");
        let external_backup_dir = resolve_external_backup_dir(home);
        Self {
            config_toml: codex_home.join("config.toml"),
            auth_json: codex_home.join("auth.json"),
            model_catalog_json: app_home.join("config.json"),
            electron_global_state: codex_home.join(".codex-global-state.json"),
            mcp_credentials: codex_home.join(".credentials.json"),
            mcp_credentials_mirror: app_home.join("mcp-credentials.json"),
            snapshot_config: snapshot_dir.join("config.toml"),
            snapshot_auth: snapshot_dir.join("auth.json"),
            snapshot_manifest: snapshot_dir.join("manifest.json"),
            snapshot_dir,
            snapshots_dir,
            active_snapshots_dir,
            recovery_snapshots_dir,
            trash_snapshots_dir,
            external_backup_dir,
            codex_home,
            app_home,
        }
    }
}

/// 跨平台系统级用户数据目录下 `CodexAppTransfer/snapshot-backups/` 路径。
/// 不引入 `dirs` crate 保 codex_integration 边界干净,自己 cfg(target_os)
/// + env var fallback。
fn resolve_external_backup_dir(home: &Path) -> PathBuf {
    const APP_SUBDIR: &str = "CodexAppTransfer/snapshot-backups";
    #[cfg(target_os = "macos")]
    {
        return home.join("Library/Application Support").join(APP_SUBDIR);
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"));
        return appdata.join(APP_SUBDIR);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let xdg = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        return xdg.join(APP_SUBDIR);
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
            p.electron_global_state,
            PathBuf::from("/x/.codex/.codex-global-state.json")
        );
        assert_eq!(
            p.mcp_credentials,
            PathBuf::from("/x/.codex/.credentials.json")
        );
        assert_eq!(
            p.mcp_credentials_mirror,
            PathBuf::from("/x/.codex-app-transfer/mcp-credentials.json")
        );
        assert_eq!(
            p.snapshot_dir,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshot")
        );
        assert_eq!(
            p.snapshot_manifest,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshot/manifest.json")
        );
        assert_eq!(
            p.snapshots_dir,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshots")
        );
        assert_eq!(
            p.active_snapshots_dir,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshots/active")
        );
        assert_eq!(
            p.recovery_snapshots_dir,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshots/recovery")
        );
        assert_eq!(
            p.trash_snapshots_dir,
            PathBuf::from("/x/.codex-app-transfer/codex-snapshots/trash")
        );
        // external_backup_dir 跨平台变化 — 验当前 host 路径含 CodexAppTransfer/snapshot-backups
        let backup_str = p.external_backup_dir.to_string_lossy();
        assert!(
            backup_str.contains("CodexAppTransfer/snapshot-backups")
                || backup_str.contains("CodexAppTransfer\\snapshot-backups"),
            "external_backup_dir 必须含 CodexAppTransfer/snapshot-backups,实际: {}",
            p.external_backup_dir.display()
        );
    }
}
