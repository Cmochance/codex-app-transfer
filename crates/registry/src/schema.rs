//! 类型化 schema —— 与 backend/config.py 中 DEFAULT_CONFIG 一一对应.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const APP_VERSION: &str = "1.0.4";
pub const DEFAULT_UPDATE_URL: &str =
    "https://github.com/Cmochance/codex-app-transfer/releases/latest/download/latest.json";

pub const DEFAULT_THEME: &str = "default";
pub const DEFAULT_LANGUAGE: &str = "zh";
pub const DEFAULT_PROXY_PORT: u16 = 18080;
pub const DEFAULT_ADMIN_PORT: u16 = 18081;

/// Provider 缺省 `authScheme`：旧版 / 手编 config 不写时按主流 OpenAI 兼容
/// 上游回退为 `bearer`，避免反序列化失败。
fn default_auth_scheme() -> String {
    "bearer".to_owned()
}

/// Provider 缺省 `apiFormat`：与现存 5 家用户 + 7 家内置预设保持一致。
fn default_api_format() -> String {
    "openai_chat".to_owned()
}

/// 顶层配置文件结构(对应 `~/.codex-app-transfer/config.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub version: String,
    pub active_provider: Option<String>,
    pub gateway_api_key: Option<String>,
    #[serde(default)]
    pub providers: Vec<Provider>,
    pub settings: Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: APP_VERSION.to_owned(),
            active_provider: None,
            gateway_api_key: None,
            providers: Vec::new(),
            settings: Settings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub theme: String,
    pub language: String,
    pub proxy_port: u16,
    pub admin_port: u16,
    pub auto_start: bool,
    pub auto_apply_on_start: bool,
    pub expose_all_provider_models: bool,
    pub restore_codex_on_exit: bool,
    pub update_url: String,
    /// 是否允许 Codex shell 工具(`curl` / 网络命令)进行网络访问。
    ///
    /// 写入 `~/.codex/config.toml` 的 `sandbox_workspace_write.network_access`,
    /// 控制 `sandbox_mode = workspace-write`(Codex 默认)下 shell 能否联网。
    ///
    /// **默认 `false`**(MOC-185):full access(danger-full-access + approval=never)
    /// 等于完全信任模型、有风险,缺省关;需要联网 / 无审批的用户在 UI 自行开启。
    /// 关闭**不影响** Codex 内置 `web_search` 工具(走 OpenAI 缓存,不需要
    /// `network_access`)。已显式设过此 bool 的老 config 照旧解析、不被覆盖
    /// (serde 反序列化仅在字段缺失时套默认)。
    #[serde(default = "default_codex_network_access")]
    pub codex_network_access: bool,

    /// 内置联网抓取工具的后端档位 (MOC-144): `off`(不暴露抓取工具) / `curl`(reqwest
    /// 静态 GET) / `wreq`(浏览器 TLS 指纹, 绕 Cloudflare JS 挑战) / `headless`(headless
    /// Chromium 跑 JS, 取渲染后 DOM)。**独立于** [`Self::codex_network_access`](后者管
    /// Codex 沙箱 shell 的联网权限, 是两套机制)。**默认 `auto`**(MOC-215:从 `off` 改,
    /// 让内置联网工具开箱可用;web_search 仍受 chrome_ready gate 保护、不静默下载)。
    #[serde(default = "default_web_fetch_backend")]
    pub web_fetch_backend: String,

    /// 「诊断模式」开关(MOC-169/MOC-185):开启后启动独立端口诊断流量查看器
    /// (默认 `127.0.0.1:18090`)并采集 forward-trace / MCP 流量。**默认 `false`** —— 纯
    /// 开发者诊断,普通用户零影响、仅本地 loopback;正文按结构化 credential 脱敏但
    /// prompt/代码/回复完整落盘,故默认关。**MOC-185 起改为 session 级一次性**:UI 开关
    /// 纯运行时起/停、退出 transfer 即关,**不再持久化也不随启动自启**,故本字段已不被
    /// 写入 / 读取(保留仅为兼容解析遗留 config);开发者长期采集走 env `CAS_DIAG_TRACE`。
    #[serde(default)]
    pub trace_viewer_enabled: bool,
}

