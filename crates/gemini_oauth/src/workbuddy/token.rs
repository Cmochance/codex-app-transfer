//! WorkBuddy 账号登录凭证持久化 —— **单 provider 多账号池**。
//!
//! ## 存储布局(多账号 / 风控隔离)
//!
//! 旧版「单文件单账号」(`workbuddy-oauth.json`)只能登一个账号。要支持「一个
//! `workbuddy-login` provider 内多账号 + 额度守护自动切换」,改成**每账号一文件**(对齐
//! `trae/token.rs` 的 per-id keying):
//! - 账号:`~/.codex-app-transfer/workbuddy/<provider_id>/accounts/<uid>.json`(凭证含**本账号专属
//!   `device_id`**,网关眼里每账号是独立设备,避免同客户端被风控关联);
//! - 池态:`~/.codex-app-transfer/workbuddy/<provider_id>/_pool.json`(当前服务账号
//!   `active_uid` + 每账号 `exhausted_until_ms`,额度刷新时刻前不再选)。
//!
//! 旧单文件 + 全局 `workbuddy-device-id` 由 [`super::pool`] 的迁移逻辑一次性搬进池首账号。

use std::collections::HashMap;
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
    /// access token 过期时刻,UNIX **毫秒** epoch(obtain/refresh 时按 expiresIn 算)。
    pub expiry_date: i64,
    /// 拿到凭证的时刻(ms epoch)—— UI 展示 / 排查用。
    pub obtained_at_ms: i64,
    /// 登录用户昵称(展示当前账号)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// 登录用户 uid(= JWT sub;`X-User-Id` 指纹头用 + 账号文件名)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// **本账号专属** `X-Device-Id`(登录时生成的 v4 UUID)。每账号独立 → 网关看作不同设备,
    /// 避免多账号同设备被风控关联。`None`(老凭证/迁移)由 caller 补一个。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
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

/// `<home>/.codex-app-transfer` 根。
fn transfer_root() -> Result<PathBuf, WorkbuddyTokenError> {
    codex_app_transfer_registry::paths::resolve_home()
        .map(|h| h.join(".codex-app-transfer"))
        .ok_or(WorkbuddyTokenError::HomeNotSet)
}

/// 把 provider id / uid 清洗成安全文件名片段(只留 `[A-Za-z0-9._-]`,其余换 `_`;
/// 防 `../`、路径分隔符注入)。空 / 纯点退化成 `default`。对齐 `trae/token.rs::sanitize_id`。
fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 单账号凭证文件句柄。`for_account(provider_id, uid)` →
/// `workbuddy/<provider_id>/<uid>.json`;`legacy_single()` 指老单文件(迁移用)。
pub struct WorkbuddyCredentialStore {
    path: PathBuf,
}

impl WorkbuddyCredentialStore {
    /// 池内某账号:`<root>/workbuddy/<provider_id>/<uid>.json`。
    pub fn for_account(provider_id: &str, uid: &str) -> Result<Self, WorkbuddyTokenError> {
        Ok(Self {
            path: account_path(&transfer_root()?, provider_id, uid),
        })
    }

    /// 老的「单文件单账号」路径 `<root>/workbuddy-oauth.json`(仅迁移读取用)。
    pub fn legacy_single() -> Result<Self, WorkbuddyTokenError> {
        Ok(Self {
            path: transfer_root()?.join("workbuddy-oauth.json"),
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
        read_json_opt(&self.path)
    }

    /// atomic 写(唯一 temp + rename),Unix 创建即 0600。
    pub fn save(&self, cred: &WorkbuddyCredential) -> Result<(), WorkbuddyTokenError> {
        write_json_atomic(&self.path, cred)
    }

    /// 删除(logout 单账号);文件不存在算成功。
    pub fn delete(&self) -> Result<(), WorkbuddyTokenError> {
        delete_file_idempotent(&self.path)
    }
}

fn account_path(root: &Path, provider_id: &str, uid: &str) -> PathBuf {
    root.join("workbuddy")
        .join(sanitize_id(provider_id))
        // 账号文件放 `accounts/` 子目录,与同级 `_pool.json` 物理隔离:防某 uid 清洗后恰为
        // `_pool` 时账号文件覆盖池态文件(codex review:uid 来自 JWT sub,理论可影响)。
        .join("accounts")
        .join(format!("{}.json", sanitize_id(uid)))
}

/// 列某 provider 池内所有账号凭证(读 `workbuddy/<provider_id>/accounts/*.json`,跳过 temp)。
/// 目录不存在 → 空。单个文件损坏 → 跳过(不让一个坏文件废掉整池)。
pub fn list_accounts(provider_id: &str) -> Result<Vec<WorkbuddyCredential>, WorkbuddyTokenError> {
    let dir = transfer_root()?
        .join("workbuddy")
        .join(sanitize_id(provider_id))
        .join("accounts");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(WorkbuddyTokenError::Io(e)),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // 只认 *.json,排除 _pool.json 与 .json.tmp.* 临时文件
        if !name.ends_with(".json") || name == "_pool.json" || name.contains(".tmp") {
            continue;
        }
        if let Ok(Some(cred)) = read_json_opt::<WorkbuddyCredential>(&path) {
            out.push(cred);
        }
    }
    Ok(out)
}

/// 池运行态:当前服务账号 + 每账号耗尽到期时刻。存 `workbuddy/<provider_id>/_pool.json`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolState {
    /// 当前 sticky 服务账号 uid(`None` = 未定,选择器现选)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_uid: Option<String>,
    /// uid → 耗尽到期 UNIX ms(在此之前不选该账号;通常 = 会刷新的包 CycleEndTime)。
    #[serde(default)]
    pub exhausted_until: HashMap<String, i64>,
}

