//! WorkBuddy 账号登录凭证持久化 —— `~/.codex-app-transfer/workbuddy-oauth.json`。
//!
//! 与 API-key 路(用户手动粘 token)区分:账号登录路把 access/refresh token 落盘,
//! 由 [`super::login::ensure_valid_workbuddy_token`] 在请求前 load + 过期自动 refresh。
//! 形态对齐 [`crate::token::OauthToken`](gemini),但字段是 WorkBuddy 自己的语义。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// access token 过期前多少秒就主动 refresh(防请求到上游时刚好过期的 network race)。
const REFRESH_BUFFER_SECS: i64 = 300;

#[derive(Debug, Error)]
pub enum WorkbuddyTokenError {
    #[error("无法定位 token 持久化目录:HOME 与 USERPROFILE 环境变量都未设置")]
    HomeNotSet,
    #[error("workbuddy token 文件 IO 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("workbuddy token JSON 序列化失败: {0}")]
    Serde(#[from] serde_json::Error),
}

/// 持久化的 WorkBuddy 登录凭证。字段命名走 snake_case(本地落盘),与上游 `/auth/token`
/// 响应的 camelCase(`accessToken`/`refreshToken`/`expiresIn`)在 login flow 解析时转换。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct WorkbuddyCredential {
    /// `Authorization: Bearer <…>` 用的 access token(Keycloak JWT)。
    pub access_token: String,
    /// refresh token —— refresh 时放 `X-Refresh-Token` 头。
    pub refresh_token: String,
    /// `Bearer`。
    pub token_type: String,
    /// access token 过期时刻,UNIX **毫秒** epoch(obtain/refresh 时按 expiresIn 算)。
    pub expiry_date: i64,
    /// 拿到凭证的时刻(ms epoch)—— UI 展示 / 排查用。
    pub obtained_at_ms: i64,
    /// 登录用户昵称(展示当前账号)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 登录用户 uid(= JWT sub;`X-User-Id` 指纹头用)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

impl WorkbuddyCredential {
    /// 是否应主动 refresh —— `expiry_date` 前 [`REFRESH_BUFFER_SECS`] 秒即触发。
    pub fn should_refresh(&self) -> bool {
        unix_now_ms() >= self.expiry_date.saturating_sub(REFRESH_BUFFER_SECS * 1000)
    }
}

/// 当前 UNIX 毫秒(系统时钟早于 1970 返 0)。
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `~/.codex-app-transfer/workbuddy-oauth.json` 持久化句柄(atomic write + Unix 0600)。
pub struct WorkbuddyCredentialStore {
    path: PathBuf,
}

impl WorkbuddyCredentialStore {
    /// 默认路径 `<home>/.codex-app-transfer/workbuddy-oauth.json`。home 解析走
    /// `registry::paths::resolve_home`(`CODEX_APP_TRANSFER_HOME`→`HOME`→`USERPROFILE`)。
    pub fn from_home_env() -> Result<Self, WorkbuddyTokenError> {
        let home = codex_app_transfer_registry::paths::resolve_home()
            .ok_or(WorkbuddyTokenError::HomeNotSet)?;
        Ok(Self {
            path: home
                .join(".codex-app-transfer")
                .join("workbuddy-oauth.json"),
        })
    }

    /// 显式路径(单测用)。
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载;文件不存在返 `Ok(None)`(未登录正常路径)。
    pub fn load(&self) -> Result<Option<WorkbuddyCredential>, WorkbuddyTokenError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkbuddyTokenError::Io(e)),
        }
    }

    /// atomic 写(temp + rename),Unix 创建即 0600。
    pub fn save(&self, cred: &WorkbuddyCredential) -> Result<(), WorkbuddyTokenError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(cred)?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::remove_file(&tmp) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(WorkbuddyTokenError::Io(e)),
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, &json)?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 删除(logout);文件不存在算成功。
    pub fn delete(&self) -> Result<(), WorkbuddyTokenError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(WorkbuddyTokenError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cred(expiry_offset_secs: i64) -> WorkbuddyCredential {
        WorkbuddyCredential {
            access_token: "ey.access".into(),
            refresh_token: "ey.refresh".into(),
            token_type: "Bearer".into(),
            expiry_date: unix_now_ms() + expiry_offset_secs * 1000,
            obtained_at_ms: unix_now_ms(),
            nickname: Some("陈墨城".into()),
            uid: Some("u-123".into()),
        }
    }

    #[test]
    fn should_refresh_within_buffer() {
        assert!(cred(60).should_refresh(), "60s 内进 buffer 应 refresh");
        assert!(!cred(3600).should_refresh(), "1h 后不该 refresh");
        assert!(cred(-100).should_refresh(), "已过期必 refresh");
    }

    #[test]
    fn save_load_delete_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = WorkbuddyCredentialStore::at_path(dir.path().join("wb.json"));
        assert_eq!(store.load().unwrap(), None);
        let c = cred(3600);
        store.save(&c).unwrap();
        assert_eq!(store.load().unwrap().as_ref(), Some(&c));
        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn corrupt_json_surfaces_serde_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wb.json");
        std::fs::write(&path, b"{bad").unwrap();
        let store = WorkbuddyCredentialStore::at_path(&path);
        assert!(matches!(
            store.load().unwrap_err(),
            WorkbuddyTokenError::Serde(_)
        ));
    }
}
