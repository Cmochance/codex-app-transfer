//! Trae(字节跳动 TRAE SOLO CN / TRAE Work CN 桌面 IDE)账号登录的 wire 常量。
//!
//! 逆向自本地解包 `TRAE SOLO CN.app`(VSCode fork)`out/main.js` 的
//! `vs/code/electron-main/oauth/` + 实测抓包印证(2026-06-22)。设计文档见
//! `archives/codex-app-transfer/2026-06-22-trae-integration-analysis.md`。
//!
//! ## 与 zai/bigmodel 的差异
//!
//! - **登录是标准 loopback OAuth2 + PKCE(S256)**(zai 是自定义 JSON 信封 + state)。
//! - **token 直接可用**:换出来的 `Token`(JWT)直接做 `x-icube-token` /
//!   `Authorization: Cloud-IDE-JWT`,**无**「换组织 key」那一步。
//! - **有 refresh**:`RefreshToken` + 设备私钥签名(`DeviceProof`)续期;zai 无 refresh。
//! - **每登录一套设备指纹**:EC P-256 keypair + DeviceID + MachineID,首登上传公钥、
//!   服务端绑定,refresh 用私钥验签 —— 天然实现「同设备多账号指纹隔离」。
//!
//! ## edition
//!
//! 当前只实现 **CN(SOLO CN 身份)**:这是完整逆向 + 抓包印证的那套,首次真机
//! 成功率最高。国际版(`api.trae.ai`)的 client_id / host 尚未逆向,留作 fast-follow
//! ([`TraeEdition`] 已预留扩展点)。

use serde::{Deserialize, Serialize};

/// Trae 账号体系版本(国内 vs 国际)。当前只实现 CN。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraeEdition {
    /// 国内版(`www.trae.cn` 登录 + `api.trae.cn` 业务面,SOLO CN 身份)。
    #[serde(rename = "cn")]
    Cn,
}

impl TraeEdition {
    /// authScheme 字面值(preset / resolver 用)。
    pub fn auth_scheme(self) -> &'static str {
        match self {
            TraeEdition::Cn => "trae_oauth",
        }
    }

    /// 完整 wire 配置。
    pub fn config(self) -> TraeProviderConfig {
        match self {
            TraeEdition::Cn => TRAE_CN_CONFIG,
        }
    }

    /// 从 authScheme 字符串解析(resolver 用)。
    pub fn from_auth_scheme(s: &str) -> Option<Self> {
        match s {
            "trae_oauth" | "trae" | "trae_cn_oauth" => Some(TraeEdition::Cn),
            _ => None,
        }
    }
}

/// 单 edition 的 OAuth + 业务面 endpoint / 身份常量集合。
///
/// host 不带末尾 `/`;path 常量(authorize / exchange / userinfo / quota)在下方
/// 单独定义,跨 edition 共用。
#[derive(Debug, Clone, Copy)]
pub struct TraeProviderConfig {
    pub edition: TraeEdition,
    /// 浏览器授权页 host(`bootConfig.consoleHost`)。
    pub console_host: &'static str,
    /// 账号 / token / 额度业务面 host(`bootConfig.ug.trae.normal`)。
    pub api_host: &'static str,
    /// authorize `client_id`(product.json `iCubeApp.authConfig`)。
    pub client_id: &'static str,
    /// authorize `auth_from`(SOLO=`solo`,IDE=`trae`)。
    pub auth_from: &'static str,
    /// DeviceInfo.PlatformCode(SOLO=`SOLO_PC`,IDE=`IDE_PC`)。
    pub platform_code: &'static str,
    /// authorize `x_app_type` / 渠道(stable / beta / ...)。
    pub app_channel: &'static str,
    /// SOLO 身份追加 `hide_saas_login=true`(隐藏企业 SaaS 登录,走个人账号)。
    pub hide_saas_login: bool,
    /// `IDEVersion` / `ClientVersion`(对齐真实 app 版本,过弱校验)。
    pub ide_version: &'static str,
}

/// CN edition —— SOLO CN 身份(完整逆向 + 抓包印证的那套)。
pub const TRAE_CN_CONFIG: TraeProviderConfig = TraeProviderConfig {
    edition: TraeEdition::Cn,
    console_host: "https://www.trae.cn",
    api_host: "https://api.trae.cn",
    client_id: "en1oxy7wnw8j9n",
    auth_from: "solo",
    platform_code: "SOLO_PC",
    app_channel: "stable",
    hide_saas_login: true,
    ide_version: "0.1.21",
};

/// authorize 页 path(`consoleHost` 下)。
pub const AUTHORIZE_PATH: &str = "/authorization";
/// token 交换 / refresh path(`api_host` 下,签名覆盖的 path 也是它)。
pub const EXCHANGE_TOKEN_PATH: &str = "/trae/api/v3/oauth/ExchangeToken";
/// 拉用户信息 path(`api_host` 下,header `x-icube-token`)。
pub const USERINFO_PATH: &str = "/cloudide/api/v3/trae/GetUserInfo";
/// 额度 path(`api_host` 下,header `Authorization: Cloud-IDE-JWT`)。
pub const QUOTA_PATH: &str = "/trae/api/v2/pay/ide_user_ent_usage";

/// loopback 回调 path(本地 server 上,对齐 Trae `OAuthLocalServer` 的 `/authorize`)。
pub const CALLBACK_PATH: &str = "/authorize";

/// authorize 固定参数。
pub const LOGIN_VERSION: &str = "1";
pub const LOGIN_CHANNEL: &str = "native_ide";
pub const AUTH_TYPE: &str = "local";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cn_config_matches_reversed_constants() {
        let c = TraeEdition::Cn.config();
        assert_eq!(c.console_host, "https://www.trae.cn");
        assert_eq!(c.api_host, "https://api.trae.cn");
        assert_eq!(c.client_id, "en1oxy7wnw8j9n");
        assert_eq!(c.auth_from, "solo");
        assert_eq!(c.platform_code, "SOLO_PC");
        assert!(c.hide_saas_login);
    }

    #[test]
    fn auth_scheme_roundtrip() {
        assert_eq!(TraeEdition::Cn.auth_scheme(), "trae_oauth");
        assert_eq!(
            TraeEdition::from_auth_scheme("trae_oauth"),
            Some(TraeEdition::Cn)
        );
        assert_eq!(TraeEdition::from_auth_scheme("trae"), Some(TraeEdition::Cn));
        assert_eq!(TraeEdition::from_auth_scheme("bogus"), None);
    }
}
