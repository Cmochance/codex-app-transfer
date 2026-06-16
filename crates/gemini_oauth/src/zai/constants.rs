//! z.ai / bigmodel(GLM **Coding Plan** 订阅)OAuth provider 的 wire 常量。
//!
//! 借鉴 ZCode 3.1.0(智谱多 CLI agent GUI)解包源
//! `~/zcode-unpack-310/out/host/index.js`,但走本项目现成的 loopback OAuth 机制
//! 重写(ZCode 用 `zcode://` deeplink,我们实测改 `http://localhost:<port>`
//! loopback,两套账号 authorize 回跳 + token 交换都被接受)。
//!
//! ## 两套账号体系
//!
//! | 维度 | z.ai(国际) | bigmodel(国内智谱) | ZCode 源 |
//! |---|---|---|---|
//! | provider id(token body) | `zai` | `bigmodel` | `_m.id` / `Vc.id=Ve` |
//! | authorize URL | `chat.z.ai/api/oauth/authorize` | `bigmodel.cn/login`(网页登录页) | `_m`/`Vc.authorizeUrl` |
//! | authorize 参数样式 | `redirect_uri`+`response_type=code`+`client_id`+`state` | `redirect`+`appId`+`state` | 两个 adapter 的 `buildAuthorizeUrl` |
//! | appId | `client_P8X5CMWmlaRO9gyO-KSqtg` | `zcode` | `_m`/`Vc.appId` |
//! | token URL | `zcode.z.ai/api/v1/oauth/token`(共用) | 同 | `_m`/`Vc.tokenUrl` |
//! | biz base(换组织 key) | `https://api.z.ai`(`Ch`) | `https://zcode.z.ai`(`wh`) | `resolveZaiApiKey`/`resolveBizApiKey` |
//! | 模型 base(Anthropic wire) | `api.z.ai/api/anthropic` | `open.bigmodel.cn/api/anthropic`(`$m`) | catalog |
//! | 业务 token 中转 | 需要(`km` ZaiBusinessTokenResolver) | 不需要 | `businessLoginUrl` |
//! | 换 key requireSecretKey | `true` | `false` | `resolveZaiApiKey`/`resolveBizApiKey` |
//!
//! **无 refresh flow** —— ZCode adapter 也未实现 refreshToken,401/403 即重登
//! (`~/.codex-app-transfer/{zai,bigmodel}-oauth.json` 删掉重新走 OAuth)。

use serde::{Deserialize, Serialize};

/// z.ai vs bigmodel —— 同一套 ZCode 后端、不同账号体系 / authorize 样式 / biz base。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZaiProvider {
    /// Z.ai 国际版(`chat.z.ai` 登录 + `api.z.ai` 业务面)。
    #[serde(rename = "zai")]
    Zai,
    /// 智谱 BigModel 国内版(`bigmodel.cn` 网页登录 + `zcode.z.ai` 业务面)。
    #[serde(rename = "bigmodel")]
    BigModel,
}

impl ZaiProvider {
    /// token 交换 POST body 的 `provider` 字段字面值(`_m.id` / `Ve`)。
    pub fn wire_id(self) -> &'static str {
        match self {
            ZaiProvider::Zai => "zai",
            ZaiProvider::BigModel => "bigmodel",
        }
    }

    /// 持久化文件名 —— **每 provider 独立一个文件**(镜像 antigravity 的
    /// `antigravity-oauth.json` 单 provider 单文件),让 forward.rs 按 provider
    /// 直接定位,不用 map 结构。
    pub fn token_filename(self) -> &'static str {
        match self {
            ZaiProvider::Zai => "zai-oauth.json",
            ZaiProvider::BigModel => "bigmodel-oauth.json",
        }
    }

    /// 完整 wire 配置。
    pub fn config(self) -> ZaiProviderConfig {
        match self {
            ZaiProvider::Zai => ZAI_CONFIG,
            ZaiProvider::BigModel => BIGMODEL_CONFIG,
        }
    }
}

