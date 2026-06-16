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
//! 2. **安全网**:浏览器 OAuth 授权(消耗登录的部分)一旦成功,立即把 token 落盘
//!    [`token::ZaiPendingTokens`](`<provider>-oauth-pending.json`)。之后换 key 失败
//!    可用 [`resume_zai_login`] **不重走浏览器**地重试(限 token 有效期内)。
//! 3. z.ai 多一步:[`coding_plan::fetch_business_token`] 把 provider access_token
//!    换成业务 access_token(bigmodel 跳过,直接用 provider access_token)
//! 4. [`coding_plan::resolve_org_api_key`] — 换出组织 API key(`<apiKey>.<secretKey>`)
//! 5. 组装 [`token::ZaiCredential`] 落盘 `~/.codex-app-transfer/<provider>-oauth.json`,
//!    删除 pending 文件(安全网使命完成)
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
pub use token::{ZaiCredential, ZaiCredentialStore, ZaiPendingStore, ZaiPendingTokens};

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
/// **安全网**:浏览器 OAuth 一旦成功就先落盘 [`ZaiPendingTokens`],之后换组织 key
/// 失败时 pending 文件保留 —— 可用 [`resume_zai_login`] 不重走浏览器地重试。
///
/// `cancel`:可选取消信号(UI 关窗 / app 退出 / 新登录抢占),透传给 loopback flow。
pub async fn run_zai_login(
    http: &reqwest::Client,
    provider: ZaiProvider,
    flow_config: &OauthFlowConfig,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<ZaiCredential, ZaiError> {
    let config = provider.config();

    // 1. OAuth → zcode_jwt + provider access_token(浏览器授权 = 消耗登录的部分)
    let exchange = flow::run_zai_oauth_flow_with_cancel(http, &config, flow_config, cancel).await?;
    let provider_at = exchange
        .provider_access_token
        .clone()
        .ok_or(ZaiError::MissingField("provider access_token"))?;

    // 2. 安全网:授权已成功,立即把 token 落盘 pending —— 之后换 key 失败也能用
    //    resume_zai_login 不重走浏览器地重试。落盘失败不致命(只是少了安全网),warn 继续。
    let pending = ZaiPendingTokens {
        provider,
        zcode_jwt: exchange.zcode_jwt.clone(),
        provider_access_token: provider_at,
        email: exchange.email.clone(),
        obtained_at_ms: flow::unix_now_ms(),
    };
    match ZaiPendingStore::for_provider(provider).and_then(|s| s.save(&pending)) {
        Ok(()) => tracing::info!(
            provider = provider.wire_id(),
            "OAuth 授权成功,token 已存 pending(安全网就绪)"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            provider = provider.wire_id(),
            "pending token 落盘失败,安全网缺失但继续换 key"
        ),
    }

    // 3-5. 换 key → 组装完整凭证 → 落盘 + 删 pending
    finalize_from_pending(http, &config, &pending).await
}

/// 用已存的 [`ZaiPendingTokens`] **不重走浏览器**地续传换组织 key(浏览器授权
/// 成功但换 key 那步失败后,修复 / 重试用)。需 `provider_access_token` 仍在有效期内。
pub async fn resume_zai_login(
    http: &reqwest::Client,
    provider: ZaiProvider,
) -> Result<ZaiCredential, ZaiError> {
    let config = provider.config();
    let pending =
        ZaiPendingStore::for_provider(provider)?
            .load()?
            .ok_or(ZaiError::KeyResolution(
                "没有可续传的 pending 登录(请先走一次浏览器登录)".into(),
            ))?;
    tracing::info!(
        provider = provider.wire_id(),
        "从 pending token 续传换组织 key(不重走浏览器)"
    );
    finalize_from_pending(http, &config, &pending).await
}

/// 共享收尾:从 [`ZaiPendingTokens`] 换组织 key → 组装完整 [`ZaiCredential`] 落盘 →
/// 删 pending 文件。`run_zai_login` 与 `resume_zai_login` 共用。
async fn finalize_from_pending(
    http: &reqwest::Client,
    config: &ZaiProviderConfig,
    pending: &ZaiPendingTokens,
) -> Result<ZaiCredential, ZaiError> {
    let provider = pending.provider;

    // 决定换 key 用的 biz Bearer(z.ai 先换业务 token;bigmodel 直接用 provider token)
    let biz_bearer = match config.business_login_url {
        Some(_) => {
            coding_plan::fetch_business_token(http, config, &pending.provider_access_token).await?
        }
        None => pending.provider_access_token.clone(),
    };

    // 换组织 API key
    let org_api_key = coding_plan::resolve_org_api_key(http, config, &biz_bearer).await?;

    // 组装完整凭证 + 落盘(失败加 error 日志带 path,对齐 gemini M3 修)
    let cred = ZaiCredential {
        provider,
        org_api_key,
        zcode_jwt: pending.zcode_jwt.clone(),
        provider_access_token: Some(pending.provider_access_token.clone()),
        email: pending.email.clone(),
        obtained_at_ms: pending.obtained_at_ms,
    };
    let store = ZaiCredentialStore::for_provider(provider)?;
    if let Err(e) = store.save(&cred) {
        tracing::error!(
            error = %e,
            provider = provider.wire_id(),
            path = %store.path().display(),
            "z.ai 凭证落盘失败 — 用户重启后会被当成未登录"
        );
        return Err(e.into());
    }

    // 完整凭证已落盘 → 删 pending(安全网使命完成);删失败只 warn,不影响登录结果
    if let Ok(ps) = ZaiPendingStore::for_provider(provider) {
        if let Err(e) = ps.delete() {
            tracing::warn!(error = %e, "删除 pending token 失败(不影响登录结果)");
        }
    }
    tracing::info!(
        provider = provider.wire_id(),
        email = cred.email.as_deref().unwrap_or(""),
        path = %store.path().display(),
        "z.ai 账号登录完成,组织 key 已落盘"
    );
    Ok(cred)
}