fn default_codex_network_access() -> bool {
    false
}

fn default_web_fetch_backend() -> String {
    // MOC-215: 默认从 `off` 改 `auto` —— 让内置联网抓取/搜索工具(cat-webfetch:web_fetch +
    // web_search)开箱可用,否则新用户默认无 web_search(Codex 内置 web_search 被 adapter drop
    // 又无 cat-webfetch 兜底 = 彻底没搜索)。`auto` 安全:web_fetch 用 curl/wreq 不需 Chrome;
    // web_search 仍受 mcp_webfetch_server 的 chrome_ready gate 保护,Chrome 未就绪时不暴露、
    // **不会静默下载 ~86MB**。用户可在设置里改回 off。
    "auto".to_owned()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            language: DEFAULT_LANGUAGE.to_owned(),
            proxy_port: DEFAULT_PROXY_PORT,
            admin_port: DEFAULT_ADMIN_PORT,
            auto_start: false,
            auto_apply_on_start: true,
            expose_all_provider_models: false,
            restore_codex_on_exit: true,
            update_url: DEFAULT_UPDATE_URL.to_owned(),
            codex_network_access: false,
            web_fetch_backend: default_web_fetch_backend(),
            trace_viewer_enabled: false,
        }
    }
}

/// Provider 记录 —— 字段集是已知必备 + 可选,未知字段挂在 `extra` 里
/// 透传(典型如内置预设的 `notices` / `baseUrlOptions` / `requestOptionPresets`
/// / `baseUrlHint` 等只在部分 provider 出现的字段).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default = "default_auth_scheme")]
    pub auth_scheme: String,
    #[serde(default = "default_api_format")]
    pub api_format: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: ModelMappings,
    #[serde(default)]
    pub extra_headers: IndexMap<String, String>,
    #[serde(default)]
    pub model_capabilities: IndexMap<String, Value>,
    #[serde(default)]
    pub request_options: IndexMap<String, Value>,
    #[serde(default)]
    pub is_builtin: bool,
    #[serde(default)]
    pub sort_index: i64,
    /// 透传任何此结构未显式枚举的字段(notices / baseUrlOptions /
    /// requestOptionPresets / baseUrlHint / docsUrl / `summaryModel`(MOC-152
    /// web_fetch 网页摘要模型, 空→`models["default"]`)/ ...).
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

/// 模型槽位映射 —— 与 `backend/model_alias.py` MODEL_SLOTS 顺序保持一致.
///
/// 用 `IndexMap` 保留磁盘顺序;键值由 `model_alias::MODEL_ORDER` 提供.
pub type ModelMappings = IndexMap<String, String>;

/// 用枚举形式记录槽位 key,便于业务代码以编译期保证引用.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelSlotKey {
    Default,
    Gpt55,
    Gpt54,
    Gpt54Mini,
    Gpt53Codex,
    Gpt52,
}

impl ModelSlotKey {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelSlotKey::Default => "default",
            ModelSlotKey::Gpt55 => "gpt_5_5",
            ModelSlotKey::Gpt54 => "gpt_5_4",
            ModelSlotKey::Gpt54Mini => "gpt_5_4_mini",
            ModelSlotKey::Gpt53Codex => "gpt_5_3_codex",
            ModelSlotKey::Gpt52 => "gpt_5_2",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_tolerates_missing_auth_scheme_and_api_format() {
        let json = r#"{
            "id": "mock-provider",
            "name": "Mock",
            "baseUrl": "http://127.0.0.1:29090",
            "apiKey": "mock-key",
            "models": { "default": "mock-model" },
            "sortIndex": 0
        }"#;
        let p: Provider = serde_json::from_str(json).expect("旧版 / 手编 config 应能加载");
        assert_eq!(p.auth_scheme, "bearer");
        assert_eq!(p.api_format, "openai_chat");
    }

    #[test]
    fn provider_tolerates_missing_models() {
        let json = r#"{
            "id": "p",
            "name": "P",
            "baseUrl": "http://x",
            "authScheme": "bearer",
            "apiFormat": "openai_chat"
        }"#;
        let p: Provider = serde_json::from_str(json).unwrap();
        assert!(p.models.is_empty());
    }
}
