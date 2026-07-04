//! Provider 解析器:在 forward 之前完成"鉴权 + 路由 + 鉴权改写"三件事.
//!
//! 一次解析的输入是 incoming `Request<Body>` 的 parts 与 body bytes;
//! 输出 `ResolvedProvider` 描述这次请求实际应该送到哪个 provider、用什么
//! Authorization、附加哪些 header.
//!
//! 解耦点:`ProviderResolver` 是 trait,`StaticResolver` 是基于
//! `registry::Config` 的内存实现;Stage 4 接入 UI / 文件监听后,可换成
//! `ConfigWatcher` 持有实时 config 的版本.

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use codex_app_transfer_gemini_oauth::ZaiProvider;
use codex_app_transfer_registry::model_alias::{
    normalize_model_mappings, openai_model_slot, provider_slug, strip_internal_model_suffix,
};
use codex_app_transfer_registry::Provider;
use thiserror::Error;

/// 已解析的"下一跳上游"信息.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider_id: String,
    pub upstream_base: String,
    pub api_key: String,
    pub auth_scheme: AuthScheme,
    pub extra_headers: HeaderMap,
    /// 若请求体里写的是 `"<slug>/<model>"`,这里给出剥掉前缀后的纯模型名.
    /// `None` 表示路由没改 model 字段(让上游按原值处理).
    pub rewritten_model: Option<String>,
    /// 完整的 Provider 记录;adapter 在 prepare_request / transform_response_stream
    /// 阶段需要拿到 api_format / model_capabilities / request_options 等字段.
    pub provider: Arc<Provider>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    Bearer,
    XApiKey,
    /// Google AI Studio Gemini API:`x-goog-api-key: <api_key>` header.
    /// LiteLLM 注释(`common_utils.py:402`):API key 不放 URL,放 header
    /// 防 traceback 泄露。
    GoogleApiKey,
    /// Google Cloud Code Assist OAuth 2.0:`Authorization: Bearer <oauth_access_token>`,
    /// 但 access_token 不在 provider.api_key 里 — 由 `gemini_oauth::TokenStore`
    /// 持久化 + `ensure_valid_access_token` 在请求时 load + auto refresh。
    GoogleOauthCloudCode,
    /// Antigravity OAuth — 跟 GoogleOauthCloudCode 共用 `cloudcode-pa` 上游端点
    /// 但 OAuth 身份不同(client_id / scopes / UA / metadata)+ token 文件独立
    /// (`~/.codex-app-transfer/antigravity-oauth.json`)。`gemini_oauth::antigravity::*`
    /// 处理 flow / refresh / bootstrap;forward.rs 用此 scheme 路由到 antigravity
    /// token store + 注入 antigravity UA / X-Goog-Api-Client。
    GoogleOauthAntigravity,
    /// grok.com Web 后端 cookie 鉴权:`Cookie: sso=<JWT>; sso-rw=<JWT>; cf_clearance=<token>`
    /// + `x-statsig-id` + `x-xai-request-id`(详见 `crates/adapters/src/grok_web/auth.rs`)。
    ///
    /// Cookie 不在 provider.api_key,而在 `provider.extra["grokWeb"]["cookies"]` JSON object。
    GrokCookie,
    /// z.ai / bigmodel(GLM **Coding Plan** 账号登录)。换出的**组织 API key** 不在
    /// `provider.api_key`,由 `gemini_oauth::zai::ZaiCredentialStore` 持久化
    /// (`~/.codex-app-transfer/{zai,bigmodel}-oauth.json`)+ 请求时 load(**无 refresh**,
    /// 过期/未登录 → needs_login)。打 [`ZaiProvider::config().model_base`] 的 Anthropic
    /// Messages wire,forward.rs 注 `Authorization: Bearer <org_key>` + ZCode 指纹头。
    /// 内含 [`ZaiProvider`] 区分 z.ai vs bigmodel(两者 model_base / token 文件不同)。
    ZaiOauth(ZaiProvider),
    /// WorkBuddy(腾讯 CodeBuddy)账号登录。access token 不在 `provider.api_key`,
    /// 由 `gemini_oauth::workbuddy::WorkbuddyCredentialStore` 持久化
    /// (`~/.codex-app-transfer/workbuddy-oauth.json`)+ 请求时 `ensure_valid_workbuddy_token`
    /// load + **自动 refresh**(`X-Refresh-Token` 头)。打 `copilot.tencent.com/v2/chat/completions`
    /// 的 OpenAI Chat 兼容 wire,forward.rs 注 `Authorization: Bearer <access_token>` +
    /// 完整 coding 模式指纹头(与 API-key 路 `workbuddy` preset 对齐)。
    WorkbuddyOauth,
    /// QoderWork CN(阿里 Qoder)legacy 模型通道:鉴权由 WASM(`qoder_auth`)生成的
    /// **Cosy 签名**(`Authorization: Bearer COSY.<sig>` + 一整套 `Cosy-*` 头 + AES-GCM
    /// 加密 body),host 钉死 `gateway.qoder.com.cn`。device token 由
    /// `gemini_oauth::qoder` 多账号池持久化 + 请求时 refresh。签名/加密/整份出站
    /// url+header+body 在 `forward.rs` 的 build_and_send_upstream QoderCosy 分支产出。
    QoderCosy,
    /// 不写鉴权头(上游免认证 / 走 cookie 等少见情况).
    None,
}

