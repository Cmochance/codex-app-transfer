//! WorkBuddy(腾讯 CodeBuddy)provider 集成。
//!
//! WorkBuddy 桌面端的模型走云网关 `https://copilot.tencent.com/v2/chat/completions`
//! (OpenAI Chat Completions 兼容,纯 Bearer 鉴权)。本模块提供两块能力:
//!
//! 1. **coding 模式 wire 指纹伪装**([`workbuddy_source_headers`])—— 让 Codex 经
//!    代理发出的请求在传输层与真实 CodeBuddy 桌面端 coding 模式一致,避免被服务端
//!    风控判定为"非官方客户端"。指纹三类:① 固定身份头(`X-Agent-Intent: coding` /
//!    `X-Product: SaaS` / `X-IDE-*` / `User-Agent: OpenAI/JS <ver>` / `X-Stainless-*`)、
//!    ② 每用户稳定头(`X-User-Id` 解自 JWT `sub`、`X-Device-Id` 持久化稳定 UUID)、
//!    ③ 每请求随机 UUID(`X-Conversation-*` / `X-Request-Id`)。逆向自 `codebuddy.js`
//!    的 chat 请求 header builder(`ef[eS.AGENT_INTENT]=meta["codebuddy.ai/mode"]??"craft"`
//!    + `IDE_TYPE/IDE_NAME/IDE_VERSION` + OpenAI SDK `AG="6.25.0"`)。
//!
//! 2. **账号登录 OAuth**(external-link 轮询式;[`run_workbuddy_login`] 等)—— 复用
//!    桌面端的 `/v2/plugin/auth/{state,token,token/refresh}` 流程,免手动粘 token。
//!
//! 设计参照 `zai` 模块的 [`super::zai::constants::zcode_source_headers`](GLM/ZCode
//! 指纹伪装)—— 同样"代码层独占注入 + 入站同名头去重"模式。

use std::sync::OnceLock;

pub mod login;
pub mod pool;
pub mod token;

pub use login::{
    ensure_valid_workbuddy_token, refresh_workbuddy_token, run_workbuddy_login, WorkbuddyError,
};
pub use pool::{add_account, select_serving_account, PoolAccount, ServingAccount};
pub use token::{WorkbuddyCredential, WorkbuddyCredentialStore, WorkbuddyTokenError};

/// WorkBuddy 模型网关 host —— `injects_workbuddy_source_headers` 据此判定是否注入
/// coding 指纹。staging / 正式 / codebuddy.cn 均含 `tencent.com`/`codebuddy`,这里
/// 取最稳的正式 host 子串。
pub const WORKBUDDY_HOST: &str = "copilot.tencent.com";

/// app 版本 —— `X-Product-Version` / `X-IDE-Version` 的来源 app 实际版本(5.1.7)。
/// 随 WorkBuddy 升级会变,但服务端不强校验具体值,取一个真实发布过的版本即可。
pub const WORKBUDDY_PRODUCT_VERSION: &str = "5.1.7";

/// `X-Product` —— cli/product.json `deploymentType: "SaaS"`。
pub const WORKBUDDY_PRODUCT: &str = "SaaS";

/// `X-Agent-Intent` —— 固定 coding 模式(用户显式要求伪装成代码开发,Codex 本就是
/// 编码 agent,coding 最贴合)。真实客户端取自会话 meta `codebuddy.ai/mode`,缺省 craft。
pub const WORKBUDDY_AGENT_INTENT: &str = "coding";

/// OpenAI SDK 版本 —— 模型请求由 bundle 内 OpenAI client 发出,`User-Agent` =
/// `OpenAI/JS <AG>`、`X-Stainless-Package-Version` = `<AG>`。逆向自 `let AG="6.25.0"`。
pub const WORKBUDDY_OPENAI_SDK_VERSION: &str = "6.25.0";

/// `X-Stainless-Runtime-Version` —— 真实客户端是 bundle node 的 `process.version`。
/// 取一个真实 LTS 值;服务端不会逐版本校验。
pub const WORKBUDDY_NODE_VERSION: &str = "v20.18.1";

/// `User-Agent` 字面值 `OpenAI/JS 6.25.0`(OpenAI SDK `getUserAgent()` =
/// `${constructor.name}/JS ${AG}`,base client 类名 `OpenAI`)。
pub fn workbuddy_user_agent() -> String {
    format!("OpenAI/JS {WORKBUDDY_OPENAI_SDK_VERSION}")
}

