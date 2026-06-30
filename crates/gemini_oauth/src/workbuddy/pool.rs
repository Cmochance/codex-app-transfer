//! WorkBuddy **账号池**选择器 —— 单 provider 多账号 + 额度守护自动切换。
//!
//! 一个 `workbuddy-login` provider 维护一个账号池([`token`] 的 `<provider_id>/<uid>.json`)。
//! 代理转发时 [`select_serving_account`] 选一个「当前服务账号」并自动续期 token;某账号被
//! 额度守护 / 反应式失败转移标记 `exhausted_until`(见 [`set_exhausted`])后,选择器自动跳到
//! 下一个可用账号(sticky:不切则一直用同一个,直到它被标记耗尽)。
//!
//! **额度守护本身**(总剩余 < 阈值 → 标记耗尽)在 quota 守护进程算,这里只消费 `exhausted_until`。
//! **每账号独立 device_id**(登录生成、存账号文件)由 [`add_account`] 分配,避免多账号同设备被风控关联。

use super::token::{
    self, unix_now_ms, PoolState, WorkbuddyCredential, WorkbuddyCredentialStore, WorkbuddyPoolStore,
};
use super::{ensure_valid_workbuddy_token, user_id_from_jwt, uuid_v4, WorkbuddyError};

/// 选中的服务账号 —— 代理转发用。
#[derive(Debug, Clone)]
pub struct ServingAccount {
    pub uid: String,
    /// 已续期的有效 access token。
    pub token: String,
    /// 本账号专属 `X-Device-Id`。
    pub device_id: String,
}

/// 池内账号摘要(UI / quota 守护用)。
#[derive(Debug, Clone)]
pub struct PoolAccount {
    pub uid: String,
    pub nickname: Option<String>,
    pub device_id: Option<String>,
    /// 是否为当前 sticky 服务账号。
    pub is_active: bool,
    /// 耗尽到期 UNIX ms(0 = 未标记);> now 即当前不可选。
    pub exhausted_until: i64,
}

/// **纯函数**选服务账号:sticky active(若可用)→ 否则可用集第一个(uid 排序稳定)→
/// 全耗尽则最快解禁的那个(尽力,大概率仍失败但比不发好)。可单测,无 IO。
///
/// `now_ms` 当前时刻;`exhausted_until[uid] > now` 视为不可选。
pub fn choose_serving_uid(uids: &[String], pool: &PoolState, now_ms: i64) -> Option<String> {
    if uids.is_empty() {
        return None;
    }
    let exhausted_at = |uid: &str| pool.exhausted_until.get(uid).copied().unwrap_or(0);
    let mut available: Vec<&String> = uids.iter().filter(|u| exhausted_at(u) <= now_ms).collect();
    available.sort(); // 稳定顺序(防每次选不同账号抖动)
    if let Some(active) = pool.active_uid.as_deref() {
        if available.iter().any(|u| u.as_str() == active) {
            return Some(active.to_string());
        }
    }
    if let Some(first) = available.first() {
        return Some((*first).clone());
    }
    // 全耗尽 → 选最快解禁的(exhausted_until 最小)
    uids.iter().min_by_key(|u| exhausted_at(u)).cloned()
}

/// 代理转发入口:迁移老凭证 → 选服务账号 → 续期 token → 返回 token + 该账号 device_id。
/// 池空(无账号)→ `NotLoggedIn`(forward 据此提示登录)。
pub async fn select_serving_account(
    http: &reqwest::Client,
    provider_id: &str,
) -> Result<ServingAccount, WorkbuddyError> {
    migrate_legacy_if_needed(provider_id)?;
    let accounts = token::list_accounts(provider_id)?;
    let uids: Vec<String> = accounts.iter().filter_map(|c| c.uid.clone()).collect();
    if uids.is_empty() {
        return Err(WorkbuddyError::NotLoggedIn);
    }
    let pool_store = WorkbuddyPoolStore::for_provider(provider_id)?;
    let mut pool = pool_store.load()?;
    let chosen =
        choose_serving_uid(&uids, &pool, unix_now_ms()).ok_or(WorkbuddyError::NotLoggedIn)?;
    // sticky 切换才落盘(避免每请求写 _pool.json)
    if pool.active_uid.as_deref() != Some(chosen.as_str()) {
        pool.active_uid = Some(chosen.clone());
        pool_store.save(&pool)?;
    }
    let store = WorkbuddyCredentialStore::for_account(provider_id, &chosen)?;
    let cred = store.load()?.ok_or(WorkbuddyError::NotLoggedIn)?;
    let token = ensure_valid_workbuddy_token(http, &store).await?;
    // device_id 正常恒 Some(add_account/迁移已补);老凭证兜底全局 device-id。
    let device_id = cred
        .device_id
        .clone()
        .unwrap_or_else(super::workbuddy_device_id);
    Ok(ServingAccount {
        uid: chosen,
        token,
        device_id,
    })
}