impl AuthScheme {
    pub fn parse(s: &str) -> Self {
        // 统一 normalize:trim + lowercase + dash→underscore,所有 alias 不再单独
        // 列 dash 形态(对齐 AdapterRegistry::lookup 同样 normalize)。
        let normalized = s.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "x_api_key" | "xapikey" | "apikey" => AuthScheme::XApiKey,
            "google_api_key" | "x_goog_api_key" | "google" | "gemini" => AuthScheme::GoogleApiKey,
            "google_oauth_cloud_code" | "google_oauth" | "gemini_cli_oauth" | "gemini_oauth" => {
                AuthScheme::GoogleOauthCloudCode
            }
            "google_oauth_antigravity" | "antigravity_oauth" | "antigravity" => {
                AuthScheme::GoogleOauthAntigravity
            }
            "grok_cookie" | "grok" | "grok_web" => AuthScheme::GrokCookie,
            "zai_oauth" | "zai" => AuthScheme::ZaiOauth(ZaiProvider::Zai),
            "bigmodel_oauth" | "bigmodel" => AuthScheme::ZaiOauth(ZaiProvider::BigModel),
            "workbuddy_oauth" | "workbuddy_login" => AuthScheme::WorkbuddyOauth,
            "qoder_oauth" | "qoder" | "qoder_cosy" => AuthScheme::QoderCosy,
            "" | "none" | "no" => AuthScheme::None,
            // bearer 与未知 scheme 都按 Bearer 处理(与 Python 默认一致)
            _ => AuthScheme::Bearer,
        }
    }
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("missing or invalid gateway api key")]
    Unauthorized,
    #[error("no provider matches request: {0}")]
    NotFound(String),
    #[error("malformed request: {0}")]
    BadRequest(String),
}

impl ResolveError {
    pub fn status(&self) -> StatusCode {
        match self {
            ResolveError::Unauthorized => StatusCode::UNAUTHORIZED,
            ResolveError::NotFound(_) => StatusCode::NOT_FOUND,
            ResolveError::BadRequest(_) => StatusCode::BAD_REQUEST,
        }
    }
}

/// 抽象 trait,Stage 4 起会有"基于实时 config 文件"的实现替换它.
pub trait ProviderResolver: Send + Sync {
    fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        body: &[u8],
    ) -> Result<ResolvedProvider, ResolveError>;
}

/// 内存版解析器:启动时把 Config 一次性灌进来,之后只读.
pub struct StaticResolver {
    /// `None` = 不要求 gateway 鉴权(开发场景);`Some` = incoming
    /// `Authorization: Bearer <gw>` 必须等于该值.
    pub gateway_key: Option<String>,
    pub providers: Vec<Provider>,
    /// 当 incoming 请求里没法决定 provider 时,fallback 用的 id.
    /// 一般等于 `Config::active_provider`.
    pub default_provider_id: Option<String>,
}

impl StaticResolver {
    pub fn new(
        gateway_key: Option<String>,
        providers: Vec<Provider>,
        default_provider_id: Option<String>,
    ) -> Self {
        Self {
            gateway_key,
            providers,
            default_provider_id,
        }
    }

    fn find_by_id(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    fn find_by_slug(&self, slug: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| provider_slug(p) == slug)
    }

    fn default_provider(&self) -> Option<&Provider> {
        if let Some(id) = self.default_provider_id.as_deref() {
            if let Some(p) = self.find_by_id(id) {
                return Some(p);
            }
        }
        self.providers.first()
    }

    fn map_model_for_provider(&self, provider: &Provider, requested_model: &str) -> Option<String> {
        let mappings_value = serde_json::to_value(&provider.models).ok();
        let mappings = normalize_model_mappings(mappings_value.as_ref());

        // 1. 已知 slot(gpt-5.5 → gpt_5_5 等):优先用 slot 映射
        let slot = openai_model_slot(requested_model);
        if let Some(slot) = slot {
            let mapped = mappings.get(slot).map(|s| s.trim()).unwrap_or("");
            if !mapped.is_empty() {
                return Some(strip_internal_model_suffix(mapped));
            }
        }

        // 2. 自定义映射:直接在 provider.models 中按 key 匹配(case-insensitive)
        let requested_lower = requested_model.trim().to_ascii_lowercase();
        for (key, value) in &provider.models {
            if key.trim().to_ascii_lowercase() == requested_lower {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(strip_internal_model_suffix(trimmed));
                }
            }
        }

        // 3. 所有未匹配的模型均降级到 default
        let default = mappings.get("default").map(|s| s.trim()).unwrap_or("");
        if !default.is_empty() {
            return Some(strip_internal_model_suffix(default));
        }

