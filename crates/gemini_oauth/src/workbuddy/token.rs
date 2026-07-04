//! WorkBuddy 账号登录凭证持久化 —— 每账号一文件,存储/池态复用通用
//! [`crate::account_pool`]。本文件只保留 WorkBuddy 凭证类型 + 薄 store wrapper。
//!
//! 存储布局(见 [`crate::account_pool`]):账号
//! `~/.codex-app-transfer/workbuddy/<provider_id>/accounts/<uid>.json`;池态
//! `.../_pool.json`。旧单文件 `workbuddy-oauth.json` 由 [`super::pool`] 迁移逻辑搬入。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::account_pool::{self, PoolStorageError};

/// access token 过期前多少秒就主动 refresh(防请求到上游时刚好过期的 network race)。
const REFRESH_BUFFER_SECS: i64 = 300;

/// 当前 UNIX 毫秒 —— 复用通用池的时钟(codex_quota_injector / handler 经 `token::unix_now_ms` 用)。
pub use crate::account_pool::unix_now_ms;

#[derive(Debug, Error)]
pub enum WorkbuddyTokenError {
    #[error("无法定位 token 持久化目录:HOME 与 USERPROFILE 环境变量都未设置")]
    HomeNotSet,
    #[error("workbuddy token 文件 IO 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("workbuddy token JSON 序列化失败: {0}")]
    Serde(#[from] serde_json::Error),
}

impl From<PoolStorageError> for WorkbuddyTokenError {
    fn from(e: PoolStorageError) -> Self {
        match e {
            PoolStorageError::HomeNotSet => WorkbuddyTokenError::HomeNotSet,
            PoolStorageError::Io(io) => WorkbuddyTokenError::Io(io),
            PoolStorageError::Serde(s) => WorkbuddyTokenError::Serde(s),
        }
    }
}

/// 持久化的 WorkBuddy 登录凭证(每账号一套)。字段命名走 snake_case(本地落盘)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct WorkbuddyCredential {
    /// `Authorization: Bearer <…>` 用的 access token(Keycloak JWT)。
    pub access_token: String,
    /// refresh token —— refresh 时放 `X-Refresh-Token` 头。
    pub refresh_token: String,
    /// `Bearer`。
    pub token_type: String,
    /// access token 过期时刻,UNIX **毫秒** epoch。
    pub expiry_date: i64,
    /// 拿到凭证的时刻(ms epoch)。
    pub obtained_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 登录用户 uid(= JWT sub;`X-User-Id` 指纹头用 + 账号文件名)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// **本账号专属** `X-Device-Id`(登录生成的 v4 UUID)。每账号独立 → 网关看作不同设备。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

impl WorkbuddyCredential {
    /// 是否应主动 refresh —— `expiry_date` 前 [`REFRESH_BUFFER_SECS`] 秒即触发。
    pub fn should_refresh(&self) -> bool {
        unix_now_ms() >= self.expiry_date.saturating_sub(REFRESH_BUFFER_SECS * 1000)
    }
}

/// 单账号凭证文件句柄(薄 wrapper,存储走 [`crate::account_pool`] 原语)。
pub struct WorkbuddyCredentialStore {
    path: PathBuf,
}

impl WorkbuddyCredentialStore {
    /// 池内某账号:`<root>/workbuddy/<provider_id>/accounts/<uid>.json`。
    pub fn for_account(provider_id: &str, uid: &str) -> Result<Self, WorkbuddyTokenError> {
        Ok(Self {
            path: account_pool::account_file_path("workbuddy", provider_id, uid)?,
        })
    }

    /// 老「单文件单账号」路径 `<root>/workbuddy-oauth.json`(仅迁移读取用)。
    pub fn legacy_single() -> Result<Self, WorkbuddyTokenError> {
        Ok(Self {
            path: account_pool::legacy_file_path("workbuddy-oauth.json")?,
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
    pub fn load(&self) -> Result<Option<WorkbuddyCredential>, WorkbuddyTokenError> {
        Ok(account_pool::read_json_opt(&self.path)?)
    }

    /// atomic 写。
    pub fn save(&self, cred: &WorkbuddyCredential) -> Result<(), WorkbuddyTokenError> {
        account_pool::write_json_atomic(&self.path, cred)?;
        Ok(())
    }

    /// 删除(logout 单账号);文件不存在算成功。
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
            device_id: Some("dev-abc".into()),
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
        let store = WorkbuddyCredentialStore::at_path(dir.path().join("wb/p/u.json"));
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