/// 单 provider 的 OAuth + 业务面 endpoint 集合(全静态,无 PKCE / client_secret)。
#[derive(Debug, Clone, Copy)]
pub struct ZaiProviderConfig {
    pub provider: ZaiProvider,
    /// 用户浏览器跳转的授权页。
    pub authorize_url: &'static str,
    /// code → zcodeJWT + provider access_token 的交换端点(两套共用)。
    pub token_url: &'static str,
    /// authorize 的 `client_id`(z.ai) / `appId`(bigmodel)值。
    pub app_id: &'static str,
    /// 换组织 API key 的业务面 base(`getCustomerInfo` / `api_keys` 挂这上面)。
    pub biz_base: &'static str,
    /// 实际打模型的 Anthropic Messages wire base(`/v1/messages` 挂这下面)。
    pub model_base: &'static str,
    /// z.ai 专属:oauth access_token 换业务 token 的端点(bigmodel 为 `None`,
    /// 直接拿 oauth access_token 当 biz Bearer)。
    pub business_login_url: Option<&'static str>,
    /// 换 key 时是否必须拿到 `secretKey`(z.ai=true;bigmodel 可只用 apiKey)。
    pub require_secret_key: bool,
}

/// z.ai 国际版配置(ZCode `_m`)。
pub const ZAI_CONFIG: ZaiProviderConfig = ZaiProviderConfig {
    provider: ZaiProvider::Zai,
    authorize_url: "https://chat.z.ai/api/oauth/authorize",
    token_url: "https://zcode.z.ai/api/v1/oauth/token",
    app_id: "client_P8X5CMWmlaRO9gyO-KSqtg",
    biz_base: "https://api.z.ai",
    model_base: "https://api.z.ai/api/anthropic",
    business_login_url: Some("https://api.z.ai/api/auth/z/login"),
    require_secret_key: true,
};

/// 智谱 BigModel 国内版配置(ZCode `Vc`)。**biz base 走 ZCode 后端
/// `zcode.z.ai`**(不是模型面 `open.bigmodel.cn`)—— ZCode 后端代为经手组织 key。
pub const BIGMODEL_CONFIG: ZaiProviderConfig = ZaiProviderConfig {
    provider: ZaiProvider::BigModel,
    authorize_url: "https://bigmodel.cn/login",
    token_url: "https://zcode.z.ai/api/v1/oauth/token",
    app_id: "zcode",
    biz_base: "https://zcode.z.ai",
    model_base: "https://open.bigmodel.cn/api/anthropic",
    business_login_url: None,
    require_secret_key: false,
};

/// 换组织 key 时,api_keys 列表里查找 / 新建的 key name(ZCode `RI`)。
pub const ZCODE_API_KEY_NAME: &str = "zcode-api-key";

/// pickOrgAndProject 偏好的机构名子串(ZCode `fN`="默认机构");匹配不到回退第一个。
pub const DEFAULT_ORG_NAME_HINT: &str = "默认机构";
/// pickOrgAndProject 偏好的项目名子串(ZCode `gN`="默认项目");匹配不到回退第一个。
pub const DEFAULT_PROJECT_NAME_HINT: &str = "默认项目";

/// ZCode 应用版本 —— 出现在出站 `User-Agent: ZCode/<ver>` + `X-ZCode-App-Version`。
/// 解包自 ZCode 3.1.0(`~/zcode-unpack-310`)。
pub const ZCODE_VERSION: &str = "3.1.0";

/// Anthropic Messages wire 版本头(ZCode `buildAnthropicHeaders` 写死)。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// ZCode 指纹头里的固定 referer(`Or["HTTP-Referer"]`)。
pub const ZCODE_REFERER: &str = "https://zcode.z.ai";
/// ZCode 指纹头里的固定 title(`Or["X-Title"]`)。
pub const ZCODE_TITLE: &str = "Z Code@electron";
/// 连通性探测 / coding-plan provider 标识(`buildConnectivitySourceHeaders` 加
/// `X-ZCode-Agent: glm`)。
pub const ZCODE_AGENT: &str = "glm";

/// 出站 `User-Agent`(`ZCode/<ver>`,ZCode `Or` + `buildZCodeSourceHeaders`)。
pub fn zcode_user_agent() -> String {
    format!("ZCode/{ZCODE_VERSION}")
}

