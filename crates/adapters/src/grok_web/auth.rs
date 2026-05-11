//! grok.com Web 鉴权头注入。
//!
//! ## 现行协议
//!
//! 实测 2026-05-11 SuperGrok 账号(三次 cURL 抓包),grok.com 现行鉴权头:
//!
//! - **Cookie**:`sso=<JWT>; sso-rw=<JWT>; cf_clearance=<TOKEN>` (核心)
//!   - 可选:`x-userid=<UUID>`(已登录用户 UUID)、`__cf_bm=<TOKEN>`(Cloudflare Bot Management)
//!   - 可选:`mp_..._mixpanel` / `__stripe_*` / `OptanonConsent` / `i18nextLng`(分析/支付/合规,可省)
//! - **x-statsig-id**:Base64-encoded Statsig feature flag context(每次请求不同)
//! - **x-xai-request-id**:UUID v4(client 生成,每次请求不同)
//! - **traceparent**:W3C trace context(`00-<32hex>-<16hex>-00`)
//! - **sentry-trace** / **baggage**:Sentry distributed tracing
//! - **User-Agent**:必须是真实浏览器 UA(防风控)
//!
//! ## chenyme 用的 headers 已过时
//!
//! `x-anonuserid` / `x-challenge` / `x-signature` 三个 header **现行协议不再使用**。
//! 不要复用 chenyme `transport/http.py` 的鉴权头组合。
//!
//! ## 本模块职责
//!
//! - 提供 [`apply_grok_headers`]:在 `RequestPlan` headers 上注入 cookie + statsig 等
//! - 提供 [`GrokCookies`]:用户提供的 cookie 集合(从 Provider.extra.grokWeb.cookies 读)
//! - **不负责** statsig-id 生成(grok.com 用 Statsig SDK 客户端生成 base64 blob,
//!   反向工程难度高;R1 阶段让用户从浏览器一次性抓 + 复用一段时间)
//!
//! 实际 header 注入在 [`crate::mapper::grok_web::GrokWebMapper`](../mapper/grok_web.rs)
//! 准备 `RequestPlan` 时调用本模块函数。

use codex_app_transfer_registry::Provider;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

/// 用户提供的 grok.com cookie 集合。
///
/// 从 `Provider.extra["grokWeb"]["cookies"]` JSON object 中读取。
///
/// 必需字段:`sso`、`sso-rw`、`cf_clearance`。
/// 可选字段:`x-userid`、`__cf_bm`、其他 grok.com 设置的 cookie。
///
/// **不实现 `Default`**(review-feedback TD4):empty `GrokCookies` 没有意义,
/// `to_cookie_header()` 会拼出 `sso=; sso-rw=; cf_clearance=` 让上游 401。
/// 唯一合法构造路径是 [`GrokCookies::from_provider`]。
#[derive(Debug, Clone)]
pub struct GrokCookies {
    /// JWT session token(写入 `Cookie: sso=...`)
    pub sso: String,
    /// JWT session token(读写,写入 `Cookie: sso-rw=...`)
    pub sso_rw: String,
    /// Cloudflare 通过 token(必需,~7 天过期)
    pub cf_clearance: String,
    /// 用户 UUID(可选,推测 grok.com 用于路由优化)
    pub x_userid: Option<String>,
    /// Cloudflare bot management token(可选)
    pub cf_bm: Option<String>,
    /// 其他 cookie 透传(mixpanel/stripe/optanon/i18next 等)
    pub others: Vec<(String, String)>,
}