        None
    }

    /// 校验 incoming 的 `Authorization: Bearer <gw>`,匹配 self.gateway_key.
    fn check_gateway(&self, headers: &HeaderMap) -> Result<(), ResolveError> {
        let Some(expected) = self.gateway_key.as_deref() else {
            return Ok(());
        };
        let actual = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = actual.strip_prefix("Bearer ").unwrap_or(actual);
        if token == expected {
            return Ok(());
        }
        // [MOC-546] gateway 宽松化:精确匹配失败后,若 token 仍符合 cas_ 格式则放行。
        // gateway 定位为防误调用而非防攻击(真安全在本地文件权限),cas_ 格式校验足以
        // 区分"被 transfer 配置过的 Codex"与"不相干的外部请求"。消除 gateway key 双写
        // (proxy config ↔ auth.json)不一致导致的间歇性 401(Refs #546):无论双写怎么
        // 漂移,两边都是 cas_ 格式,格式兜底恒放行。与 MOC-189 relay JWT 形状校验同思路。
        if is_cas_gateway_key(token) {
            return Ok(());
        }
        // [MOC-189] relay 模式活动 `~/.codex/auth.json` 是真实 chatgpt,Codex 的**模型请求**
        // 发的是 chatgpt access_token(JWT,**不是** cas_ gateway key);放行让 `decide_provider`
        // 按 active_provider 转发到第三方 provider(用 provider 自己的 key,GPT JWT 不出本机、
        // 被 `forward::is_strip_on_forward` 剥掉)。
        //
        // 这里**只验形状**(chatgpt JWT),**不再**要求逐字匹配本地 auth.json(放宽 MOC-124 SEC-1):
        // - 鉴权的真实边界是 proxy 只绑 `127.0.0.1`(`proxy_runner.rs`)+ apikey 模式的 cas_ 兜底。
        //   能连 loopback 又能伪造 chatgpt 形状 JWT 的本机进程,本就能直接读 auth.json 拿真 token,
        //   逐字匹配挡不住该威胁、属冗余门槛。
        // - 旧的逐字匹配把「第三方对话可用性」错误绑死在 ChatGPT token 匹配状态上:token 轮换竞态 /
        //   `CODEX_HOME` 读串 / 未真正登 ChatGPT 都会让模型请求 401 → Codex WS idle timeout → 对话
        //   卡死。而 GPT JWT 功能上只服务 `/backend-api/*` 透传,坏了应只影响 plugins/账号,不该拖垮对话。
        if is_chatgpt_access_token(token) {
            return Ok(());
        }
        Err(ResolveError::Unauthorized)
    }
}

