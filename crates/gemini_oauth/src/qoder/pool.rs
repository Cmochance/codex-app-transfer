//! QoderWork 多账号池 —— 复用通用 [`crate::account_pool`],只提供 QoderWork 特化。
//!
//! 存储/选择/失败转移全在 [`crate::account_pool`];本模块只:
//! 1. 实现 [`PoolBackend`](QoderWork 凭证字段抽取 + 续期);
//! 2. 导出以 `QoderBackend` 特化后的薄 wrapper(供 oauth handler / forward 调用)。

use crate::account_pool::{self, PoolBackend, RefreshOutcome};

pub use crate::account_pool::{PoolAccount, PoolError, PoolStorageError, ServingAccount};

use super::login::{refresh_qoder_token, QoderError};
use super::token::{merge_refreshed_cred, QoderCredential};
use super::{qoder_machine_id, user_id_from_jwt, uuid_v4};

/// QoderWork 的 [`PoolBackend`] 特化。
pub struct QoderBackend;

impl PoolBackend for QoderBackend {
    type Cred = QoderCredential;

    fn namespace() -> &'static str {
        "qoder"
    }

    /// 阶段一单账号文件(迁移源):`~/.codex-app-transfer/qoder-oauth.json`。
    fn legacy_single_filename() -> &'static str {
        "qoder-oauth.json"
    }

    fn cred_uid(cred: &Self::Cred) -> Option<String> {
        cred.uid.clone()
    }

    fn uid_from_token(cred: &Self::Cred) -> Option<String> {
        user_id_from_jwt(&cred.personal_token)
    }

    fn set_uid(cred: &mut Self::Cred, uid: String) {
        cred.uid = Some(uid);
    }

    /// QoderWork 的设备指纹 = `machine_id`(签名 `Cosy-MachineId` 用)。
    ///
    /// **per-account 设备隔离已达成**(`login.rs::run_qoder_login`):qoder 的 machine_id 必须在登录
    /// PKCE authUrl 阶段就绑进 device token,故只能登录时定;run_qoder_login 每次登录**新生成**一枚
    /// `uuid_v4` machine_id 并贯穿 authUrl+落盘+签名 → 池内各账号 `Cosy-MachineId` 互不相同,网关不把
    /// 多号看作同一设备。因此 machine_id 恒由 login 预填(`cred_fingerprint` 恒 `Some`),`add_account`
    /// 的 `new_fingerprint` 对 qoder 走不到——这是「指纹须在 authUrl 阶段就定、不能等 add_account 才生成」
    /// 所致(与 workbuddy 登录后才分配 device_id 不同),非缺陷。全局 `qoder_machine_id()` 仅留作
    /// refresh/migrate 兜底(老账号)。
    fn cred_fingerprint(cred: &Self::Cred) -> Option<String> {
        cred.machine_id.clone()
    }

    fn set_fingerprint(cred: &mut Self::Cred, fp: String) {
        cred.machine_id = Some(fp);
    }

    fn new_fingerprint() -> String {
        uuid_v4()
    }

    fn fingerprint_fallback() -> String {
        qoder_machine_id()
    }

    fn cred_nickname(cred: &Self::Cred) -> Option<String> {
        cred.nickname.clone()
    }

    /// 续期该账号 device token,返回可用 personal_token。
    /// 网关拒 refresh(Business)/ 响应缺字段(Parse)= 账号级 → 失败转移;
    /// Http / 存储 = 基础设施级 → 直接返回。
    async fn ensure_valid(
        http: &reqwest::Client,
        provider_id: &str,
        uid: &str,
    ) -> Result<String, RefreshOutcome> {
        let ns = Self::namespace();
        let cred = account_pool::load_account::<QoderCredential>(ns, provider_id, uid)
            .map_err(|e| RefreshOutcome::Infra(e.to_string()))?
            .ok_or_else(|| RefreshOutcome::AccountLevel("凭证丢失".into()))?;
        if !cred.should_refresh() {
            return Ok(cred.personal_token);
        }
        let machine_id = cred.machine_id.clone().unwrap_or_else(qoder_machine_id);
        match refresh_qoder_token(http, &cred.refresh_token, machine_id).await {
            Ok(fresh) => {
                // refresh 响应通常不回带 昵称/uid/machine_id/org/refresh_token → 逐字段用旧值兜底
                //(refresh_token 空保留旧值防 brick、machine_id 保 per-account 隔离、org 保签名缓存)。
                let fresh = merge_refreshed_cred(fresh, &cred);
                let token = fresh.personal_token.clone();
                account_pool::save_account(ns, provider_id, uid, &fresh)
                    .map_err(|e| RefreshOutcome::Infra(e.to_string()))?;
                Ok(token)
            }
            Err(e @ (QoderError::Business { .. } | QoderError::Parse(_))) => {
                RefreshOutcome::AccountLevel(e.to_string()).into_err()
            }
            Err(e) => RefreshOutcome::Infra(e.to_string()).into_err(),
        }
    }
}

// `RefreshOutcome` 无 Result 便捷构造,补个小助手让 match 分支简洁。
trait IntoErr {
    fn into_err(self) -> Result<String, RefreshOutcome>;
}
impl IntoErr for RefreshOutcome {
    fn into_err(self) -> Result<String, RefreshOutcome> {
        Err(self)
    }
}

// ── 薄 wrapper(QoderBackend 特化)──────────────────────────────────

/// 代理转发选服务账号(续期 + 失败转移)。返回 token=personal_token,fingerprint=machine_id。
pub async fn select_serving_account(
    http: &reqwest::Client,
    provider_id: &str,
) -> Result<ServingAccount, PoolError> {
    account_pool::select_serving_account::<QoderBackend>(http, provider_id).await
}

/// 登录成功后加账号入池。
pub fn add_account(provider_id: &str, cred: QoderCredential) -> Result<String, PoolError> {
    account_pool::add_account::<QoderBackend>(provider_id, cred)
}

/// 取指定账号完整凭证(签名复用登录时缓存的 uid/org,免每请求打账号级 `/userinfo`)。
pub fn load_account(
    provider_id: &str,
    uid: &str,
) -> Result<Option<QoderCredential>, PoolStorageError> {
    account_pool::load_account::<QoderCredential>(QoderBackend::namespace(), provider_id, uid)
}

/// 列池内账号摘要(UI)。
pub fn list_pool(provider_id: &str) -> Result<Vec<PoolAccount>, PoolStorageError> {
    account_pool::list_pool::<QoderBackend>(provider_id)
}

/// 标记账号耗尽(quota 守护)。
pub fn set_exhausted(provider_id: &str, uid: &str, until_ms: i64) -> Result<(), PoolStorageError> {
    account_pool::set_exhausted(QoderBackend::namespace(), provider_id, uid, until_ms)
}

/// 清除账号耗尽标记。
pub fn clear_exhausted(provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    account_pool::clear_exhausted(QoderBackend::namespace(), provider_id, uid)
}

/// 手动切换当前服务账号(UI)。
pub fn set_active(provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    account_pool::set_active(QoderBackend::namespace(), provider_id, uid)
}

/// 移除账号(UI)。
pub fn remove_account(provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    account_pool::remove_account(QoderBackend::namespace(), provider_id, uid)
}
