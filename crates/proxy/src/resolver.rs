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
    /// 不写鉴权头(上游免认证 / 走 cookie 等少见情况).
    None,
}

impl AuthScheme {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "x-api-key" | "x_api_key" | "xapikey" | "apikey" => AuthScheme::XApiKey,
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
        let slot = openai_model_slot(requested_model);
        if let Some(slot) = slot {
            let mapped = mappings.get(slot).map(|s| s.trim()).unwrap_or("");
            if !mapped.is_empty() {
                return Some(strip_internal_model_suffix(mapped));
            }
            let default = mappings.get("default").map(|s| s.trim()).unwrap_or("");
            if !default.is_empty() {
                return Some(strip_internal_model_suffix(default));
            }
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
            Ok(())
        } else {
            Err(ResolveError::Unauthorized)
        }
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

        // 把 provider.extraHeaders 转成 HeaderMap;非法名/值跳过(不阻塞请求)
        let mut extras = HeaderMap::new();
        for (k, v) in &provider.extra_headers {
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                extras.append(name, val);
            }
        }

        Ok(ResolvedProvider {
            provider_id: provider.id.clone(),
            upstream_base: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            auth_scheme: AuthScheme::parse(&provider.auth_scheme),
            extra_headers: extras,
            rewritten_model,
            provider: Arc::new(provider.clone()),
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
        assert_eq!(AuthScheme::parse(""), AuthScheme::None);
        assert_eq!(AuthScheme::parse("unknown"), AuthScheme::Bearer);
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
    fn falls_back_to_default_when_no_slash_in_model() {
        let r = StaticResolver::new(
            None,
            vec![
                provider("openai", "https://up-1", "sk-1"),
                provider("deepseek", "https://up-2", "sk-2"),
            ],
            Some("deepseek".into()),
        );
        let p = parts_with(&[]);
        let res = r.resolve(&p, br#"{"model":"any-name"}"#).unwrap();
        assert_eq!(res.provider_id, "deepseek");
        assert_eq!(res.rewritten_model, None);
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
}
