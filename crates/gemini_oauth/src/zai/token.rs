//! z.ai / bigmodel 登录凭证模型 + 持久化。
//!
//! 跟 gemini-cli / antigravity 的 [`super::super::token::OauthToken`] **并行**(不同
//! vendor、不同 token shape):z.ai 路径没有 Google 那套 `expiry_date`/`refresh_token`
//! 语义,核心是「换出来的组织 API key」—— forward.rs 拿它当 `Authorization: Bearer`
//! 直接打模型面。这里复刻 gemini `TokenStore` 的 atomic write(temp+rename)+ Unix
//! 0600 安全权限,但序列化自己的 [`ZaiCredential`] shape。
//!
//! **每 provider 一个文件**:`~/.codex-app-transfer/{zai,bigmodel}-oauth.json`
//! (镜像 antigravity 单 provider 单文件),由 [`ZaiProvider::token_filename`] 决定。
//!
//! **无 refresh**:ZCode 自己也没有 refresh flow,组织 key 长期有效,401/403 即
//! 删文件重登。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::token::TokenError;
use super::constants::ZaiProvider;

/// 一次成功登录后落盘的完整凭证。
///
/// 操作上 forward.rs 只需要 [`org_api_key`](Self::org_api_key)(打模型面的 Bearer);
/// 其余字段(zcode_jwt / provider_access_token)留作「key 被吊销时不必重新走浏览器
/// OAuth、可直接重新 mint 一把组织 key」的能力,以及 UI 展示当前账号(email)。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZaiCredential {
    /// 这把凭证属于哪个账号体系(决定打哪个模型 base / biz base)。
    pub provider: ZaiProvider,
    /// **核心**:Coding Plan 换出来的组织 API key(形如 `<apiKey>.<secretKey>`)。
    /// forward.rs 用它做 `Authorization: Bearer <org_api_key>`。
    pub org_api_key: String,
    /// token 交换拿到的 ZCode 业务 JWT(`data.token`)—— 重新 mint key 时复用。
    pub zcode_jwt: String,
    /// provider 侧 access_token(`data.zai.access_token` / `data.bigmodel.access_token`)
    /// —— z.ai 还需拿它换业务 token,留作重 mint。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_access_token: Option<String>,
    /// 当前登录账号邮箱 / 标识(`data.user` 解析,可空)—— UI 展示用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 登录完成时刻(UNIX ms-epoch)—— 诊断 / UI「上次登录」展示用。
    pub obtained_at_ms: i64,
}

/// 手写 `Debug`,**脱敏长期有效的 secret**(`org_api_key`/`zcode_jwt`/
/// `provider_access_token`)—— 防 `tracing::debug!(?cred)` / panic backtrace 把整把
/// key 打进日志(type-design review IMPORTANT)。`email` 非 secret、production 日志
/// 本就打,保留可见。
impl std::fmt::Debug for ZaiCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZaiCredential")
            .field("provider", &self.provider)
            .field("org_api_key", &"<redacted>")
            .field("zcode_jwt", &"<redacted>")
            .field(
                "provider_access_token",
                &self.provider_access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("email", &self.email)
            .field("obtained_at_ms", &self.obtained_at_ms)
            .finish()
    }
}

/// `~/.codex-app-transfer/<provider>-oauth.json` 持久化句柄。复刻 gemini
/// `TokenStore` 的 atomic write + 0600,序列化 [`ZaiCredential`]。
pub struct ZaiCredentialStore {
    path: PathBuf,
}

impl ZaiCredentialStore {
    /// 按 provider 解析默认路径 `<home>/.codex-app-transfer/<provider>-oauth.json`。
    /// `<home>` 走 workspace 唯一入口 `registry::paths::resolve_home`(跟 gemini /
    /// antigravity / CodexPaths 一致:`CODEX_APP_TRANSFER_HOME` → `HOME` →
    /// `USERPROFILE`)。
    pub fn for_provider(provider: ZaiProvider) -> Result<Self, TokenError> {
        let home =
            codex_app_transfer_registry::paths::resolve_home().ok_or(TokenError::HomeNotSet)?;
        let path = home
            .join(".codex-app-transfer")
            .join(provider.token_filename());
        Ok(Self { path })
    }