/// `X-Platform` 值,形如 `darwin-arm64` / `win32-x64`。**忠于 ZCode 源**:
/// `buildZCodeSourceHeaders`(`Ol`)写 `` `${process.platform}-${process.arch}` `` ——
/// 用的是 **Node 原始** `process.platform`(`darwin`/`linux`/`win32`)+
/// `process.arch`(`arm64`/`x64`/`ia32`),不是 Rust 的 `aarch64`/`x86_64`/`windows`。
/// 运行时检测,不 hardcode(跨平台用户上传错平台会污染上游 telemetry)。
pub fn zcode_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        other => other,
    };
    format!("{os}-{arch}")
}

/// ZCode 来源指纹头集合(`Or` 基础三件 + UA/platform/app-version)。biz 面调用与
/// 模型调用都带这套身份;`Authorization` / `Content-Type` / `anthropic-version`
/// 由各 call site 另加。返回 `(name, value)` 列表,call site reqwest 直接塞 header。
pub fn zcode_source_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", zcode_user_agent()),
        ("X-ZCode-App-Version", ZCODE_VERSION.to_string()),
        ("X-Platform", zcode_platform()),
        ("HTTP-Referer", ZCODE_REFERER.to_string()),
        ("X-Title", ZCODE_TITLE.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_wire_ids_match_zcode_source() {
        assert_eq!(ZaiProvider::Zai.wire_id(), "zai");
        assert_eq!(ZaiProvider::BigModel.wire_id(), "bigmodel");
    }

    #[test]
    fn provider_token_filenames_are_distinct_per_provider() {
        assert_eq!(ZaiProvider::Zai.token_filename(), "zai-oauth.json");
        assert_eq!(
            ZaiProvider::BigModel.token_filename(),
            "bigmodel-oauth.json"
        );
        assert_ne!(
            ZaiProvider::Zai.token_filename(),
            ZaiProvider::BigModel.token_filename()
        );
    }

    #[test]
    fn provider_serde_roundtrip_uses_wire_names() {
        // 持久化用 wire 名("zai"/"bigmodel"),不能漂成 "big_model"
        let json = serde_json::to_string(&ZaiProvider::BigModel).unwrap();
        assert_eq!(json, "\"bigmodel\"");
        let back: ZaiProvider = serde_json::from_str("\"zai\"").unwrap();
        assert_eq!(back, ZaiProvider::Zai);
    }

    #[test]
    fn wire_id_stays_in_sync_with_serde_name() {
        // wire_id()(手写 match)与 serde rename 是两套独立维护的「同一字符串」,
        // 钉在一起防未来漂移(type-design review 建议)。
        for p in [ZaiProvider::Zai, ZaiProvider::BigModel] {
            let serde_name = serde_json::to_string(&p).unwrap();
            assert_eq!(serde_name.trim_matches('"'), p.wire_id());
        }
    }

    #[test]
    fn zai_config_pins_business_token_and_secret_key() {
        let c = ZaiProvider::Zai.config();
        assert_eq!(c.biz_base, "https://api.z.ai");
        assert!(c.business_login_url.is_some(), "z.ai 需要业务 token 中转");
        assert!(c.require_secret_key, "z.ai 换 key 必须拿 secretKey");
        assert_eq!(c.model_base, "https://api.z.ai/api/anthropic");
    }

    #[test]
    fn bigmodel_config_uses_zcode_backend_biz_base() {
        let c = ZaiProvider::BigModel.config();
        // biz base 是 ZCode 后端,**不是**模型面 open.bigmodel.cn
        assert_eq!(c.biz_base, "https://zcode.z.ai");
        assert!(
            c.business_login_url.is_none(),
            "bigmodel 直接用 oauth token 当 biz Bearer"
        );
        assert!(!c.require_secret_key);
        assert_eq!(c.model_base, "https://open.bigmodel.cn/api/anthropic");
    }

    #[test]
    fn source_headers_include_zcode_identity() {
        let headers = zcode_source_headers();
        let ua = headers.iter().find(|(k, _)| *k == "User-Agent").unwrap();
        assert_eq!(ua.1, "ZCode/3.1.0");
        assert!(headers.iter().any(|(k, _)| *k == "HTTP-Referer"));
        assert!(headers.iter().any(|(k, _)| *k == "X-Title"));
        // X-Platform 形如 <os>-<arch>,含连字符
        let plat = headers.iter().find(|(k, _)| *k == "X-Platform").unwrap();
        assert!(
            plat.1.contains('-'),
            "X-Platform 应为 <os>-<arch>: {}",
            plat.1
        );
    }
}
