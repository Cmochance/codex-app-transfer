//! grok build(xAI grok CLI 编码后端)账号登录凭证持久化。
//!
//! 单账号单文件 `~/.codex-app-transfer/grok-build-oauth.json`(v1 不做多账号池;
//! 与 antigravity 单账号一致,多账号可后续按 [`crate::account_pool`] 扩展)。存储 IO
//! 复用池原语的原子读写,凭证类型/字段本文件私有。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::account_pool::{self, PoolStorageError};

/// access token 过期前多少秒就主动 refresh(防请求到上游时刚好过期的 network race)。
const REFRESH_BUFFER_SECS: i64 = 300;

/// 当前 UNIX 毫秒 —— 复用通用池的时钟(与 workbuddy / handler 同源)。
pub use crate::account_pool::unix_now_ms;

#[derive(Debug, Error)]
pub enum GrokBuildTokenError {
    #[error("无法定位 token 持久化目录:HOME 与 USERPROFILE 环境变量都未设置")]
    HomeNotSet,
    #[error("grok-build token 文件 IO 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("grok-build token JSON 序列化失败: {0}")]
    Serde(#[from] serde_json::Error),
}

impl From<PoolStorageError> for GrokBuildTokenError {
    fn from(e: PoolStorageError) -> Self {
        match e {
            PoolStorageError::HomeNotSet => GrokBuildTokenError::HomeNotSet,
            PoolStorageError::Io(io) => GrokBuildTokenError::Io(io),
            PoolStorageError::Serde(s) => GrokBuildTokenError::Serde(s),
        }
    }
}

/// 持久化的 grok build 登录凭证(单账号)。字段命名 snake_case(本地落盘)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct GrokBuildCredential {
    /// `Authorization: Bearer <…>` 用的 access token(auth.x.ai 签发的 OIDC JWT,ES256)。
    pub access_token: String,
    /// refresh token —— 临期用 `grant_type=refresh_token` 到 `accounts.x.ai/oauth2/token` 续期。
    pub refresh_token: String,
    /// `Bearer`。
    pub token_type: String,
    /// access token 过期时刻,UNIX **毫秒** epoch。
    pub expiry_date: i64,
    /// 拿到凭证的时刻(ms epoch)。
    pub obtained_at_ms: i64,
    /// 登录使用的 OAuth client_id(refresh 时回填同一个;login-config 可能轮换,故随凭证存)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 账号 email(best-effort,从 id_token / userinfo 取,仅 UI 展示)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 账号 user_id(= JWT sub;best-effort,仅 UI 展示)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

impl GrokBuildCredential {
    /// 是否应主动 refresh —— `expiry_date` 前 [`REFRESH_BUFFER_SECS`] 秒即触发。
    pub fn should_refresh(&self) -> bool {
        unix_now_ms() >= self.expiry_date.saturating_sub(REFRESH_BUFFER_SECS * 1000)
    }
}

/// 单账号凭证文件句柄(薄 wrapper,IO 走 [`crate::account_pool`] 原子读写原语)。
pub struct GrokBuildCredentialStore {
    path: PathBuf,
}

impl GrokBuildCredentialStore {
    /// 顶层单文件 `<root>/grok-build-oauth.json`(非池;`legacy_file_path` 即顶层根路径原语)。
    pub fn single() -> Result<Self, GrokBuildTokenError> {
        Ok(Self {
            path: account_pool::legacy_file_path("grok-build-oauth.json")?,
        })
    }

    /// 显式路径(单测用)。
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载;文件不存在返 `Ok(None)`。
    pub fn load(&self) -> Result<Option<GrokBuildCredential>, GrokBuildTokenError> {
        Ok(account_pool::read_json_opt(&self.path)?)
    }

    /// atomic 写。
    pub fn save(&self, cred: &GrokBuildCredential) -> Result<(), GrokBuildTokenError> {
        account_pool::write_json_atomic(&self.path, cred)?;
        Ok(())
    }

    /// 删除(logout);文件不存在算成功。
    pub fn delete(&self) -> Result<(), GrokBuildTokenError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(GrokBuildTokenError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cred(expiry_offset_secs: i64) -> GrokBuildCredential {
        GrokBuildCredential {
            access_token: "ey.access".into(),
            refresh_token: "rt.refresh".into(),
            token_type: "Bearer".into(),
            expiry_date: unix_now_ms() + expiry_offset_secs * 1000,
            obtained_at_ms: unix_now_ms(),
            client_id: Some("b1a00492-073a-47ea-816f-4c329264a828".into()),
            email: Some("user@example.com".into()),
            user_id: Some("u-123".into()),
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
        let store = GrokBuildCredentialStore::at_path(dir.path().join("grok-build-oauth.json"));
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
        let path = dir.path().join("grok-build-oauth.json");
        std::fs::write(&path, b"{bad").unwrap();
        let store = GrokBuildCredentialStore::at_path(&path);
        assert!(matches!(
            store.load().unwrap_err(),
            GrokBuildTokenError::Serde(_)
        ));
    }
}
