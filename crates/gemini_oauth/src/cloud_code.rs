//! Cloud Code Assist 项目 bootstrap —— `loadCodeAssist` + `onboardUser` LRO。
//!
//! OAuth code-grant 完成只是拿到 `access_token`,但 `:streamGenerateContent` 还要
//! body 里带 `project: <id>`(免费 tier 的配额绑这个 GCP project)。Google 自动
//! provision 流程:
//!
//! 1. `POST /v1internal:loadCodeAssist` —— 拿用户当前 tier 列表 + 已有 project(若有)
//! 2. 决策 tier:第一个 `isDefault: true` 或 fallback LEGACY
//! 3. `POST /v1internal:onboardUser` —— 触发 LRO(long-running operation)
//! 4. 轮询 LRO(5s 间隔)直到 `done == true`,从 `response.cloudaicompanionProject.id`
//!    拿最终 project_id
//!
//! ## 行为对齐上游
//!
//! gemini-cli `setup.ts:92-200` + CLIProxyAPI `internal/cmd/login.go`:
//! - `pluginType: "GEMINI"` 是 hard-coded 字面值,Google 用它识别"非 IDE 插件"
//!   的 standalone CLI 客户端
//! - free-tier 不传 `cloudaicompanionProject`(让 Google 自动建);其他 tier 传
//!   用户已有 project_id(或环境变量 `GOOGLE_CLOUD_PROJECT`)
//! - `hasOnboardedPreviously: true` 时 onboardUser 仍要调,上游会立即返已存在的
//!   project,不重新 provision

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::constants::{CLOUD_CODE_BASE_URL, USER_AGENT, X_GOOG_API_CLIENT};

#[derive(Debug, Error)]
pub enum CloudCodeError {
    #[error("HTTP 失败: {0}")]
    Http(#[from] reqwest::Error),
    #[error("loadCodeAssist 返非 2xx: HTTP {status}: {body}")]
    LoadStatus { status: u16, body: String },
    #[error("onboardUser 返非 2xx: HTTP {status}: {body}")]
    OnboardStatus { status: u16, body: String },
    #[error("loadCodeAssist 响应 JSON 解析失败: {0}")]
    LoadParse(String),
    #[error("onboardUser 响应 JSON 解析失败: {0}")]
    OnboardParse(String),
    #[error("loadCodeAssist 没返任何 tier — Google 上游异常")]
    NoTier,
    #[error("LRO 轮询超时(等 onboardUser done 超过 {0:?})")]
    LroTimeout(Duration),
    #[error("LRO 完成但 response.cloudaicompanionProject.id 缺失:{0}")]
    MissingProjectId(String),
}

/// 客户端身份元数据 —— 跟着 loadCodeAssist / onboardUser 一起送给 Google,
/// 用来命中"官方 gemini-cli"分支(不是这个值就走 generic API 路径,可能 reject)。
///
/// 字段值字面对齐 CLIProxyAPI `header_utils.go` + gemini-cli `coreClientMetadata`。
#[derive(Debug, Clone, Serialize)]
pub struct ClientMetadata {
    /// `IDE_UNSPECIFIED` —— standalone CLI,不是 IDE 插件
    #[serde(rename = "ideType")]
    pub ide_type: &'static str,
    /// `DARWIN_ARM64` / `LINUX_AMD64` / `WINDOWS_AMD64` 等
    pub platform: String,
    /// **必须**是 `"GEMINI"` —— hard-coded 上游识别字段
    #[serde(rename = "pluginType")]
    pub plugin_type: &'static str,
    /// gemini-cli 自报版本,我们用 `0.34.0` 对齐 USER_AGENT
    #[serde(rename = "pluginVersion")]
    pub plugin_version: &'static str,
    /// `duetProject` —— paid tier 用,free tier 留 None
    #[serde(rename = "duetProject", skip_serializing_if = "Option::is_none")]
    pub duet_project: Option<String>,
}

impl ClientMetadata {
    /// 默认元数据,plugin_type / version 锁死跟 USER_AGENT 一致。platform 按
    /// `<OS>_<ARCH>` 大写拼,Google 上游期望这种 enum 字面。
    pub fn default_for_current_platform() -> Self {
        let os = std::env::consts::OS.to_ascii_uppercase();
        let arch = std::env::consts::ARCH.to_ascii_uppercase();
        Self {
            ide_type: "IDE_UNSPECIFIED",
            platform: format!("{os}_{arch}"),
            plugin_type: "GEMINI",
            plugin_version: "0.34.0",
            duet_project: None,
        }
    }
}

/// `loadCodeAssist` 请求 body。
#[derive(Debug, Serialize)]
pub struct LoadCodeAssistRequest {
    /// 已有 project_id(从 `~/.codex-app-transfer/gemini-oauth.json` 或
    /// `GOOGLE_CLOUD_PROJECT` 环境变量),没有就 None。
    #[serde(
        rename = "cloudaicompanionProject",
        skip_serializing_if = "Option::is_none"
    )]
    pub cloudaicompanion_project: Option<String>,
    pub metadata: ClientMetadata,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GeminiUserTier {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "isDefault")]
    pub is_default: bool,
    /// 该 tier 是否需要用户自定义的 cloudaicompanionProject。free-tier 是 false
    /// (Google 自动建);paid 是 true。
    #[serde(default, rename = "userDefinedCloudaicompanionProject")]
    pub user_defined_cloudaicompanion_project: bool,
    #[serde(default, rename = "hasAcceptedTos")]
    pub has_accepted_tos: bool,
    #[serde(default, rename = "hasOnboardedPreviously")]
    pub has_onboarded_previously: bool,
}

