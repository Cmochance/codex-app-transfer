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
    /// 组织 id(个人账号为空串)。登录时经 `/userinfo` 取,签名 `Cosy-User`/`encrypt_user_info` 用。
    /// 与 uid 一样是账号静态属性 → 落盘缓存,出站签名直接复用,免每请求打账号级 `/userinfo`
    /// (对齐 QoderWork 原 app 的 user_info 缓存行为;`None` = 老凭证未存过 → 出站兜底拉一次回填)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_tags: Option<Vec<String>>,
}

impl QoderCredential {
    /// 距 personal_token 过期 < 5 分钟即应 refresh(与 workbuddy / gemini 同窗口)。
    pub fn should_refresh(&self) -> bool {
        let now = unix_now_ms();
        now + 5 * 60 * 1000 >= self.expiry_date
    }

    /// 登录时缓存的用户信息(uid + organization_id + organization_tags)—— **三者齐备才返回**,
    /// 供出站 Cosy 签名复用,免每请求打账号级 `/userinfo`(②)。老凭证(登录时未缓存 org、或缺 uid)
    /// 返 `None` → caller 兜底拉一次 `/userinfo`。
    ///
    /// **个人号 org 存 `Some("")`(空串,非 `None`)**:算「已缓存」正常返回(签名侧接受空 org),
    /// **不**触发兜底 —— 否则个人号每请求都白拉一次 /userinfo。
    pub fn cached_user_info(&self) -> Option<(String, String, Vec<String>)> {
        match (&self.uid, &self.organization_id, &self.organization_tags) {
            (Some(uid), Some(org_id), Some(tags)) => {
                Some((uid.clone(), org_id.clone(), tags.clone()))
            }
            _ => None,
        }
    }
}

