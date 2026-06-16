//! z.ai / bigmodel(GLM **Coding Plan** 账号登录)OAuth provider。
//!
//! 跟 [`super::antigravity`] **并行**:同样是「浏览器登录一次 → 本地持久化凭证 →
//! 后续请求复用,免手填 API key」,但面向**智谱 GLM Coding Plan 订阅**,且是
//! 不同 vendor 的 wire(JSON 信封 token 交换 + 多步换组织 key + Anthropic Messages
//! 模型面),所以独立一套常量 / flow / token store。借鉴 ZCode 3.1.0 解包,见
//! [`constants`] 的 wire 对照表。
//!
//! ## 端到端流程([`run_zai_login`])
//!
//! 1. [`flow::run_zai_oauth_flow_with_cancel`] — loopback 回调 + JSON token 交换
//!    → 拿到 ZCode 业务 JWT + provider access_token
//! 2. z.ai 多一步:[`coding_plan::fetch_business_token`] 把 provider access_token
//!    换成业务 access_token(bigmodel 跳过,直接用 provider access_token)
//! 3. [`coding_plan::resolve_org_api_key`] — 换出组织 API key(`<apiKey>.<secretKey>`)
//! 4. 组装 [`token::ZaiCredential`] 落盘 `~/.codex-app-transfer/<provider>-oauth.json`
//!
//! 换出的组织 key 由 proxy `forward.rs`(Stage 2)当 `Authorization: Bearer` 注入,
//! 打 [`constants::ZaiProviderConfig::model_base`] 的 Anthropic Messages wire。

pub mod coding_plan;
pub mod constants;
pub mod flow;
pub mod token;

use thiserror::Error;

use super::flow::{FlowError, OauthFlowConfig};
pub use constants::{ZaiProvider, ZaiProviderConfig};
pub use token::{ZaiCredential, ZaiCredentialStore};

/// z.ai / bigmodel 登录链路的统一错误。loopback / state / 超时 / 取消等流程级错误
/// 复用 gemini [`FlowError`];其余是 ZCode 业务面特有(信封业务码、缺字段、换 key)。
#[derive(Debug, Error)]
pub enum ZaiError {
    #[error("OAuth loopback/授权流程错误: {0}")]
    Flow(#[from] FlowError),
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ZCode 端返非 2xx: HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("响应 JSON 解析失败: {0}")]
    Parse(String),
    #[error("ZCode 业务响应拒绝 (code={code}): {msg}")]
    Business { code: i64, msg: String },
    #[error("响应缺少必需字段: {0}")]
    MissingField(&'static str),
    #[error("换组织 API key 失败: {0}")]
    KeyResolution(String),
    #[error("凭证持久化失败: {0}")]
    Token(#[from] super::token::TokenError),
}

/// 跑完整 z.ai / bigmodel 账号登录,成功后**已落盘** [`ZaiCredential`] 并返回。
///
/// `cancel`:可选取消信号(UI 关窗 / app 退出 / 新登录抢占),透传给 loopback flow。
pub async fn run_zai_login(
    http: &reqwest::Client,
    provider: ZaiProvider,
    flow_config: &OauthFlowConfig,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<ZaiCredential, ZaiError> {
    let config = provider.config();

    // 1. OAuth → zcode_jwt + provider access_token
    let exchange = flow::run_zai_oauth_flow_with_cancel(http, &config, flow_config, cancel).await?;

    // 2. 决定换 key 用的 biz Bearer
    let provider_at = exchange
        .provider_access_token
        .clone()
        .ok_or(ZaiError::MissingField("provider access_token"))?;
    let biz_bearer = match config.business_login_url {
        // z.ai:provider access_token 先换业务 token
        Some(_) => coding_plan::fetch_business_token(http, &config, &provider_at).await?,
        // bigmodel:直接用 provider access_token
        None => provider_at,
    };

    // 3. 换组织 API key
    let org_api_key = coding_plan::resolve_org_api_key(http, &config, &biz_bearer).await?;

    // 4. 组装凭证 + 落盘
    let cred = ZaiCredential {
        provider,
        org_api_key,
        zcode_jwt: exchange.zcode_jwt,
        provider_access_token: exchange.provider_access_token,
        email: exchange.email,
        obtained_at_ms: flow::unix_now_ms(),
    };
    let store = ZaiCredentialStore::for_provider(provider)?;
    // 落盘失败加 error 日志带 path(对齐 gemini service::persist_token 的 M3 修:
    // 裸 `?` 只有 TokenError Display,丢了 path 上下文 + 「重启后会被当未登录」提示)
    if let Err(e) = store.save(&cred) {
        tracing::error!(
            error = %e,
            provider = provider.wire_id(),
            path = %store.path().display(),
            "z.ai 凭证落盘失败 — 用户重启后会被当成未登录"
        );
        return Err(e.into());
    }
    tracing::info!(
        provider = provider.wire_id(),
        email = cred.email.as_deref().unwrap_or(""),
        path = %store.path().display(),
        "z.ai 账号登录完成,组织 key 已落盘"
    );
    Ok(cred)
}