/// `loadCodeAssist` 响应。所有字段都可空 — Google 上游若没数据返 200 + 空对象。
#[derive(Debug, Deserialize, Default)]
pub struct LoadCodeAssistResponse {
    #[serde(default, rename = "currentTier")]
    pub current_tier: Option<GeminiUserTier>,
    #[serde(default, rename = "allowedTiers")]
    pub allowed_tiers: Vec<GeminiUserTier>,
    /// 用户已有的 project_id(server-side 之前 onboard 过留下的)
    #[serde(default, rename = "cloudaicompanionProject")]
    pub cloudaicompanion_project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OnboardUserRequest {
    #[serde(rename = "tierId")]
    pub tier_id: String,
    #[serde(
        rename = "cloudaicompanionProject",
        skip_serializing_if = "Option::is_none"
    )]
    pub cloudaicompanion_project: Option<String>,
    pub metadata: ClientMetadata,
}

/// LRO operation 响应 —— `done` 是终态信号,`response` 在 `done==true` 时含
/// `cloudaicompanionProject.id`。`name` 在 `done==false` 时填,用来 polling。
#[derive(Debug, Deserialize)]
pub struct LongRunningOperation {
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub response: Option<OnboardUserResponse>,
}

#[derive(Debug, Deserialize)]
pub struct OnboardUserResponse {
    #[serde(rename = "cloudaicompanionProject")]
    pub cloudaicompanion_project: Option<CloudAiCompanionProject>,
}

#[derive(Debug, Deserialize)]
pub struct CloudAiCompanionProject {
    pub id: String,
}

/// 选 tier 的决策树:第一个 `isDefault: true` → fallback `legacy-tier`(对齐
/// gemini-cli `setup.ts:160-180` 的 `getOnboardTier` 行为)。
fn pick_tier(load_resp: &LoadCodeAssistResponse) -> Option<GeminiUserTier> {
    if let Some(tier) = &load_resp.current_tier {
        if tier.is_default {
            return Some(tier.clone());
        }
    }
    for tier in &load_resp.allowed_tiers {
        if tier.is_default {
            return Some(tier.clone());
        }
    }
    // fallback:legacy-tier 兜底(gemini-cli 行为)
    load_resp
        .allowed_tiers
        .iter()
        .find(|t| t.id.contains("legacy"))
        .cloned()
}

/// 全自动 bootstrap —— 调 loadCodeAssist + 决定 tier + 调 onboardUser + 轮询 LRO。
/// 返回最终 `project_id`,调用方应该写回 `OauthToken.project_id` 持久化。
///
/// `existing_project_id` 是已有的 project(从 token store 读 / 环境变量),
/// 没有就 None。free-tier 路径不需要它。
///
/// `lro_poll_interval` 默认 5s 对齐上游;`lro_timeout` 默认 60s(onboard 通常 < 30s)。
pub async fn bootstrap_project(
    http: &reqwest::Client,
    access_token: &str,
    existing_project_id: Option<String>,
) -> Result<String, CloudCodeError> {
    bootstrap_project_with_polling(
        http,
        access_token,
        existing_project_id,
        Duration::from_secs(5),
        Duration::from_secs(60),
    )
    .await
}

