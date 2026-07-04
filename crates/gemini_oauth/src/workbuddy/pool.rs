//! WorkBuddy 多账号池 —— 复用通用 [`crate::account_pool`],只提供 WorkBuddy 特化 +
//! 保留原对外 API(`ServingAccount{uid,token,device_id}` / `PoolAccount` / 各操作)。
//!
//! 选择/失败转移/存储全在 [`crate::account_pool`];本模块只实现 [`PoolBackend`]
//! (WorkBuddy 凭证字段抽取 + 续期)+ 把泛型结果 map 回 WorkBuddy 既有形态。

use crate::account_pool::{self, PoolBackend, PoolError, RefreshOutcome};

use super::login::{ensure_valid_workbuddy_token, WorkbuddyError};
use super::token::{WorkbuddyCredential, WorkbuddyCredentialStore, WorkbuddyTokenError};
use super::{account_display_from_jwt, user_id_from_jwt, uuid_v4, workbuddy_device_id};

/// WorkBuddy 的 [`PoolBackend`] 特化。
pub struct WorkbuddyBackend;

impl PoolBackend for WorkbuddyBackend {
    type Cred = WorkbuddyCredential;

    fn namespace() -> &'static str {
        "workbuddy"
    }

    fn legacy_single_filename() -> &'static str {
        "workbuddy-oauth.json"
    }

    fn cred_uid(cred: &Self::Cred) -> Option<String> {
        cred.uid.clone()
    }

    fn uid_from_token(cred: &Self::Cred) -> Option<String> {
        user_id_from_jwt(&cred.access_token)
    }

    fn set_uid(cred: &mut Self::Cred, uid: String) {
        cred.uid = Some(uid);
    }

    /// WorkBuddy 的设备指纹 = `device_id`(`X-Device-Id` 用)。
    fn cred_fingerprint(cred: &Self::Cred) -> Option<String> {
        cred.device_id.clone()
    }

    fn set_fingerprint(cred: &mut Self::Cred, fp: String) {
        cred.device_id = Some(fp);
    }

    fn new_fingerprint() -> String {
        uuid_v4()
    }

    fn fingerprint_fallback() -> String {
        workbuddy_device_id()
    }

    fn cred_nickname(cred: &Self::Cred) -> Option<String> {
        cred.nickname.clone()
    }

    fn cred_display(cred: &Self::Cred) -> Option<String> {
        account_display_from_jwt(&cred.access_token)
    }

    /// 续期该账号 access token。网关拒 refresh(Business)/ 凭证丢失(NotLoggedIn)= 账号级
    /// → 失败转移;Http / 存储 = 基础设施级 → 直接返回。
    async fn ensure_valid(
        http: &reqwest::Client,
        provider_id: &str,
        uid: &str,
    ) -> Result<String, RefreshOutcome> {
        let store = WorkbuddyCredentialStore::for_account(provider_id, uid)
            .map_err(|e| RefreshOutcome::Infra(e.to_string()))?;
        match ensure_valid_workbuddy_token(http, &store).await {
            Ok(token) => Ok(token),
            Err(e @ (WorkbuddyError::Business { .. } | WorkbuddyError::NotLoggedIn)) => {
                Err(RefreshOutcome::AccountLevel(e.to_string()))
            }
            Err(e) => Err(RefreshOutcome::Infra(e.to_string())),
        }
    }
}

// ── 保留 WorkBuddy 既有对外形态 ─────────────────────────────────────

/// 选中的服务账号 —— 代理转发用(字段名保持 `token`/`device_id`,forward.rs 依赖)。
#[derive(Debug, Clone)]
pub struct ServingAccount {
    pub uid: String,
    pub token: String,
    pub device_id: String,
}

/// 池内账号摘要(UI / quota 守护)。
#[derive(Debug, Clone)]
pub struct PoolAccount {
    pub uid: String,
    pub nickname: Option<String>,
    pub display: Option<String>,
    pub device_id: Option<String>,
    pub is_active: bool,
    pub exhausted_until: i64,
}

/// 泛型池错误 → WorkbuddyError(`classify_workbuddy_service_error` 只区分 needs_login:
/// NotLoggedIn / Business / Token(Serde);其余为基础设施错误 needs_login=false)。
fn map_pool_err(e: PoolError) -> WorkbuddyError {
    match e {
        PoolError::NotLoggedIn => WorkbuddyError::NotLoggedIn,
        PoolError::Account(msg) => WorkbuddyError::Business { code: -1, msg },
        PoolError::Infra(msg) => {
            WorkbuddyError::Token(WorkbuddyTokenError::Io(std::io::Error::other(msg)))
        }
        PoolError::Storage(s) => WorkbuddyError::Token(s.into()),
    }
}

/// 代理转发选服务账号(续期 + 失败转移)。
pub async fn select_serving_account(
    http: &reqwest::Client,
    provider_id: &str,
) -> Result<ServingAccount, WorkbuddyError> {
    let a = account_pool::select_serving_account::<WorkbuddyBackend>(http, provider_id)
        .await
        .map_err(map_pool_err)?;
    Ok(ServingAccount {
        uid: a.uid,
        token: a.serving_token,
        device_id: a.fingerprint,
    })
}

/// 登录成功后加账号入池。
pub fn add_account(provider_id: &str, cred: WorkbuddyCredential) -> Result<String, WorkbuddyError> {
    account_pool::add_account::<WorkbuddyBackend>(provider_id, cred).map_err(map_pool_err)
}

/// 列池内账号摘要(UI)。
pub fn list_pool(provider_id: &str) -> Result<Vec<PoolAccount>, WorkbuddyError> {
    let items = account_pool::list_pool::<WorkbuddyBackend>(provider_id)
        .map_err(|e| map_pool_err(PoolError::Storage(e)))?;
    Ok(items
        .into_iter()
        .map(|p| PoolAccount {
            uid: p.uid,
            nickname: p.nickname,
            display: p.display,
            device_id: p.fingerprint,
            is_active: p.is_active,
            exhausted_until: p.exhausted_until,
        })
        .collect())
}

/// 标记账号耗尽(quota 守护)。
pub fn set_exhausted(provider_id: &str, uid: &str, until_ms: i64) -> Result<(), WorkbuddyError> {
    account_pool::set_exhausted("workbuddy", provider_id, uid, until_ms)
        .map_err(|e| map_pool_err(PoolError::Storage(e)))
}

/// 清除账号耗尽标记。
pub fn clear_exhausted(provider_id: &str, uid: &str) -> Result<(), WorkbuddyError> {
    account_pool::clear_exhausted("workbuddy", provider_id, uid)
        .map_err(|e| map_pool_err(PoolError::Storage(e)))
}

/// 手动切换当前服务账号(UI)。
pub fn set_active(provider_id: &str, uid: &str) -> Result<(), WorkbuddyError> {
    account_pool::set_active("workbuddy", provider_id, uid)
        .map_err(|e| map_pool_err(PoolError::Storage(e)))
}

/// 移除账号(UI)。
pub fn remove_account(provider_id: &str, uid: &str) -> Result<(), WorkbuddyError> {
    account_pool::remove_account("workbuddy", provider_id, uid)
        .map_err(|e| map_pool_err(PoolError::Storage(e)))
}
