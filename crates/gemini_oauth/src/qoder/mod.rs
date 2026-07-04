//! QoderWork CN(阿里 Qoder 系)provider 集成。
//!
//! QoderWork 桌面端(`com.qoder.work.cn`,Electron + `qoderclicn` sidecar,SDK 是
//! Gemini CLI fork)的模型走云网关。逆向确认有一条 **OpenAI Chat Completions 兼容
//! 的 REST + SSE 通道**:`POST https://api2-v2.qoder.sh/model/v1/chat/completions`
//! (`Authorization: Bearer <jobToken>`),与 workbuddy / GLM 同构、可被本仓
//! `openai_chat` provider 框架接入。
//!
//! 鉴权链比 workbuddy 长一级(逆向 `out/main/main.js` + worker runtime 实证):
//!
//! ```text
//! ① 登录 device flow(纯客户端 PKCE,无服务端 authorize 端点):
//!    本地生成 verifier/challenge(S256)+ nonce(uuid)+ machine_id
//!    → 浏览器开 https://qoder.com.cn/device/selectAccounts?challenge=&nonce=&machine_id=&client_id=&redirect_uri=
//!    → 轮询 GET https://openapi.qoder.com.cn/api/v1/deviceToken/poll?nonce=&verifier=&challenge_method=S256
//!      → { token(=personal_token), refresh_token, expires_at, refresh_token_expires_at }
//! ② refresh: POST openapi.qoder.com.cn/api/v1/deviceToken/refresh { refresh_token }
//! ③ 换 jobToken(每请求即时,阶段二): POST openapi.qoder.com.cn/api/v1/jobToken/exchange { personal_token } → { token }
//! ④ 调模型(阶段二): POST api2-v2.qoder.sh/model/v1/chat/completions, Bearer jobToken, SSE
//! ```
//!
//! **阶段一**(本模块当前范围):只做 ① + ② —— 账号登录 + 凭证保存 + 自动 refresh。
//! ③④(jobToken 交换 + 模型出站)留待用户登录后实测确定 TTL / 模型列表再补。

use std::sync::OnceLock;

pub mod login;
pub mod pool;
pub mod token;

pub use login::{
    ensure_valid_personal_token, fetch_user_info, refresh_qoder_token, run_qoder_login, QoderError,
    QoderUserInfo,
};
pub use token::{QoderCredential, QoderCredentialStore, QoderTokenError};

/// 登录页 host(`getAuthBaseUrl` → `WEBSITE_DOMAIN`)。authUrl 打这个域。
pub const QODER_WEBSITE_HOST: &str = "qoder.com.cn";

/// OpenApi host(`getOpenApiBaseUrl` → `OPENAPI_DOMAIN`)。deviceToken/jobToken 端点在此。
pub const QODER_OPENAPI_HOST: &str = "openapi.qoder.com.cn";

/// 模型网关 host(`PJn()` prod = `api2-v2.qoder.sh`,可被 env `QODER_MODEL_SERVER_HOST`
/// 覆盖)。阶段二出站 `openai_chat` base 用。
pub const QODER_MODEL_HOST: &str = "api2-v2.qoder.sh";

/// QoderWork prod OAuth client_id(`CLIENT_ID.prod`,逆向自 `out/main/main.js`)。
/// transfer 冒充 QoderWork 桌面端发起 device flow 必须用这个。
pub const QODER_CLIENT_ID: &str = "1c5e33e1-364d-4ce6-b02c-acaa81274a5c";

/// device flow 的 `redirect_uri`(`getRedirectUri()` = `qoder-work-cn://`)。
pub const QODER_REDIRECT_URI: &str = "qoder-work-cn://";

/// 生成一个 RFC 4122 v4 UUID(小写带连字符),用 `getrandom` 取 16 字节随机。
/// nonce / machine_id 用。
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |bytes: &[u8]| bytes.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&b[0..4]),
        h(&b[4..6]),
        h(&b[6..8]),
        h(&b[8..10]),
        h(&b[10..16])
    )
}

/// 稳定的 `machine_id` —— 真实客户端一台机器一个稳定设备 id(`getMachineId()`)。
/// transfer 自生成一个 v4 UUID 持久化到 `~/.codex-app-transfer/qoder-machine-id`,
/// 之后复用(每个 client 本就独立设备,无需与 QoderWork 一致)。读写失败退化成进程内稳定值。
pub fn qoder_machine_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let path = codex_app_transfer_registry::paths::resolve_home()
                .map(|h| h.join(".codex-app-transfer").join("qoder-machine-id"));
            if let Some(p) = &path {
                if let Ok(s) = std::fs::read_to_string(p) {
                    let t = s.trim();
                    if !t.is_empty() {
                        return t.to_string();
                    }
                }
            }
            let id = uuid_v4();
            if let Some(p) = &path {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(p, &id);
            }
            id
        })
        .clone()
}

/// 解 JWT(不验签)的 payload 成 JSON(只读 claim)。personal_token 若是 JWT 用。
fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 从 token 的 `sub` claim 解 uid(personal_token 是 JWT 时);非 JWT 返 None。
pub fn user_id_from_jwt(token: &str) -> Option<String> {
    jwt_payload(token)?
        .get("sub")?
        .as_str()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble 应为 4:{id}");
        assert_ne!(uuid_v4(), uuid_v4());
    }

    #[test]
    fn user_id_from_jwt_reads_sub() {
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"sub":"u-42"}"#,
        );
        let token = format!("h.{payload}.sig");
        assert_eq!(user_id_from_jwt(&token), Some("u-42".to_string()));
        assert_eq!(user_id_from_jwt("nope"), None);
    }

    #[test]
    fn opaque_device_token_yields_no_jwt_uid() {
        // QoderWork 的 device token 是不透明串(`dt-` 前缀,单段,非 JWT),`user_id_from_jwt`
        // 恒 None。这正是 `run_qoder_login` 必须走 `GET /userinfo` 补 uid 的原因:池按 uid
        // keying,若靠 token 解 uid 会永远失败 → `add_account` 报 `uid` → 多账号池不可用。
        assert_eq!(user_id_from_jwt("dt-6oAexampleopaquetokenQTwS"), None);
    }
}