/// 登录成功后**加账号入池**:分配/复用本账号 device_id → 写 `<uid>.json` → 更新池
/// (首账号设 active;清该账号 exhausted)。返回 uid。重登同 uid = 更新(保留 device_id 稳定)。
pub fn add_account(
    provider_id: &str,
    mut cred: WorkbuddyCredential,
) -> Result<String, WorkbuddyError> {
    let uid = cred
        .uid
        .clone()
        .or_else(|| user_id_from_jwt(&cred.access_token))
        .ok_or(WorkbuddyError::Parse("uid"))?;
    cred.uid = Some(uid.clone());
    // device_id:重登复用该 uid 既有账号的(设备指纹保持稳定),首登新生成 v4 UUID。
    let existing_dev = WorkbuddyCredentialStore::for_account(provider_id, &uid)?
        .load()?
        .and_then(|c| c.device_id);
    cred.device_id = cred.device_id.or(existing_dev).or_else(|| Some(uuid_v4()));
    WorkbuddyCredentialStore::for_account(provider_id, &uid)?.save(&cred)?;

    let pool_store = WorkbuddyPoolStore::for_provider(provider_id)?;
    let mut pool = pool_store.load()?;
    if pool.active_uid.is_none() {
        pool.active_uid = Some(uid.clone());
    }
    pool.exhausted_until.remove(&uid); // 新登录的账号一定可用
    pool_store.save(&pool)?;
    Ok(uid)
}

/// 标记账号耗尽到 `until_ms`(配额守护:剩余<阈值 → 该账号会刷新的包 CycleEndTime;
/// 反应式失败转移:now + 短退避)。`until_ms <= now` 等价清除。
pub fn set_exhausted(provider_id: &str, uid: &str, until_ms: i64) -> Result<(), WorkbuddyError> {
    let store = WorkbuddyPoolStore::for_provider(provider_id)?;
    let mut pool = store.load()?;
    if until_ms <= unix_now_ms() {
        pool.exhausted_until.remove(uid);
    } else {
        pool.exhausted_until.insert(uid.to_string(), until_ms);
    }
    store.save(&pool)?;
    Ok(())
}

/// 清除账号耗尽标记(配额守护:剩余恢复到阈值上)。
pub fn clear_exhausted(provider_id: &str, uid: &str) -> Result<(), WorkbuddyError> {
    set_exhausted(provider_id, uid, 0)
}

/// 手动指定当前服务账号(UI 切换)。不存在的 uid 也允许写(下次 select 校验回退)。
pub fn set_active(provider_id: &str, uid: &str) -> Result<(), WorkbuddyError> {
    let store = WorkbuddyPoolStore::for_provider(provider_id)?;
    let mut pool = store.load()?;
    pool.active_uid = Some(uid.to_string());
    store.save(&pool)?;
    Ok(())
}

/// 移除账号(UI):删账号文件 + 从池态摘除;若删的是 active 则清 active(下次 select 现选)。
pub fn remove_account(provider_id: &str, uid: &str) -> Result<(), WorkbuddyError> {
    WorkbuddyCredentialStore::for_account(provider_id, uid)?.delete()?;
    let store = WorkbuddyPoolStore::for_provider(provider_id)?;
    let mut pool = store.load()?;
    pool.exhausted_until.remove(uid);
    if pool.active_uid.as_deref() == Some(uid) {
        pool.active_uid = None;
    }
    store.save(&pool)?;
    Ok(())
}