/// `X-Stainless-OS` —— OpenAI SDK `normalizePlatform(process.platform)`:
/// darwin→`MacOS` / linux→`Linux` / win32→`Windows`。运行时检测(伪装跟随本机平台,
/// 跨平台用户不应硬钉 MacOS)。
fn stainless_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "MacOS",
        "linux" => "Linux",
        "windows" => "Windows",
        _ => "Unknown",
    }
}

/// `X-Stainless-Arch` —— OpenAI SDK `normalizeArch(process.arch)`:
/// arm64→`arm64` / x64→`x64` / ia32→`x32`。
fn stainless_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "x32",
        other => other,
    }
}

/// 生成一个 RFC 4122 v4 UUID(小写带连字符),用 `getrandom` 取 16 字节随机。
/// 用于每请求的 `X-Conversation-*` / `X-Request-Id`(真实客户端每条消息都换新)。
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    // getrandom 失败极罕见(OS 熵源不可用);退化用全 0 也不致命(只是指纹弱一点),
    // 但这里 propagate 不了错,失败就用零字节——服务端只校验 UUID 形态不校验唯一性。
    let _ = getrandom::getrandom(&mut b);
    // version 4 + variant 10xx
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

/// 稳定的 `X-Device-Id` —— 真实客户端一台机器一个稳定设备 id。首次生成一个 v4 UUID
/// 持久化到 `~/.codex-app-transfer/workbuddy-device-id`,之后复用。读写失败(无 HOME /
/// IO 错)退化成进程内稳定值(仍比每请求换更像真实客户端)。
pub fn workbuddy_device_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let path = codex_app_transfer_registry::paths::resolve_home()
                .map(|h| h.join(".codex-app-transfer").join("workbuddy-device-id"));
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

/// 进程内稳定的"会话 id"(`X-Conversation-ID`)—— 真实客户端一个对话复用同一 id。
/// Codex 的会话与 WorkBuddy 会话无法精确对齐,取进程级稳定值即可(比每请求换更真实)。
fn conversation_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(uuid_v4).clone()
}

/// 解 Bearer access token(Keycloak JWT)的 payload 成 JSON。**不验签**(只读 claim)。
fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 从 access token 的 `sub` claim 解 `X-User-Id`(= 账号唯一 uid)。解析失败返 None。
pub fn user_id_from_jwt(token: &str) -> Option<String> {
    jwt_payload(token)?
        .get("sub")?
        .as_str()
        .map(|s| s.to_string())
}

/// 中国手机号脱敏 `183****5600`(11 位保留前 3 后 4);其它长度原样。
fn mask_phone(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
    if chars.len() == 11 && chars.iter().all(|c| c.is_ascii_digit()) {
        format!(
            "{}****{}",
            chars[..3].iter().collect::<String>(),
            chars[7..].iter().collect::<String>()
        )
    } else {
        p.to_string()
    }
}

/// 账号**人类可读标签**(UI 显示用,替代不可读的 uid)。默认显示脱敏手机号
/// (`preferred_username`,WorkBuddy 登录身份、各账号唯一),不显示昵称。
/// 取不到手机号 → None(caller 退回短 uid)。
pub fn account_display_from_jwt(token: &str) -> Option<String> {
    jwt_payload(token)?
        .get("preferred_username")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(mask_phone)
}

/// 完整 coding 模式 wire 指纹头集合(① 固定身份 + ③ 每请求 UUID)。
///
/// `model_id` 填 `X-Model-ID`(本次请求实际模型)。② 每用户稳定头(`X-User-Id` /
/// `X-Device-Id`)在 call site 另加(需要 token / device-id)。`Authorization` 由
/// `inject_auth` 注。返回 `(name, value)` 列表,call site reqwest 直接塞。
///
/// **每次调用都重新生成** `X-Conversation-Request-ID` / `X-Conversation-Message-ID` /
/// `X-Request-Id`(后两者真实客户端同值 = messageId),`X-Conversation-ID` 进程级稳定。
pub fn workbuddy_source_headers(model_id: &str) -> Vec<(&'static str, String)> {
    let message_id = uuid_v4();
    vec![
        // —— OpenAI SDK 自带身份 ——
        ("User-Agent", workbuddy_user_agent()),
        ("X-Stainless-Lang", "js".to_string()),
        (
            "X-Stainless-Package-Version",
            WORKBUDDY_OPENAI_SDK_VERSION.to_string(),
        ),
        ("X-Stainless-OS", stainless_os().to_string()),
        ("X-Stainless-Arch", stainless_arch().to_string()),
        ("X-Stainless-Runtime", "node".to_string()),
        (
            "X-Stainless-Runtime-Version",
            WORKBUDDY_NODE_VERSION.to_string(),
        ),
        // —— CodeBuddy 客户端身份(模式 / 产品 / IDE)——
        ("X-Agent-Intent", WORKBUDDY_AGENT_INTENT.to_string()),
        ("X-Product", WORKBUDDY_PRODUCT.to_string()),
        ("X-Product-Version", WORKBUDDY_PRODUCT_VERSION.to_string()),
        ("X-IDE-Type", "CLI".to_string()),
        ("X-IDE-Name", "IDE".to_string()),
        ("X-IDE-Version", "0.0.0".to_string()),
        ("X-Model-ID", model_id.to_string()),
        // —— 会话 / 请求标识 ——
        ("X-Conversation-ID", conversation_id()),
        ("X-Conversation-Request-ID", uuid_v4()),
        ("X-Conversation-Message-ID", message_id.clone()),
        ("X-Request-Id", message_id),
    ]
}

