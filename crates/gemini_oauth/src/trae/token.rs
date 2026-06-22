//! Trae 登录凭证(每账号一套指纹包)+ 按 provider id 持久化。
//!
//! ## 与 zai 的关键区别:按 **provider id** keying(多账号)
//!
//! zai 是「每 vendor 单文件单账号」(`zai-oauth.json`)。Trae 要满足「同设备多账号
//! 指纹隔离」,所以**每个 provider 条目(= 一个账号)一个文件**:
//! `~/.codex-app-transfer/trae/<sanitized provider id>.json`。`forward.rs` /
//! 额度注入 / admin handler 都按当前 active provider 的 `id` 定位凭证。切 provider
//! = 整包切换(token + 指纹 + keypair 全换)。
//!
//! 凭证包自包含:JWT + refresh token + [`DeviceFingerprint`] + [`DeviceKeyPair`],
//! refresh 时无需别处取料。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::token::TokenError;
use super::constants::TraeEdition;
use super::crypto::DeviceKeyPair;
use super::device::DeviceFingerprint;

/// 一次成功登录后落盘的完整凭证(每账号一套)。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraeCredential {
    /// 账号体系版本(决定 host / client_id)。
    pub edition: TraeEdition,
    /// **核心**:`Result.Token`(JWT)—— forward.rs 做 `x-icube-token` /
    /// `Authorization: Cloud-IDE-JWT`;额度查询也用它。
    pub token: String,
    /// `Result.RefreshToken` —— 过期续期用(带设备私钥签名)。
    pub refresh_token: String,
    /// access token 过期时刻(UNIX ms-epoch,0 = 未知)。
    #[serde(default)]
    pub token_expire_at_ms: i64,
    /// refresh token 过期时刻(UNIX ms-epoch,0 = 未知)。过了就得重走浏览器。
    #[serde(default)]
    pub refresh_expire_at_ms: i64,
    /// 账号 UserID(`Result.UserID`)—— UI / 诊断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 账号邮箱 / 标识(GetUserInfo)—— UI 展示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// `Result.AIRegion`(cn / sg / us)—— region host 路由 / 诊断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_region: Option<String>,
    /// **本账号专属**合成设备指纹(隔离核心,固定复用)。
    pub fingerprint: DeviceFingerprint,
    /// **本账号专属**设备密钥对(公钥首登上传、私钥 refresh 验签)。
    pub keypair: DeviceKeyPair,
    /// 登录完成时刻(UNIX ms-epoch)。
    pub obtained_at_ms: i64,
}

/// 手写 `Debug`,脱敏长期 secret(`token`/`refresh_token`/`keypair`);
/// `email`/`user_id`/`ai_region`/指纹的非密字段保留可见(诊断用)。
impl std::fmt::Debug for TraeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraeCredential")
            .field("edition", &self.edition)
            .field("token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("token_expire_at_ms", &self.token_expire_at_ms)
            .field("refresh_expire_at_ms", &self.refresh_expire_at_ms)
            .field("user_id", &self.user_id)
            .field("email", &self.email)
            .field("ai_region", &self.ai_region)
            .field("fingerprint", &self.fingerprint)
            .field("keypair", &self.keypair)
            .field("obtained_at_ms", &self.obtained_at_ms)
            .finish()
    }
}

/// `~/.codex-app-transfer/trae/<provider id>.json` 持久化句柄。
/// atomic write(temp+rename)+ Unix 0600,对齐 zai/gemini TokenStore。
pub struct TraeCredentialStore {
    path: PathBuf,
}

impl TraeCredentialStore {
    /// 按 provider id 解析路径 `<home>/.codex-app-transfer/trae/<sanitized id>.json`。
    pub fn for_provider_id(provider_id: &str) -> Result<Self, TokenError> {
        let home =
            codex_app_transfer_registry::paths::resolve_home().ok_or(TokenError::HomeNotSet)?;
        let path = home
            .join(".codex-app-transfer")
            .join("trae")
            .join(format!("{}.json", sanitize_id(provider_id)));
        Ok(Self { path })
    }

    /// 显式指定路径(单测用)。
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载凭证。文件不存在返 `Ok(None)`;JSON 损坏返 `Serde` 错(不静默)。
    pub fn load(&self) -> Result<Option<TraeCredential>, TokenError> {
        read_json_opt(&self.path)
    }

    /// 写凭证 —— temp + rename atomic;Unix 创建时即 0600。
    pub fn save(&self, cred: &TraeCredential) -> Result<(), TokenError> {
        write_json_atomic(&self.path, cred)
    }

    /// 删除凭证(logout / refresh token 过期重登)。不存在算成功。
    pub fn delete(&self) -> Result<(), TokenError> {
        delete_file_idempotent(&self.path)
    }
}

/// **登录后保存**(login-first)的中间凭证句柄:`<home>/.codex-app-transfer/trae/_pending.json`。
///
/// 在**尚未保存的 provider**(无 id)上点登录时,登录产出的完整 [`TraeCredential`](含本次
/// 新生成的设备指纹)先落 pending;用户保存 provider 拿到 id 后,由
/// [`claim_pending_for_provider`](super::claim_pending_for_provider) 把 pending 迁成
/// `trae/<id>.json` 并删 pending。单槽(同一时刻只一个 in-flight 新登录,UI 串行)。
pub struct TraePendingStore {
    path: PathBuf,
}