/// 列池内账号摘要(UI / quota 守护)。
pub fn list_pool(provider_id: &str) -> Result<Vec<PoolAccount>, WorkbuddyError> {
    migrate_legacy_if_needed(provider_id)?;
    let accounts = token::list_accounts(provider_id)?;
    let pool = WorkbuddyPoolStore::for_provider(provider_id)?.load()?;
    let active = pool.active_uid.as_deref();
    Ok(accounts
        .into_iter()
        .filter_map(|c| {
            let uid = c.uid.clone()?;
            Some(PoolAccount {
                is_active: active == Some(uid.as_str()),
                exhausted_until: pool.exhausted_until.get(&uid).copied().unwrap_or(0),
                nickname: c.nickname,
                device_id: c.device_id,
                uid,
            })
        })
        .collect())
}

/// 一次性迁移:老「单文件单账号」(`workbuddy-oauth.json` + 全局 `workbuddy-device-id`)
/// → 池首账号 `<provider_id>/<uid>.json`。仅当**本 provider 池为空且老单文件存在**时执行,
/// 把老全局 device-id 绑给该账号(保持其设备指纹不变),迁完删老单文件。失败不致命(吞掉,
/// 用户重登即可),不阻塞正常流程。
fn migrate_legacy_if_needed(provider_id: &str) -> Result<(), WorkbuddyError> {
    if !token::list_accounts(provider_id)?.is_empty() {
        return Ok(()); // 已有账号,不迁
    }
    let legacy = WorkbuddyCredentialStore::legacy_single()?;
    let Some(mut cred) = legacy.load()? else {
        return Ok(()); // 无老单文件
    };
    let uid = cred
        .uid
        .clone()
        .or_else(|| user_id_from_jwt(&cred.access_token));
    let Some(uid) = uid else {
        return Ok(()); // 解不出 uid 不迁(让用户重登)
    };
    cred.uid = Some(uid.clone());
    // 老账号沿用全局 device-id(它一直用这个设备指纹)。
    cred.device_id = cred
        .device_id
        .clone()
        .or_else(|| Some(super::workbuddy_device_id()));
    WorkbuddyCredentialStore::for_account(provider_id, &uid)?.save(&cred)?;
    let pool_store = WorkbuddyPoolStore::for_provider(provider_id)?;
    let mut pool = pool_store.load()?;
    pool.active_uid = Some(uid);
    pool_store.save(&pool)?;
    let _ = legacy.delete(); // 删老单文件(失败无妨)
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_with(active: Option<&str>, exhausted: &[(&str, i64)]) -> PoolState {
        let mut p = PoolState {
            active_uid: active.map(str::to_string),
            ..Default::default()
        };
        for (u, t) in exhausted {
            p.exhausted_until.insert(u.to_string(), *t);
        }
        p
    }

    #[test]
    fn empty_pool_returns_none() {
        assert_eq!(choose_serving_uid(&[], &PoolState::default(), 1000), None);
    }

    #[test]
    fn sticky_keeps_active_when_available() {
        let uids = vec!["a".to_string(), "b".to_string()];
        let p = pool_with(Some("b"), &[]);
        assert_eq!(choose_serving_uid(&uids, &p, 1000).as_deref(), Some("b"));
    }

    #[test]
    fn switches_off_exhausted_active() {
        let uids = vec!["a".to_string(), "b".to_string()];
        // active=a 但 a 耗尽到 5000(> now 1000)→ 切到 b
        let p = pool_with(Some("a"), &[("a", 5000)]);
        assert_eq!(choose_serving_uid(&uids, &p, 1000).as_deref(), Some("b"));
    }

    #[test]
    fn exhaustion_expires_after_until() {
        let uids = vec!["a".to_string(), "b".to_string()];
        let p = pool_with(Some("a"), &[("a", 5000)]);
        // now=6000 > 5000 → a 解禁,sticky 回 a
        assert_eq!(choose_serving_uid(&uids, &p, 6000).as_deref(), Some("a"));
    }

    #[test]
    fn no_active_picks_first_available_sorted() {
        let uids = vec!["b".to_string(), "a".to_string()];
        let p = pool_with(None, &[]);
        assert_eq!(
            choose_serving_uid(&uids, &p, 1000).as_deref(),
            Some("a"),
            "uid 排序后取首个,稳定"
        );
    }

    #[test]
    fn all_exhausted_picks_soonest_to_recover() {
        let uids = vec!["a".to_string(), "b".to_string()];
        // 全耗尽:a 到 9000,b 到 7000 → 选 b(最快解禁)
        let p = pool_with(Some("a"), &[("a", 9000), ("b", 7000)]);
        assert_eq!(choose_serving_uid(&uids, &p, 1000).as_deref(), Some("b"));
    }
}