/// `workbuddy/<provider_id>/_pool.json` 句柄。
pub struct WorkbuddyPoolStore {
    path: PathBuf,
}

impl WorkbuddyPoolStore {
    pub fn for_provider(provider_id: &str) -> Result<Self, WorkbuddyTokenError> {
        Ok(Self {
            path: transfer_root()?
                .join("workbuddy")
                .join(sanitize_id(provider_id))
                .join("_pool.json"),
        })
    }

    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载池态;文件不存在 → 默认空态(非错误)。
    pub fn load(&self) -> Result<PoolState, WorkbuddyTokenError> {
        Ok(read_json_opt(&self.path)?.unwrap_or_default())
    }

    pub fn save(&self, state: &PoolState) -> Result<(), WorkbuddyTokenError> {
        write_json_atomic(&self.path, state)
    }
}

/// atomic 写(唯一 temp + rename,Unix 创建即 0600)。唯一 temp(pid + 进程内递增计数):
/// login handler 与 quota 守护可能**并发**写同一 `<uid>.json`(续期)/ `_pool.json`,固定
/// temp 名会 create_new 互撞。对齐 `trae/token.rs::write_json_atomic`。
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), WorkbuddyTokenError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{nonce}", std::process::id()));
    let json = serde_json::to_vec_pretty(value)?;

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
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json_opt<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, WorkbuddyTokenError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WorkbuddyTokenError::Io(e)),
    }
}

fn delete_file_idempotent(path: &Path) -> Result<(), WorkbuddyTokenError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkbuddyTokenError::Io(e)),
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

    #[test]
    fn account_path_isolates_by_provider_and_uid() {
        let root = Path::new("/r");
        let a = account_path(root, "wb-login", "u-alice");
        let b = account_path(root, "wb-login", "u-bob");
        let c = account_path(root, "wb-login-2", "u-alice");
        assert_ne!(a, b, "同 provider 不同 uid 分文件");
        assert_ne!(a, c, "不同 provider 分目录");
        assert!(a.ends_with("workbuddy/wb-login/accounts/u-alice.json"));
    }

    #[test]
    fn sanitize_blocks_path_traversal() {
        for raw in ["../../etc/passwd", "a/b\\c", "..", "", "x/../y", "/etc"] {
            let s = sanitize_id(raw);
            assert!(
                !s.contains('/') && !s.contains('\\'),
                "{raw} → {s} 含分隔符"
            );
            assert!(!s.is_empty() && s != "." && s != "..", "{raw} → {s} 退化");
        }
        assert_eq!(sanitize_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_id(".."), "default");
        assert_eq!(sanitize_id("wb-login_1.0"), "wb-login_1.0");
    }

    #[test]
    fn pool_state_roundtrip_and_default() {
        let dir = TempDir::new().unwrap();
        let store = WorkbuddyPoolStore::at_path(dir.path().join("wb/p/_pool.json"));
        assert_eq!(store.load().unwrap(), PoolState::default(), "缺文件=空态");
        let mut st = PoolState::default();
        st.active_uid = Some("u-1".into());
        st.exhausted_until.insert("u-2".into(), 1_800_000_000_000);
        store.save(&st).unwrap();
        assert_eq!(store.load().unwrap(), st);
    }

    #[test]
    fn list_accounts_skips_pool_and_temp() {
        // 用 at_path 写两个账号 + _pool.json 到同目录,list 只认账号(此处验过滤规则的纯逻辑:
        // 直接铺文件再走目录枚举)。
        let dir = TempDir::new().unwrap();
        let acct_dir = dir.path().join("workbuddy").join("wb-login");
        std::fs::create_dir_all(&acct_dir).unwrap();
        for (name, c) in [("u-a.json", cred(3600)), ("u-b.json", cred(3600))] {
            WorkbuddyCredentialStore::at_path(acct_dir.join(name))
                .save(&c)
                .unwrap();
        }
        std::fs::write(acct_dir.join("_pool.json"), b"{}").unwrap();
        std::fs::write(acct_dir.join("u-c.json.tmp.1.2"), b"{}").unwrap();
        let entries: Vec<_> = std::fs::read_dir(&acct_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".json") && n != "_pool.json" && !n.contains(".tmp"))
            .collect();
        assert_eq!(entries.len(), 2, "只剩两个账号 json");
    }
}