/// `workbuddy_source_headers` 注入、需在注入路径上**独占**的指纹头名集合(小写)。
/// 入站 Codex 同名头先 strip,避免 reqwest `header()` append 成双值。`User-Agent`
/// 已由 `is_strip_on_forward` 全局 strip,不在此列。
pub fn is_workbuddy_owned_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "x-stainless-lang"
            | "x-stainless-package-version"
            | "x-stainless-os"
            | "x-stainless-arch"
            | "x-stainless-runtime"
            | "x-stainless-runtime-version"
            | "x-agent-intent"
            | "x-product"
            | "x-product-version"
            | "x-ide-type"
            | "x-ide-name"
            | "x-ide-version"
            | "x-model-id"
            | "x-user-id"
            | "x-device-id"
            | "x-conversation-id"
            | "x-conversation-request-id"
            | "x-conversation-message-id"
            | "x-request-id"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_has_correct_shape_and_version() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36, "UUID 长度 36:{id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble 应为 4:{id}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "variant nibble 应为 8/9/a/b:{id}"
        );
        // 两次调用不同(随机性 sanity)
        assert_ne!(uuid_v4(), uuid_v4());
    }

    #[test]
    fn source_headers_carry_coding_fingerprint() {
        let h = workbuddy_source_headers("deepseek-v4-pro");
        let get = |k: &str| {
            h.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("X-Agent-Intent"), Some("coding"), "必须伪装成 coding");
        assert_eq!(get("X-Product"), Some("SaaS"));
        assert_eq!(get("X-Product-Version"), Some("5.1.7"));
        assert_eq!(get("X-IDE-Type"), Some("CLI"));
        assert_eq!(get("User-Agent"), Some("OpenAI/JS 6.25.0"));
        assert_eq!(get("X-Stainless-Package-Version"), Some("6.25.0"));
        assert_eq!(get("X-Model-ID"), Some("deepseek-v4-pro"));
        // X-Request-Id 与 X-Conversation-Message-ID 同值(对齐真实客户端 = messageId)
        assert_eq!(
            get("X-Request-Id"),
            get("X-Conversation-Message-ID"),
            "request-id 应与 message-id 同值"
        );
    }

    #[test]
    fn source_headers_message_id_rotates_per_call() {
        let a = workbuddy_source_headers("m");
        let b = workbuddy_source_headers("m");
        let msg = |h: &[(&'static str, String)]| {
            h.iter()
                .find(|(n, _)| *n == "X-Conversation-Message-ID")
                .unwrap()
                .1
                .clone()
        };
        assert_ne!(msg(&a), msg(&b), "每请求 message-id 必须不同");
        // 但 conversation-id 进程内稳定
        let conv = |h: &[(&'static str, String)]| {
            h.iter()
                .find(|(n, _)| *n == "X-Conversation-ID")
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(conv(&a), conv(&b), "conversation-id 进程内应稳定");
    }

    #[test]
    fn owned_header_set_covers_all_injected() {
        // 注入的每个头都必须在 owned 集合里(否则入站 Codex 同名头不被 strip → 双值)。
        // User-Agent 例外(全局 strip)。
        for (name, _) in workbuddy_source_headers("m") {
            if name.eq_ignore_ascii_case("user-agent") {
                continue;
            }
            assert!(
                is_workbuddy_owned_header(name),
                "注入头 {name} 必须在 is_workbuddy_owned_header 集合里"
            );
        }
    }

    #[test]
    fn user_id_from_jwt_reads_sub_claim() {
        // header.payload.sig,payload = {"sub":"u-123","exp":1}
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"sub":"u-123","exp":1}"#,
        );
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.sig");
        assert_eq!(user_id_from_jwt(&token), Some("u-123".to_string()));
        // 非 JWT / 缺 sub → None
        assert_eq!(user_id_from_jwt("not-a-jwt"), None);
    }
}