    /// 显式指定路径(单测用)。
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载凭证。文件不存在返 `Ok(None)`(未登录正常路径);JSON 损坏返
    /// `Serde` 错(不静默当未登录,跟 gemini TokenStore 一致)。
    pub fn load(&self) -> Result<Option<ZaiCredential>, TokenError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let cred: ZaiCredential = serde_json::from_slice(&bytes)?;
                Ok(Some(cred))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TokenError::Io(e)),
        }
    }

    /// 写凭证 —— temp + rename 保证 atomic;Unix 上**创建时即 0600**(避免先
    /// 0644 再 chmod 的世界可读窗口,跟 gemini TokenStore H3 修一致)。
    pub fn save(&self, cred: &ZaiCredential) -> Result<(), TokenError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(cred)?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            // 清残留 tmp:只吞 NotFound,别的 IO 错必须 propagate
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
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 删除凭证(logout / 401 重登)。文件不存在算成功(idempotent)。
    pub fn delete(&self) -> Result<(), TokenError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TokenError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(provider: ZaiProvider) -> ZaiCredential {
        ZaiCredential {
            provider,
            org_api_key: "ak-abc123.sk-secret789".into(),
            zcode_jwt: "ey.zcode.jwt".into(),
            provider_access_token: Some("provider-at-xyz".into()),
            email: Some("user@example.com".into()),
            obtained_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = ZaiCredentialStore::at_path(dir.path().join("zai-oauth.json"));
        let cred = sample(ZaiProvider::Zai);

        assert_eq!(store.load().unwrap(), None, "首次 load 必须 None");
        store.save(&cred).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), cred);

        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn for_provider_uses_distinct_filenames() {
        // 不实际落盘,只验路径按 provider 分文件(依赖 resolve_home 存在)
        if codex_app_transfer_registry::paths::resolve_home().is_none() {
            return; // CI 无 HOME 的极端环境跳过
        }
        let zai = ZaiCredentialStore::for_provider(ZaiProvider::Zai).unwrap();
        let big = ZaiCredentialStore::for_provider(ZaiProvider::BigModel).unwrap();
        assert!(zai.path().ends_with("zai-oauth.json"));
        assert!(big.path().ends_with("bigmodel-oauth.json"));
        assert_ne!(zai.path(), big.path());
    }

    #[test]
    fn load_returns_serde_error_on_corrupt_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("zai-oauth.json");
        std::fs::write(&path, b"{not json").unwrap();
        let store = ZaiCredentialStore::at_path(&path);
        let err = store.load().unwrap_err();
        assert!(
            matches!(err, TokenError::Serde(_)),
            "损坏 JSON 必须返 Serde 错,实际 {err:?}"
        );
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let store = ZaiCredentialStore::at_path(dir.path().join("a/b/c/zai-oauth.json"));
        store.save(&sample(ZaiProvider::BigModel)).unwrap();
        assert!(store.load().unwrap().is_some());
    }

    #[test]
    fn debug_redacts_long_lived_secrets() {
        let cred = sample(ZaiProvider::Zai);
        let dbg = format!("{cred:?}");
        assert!(!dbg.contains("ak-abc123"), "org_api_key 不该出现在 Debug: {dbg}");
        assert!(!dbg.contains("sk-secret789"), "secretKey 部分不该出现: {dbg}");
        assert!(!dbg.contains("ey.zcode.jwt"), "zcode_jwt 不该出现: {dbg}");
        assert!(!dbg.contains("provider-at-xyz"), "provider_access_token 不该出现: {dbg}");
        assert!(dbg.contains("<redacted>"), "应有脱敏标记: {dbg}");
        // email 非 secret,保留可见(production 日志本就打)
        assert!(dbg.contains("user@example.com"), "email 应可见: {dbg}");
    }

    #[test]
    fn optional_fields_skip_when_none() {
        let mut cred = sample(ZaiProvider::Zai);
        cred.provider_access_token = None;
        cred.email = None;
        let json = serde_json::to_string(&cred).unwrap();
        assert!(!json.contains("provider_access_token"), "json: {json}");
        assert!(!json.contains("email"), "json: {json}");
        assert!(json.contains("org_api_key"));
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_unix_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = ZaiCredentialStore::at_path(dir.path().join("zai-oauth.json"));
        store.save(&sample(ZaiProvider::Zai)).unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "凭证文件必须 0600,实际 {mode:o}");
    }
}