impl GrokCookies {
    /// 从 Provider extra 中提取。
    ///
    /// 路径:`provider.extra["grokWeb"]["cookies"]` —— JSON object,key 是 cookie 名,value 是 string。
    ///
    /// 必需 cookie 缺失时返回 `Err`,让 forward 层 surface 401 给客户端。
    pub fn from_provider(provider: &Provider) -> Result<Self, GrokAuthError> {
        let grok_web = provider
            .extra
            .get("grokWeb")
            .and_then(Value::as_object)
            .ok_or(GrokAuthError::MissingGrokWebConfig)?;
        let cookies = grok_web
            .get("cookies")
            .and_then(Value::as_object)
            .ok_or(GrokAuthError::MissingCookies)?;

        let get_required = |key: &str| -> Result<String, GrokAuthError> {
            cookies
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| GrokAuthError::MissingCookie(key.into()))
        };
        let get_optional = |key: &str| -> Option<String> {
            cookies
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };

        let sso = get_required("sso")?;
        let sso_rw = get_required("sso-rw")?;
        let cf_clearance = get_required("cf_clearance")?;
        let x_userid = get_optional("x-userid");
        let cf_bm = get_optional("__cf_bm");

        // 收集"其他" cookie(实测里有:mixpanel/stripe/optanon/i18nextLng 等)
        let known_keys = ["sso", "sso-rw", "cf_clearance", "x-userid", "__cf_bm"];
        let others: Vec<(String, String)> = cookies
            .iter()
            .filter_map(|(k, v)| {
                if known_keys.contains(&k.as_str()) {
                    return None;
                }
                let val = v.as_str()?.to_owned();
                Some((k.clone(), val))
            })
            .collect();

        Ok(Self {
            sso,
            sso_rw,
            cf_clearance,
            x_userid,
            cf_bm,
            others,
        })
    }

    /// 拼成 `Cookie:` header 单行(按 RFC 6265 用 `; ` 分隔)。
    pub fn to_cookie_header(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(5 + self.others.len());
        parts.push(format!("sso={}", self.sso));
        parts.push(format!("sso-rw={}", self.sso_rw));
        parts.push(format!("cf_clearance={}", self.cf_clearance));
        if let Some(uid) = &self.x_userid {
            parts.push(format!("x-userid={uid}"));
        }
        if let Some(bm) = &self.cf_bm {
            parts.push(format!("__cf_bm={bm}"));
        }
        for (k, v) in &self.others {
            parts.push(format!("{k}={v}"));
        }
        parts.join("; ")
    }
}

/// grok.com 鉴权失败分类(`forward.rs` 据此 surface 友好错误给客户端)。
#[derive(Debug, thiserror::Error)]
pub enum GrokAuthError {
    #[error("provider.extra missing `grokWeb` object")]
    MissingGrokWebConfig,
    #[error("provider.extra.grokWeb missing `cookies` object")]
    MissingCookies,
    #[error("provider.extra.grokWeb.cookies missing required cookie `{0}`")]
    MissingCookie(String),
    #[error("statsig id missing or invalid")]
    MissingStatsigId,
}

/// 注入 grok.com 所需 headers 到一个新构造的 [`HeaderMap`] 并返回。
///
/// 推荐入口(替代 [`apply_grok_headers`] 的 `&mut HeaderMap` 接口),错误会
/// 显式 propagate 给调用方;Cookie / x-statsig-id 自动 `set_sensitive(true)`
/// 防止落进 tracing 结构化日志(review-feedback I6)。
///
/// 调用方:`crates/proxy/src/forward.rs::build_and_send_upstream` GrokCookie 分支。
pub fn apply_grok_headers_typed(provider: &Provider) -> Result<HeaderMap, GrokAuthError> {
    let mut headers = HeaderMap::with_capacity(14);
    apply_grok_headers(&mut headers, provider)?;
    // 对 sensitive headers 标记后,reqwest 在日志/debug 序列化时不会暴露 value
    for name in ["cookie", "x-statsig-id"] {
        if let Some(value) = headers.get_mut(name) {
            value.set_sensitive(true);
        }
    }
    Ok(headers)
}