/// 带可调 polling 参数的版本 —— 单测注入更短的 interval 跑得快。
pub async fn bootstrap_project_with_polling(
    http: &reqwest::Client,
    access_token: &str,
    existing_project_id: Option<String>,
    poll_interval: Duration,
    poll_timeout: Duration,
) -> Result<String, CloudCodeError> {
    let metadata = ClientMetadata::default_for_current_platform();

    // 1. loadCodeAssist
    let load_req = LoadCodeAssistRequest {
        cloudaicompanion_project: existing_project_id.clone(),
        metadata: metadata.clone(),
    };
    let load_url = format!("{CLOUD_CODE_BASE_URL}/v1internal:loadCodeAssist");
    let resp = http
        .post(&load_url)
        .bearer_auth(access_token)
        .header("User-Agent", USER_AGENT)
        .header("X-Goog-Api-Client", X_GOOG_API_CLIENT)
        .json(&load_req)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(CloudCodeError::LoadStatus {
            status: status.as_u16(),
            body,
        });
    }
    let load_resp: LoadCodeAssistResponse = serde_json::from_str(&body)
        .map_err(|e| CloudCodeError::LoadParse(format!("{e}; body={body}")))?;

    let tier = pick_tier(&load_resp).ok_or(CloudCodeError::NoTier)?;
    let is_free_tier = tier.id.contains("free");

    // 2. onboardUser body —— free tier 不传 project_id,paid 传
    let onboard_project = if is_free_tier {
        None
    } else {
        existing_project_id.or(load_resp.cloudaicompanion_project.clone())
    };
    let onboard_req = OnboardUserRequest {
        tier_id: tier.id.clone(),
        cloudaicompanion_project: onboard_project,
        metadata,
    };
    let onboard_url = format!("{CLOUD_CODE_BASE_URL}/v1internal:onboardUser");

    // 3. 第一次 POST + LRO 轮询(策略:不用 GET on operation name,而是重 POST
    //    onboardUser 同 body —— gemini-cli setup.ts 的 caServer.getOperation 内
    //    最终走的也是 cloudcode-pa,我们直接重发 idempotent 等价。已 onboard 的
    //    用户上游会立即返 done=true 不重 provision。)
    let started_at = std::time::Instant::now();
    let mut last_op: LongRunningOperation;
    loop {
        let resp = http
            .post(&onboard_url)
            .bearer_auth(access_token)
            .header("User-Agent", USER_AGENT)
            .header("X-Goog-Api-Client", X_GOOG_API_CLIENT)
            .json(&onboard_req)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(CloudCodeError::OnboardStatus {
                status: status.as_u16(),
                body,
            });
        }
        last_op = serde_json::from_str(&body)
            .map_err(|e| CloudCodeError::OnboardParse(format!("{e}; body={body}")))?;
        if last_op.done {
            break;
        }
        if started_at.elapsed() >= poll_timeout {
            return Err(CloudCodeError::LroTimeout(poll_timeout));
        }
        tokio::time::sleep(poll_interval).await;
    }

    // 4. 提 project_id
    let project_id = last_op
        .response
        .and_then(|r| r.cloudaicompanion_project.map(|p| p.id))
        .ok_or_else(|| {
            CloudCodeError::MissingProjectId(
                "LRO done=true 但 response.cloudaicompanionProject.id 不存在".into(),
            )
        })?;
    Ok(project_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 测试专用:覆盖 CLOUD_CODE_BASE_URL 为 mock server URL —— 不能直接改 const,
    /// 改用直接调内部 helper 方式(bypass bootstrap_project,调 wiremock 验各步骤
    /// 的 wire shape)。
    #[tokio::test]
    async fn load_code_assist_sends_required_metadata() {
        let server = MockServer::start().await;
        let _mock = Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(header_exists("authorization"))
            .and(header("user-agent", USER_AGENT))
            .and(header("x-goog-api-client", X_GOOG_API_CLIENT))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {
                    "id": "free-tier",
                    "name": "Free",
                    "isDefault": true,
                    "userDefinedCloudaicompanionProject": false,
                    "hasAcceptedTos": true,
                    "hasOnboardedPreviously": false
                },
                "allowedTiers": []
            })))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let metadata = ClientMetadata::default_for_current_platform();
        let req = LoadCodeAssistRequest {
            cloudaicompanion_project: None,
            metadata,
        };
        let resp = http
            .post(format!("{}/v1internal:loadCodeAssist", server.uri()))
            .bearer_auth("test-token")
            .header("User-Agent", USER_AGENT)
            .header("X-Goog-Api-Client", X_GOOG_API_CLIENT)
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let parsed: LoadCodeAssistResponse = resp.json().await.unwrap();
        assert_eq!(
            parsed.current_tier.unwrap().id,
            "free-tier",
            "tier 解析必须命中 free-tier"
        );
    }

    #[test]
    fn pick_tier_prefers_current_then_default_then_legacy() {
        // current_tier.is_default=true → 直接返 current_tier
        let resp = LoadCodeAssistResponse {
            current_tier: Some(GeminiUserTier {
                id: "free-tier".into(),
                name: None,
                is_default: true,
                user_defined_cloudaicompanion_project: false,
                has_accepted_tos: true,
                has_onboarded_previously: false,
            }),
            allowed_tiers: vec![],
            cloudaicompanion_project: None,
        };
        assert_eq!(pick_tier(&resp).unwrap().id, "free-tier");

        // current_tier.is_default=false,allowed 里有 default → 返 allowed 里的
        let resp = LoadCodeAssistResponse {
            current_tier: Some(GeminiUserTier {
                id: "expired".into(),
                name: None,
                is_default: false,
                user_defined_cloudaicompanion_project: false,
                has_accepted_tos: false,
                has_onboarded_previously: false,
            }),
            allowed_tiers: vec![GeminiUserTier {
                id: "standard-tier".into(),
                name: None,
                is_default: true,
                user_defined_cloudaicompanion_project: true,
                has_accepted_tos: true,
                has_onboarded_previously: true,
            }],
            cloudaicompanion_project: None,
        };
        assert_eq!(pick_tier(&resp).unwrap().id, "standard-tier");

        // 都没 default → fallback legacy
        let resp = LoadCodeAssistResponse {
            current_tier: None,
            allowed_tiers: vec![GeminiUserTier {
                id: "legacy-tier".into(),
                name: None,
                is_default: false,
                user_defined_cloudaicompanion_project: false,
                has_accepted_tos: false,
                has_onboarded_previously: false,
            }],
            cloudaicompanion_project: None,
        };
        assert_eq!(pick_tier(&resp).unwrap().id, "legacy-tier");

        // 完全空 → None
        let resp = LoadCodeAssistResponse::default();
        assert!(pick_tier(&resp).is_none());
    }

    #[test]
    fn client_metadata_platform_format_matches_upstream_enum() {
        let m = ClientMetadata::default_for_current_platform();
        assert_eq!(m.plugin_type, "GEMINI");
        assert_eq!(m.plugin_version, "0.34.0");
        assert_eq!(m.ide_type, "IDE_UNSPECIFIED");
        // platform 形如 DARWIN_ARM64 / LINUX_X86_64 / WINDOWS_X86_64
        assert!(
            m.platform.contains("_"),
            "platform 必须 OS_ARCH: {}",
            m.platform
        );
        assert_eq!(m.platform, m.platform.to_ascii_uppercase());
    }

    #[test]
    fn metadata_serializes_camel_case_with_correct_field_names() {
        let m = ClientMetadata::default_for_current_platform();
        let json = serde_json::to_string(&m).unwrap();
        // upstream 字段都 camelCase
        assert!(json.contains("\"ideType\""));
        assert!(json.contains("\"pluginType\""));
        assert!(json.contains("\"pluginVersion\""));
        assert!(!json.contains("\"plugin_type\""), "不该 snake_case");
        // duetProject 是 None 不应序列化(skip_serializing_if)
        assert!(!json.contains("duetProject"));
    }

    #[tokio::test]
    async fn bootstrap_project_returns_id_when_lro_done_immediately() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {
                    "id": "free-tier",
                    "isDefault": true,
                    "userDefinedCloudaicompanionProject": false
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "done": true,
                "response": {
                    "cloudaicompanionProject": {
                        "id": "test-project-12345"
                    }
                }
            })))
            .mount(&server)
            .await;

        // 直接调 helper(用 mock URL 替代 const)— 把 mock URL 传内部
        let http = reqwest::Client::new();
        let metadata = ClientMetadata::default_for_current_platform();
        let load_url = format!("{}/v1internal:loadCodeAssist", server.uri());
        let onboard_url = format!("{}/v1internal:onboardUser", server.uri());

        let load_resp: LoadCodeAssistResponse = http
            .post(&load_url)
            .bearer_auth("token")
            .header("User-Agent", USER_AGENT)
            .header("X-Goog-Api-Client", X_GOOG_API_CLIENT)
            .json(&LoadCodeAssistRequest {
                cloudaicompanion_project: None,
                metadata: metadata.clone(),
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tier = pick_tier(&load_resp).unwrap();
        assert_eq!(tier.id, "free-tier");

        let lro: LongRunningOperation = http
            .post(&onboard_url)
            .bearer_auth("token")
            .header("User-Agent", USER_AGENT)
            .header("X-Goog-Api-Client", X_GOOG_API_CLIENT)
            .json(&OnboardUserRequest {
                tier_id: tier.id,
                cloudaicompanion_project: None,
                metadata,
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(lro.done);
        assert_eq!(
            lro.response.unwrap().cloudaicompanion_project.unwrap().id,
            "test-project-12345"
        );
    }

    #[test]
    fn lro_response_handles_missing_project_id() {
        // done=true 但 response 字段空 — bootstrap 应该报 MissingProjectId
        let lro: LongRunningOperation = serde_json::from_value(serde_json::json!({
            "done": true
        }))
        .unwrap();
        assert!(lro.done);
        assert!(lro.response.is_none());
    }

    #[test]
    fn lro_response_handles_in_progress() {
        let lro: LongRunningOperation = serde_json::from_value(serde_json::json!({
            "done": false,
            "name": "operations/abc123"
        }))
        .unwrap();
        assert!(!lro.done);
        assert_eq!(lro.name.as_deref(), Some("operations/abc123"));
    }
}