impl TraePendingStore {
    pub fn for_pending() -> Result<Self, TokenError> {
        let home =
            codex_app_transfer_registry::paths::resolve_home().ok_or(TokenError::HomeNotSet)?;
        let path = home
            .join(".codex-app-transfer")
            .join("trae")
            .join("_pending.json");
        Ok(Self { path })
    }

    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<TraeCredential>, TokenError> {
        read_json_opt(&self.path)
    }

    pub fn save(&self, cred: &TraeCredential) -> Result<(), TokenError> {
        write_json_atomic(&self.path, cred)
    }

    pub fn delete(&self) -> Result<(), TokenError> {
        delete_file_idempotent(&self.path)
    }
}

/// 把 provider id 清洗成安全文件名片段(只留 `[A-Za-z0-9._-]`,其余换 `_`)。
/// 防 `../`、路径分隔符等注入到文件路径。
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
    // 防全空 / 仅点(`.` / `..`)导致的退化文件名
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// atomic 写(temp + rename,Unix 创建时即 0600)。
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), TokenError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 唯一 temp 名(pid + 进程内递增计数):login handler 与 quota 守护进程的续期可能
    // **并发**写同一 `<id>.json`,固定 temp 名会让二者 create_new 撞 / 互删对方的 temp
    // (review [8])。各写各的唯一 temp,最后 rename 到正式路径(rename 原子)。
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
            Err(e) => return Err(TokenError::Io(e)),
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

fn read_json_opt<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, TokenError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(TokenError::Io(e)),
    }
}

fn delete_file_idempotent(path: &Path) -> Result<(), TokenError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(TokenError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> TraeCredential {
        TraeCredential {
            edition: TraeEdition::Cn,
            token: "ey.trae.jwt".into(),
            refresh_token: "rt-secret".into(),
            token_expire_at_ms: 1_700_000_000_000,
            refresh_expire_at_ms: 1_800_000_000_000,
            user_id: Some("2767898365400680".into()),
            email: Some("user@example.com".into()),
            ai_region: Some("cn".into()),
            fingerprint: DeviceFingerprint::generate().unwrap(),
            keypair: DeviceKeyPair::generate().unwrap(),
            obtained_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = TraeCredentialStore::at_path(dir.path().join("trae/p1.json"));
        let cred = sample();
        assert_eq!(store.load().unwrap(), None, "首次 load 必须 None");
        store.save(&cred).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), cred);
        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn distinct_provider_ids_get_distinct_files() {
        if codex_app_transfer_registry::paths::resolve_home().is_none() {
            return;
        }
        let a = TraeCredentialStore::for_provider_id("trae-cn-alice").unwrap();
        let b = TraeCredentialStore::for_provider_id("trae-cn-bob").unwrap();
        assert_ne!(
            a.path(),
            b.path(),
            "不同 provider id 必须分文件(多账号隔离)"
        );
        assert!(a.path().ends_with("trae-cn-alice.json"));
    }

    #[test]
    fn sanitize_id_blocks_path_traversal() {
        // 安全不变量:绝无路径分隔符 + 不退化成 `.`/`..`/空
        for raw in ["../../etc/passwd", "a/b\\c", "..", "", "x/../y", "/etc"] {
            let s = sanitize_id(raw);
            assert!(!s.contains('/'), "{raw} → {s} 不该含 /");
            assert!(!s.contains('\\'), "{raw} → {s} 不该含 \\");
            assert!(!s.is_empty() && s != "." && s != "..", "{raw} → {s} 退化");
        }
        // 分隔符全换 `_`
        assert_eq!(sanitize_id("a/b\\c"), "a_b_c");
        // 纯点 / 空 → default
        assert_eq!(sanitize_id(".."), "default");
        assert_eq!(sanitize_id(""), "default");
        // 合法 id 原样保留
        assert_eq!(sanitize_id("trae-cn_1.0"), "trae-cn_1.0");
    }

    #[test]
    fn debug_redacts_secrets() {
        let dbg = format!("{:?}", sample());
        assert!(!dbg.contains("ey.trae.jwt"), "token 不该出现: {dbg}");
        assert!(!dbg.contains("rt-secret"), "refresh_token 不该出现: {dbg}");
        assert!(!dbg.contains("BEGIN PRIVATE KEY"), "私钥不该出现: {dbg}");
        assert!(dbg.contains("user@example.com"), "email 应可见: {dbg}");
    }

    #[test]
    fn load_returns_serde_error_on_corrupt_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("p.json");
        std::fs::write(&path, b"{bad").unwrap();
        let store = TraeCredentialStore::at_path(&path);
        assert!(matches!(store.load().unwrap_err(), TokenError::Serde(_)));
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_unix_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = TraeCredentialStore::at_path(dir.path().join("trae/p.json"));
        store.save(&sample()).unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