impl ProviderResolver for StaticResolver {
    fn resolve(
        &self,
        parts: &axum::http::request::Parts,
        body: &[u8],
    ) -> Result<ResolvedProvider, ResolveError> {
        self.check_gateway(&parts.headers)?;

        // 解析路由:body.model 优先(支持 "<slug>/<model>" 形式),否则走默认.
        let (provider, rewritten_model) = decide_provider(self, body)
            .ok_or_else(|| ResolveError::NotFound("no provider available".into()))?;

        // 把 provider.extraHeaders 转成 HeaderMap;非法名/值跳过(不阻塞请求)。
        // 支持 `{apiKey}` 模板替换,与 v1.0.3 backend/proxy.py:381 行为对齐
        // (例如 DeepSeek 同时需要 Authorization 和 x-api-key 头)。
        // 失败的 header 写 telemetry WARN 日志(原代码静默丢,排查 401 困难)。
        let telemetry = crate::telemetry::proxy_telemetry();
        let mut extras = HeaderMap::new();
        for (k, v) in &provider.extra_headers {
            let template_uses_api_key = v.contains("{apiKey}");
            if template_uses_api_key && provider.api_key.is_empty() {
                telemetry.logs.add(
                    "WARN",
                    format!(
                        "extraHeaders {k:?} 含 {{apiKey}} 模板但 provider {} api_key 为空,生成空值头",
                        provider.id
                    ),
                );
            }
            let v_substituted = v.replace("{apiKey}", &provider.api_key);
            match (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(&v_substituted),
            ) {
                (Ok(name), Ok(val)) => {
                    // insert 而非 append:provider.extra_headers 是 IndexMap
                    // 不会有 dup key,但 insert 更准确表达"该 provider 的
                    // X 头就这一个值"的意图,防止后续重构有人误把 resolver
                    // 里某段循环加 dup,导致出站 HeaderMap 里同名多值。
                    extras.insert(name, val);
                }
                (Err(e), _) => telemetry.logs.add(
                    "WARN",
                    format!(
                        "skip extraHeader provider={} {k:?}: header name invalid ({e})",
                        provider.id
                    ),
                ),
                (_, Err(e)) => telemetry.logs.add(
                    "WARN",
                    format!(
                        "skip extraHeader provider={} {k:?}: header value invalid ({e}); check api_key for newlines / non-ASCII",
                        provider.id
                    ),
                ),
            }
        }

        // **OAuth provider baseUrl 强制覆盖**:Cloud Code Assist OAuth 系两个
        // provider 的上游 host 固定,不允许用户自定义(防 user 改成无效 host
        // 或老的 prod host 撞 429 配额池)。
        // - gemini-cli: prod cloudcode-pa(CLIProxyAPI `gc_exec.go:36`)
        // - antigravity: **daily-cloudcode-pa**(CLIProxyAPI
        //   `antigravityBaseURLFallbackOrder` chat 路径主 host;prod 仅 fallback)
        // 2026-05-11 实测 user 用 prod 命中 429,daily 配额池独立 + 更宽。
        // user-saved provider.baseUrl 漂移(旧 preset)在这里自动 self-heal,
        // 不依赖 user 手动改 / 删 + 重加
        let auth_scheme = AuthScheme::parse(&provider.auth_scheme);
        let upstream_base = match auth_scheme {
            AuthScheme::GoogleOauthCloudCode => "https://cloudcode-pa.googleapis.com".to_string(),
            AuthScheme::GoogleOauthAntigravity => {
                "https://daily-cloudcode-pa.googleapis.com".to_string()
            }
            // z.ai/bigmodel:上游 base 按 provider 钉死(`api.z.ai/api/anthropic` /
            // `open.bigmodel.cn/api/anthropic`),不允许用户 baseUrl 漂移
            AuthScheme::ZaiOauth(zai_provider) => zai_provider.config().model_base.to_string(),
            // WorkBuddy 账号登录:access token scoped 给 copilot.tencent.com,强制 pin 官方
            // 网关 base,防 user 把 authScheme=workbuddy_oauth 配到任意 baseUrl 致账号 token
            // 外泄到非官方 host(codex review P2)。
            AuthScheme::WorkbuddyOauth => "https://copilot.tencent.com/v2".to_string(),
            AuthScheme::QoderCosy => "https://gateway.qoder.com.cn".to_string(),
            _ => provider.base_url.clone(),
        };

        // ZaiOauth:把 forced base 也写回转发用的 provider.base_url。anthropic_messages
        // adapter 的 path 由 `build_anthropic_messages_upstream_path(&provider.base_url)`
        // 推导(末尾是否 `/v1` 决定 `/messages` vs `/v1/messages`);若仍用用户漂移的
        // base_url(如手填成 `…/api/anthropic/v1`)会得到 `/messages`,跟 forced base
        // `…/api/anthropic` 拼成缺 `/v1` 的错误 URL。让 path 与 forced base 同源根治
        // (bot P2)。cloudcode/antigravity 走 gemini_cli adapter、path 逻辑不同,不动。
        let mut forwarded_provider = provider.clone();
        if matches!(
            auth_scheme,
            AuthScheme::ZaiOauth(_) | AuthScheme::WorkbuddyOauth
        ) {
            forwarded_provider.base_url = upstream_base.clone();
        }

        Ok(ResolvedProvider {
            provider_id: provider.id.clone(),
            upstream_base,
            api_key: provider.api_key.clone(),
            auth_scheme,
            extra_headers: extras,
            rewritten_model,
            provider: Arc::new(forwarded_provider),
        })
    }
}

fn decide_provider<'a>(
    res: &'a StaticResolver,
    body: &[u8],
) -> Option<(&'a Provider, Option<String>)> {
    // 试着从 body JSON 里抠 "model".
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
            if let Some((slug, real)) = model.split_once('/') {
                if let Some(p) = res.find_by_slug(slug) {
                    return Some((p, Some(strip_internal_model_suffix(real))));
                }
            }
        }
    }
    let provider = res.default_provider()?;
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
            if let Some(mapped) = res.map_model_for_provider(provider, model) {
                return Some((provider, Some(mapped)));
            }
        }
    }
    // 没 / 或没可映射 model → 走默认 provider.
    Some((provider, None))
}