/// 注入 grok.com 所需 headers 到 `RequestPlan` 的 HeaderMap。
///
/// 调用方:`mapper::grok_web::prepare_grok_web_request` 在构造 RequestPlan 时调用。
///
/// **注入项**:
/// - `Cookie: sso=...; sso-rw=...; cf_clearance=...; ...`
/// - `User-Agent: <浏览器 UA>`(默认 macOS Safari,可被 Provider extra override)
/// - `Origin: https://grok.com`
/// - `Referer: https://grok.com/`
/// - `Accept: text/event-stream, */*`
/// - `Accept-Language: zh-CN,zh-Hans;q=0.9`(让 grok 默认中文回答;可被 Provider override)
/// - `x-statsig-id: <用户提供>`
/// - `x-xai-request-id: <每次请求生成 UUID>`
/// - `traceparent: 00-<32hex>-<16hex>-00`(自动生成)
///
/// **不注入**:`__cf_bm` cookie 单独 set(它在 Cookie 里已合并)、`sentry-trace`(可选,
/// 实测无该 header 也能 work)。
pub fn apply_grok_headers(
    headers: &mut HeaderMap,
    provider: &Provider,
) -> Result<(), GrokAuthError> {
    let cookies = GrokCookies::from_provider(provider)?;
    let statsig_id = read_statsig_id(provider)?;
    let user_agent = read_user_agent(provider);

    insert(headers, "Cookie", &cookies.to_cookie_header());
    insert(headers, "User-Agent", &user_agent);
    insert(headers, "Origin", "https://grok.com");
    insert(headers, "Referer", "https://grok.com/");
    insert(headers, "Accept", "text/event-stream, */*");
    insert(headers, "Accept-Language", "zh-CN,zh-Hans;q=0.9");
    insert(headers, "x-statsig-id", &statsig_id);
    insert(headers, "x-xai-request-id", &generate_uuid_v4());
    insert(headers, "traceparent", &generate_traceparent());

    // CORS hints —— 模拟浏览器行为,降低风控触发概率
    insert(headers, "Sec-Fetch-Site", "same-origin");
    insert(headers, "Sec-Fetch-Mode", "cors");
    insert(headers, "Sec-Fetch-Dest", "empty");

    Ok(())
}

fn read_statsig_id(provider: &Provider) -> Result<String, GrokAuthError> {
    provider
        .extra
        .get("grokWeb")
        .and_then(Value::as_object)
        .and_then(|o| o.get("statsigId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(GrokAuthError::MissingStatsigId)
}

fn read_user_agent(provider: &Provider) -> String {
    provider
        .extra
        .get("grokWeb")
        .and_then(Value::as_object)
        .and_then(|o| o.get("userAgent"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            // 默认 UA:macOS Safari 26.4(对齐实测抓包)
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.4 Safari/605.1.15"
                .to_owned()
        })
}

/// 生成 UUID v4(随机)。供 `x-xai-request-id` 与 grok_web 内部 response_id 复用。
///
/// 用 `getrandom`(crate 已有依赖)而非 `uuid`,避免新增依赖项;
/// 手写 RFC 4122 v4 编码也只 ~20 行。
pub(crate) fn generate_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS RNG should not fail");
    // RFC 4122 v4 + variant bits
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant 1 (RFC 4122)
    format_uuid_v4(&bytes)
}

fn format_uuid_v4(b: &[u8; 16]) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        hex_encode(&b[0..4]),
        hex_encode(&b[4..6]),
        hex_encode(&b[6..8]),
        hex_encode(&b[8..10]),
        hex_encode(&b[10..16]),
    )
}

