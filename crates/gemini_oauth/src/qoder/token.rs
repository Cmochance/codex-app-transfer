//! QoderWork CN 账号凭证存储(单账号 JSON 文件 + 原子写),对齐
//! `workbuddy::token` 的单文件模式,但 QoderWork 是两级 token
//! (personal_token 长期 + jobToken 短期),本文件只存**长期**的 device token 侧:
//! `token`(personal_token)+ `refresh_token`,jobToken 交换是每请求即时做、不落盘。
//!
//! 存储布局(`~/.codex-app-transfer/`):`qoder-oauth.json`(单账号)。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum QoderTokenError {
    #[error("HOME 目录不可用")]
    NoHome,
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
}

/// 一条 QoderWork device 凭证(`/api/v1/deviceToken/poll` / `refresh` 的产物)。
///
/// `token` 即上游 `personal_token`(device token),用于换 jobToken;
/// `refresh_token` 过期前刷新。两者各有独立过期时刻。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QoderCredential {
    /// 上游 `token` 字段 = personal_token(换 jobToken 用)。
    pub personal_token: String,
    pub refresh_token: String,
    /// personal_token 过期时刻(unix ms)。
    pub expiry_date: i64,
    /// refresh_token 过期时刻(unix ms);上游给了才有,否则 0。
    #[serde(default)]
    pub refresh_expiry_date: i64,
    /// 本条凭证获取时刻(unix ms)。
    #[serde(default)]
    pub obtained_at_ms: i64,
    /// 登录时用的设备指纹(machine_id);refresh 不变,持久保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

impl QoderCredential {
    /// 距 personal_token 过期 < 5 分钟即应 refresh(与 workbuddy / gemini 同窗口)。
    pub fn should_refresh(&self) -> bool {
        let now = unix_now_ms();
        now + 5 * 60 * 1000 >= self.expiry_date
    }
}

/// 单账号凭证文件句柄。
pub struct QoderCredentialStore {
    path: PathBuf,
}

impl QoderCredentialStore {
    /// 默认路径 `~/.codex-app-transfer/qoder-oauth.json`。
    pub fn with_default_path() -> Result<Self, QoderTokenError> {
        let home =
            codex_app_transfer_registry::paths::resolve_home().ok_or(QoderTokenError::NoHome)?;
        Ok(Self {
            path: home.join(".codex-app-transfer").join("qoder-oauth.json"),
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
    pub fn load(&self) -> Result<Option<QoderCredential>, QoderTokenError> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// atomic 写(唯一 temp + rename,避免半写)。
    pub fn save(&self, cred: &QoderCredential) -> Result<(), QoderTokenError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(cred)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 删除(logout);文件不存在算成功。
    pub fn delete(&self) -> Result<(), QoderTokenError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 当前 unix 时间(毫秒)。
pub fn unix_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("qoder-tok-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = QoderCredentialStore::at_path(dir.join("qoder-oauth.json"));
        assert!(store.load().unwrap().is_none(), "未写入时应 None");
        let cred = QoderCredential {
            personal_token: "pt-abc".into(),
            refresh_token: "rt-xyz".into(),
            expiry_date: unix_now_ms() + 3_600_000,
            refresh_expiry_date: unix_now_ms() + 30 * 86_400_000,
            obtained_at_ms: unix_now_ms(),
            machine_id: Some("mid-1".into()),
            nickname: None,
            uid: Some("u-1".into()),
        };
        store.save(&cred).unwrap();
        let got = store.load().unwrap().unwrap();
        assert_eq!(got.personal_token, "pt-abc");
        assert_eq!(got.machine_id.as_deref(), Some("mid-1"));
        assert!(!got.should_refresh(), "1h 后过期不应触发 refresh");
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none(), "delete 后应 None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_refresh_within_window() {
        let cred = QoderCredential {
            personal_token: "p".into(),
            refresh_token: "r".into(),
            expiry_date: unix_now_ms() + 60_000, // 1min 后过期,在 5min 窗口内
            refresh_expiry_date: 0,
            obtained_at_ms: unix_now_ms(),
            machine_id: None,
            nickname: None,
            uid: None,
        };
        assert!(cred.should_refresh());
    }
}