/// 判断 Bearer 是否是 OpenAI ChatGPT 的 access_token —— JWT(三段)且 payload 含
/// `https://api.openai.com/auth.chatgpt_account_id`。relay 模式(活动 auth.json 是真实
/// chatgpt)下 Codex 模型请求发此 token 到 proxy,`check_gateway` 据此放行(身份比静态
/// cas_ gateway key 更硬,且 `decide_provider` 不依赖 gateway key 即可按 active_provider
/// 转发)。验 claim 而非只看 JWT 格式,挡掉随机乱 token。
fn is_chatgpt_access_token(token: &str) -> bool {
    use base64::Engine;
    // JWT = header.payload.signature,正好三段且签名非空。
    let mut it = token.split('.');
    let payload = match (it.next(), it.next(), it.next(), it.next()) {
        (Some(_h), Some(p), Some(sig), None) if !sig.is_empty() && !p.is_empty() => p,
        _ => return false,
    };
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// 判断 token 是否是 cas_ 格式的 gateway key。用于 check_gateway 的格式兜底:
/// gateway 定位为防误调用而非防攻击(真安全在本地文件权限),cas_ 格式校验足以区分
/// "被 transfer 配置过的 Codex"与"外部误连"。容忍双写不一致的旧 key 版本(Refs #546)。
/// 格式与 conversation_export::redact 的 `cas_[A-Za-z0-9_-]{12,}` 正则一致。
fn is_cas_gateway_key(token: &str) -> bool {
    let rest = token.strip_prefix("cas_").unwrap_or("");
    rest.len() >= 12
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 让裸 Resolver 可装进 `Arc<dyn ProviderResolver>`(给 ProxyState 共享用).
pub type SharedResolver = Arc<dyn ProviderResolver>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use codex_app_transfer_registry::Provider;
    use indexmap::IndexMap;

    fn provider(id: &str, base: &str, key: &str) -> Provider {
        provider_with_name(id, id, base, key)
    }

    fn provider_with_name(id: &str, name: &str, base: &str, key: &str) -> Provider {
        let mut models = IndexMap::new();
        models.insert("default".into(), format!("{id}-default"));
        Provider {
            id: id.into(),
            name: name.into(),
            base_url: base.into(),
            auth_scheme: "bearer".into(),
            api_format: "openai_chat".into(),
            api_key: key.into(),
            models,
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    fn parts_with(headers: &[(&str, &str)]) -> axum::http::request::Parts {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let (parts, _body) = req.body(()).unwrap().into_parts();
        parts
    }

    #[test]
    fn auth_scheme_parsing() {
        assert_eq!(AuthScheme::parse("bearer"), AuthScheme::Bearer);
        assert_eq!(AuthScheme::parse("Bearer"), AuthScheme::Bearer);
        assert_eq!(AuthScheme::parse("x-api-key"), AuthScheme::XApiKey);
        assert_eq!(
            AuthScheme::parse("google_api_key"),
            AuthScheme::GoogleApiKey
        );
        assert_eq!(
            AuthScheme::parse("x-goog-api-key"),
            AuthScheme::GoogleApiKey
        );
        assert_eq!(AuthScheme::parse("gemini"), AuthScheme::GoogleApiKey);
        assert_eq!(AuthScheme::parse(""), AuthScheme::None);
        assert_eq!(AuthScheme::parse("unknown"), AuthScheme::Bearer);

        // **Critical** test gap(2026-05-11 修):4 个 GoogleOauthCloudCode alias
        // 全部明确 lock 防 typo / refactor 把它们误归 Bearer 导致 401(provider.api_key
        // 字段为空,Bearer 注入空 token,silent fail)。
        assert_eq!(
            AuthScheme::parse("google_oauth_cloud_code"),
            AuthScheme::GoogleOauthCloudCode
        );
        assert_eq!(
            AuthScheme::parse("google_oauth"),
            AuthScheme::GoogleOauthCloudCode
        );
        assert_eq!(
            AuthScheme::parse("gemini_cli_oauth"),
            AuthScheme::GoogleOauthCloudCode
        );
        assert_eq!(
            AuthScheme::parse("gemini_oauth"),
            AuthScheme::GoogleOauthCloudCode
        );
        // 大小写 / dash 混用都识别(parse 内部 to_ascii_lowercase + replace '-' '_')
        assert_eq!(
            AuthScheme::parse("Google-OAuth-Cloud-Code"),
            AuthScheme::GoogleOauthCloudCode
        );

        // Antigravity 3 别名(2026-05-11 加 antigravity provider):任何一个误归
        // Bearer 都会让 forward.rs 跳过 ensure_valid_antigravity_token + 不注入 UA
        // → 上游静默 401 / 配额错 bucket
        assert_eq!(
            AuthScheme::parse("google_oauth_antigravity"),
            AuthScheme::GoogleOauthAntigravity
        );
        assert_eq!(
            AuthScheme::parse("antigravity_oauth"),
            AuthScheme::GoogleOauthAntigravity
        );
        assert_eq!(
            AuthScheme::parse("antigravity"),
            AuthScheme::GoogleOauthAntigravity
        );
        assert_eq!(
            AuthScheme::parse("Google-OAuth-Antigravity"),
            AuthScheme::GoogleOauthAntigravity
        );

        // z.ai / bigmodel(MOC-252):别名误归 Bearer 会让 forward.rs 跳过组织 key load
        // + 不注 ZCode 指纹 → 上游 401 / 非 GLM 客户端。两套 provider 必须解到对的
        // ZaiProvider(决定 model_base + token 文件)。
        assert_eq!(
            AuthScheme::parse("zai_oauth"),
            AuthScheme::ZaiOauth(ZaiProvider::Zai)
        );
        assert_eq!(
            AuthScheme::parse("zai"),
            AuthScheme::ZaiOauth(ZaiProvider::Zai)
        );
        assert_eq!(
            AuthScheme::parse("bigmodel_oauth"),
            AuthScheme::ZaiOauth(ZaiProvider::BigModel)
        );
        assert_eq!(
            AuthScheme::parse("bigmodel"),
            AuthScheme::ZaiOauth(ZaiProvider::BigModel)
        );
        assert_eq!(
            AuthScheme::parse("BigModel-OAuth"),
            AuthScheme::ZaiOauth(ZaiProvider::BigModel)
        );

        // WorkbuddyOauth / QoderCosy(账号池系):别名误归 Bearer 会让 forward.rs 跳过各自
        // 的账号池选择 + 签名分支 → 上游静默 401/破坏。QoderCosy 更严重:误归 Bearer 会同时
        // 跳过 send_qoder_cosy WASM 签名 + unwrap_qoder_cosy_stream 信封解包 → 全链静默坏。
        assert_eq!(
            AuthScheme::parse("workbuddy_oauth"),
            AuthScheme::WorkbuddyOauth
        );
        assert_eq!(
            AuthScheme::parse("workbuddy_login"),
            AuthScheme::WorkbuddyOauth
        );
        assert_eq!(AuthScheme::parse("qoder_oauth"), AuthScheme::QoderCosy);
        assert_eq!(AuthScheme::parse("qoder"), AuthScheme::QoderCosy);
        assert_eq!(AuthScheme::parse("qoder_cosy"), AuthScheme::QoderCosy);
        assert_eq!(AuthScheme::parse("Qoder-OAuth"), AuthScheme::QoderCosy);
    }

    #[test]
    fn qoder_cosy_forces_gateway_base_ignoring_provider_baseurl() {
        // QoderCosy 的 upstream_base 钉死 gateway.qoder.com.cn,覆盖 user 保存的旧 baseUrl
        // (qoder-login preset 历史 base 是废弃的 api2-v2 通道)。防 baseUrl 漂移 + device
        // token 泄漏到 user 自定义 host(即便当前 send_qoder_cosy 硬编 gateway,此 pin 是纵深防御)。
        let mut p = provider("qoder", "https://api2-v2.qoder.sh/model/v1", "");
        p.auth_scheme = "qoder_oauth".into();
        let r = StaticResolver::new(None, vec![p], Some("qoder".into()));
        let parts = parts_with(&[]);
        let resolved = r.resolve(&parts, b"{}").expect("qoder provider 应解析");
        assert_eq!(resolved.upstream_base, "https://gateway.qoder.com.cn");
    }

    #[test]
    fn unauthorized_when_gateway_key_missing() {
        let r = StaticResolver::new(
            Some("gw".into()),
            vec![provider("openai", "https://up", "sk-1")],
            Some("openai".into()),
        );
        let p = parts_with(&[]);
        let err = r.resolve(&p, b"{}").unwrap_err();
        assert!(matches!(err, ResolveError::Unauthorized));
    }

    #[test]
    fn unauthorized_when_gateway_key_wrong() {
        let r = StaticResolver::new(
            Some("gw".into()),
            vec![provider("openai", "https://up", "sk-1")],
            Some("openai".into()),
        );
        let p = parts_with(&[("authorization", "Bearer wrong")]);
        let err = r.resolve(&p, b"{}").unwrap_err();
        assert!(matches!(err, ResolveError::Unauthorized));
    }

    #[test]
    fn ok_when_gateway_key_correct() {
        let r = StaticResolver::new(
            Some("gw".into()),
            vec![provider("openai", "https://up", "sk-1")],
            Some("openai".into()),
        );
        let p = parts_with(&[("authorization", "Bearer gw")]);
        let res = r.resolve(&p, b"{}").unwrap();
        assert_eq!(res.provider_id, "openai");
        assert_eq!(res.api_key, "sk-1");
        assert_eq!(res.rewritten_model, None);
    }

    #[test]
    fn ok_when_cas_format_but_different_value() {
        let r = StaticResolver::new(
            Some("cas_real_key_here".into()),
            vec![provider("openai", "https://up", "sk-1")],
            Some("openai".into()),
        );
        // proxy 持有 cas_real_key,incoming 发不同的 cas_ key (模拟双写不一致)
        let p = parts_with(&[("authorization", "Bearer cas_different_key")]);
        let res = r.resolve(&p, b"{}").unwrap();
        assert_eq!(res.provider_id, "openai");
        assert_eq!(res.api_key, "sk-1");
    }

    #[test]
    fn unauthorized_when_non_cas_non_jwt() {
        let r = StaticResolver::new(
            Some("cas_real_key_here".into()),
            vec![provider("openai", "https://up", "sk-1")],
            Some("openai".into()),
        );
        // 随机非 cas_ 非 JWT 字符串 -> 拒绝 (挡住外部误连)
        let p = parts_with(&[("authorization", "Bearer sk-fake-token")]);
        let err = r.resolve(&p, b"{}").unwrap_err();
        assert!(matches!(err, ResolveError::Unauthorized));
    }

    /// 构造一个 ChatGPT access_token(JWT,payload 含 chatgpt_account_id)用于测试。
    fn chatgpt_jwt() -> String {
        use base64::Engine;
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc_test"}
        });
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("eyJhbGciOiJub25lIn0.{p}.sig")
    }

    /// [MOC-189] relay 放行**任意 chatgpt 形状的 JWT**,不再要求逐字匹配本地 auth.json。
    /// 鉴权边界 = localhost 绑定 + cas_ 兜底;放宽逐字匹配是为了让「第三方对话可用性」不再被
    /// ChatGPT token 轮换 / `CODEX_HOME` 读串 / 未登 ChatGPT 拖死(模型请求 401 → WS idle
    /// timeout)。覆盖:① chatgpt 形状 JWT 放行 ② cas_ 仍放行 ③ 另一 account_id 的 chatgpt JWT
    /// 同样放行(不再匹配本地)④ 非 JWT 乱串拒。
    #[test]
    fn relay_accepts_any_chatgpt_shaped_jwt() {
        use base64::Engine;
        let r = StaticResolver::new(
            Some("cas-secret".into()),
            vec![provider("openai", "https://up", "sk-1")],
            Some("openai".into()),
        );
        // ① chatgpt 形状 JWT → 放行,decide_provider 走 active_provider
        let auth = format!("Bearer {}", chatgpt_jwt());
        let p = parts_with(&[("authorization", auth.as_str())]);
        let res = r.resolve(&p, br#"{"model":"gpt-5.5"}"#).unwrap();
        assert_eq!(res.provider_id, "openai");
        assert_eq!(res.api_key, "sk-1");
        // ② cas_ gateway key 仍放行(exact match 分支不变)
        let p_cas = parts_with(&[("authorization", "Bearer cas-secret")]);
        assert!(r.resolve(&p_cas, b"{}").is_ok());
        // ③ 另一个 account_id 的 chatgpt JWT → 同样放行(MOC-189:不再要求匹配本地 auth.json)
        let other_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(
                &serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"acc_other"}}),
            )
            .unwrap(),
        );
        let other = format!("Bearer eyJhbGciOiJub25lIn0.{other_payload}.sig");
        let po = parts_with(&[("authorization", other.as_str())]);
        assert!(
            r.resolve(&po, b"{}").is_ok(),
            "任意 chatgpt 形状 JWT 都应放行(localhost + cas_ 已是鉴权边界)"
        );
        // ④ 非 JWT 乱串 → 拒
        let pr = parts_with(&[("authorization", "Bearer random-junk")]);
        assert!(matches!(
            r.resolve(&pr, b"{}").unwrap_err(),
            ResolveError::Unauthorized
        ));
        // ⑤ 3 段 JWT 但 payload 无 chatgpt_account_id claim → 拒(pin 住 is_chatgpt_access_token
        //    的 claim 校验:gate 放宽后这是唯一剩下的判别逻辑,防未来回归成"任意 3 段 token 放行")
        let no_claim_payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"sub\":\"x\"}");
        let no_claim = format!("Bearer eyJhbGciOiJub25lIn0.{no_claim_payload}.sig");
        let pnc = parts_with(&[("authorization", no_claim.as_str())]);
        assert!(
            matches!(
                r.resolve(&pnc, b"{}").unwrap_err(),
                ResolveError::Unauthorized
            ),
            "缺 chatgpt_account_id 的 3 段 JWT 不算 chatgpt token,应拒"
        );
    }

    #[test]
    fn slug_routing_picks_named_provider_and_rewrites_model() {
        let r = StaticResolver::new(
            None,
            vec![
                provider("openai", "https://up-1", "sk-1"),
                provider("deepseek", "https://up-2", "sk-2"),
            ],
            Some("openai".into()),
        );
        let p = parts_with(&[]);
        let body = br#"{"model":"deepseek/deepseek-v4-pro"}"#;
        let res = r.resolve(&p, body).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.api_key, "sk-2");
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn slash_route_strips_internal_suffix() {
        let r = StaticResolver::new(
            None,
            vec![provider("deepseek", "https://up-2", "sk-2")],
            Some("deepseek".into()),
        );
        let p = parts_with(&[]);
        let body = br#"{"model":"deepseek/deepseek-v4-pro[1m]"}"#;
        let res = r.resolve(&p, body).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn slug_routing_normalizes_provider_id_like_legacy_model_alias() {
        let r = StaticResolver::new(
            None,
            vec![provider("OpenAI.Custom_1", "https://up-1", "sk-1")],
            None,
        );
        let p = parts_with(&[]);
        let body = br#"{"model":"openai-custom_1/gpt-real"}"#;
        let res = r.resolve(&p, body).unwrap();
        assert_eq!(res.provider_id, "OpenAI.Custom_1");
        assert_eq!(res.rewritten_model.as_deref(), Some("gpt-real"));
    }

    #[test]
    fn slug_routing_uses_provider_name_when_id_is_blank() {
        let r = StaticResolver::new(
            None,
            vec![provider_with_name(
                "",
                "Moonshot AI",
                "https://up-1",
                "sk-1",
            )],
            None,
        );
        let p = parts_with(&[]);
        let body = br#"{"model":"moonshot-ai/kimi-k2.6"}"#;
        let res = r.resolve(&p, body).unwrap();
        assert_eq!(res.provider_id, "");
        assert_eq!(res.upstream_base, "https://up-1");
        assert_eq!(res.rewritten_model.as_deref(), Some("kimi-k2.6"));
    }

    #[test]
    fn slug_routing_collapses_special_character_provider_name() {
        let r = StaticResolver::new(
            None,
            vec![provider_with_name(
                "",
                "七牛 / Qiniu++",
                "https://up-1",
                "sk-1",
            )],
            None,
        );
        let p = parts_with(&[]);
        let body = br#"{"model":"qiniu/qna-v1"}"#;
        let res = r.resolve(&p, body).unwrap();
        assert_eq!(res.provider_id, "");
        assert_eq!(res.upstream_base, "https://up-1");
        assert_eq!(res.rewritten_model.as_deref(), Some("qna-v1"));
    }

    #[test]
    fn zai_oauth_forces_base_and_path_source_even_when_baseurl_drifts() {
        // bot P2:用户 baseUrl 漂移带 /v1 时,forced upstream_base **和**转发用的
        // provider.base_url 都应钉成 model_base —— 这样 anthropic adapter 的 path 推导
        // (`build_anthropic_messages_upstream_path` 看 base 末尾是否 `/v1`)与 base 同源,
        // 不会拼出缺 `/v1` 的错误 URL。
        let mut p = provider("bm", "https://open.bigmodel.cn/api/anthropic/v1", "");
        p.auth_scheme = "bigmodel_oauth".into();
        p.api_format = "anthropic_messages".into();
        let r = StaticResolver::new(None, vec![p], Some("bm".into()));
        let parts = parts_with(&[]);
        let res = r.resolve(&parts, br#"{"model":"glm-4.7"}"#).unwrap();
        let forced = "https://open.bigmodel.cn/api/anthropic";
        assert_eq!(res.upstream_base, forced, "upstream_base 应钉成 model_base");
        assert_eq!(
            res.provider.base_url, forced,
            "转发 provider.base_url 也应钉成 forced base(否则 adapter path 推导用漂移值)"
        );
        // z.ai 同理
        let mut pz = provider("z", "https://api.z.ai/api/anthropic/v1/", "");
        pz.auth_scheme = "zai_oauth".into();
        pz.api_format = "anthropic_messages".into();
        let rz = StaticResolver::new(None, vec![pz], Some("z".into()));
        let resz = rz
            .resolve(&parts_with(&[]), br#"{"model":"glm-4.7"}"#)
            .unwrap();
        assert_eq!(resz.provider.base_url, "https://api.z.ai/api/anthropic");
    }

    #[test]
    fn unknown_model_falls_back_to_default_mapping() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("default".into(), "deepseek-v4-pro".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        // "any-name" is not a known slot nor a custom key → should fall back to default
        let res = r.resolve(&p, br#"{"model":"any-name"}"#).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn custom_key_mapping_matches_directly() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("default".into(), "deepseek-v4-pro".into());
        deepseek
            .models
            .insert("gpt-4o".into(), "deepseek-v4-lite".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"gpt-4o"}"#).unwrap();
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-lite"));
    }

    #[test]
    fn custom_key_mapping_is_case_insensitive() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("My-Custom-Model".into(), "real-model".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"my-custom-model"}"#).unwrap();
        assert_eq!(res.rewritten_model.as_deref(), Some("real-model"));
    }

    #[test]
    fn known_slot_takes_priority_over_custom_key() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("gpt_5_5".into(), "slot-model".into());
        // even if there's a custom key "gpt-5.5", the slot lookup wins
        deepseek
            .models
            .insert("gpt-5.5".into(), "custom-model".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"gpt-5.5"}"#).unwrap();
        assert_eq!(res.rewritten_model.as_deref(), Some("slot-model"));
    }

    #[test]
    fn openai_slot_model_maps_to_provider_default() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("default".into(), "deepseek-v4-pro".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"gpt-5.5"}"#).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn openai_slot_model_maps_to_provider_specific_slot() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("gpt_5_5".into(), "deepseek-v4-pro[1m]".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"gpt-5.5"}"#).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn openai_slot_model_matching_is_case_insensitive_like_legacy() {
        let mut deepseek = provider("deepseek", "https://up-2", "sk-2");
        deepseek
            .models
            .insert("gpt_5_5".into(), "deepseek-v4-pro".into());
        let r = StaticResolver::new(None, vec![deepseek], Some("deepseek".into()));
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"GPT-5.5"}"#).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.rewritten_model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn extra_headers_pulled_from_provider() {
        let mut p = provider("kimi-code", "https://up", "k");
        p.extra_headers
            .insert("User-Agent".into(), "KimiCLI/1.40.0".into());
        let r = StaticResolver::new(None, vec![p], Some("kimi-code".into()));
        let parts = parts_with(&[]);
        let res = r.resolve(&parts, b"{}").unwrap();
        assert_eq!(
            res.extra_headers.get("user-agent").unwrap(),
            "KimiCLI/1.40.0"
        );
    }

    #[test]
    fn extra_headers_substitute_api_key_template() {
        // 对齐 v1.0.3 backend/proxy.py:381 的 `{apiKey}` 模板替换。
        let mut p = provider("deepseek", "https://up", "sk-real-key");
        p.extra_headers
            .insert("x-api-key".into(), "{apiKey}".into());
        p.extra_headers
            .insert("X-Plain".into(), "no-template".into());
        let r = StaticResolver::new(None, vec![p], Some("deepseek".into()));
        let parts = parts_with(&[]);
        let res = r.resolve(&parts, b"{}").unwrap();
        assert_eq!(res.extra_headers.get("x-api-key").unwrap(), "sk-real-key");
        assert_eq!(res.extra_headers.get("x-plain").unwrap(), "no-template");
    }
}