/// 把 refresh 响应(`fresh`,`payload_to_cred` 只填了 token 相关字段,身份字段 None/空)与旧凭证
/// (`old`)合并成落盘凭证。refresh 网关通常不回带 昵称/uid/machine_id/org/refresh_token,逐字段用
/// 旧值兜底,**fresh 有值则 fresh 胜出**(轮换/更新的凭证不被旧值覆盖):
///
/// - **`refresh_token` 空 → 保留旧值**:否则续期一次即清空、账号被 brick 需重登(severity-9);
/// - **`machine_id` 缺 → 从旧值补**:否则下次签名 fall 到全局兜底 → per-account 设备隔离静默瓦解(①);
/// - **`uid` / `organization_id` / `organization_tags` 缺 → 从旧值补**:签名复用登录缓存,免每请求打
///   `/userinfo`(②,org 限流则 token 有效也全崩)。
///
/// pool 续期(`pool::ensure_valid`)与单账号续期(`login::ensure_valid_personal_token`)共用本函数,
/// 杜绝两处回填逻辑漂移(此前单账号路只回填 nickname、漏了 refresh_token/machine_id/org 三个高危字段)。
pub(crate) fn merge_refreshed_cred(
    mut fresh: QoderCredential,
    old: &QoderCredential,
) -> QoderCredential {
    fresh.nickname = fresh.nickname.or_else(|| old.nickname.clone());
    fresh.uid = fresh.uid.or_else(|| old.uid.clone());
    fresh.machine_id = fresh.machine_id.or_else(|| old.machine_id.clone());
    fresh.organization_id = fresh
        .organization_id
        .or_else(|| old.organization_id.clone());
    fresh.organization_tags = fresh
        .organization_tags
        .or_else(|| old.organization_tags.clone());
    if fresh.refresh_token.is_empty() {
        fresh.refresh_token = old.refresh_token.clone();
    }
    fresh
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
            organization_id: None,
            organization_tags: None,
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
            organization_id: None,
            organization_tags: None,
        };
        assert!(cred.should_refresh());
    }

    /// 造一个「旧凭证」:身份字段(refresh_token/machine_id/uid/org/nickname)都齐。
    fn old_cred() -> QoderCredential {
        QoderCredential {
            personal_token: "old-pt".into(),
            refresh_token: "old-rt".into(),
            expiry_date: unix_now_ms(),
            refresh_expiry_date: unix_now_ms(),
            obtained_at_ms: unix_now_ms(),
            machine_id: Some("mid-old".into()),
            nickname: Some("Alice".into()),
            uid: Some("u-old".into()),
            organization_id: Some("org-old".into()),
            organization_tags: Some(vec!["t1".into()]),
        }
    }

    /// 造一个「refresh 响应凭证」:payload_to_cred 只填 token 相关,身份字段 None、refresh_token 空。
    fn fresh_cred(refresh_token: &str) -> QoderCredential {
        QoderCredential {
            personal_token: "new-pt".into(),
            refresh_token: refresh_token.into(),
            expiry_date: unix_now_ms() + 3_600_000,
            refresh_expiry_date: unix_now_ms() + 30 * 86_400_000,
            obtained_at_ms: unix_now_ms(),
            machine_id: None,
            nickname: None,
            uid: None,
            organization_id: None,
            organization_tags: None,
        }
    }

    #[test]
    fn merge_backfills_identity_fields_from_old_when_fresh_missing() {
        // refresh 不回带身份字段 → 全部从旧凭证兜底(保 refresh_token 防 brick、machine_id 保隔离、
        // uid/org 保签名缓存);personal_token/expiry 用 fresh 的新值。
        let merged = merge_refreshed_cred(fresh_cred(""), &old_cred());
        assert_eq!(merged.personal_token, "new-pt", "token 用 fresh 新值");
        assert_eq!(
            merged.refresh_token, "old-rt",
            "refresh_token 空→保留旧值(防 brick)"
        );
        assert_eq!(
            merged.machine_id.as_deref(),
            Some("mid-old"),
            "machine_id 兜底(保隔离)"
        );
        assert_eq!(merged.uid.as_deref(), Some("u-old"));
        assert_eq!(merged.organization_id.as_deref(), Some("org-old"));
        assert_eq!(merged.organization_tags, Some(vec!["t1".to_string()]));
        assert_eq!(merged.nickname.as_deref(), Some("Alice"));
    }

    #[test]
    fn merge_prefers_fresh_when_present() {
        // fresh 有值 → fresh 胜出(轮换的 refresh_token / 更新的 machine_id·org 不被旧值覆盖)。
        let mut fresh = fresh_cred("rotated-rt");
        fresh.machine_id = Some("mid-new".into());
        fresh.uid = Some("u-new".into());
        fresh.organization_id = Some("org-new".into());
        fresh.organization_tags = Some(vec!["t2".into()]);
        fresh.nickname = Some("Bob".into());
        let merged = merge_refreshed_cred(fresh, &old_cred());
        assert_eq!(
            merged.refresh_token, "rotated-rt",
            "轮换的 refresh_token 不被旧值覆盖"
        );
        assert_eq!(merged.machine_id.as_deref(), Some("mid-new"));
        assert_eq!(merged.uid.as_deref(), Some("u-new"));
        assert_eq!(merged.organization_id.as_deref(), Some("org-new"));
        assert_eq!(merged.organization_tags, Some(vec!["t2".to_string()]));
        assert_eq!(merged.nickname.as_deref(), Some("Bob"));
    }

    #[test]
    fn merge_preserves_empty_string_org_as_personal_account() {
        // 个人号 org 存 Some("")(非 None):merge 保留 fresh 的 None→旧的 Some("")(不误判成缺失)。
        let mut old = old_cred();
        old.organization_id = Some(String::new());
        old.organization_tags = Some(vec![]);
        let merged = merge_refreshed_cred(fresh_cred(""), &old);
        assert_eq!(
            merged.organization_id.as_deref(),
            Some(""),
            "个人号空串 org 保留"
        );
        assert_eq!(merged.organization_tags, Some(vec![]));
    }

    #[test]
    fn cached_user_info_needs_all_three_fields() {
        // ② 复用缓存的判据:uid+org_id+org_tags 三者齐备才返回(否则兜底拉 /userinfo)。
        let full = old_cred();
        assert_eq!(
            full.cached_user_info(),
            Some(("u-old".into(), "org-old".into(), vec!["t1".into()])),
            "完整凭证 → 复用缓存"
        );
        // 老凭证:org 未缓存(None)→ None(触发兜底)。
        let mut no_org = old_cred();
        no_org.organization_id = None;
        assert_eq!(no_org.cached_user_info(), None, "缺 org → 兜底");
        let mut no_tags = old_cred();
        no_tags.organization_tags = None;
        assert_eq!(no_tags.cached_user_info(), None, "缺 org_tags → 兜底");
        let mut no_uid = old_cred();
        no_uid.uid = None;
        assert_eq!(no_uid.cached_user_info(), None, "缺 uid → 兜底");
    }

    #[test]
    fn cached_user_info_personal_account_empty_org_reuses_not_refetch() {
        // 个人号 org=Some("")、tags=Some(vec![]):算已缓存,复用(不因空串误判成未缓存而每请求白拉)。
        let mut personal = old_cred();
        personal.organization_id = Some(String::new());
        personal.organization_tags = Some(vec![]);
        assert_eq!(
            personal.cached_user_info(),
            Some(("u-old".into(), String::new(), vec![])),
            "个人号空串 org 应复用缓存"
        );
    }
}