/// 生成符合 W3C Trace Context spec 的 `traceparent` header。
///
/// 格式:`00-<32hex>-<16hex>-00`
/// - `00`:version
/// - 32hex:trace-id(128 bit,随机)
/// - 16hex:parent-id(64 bit,随机)
/// - `00`:flags(`00` 表示不强制采样)
fn generate_traceparent() -> String {
    let mut trace_id = [0u8; 16];
    let mut parent_id = [0u8; 8];
    getrandom::getrandom(&mut trace_id).expect("OS RNG should not fail");
    getrandom::getrandom(&mut parent_id).expect("OS RNG should not fail");
    format!("00-{}-{}-00", hex_encode(&trace_id), hex_encode(&parent_id),)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    let Ok(header_name) = HeaderName::try_from(name) else {
        tracing::warn!(name = %name, "invalid grok header name");
        return;
    };
    let Ok(header_value) = HeaderValue::from_str(value) else {
        tracing::warn!(name = %name, "invalid grok header value");
        return;
    };
    headers.insert(header_name, header_value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_transfer_registry::Provider;
    use indexmap::IndexMap;
    use serde_json::json;

    fn provider_with_grok_web(extra: Value) -> Provider {
        let mut p = Provider {
            id: "grok-web-supergrok".into(),
            name: "Grok Web".into(),
            base_url: "https://grok.com".into(),
            auth_scheme: "grok_cookie".into(),
            api_format: "grok_web".into(),
            api_key: String::new(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        };
        if let Value::Object(map) = extra {
            for (k, v) in map {
                p.extra.insert(k, v);
            }
        }
        p
    }

    #[test]
    fn from_provider_reads_required_cookies() {
        let p = provider_with_grok_web(json!({
            "grokWeb": {
                "cookies": {
                    "sso": "jwt-1",
                    "sso-rw": "jwt-2",
                    "cf_clearance": "cf-3"
                },
                "statsigId": "abc"
            }
        }));
        let c = GrokCookies::from_provider(&p).unwrap();
        assert_eq!(c.sso, "jwt-1");
        assert_eq!(c.sso_rw, "jwt-2");
        assert_eq!(c.cf_clearance, "cf-3");
        assert!(c.x_userid.is_none());
    }

    #[test]
    fn from_provider_missing_required_cookie_errors() {
        let p = provider_with_grok_web(json!({
            "grokWeb": {
                "cookies": {
                    "sso": "jwt-1",
                    "sso-rw": "jwt-2"
                    // cf_clearance 缺
                }
            }
        }));
        let err = GrokCookies::from_provider(&p).unwrap_err();
        match err {
            GrokAuthError::MissingCookie(k) => assert_eq!(k, "cf_clearance"),
            _ => panic!("expected MissingCookie"),
        }
    }

    #[test]
    fn cookie_header_concatenates_pairs() {
        let c = GrokCookies {
            sso: "a".into(),
            sso_rw: "b".into(),
            cf_clearance: "c".into(),
            x_userid: Some("d".into()),
            cf_bm: None,
            others: vec![("i18nextLng".into(), "zh".into())],
        };
        let h = c.to_cookie_header();
        assert!(h.contains("sso=a"));
        assert!(h.contains("sso-rw=b"));
        assert!(h.contains("cf_clearance=c"));
        assert!(h.contains("x-userid=d"));
        assert!(h.contains("i18nextLng=zh"));
    }

    #[test]
    fn apply_grok_headers_injects_full_set() {
        let p = provider_with_grok_web(json!({
            "grokWeb": {
                "cookies": {
                    "sso": "j1",
                    "sso-rw": "j2",
                    "cf_clearance": "c"
                },
                "statsigId": "stat-id"
            }
        }));
        let mut headers = HeaderMap::new();
        apply_grok_headers(&mut headers, &p).unwrap();
        assert!(headers.contains_key("cookie"));
        assert!(headers.contains_key("user-agent"));
        assert!(headers.contains_key("origin"));
        assert!(headers.contains_key("referer"));
        assert!(headers.contains_key("x-statsig-id"));
        assert!(headers.contains_key("x-xai-request-id"));
        assert!(headers.contains_key("traceparent"));
        assert_eq!(
            headers.get("x-statsig-id").unwrap().to_str().unwrap(),
            "stat-id"
        );
    }
}
