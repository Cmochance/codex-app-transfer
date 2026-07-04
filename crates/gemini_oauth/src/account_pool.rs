//! 通用「单 provider 多账号池」—— 独立登录 provider(workbuddy / qoder …)共用。
//!
//! 抽出三块共性逻辑,provider 只提供凭证类型 + [`PoolBackend`] 特化:
//! 1. **存储**:每账号一文件 `<ns>/<provider_id>/accounts/<uid>.json`(凭证含**本账号
//!    专属设备指纹**,网关看作独立设备,避免多账号同设备被风控关联)+ 池态
//!    `<ns>/<provider_id>/_pool.json`(active_uid + 每账号 exhausted_until)。
//! 2. **纯选择器** [`choose_serving_uid`]:sticky active(可用则不切)→ 否则可用集首个
//!    (uid 排序稳定)→ 全耗尽则最快解禁的那个。无 IO,可单测。
//! 3. **反应式失败转移** [`select_serving_account`]:选中账号续期被网关拒(账号级错误)
//!    时标记短退避并重试下一账号,基础设施错误(网络/FS)直接返回;`tried` 防死循环。
//!
//! 迁移:旧「单文件单账号」由 [`migrate_legacy`] 一次性搬进池首账号。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// 当前 UNIX 毫秒(系统时钟早于 1970 返 0)。
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── 错误 ─────────────────────────────────────────────────────────────

/// 池存储错误(文件 IO / JSON)。
#[derive(Debug, thiserror::Error)]
pub enum PoolStorageError {
    #[error("无法定位 token 持久化目录:HOME 与 USERPROFILE 都未设置")]
    HomeNotSet,
    #[error("池文件 IO 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("池文件 JSON 失败: {0}")]
    Serde(#[from] serde_json::Error),
}

/// [`select_serving_account`] 的结果错误。provider wrapper 按需 map 成自己的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("未登录(池内无可用账号)")]
    NotLoggedIn,
    #[error("账号级续期失败: {0}")]
    Account(String),
    #[error("基础设施错误: {0}")]
    Infra(String),
    #[error(transparent)]
    Storage(#[from] PoolStorageError),
}

// ── 池态 + 纯选择器 ──────────────────────────────────────────────────

/// 池运行态:当前服务账号 + 每账号耗尽到期时刻。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolState {
    /// 当前 sticky 服务账号 uid(`None` = 未定,选择器现选)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_uid: Option<String>,
    /// uid → 耗尽到期 UNIX ms(在此之前不选该账号)。
    #[serde(default)]
    pub exhausted_until: HashMap<String, i64>,
}

/// **纯函数**选服务账号:sticky active(若可用)→ 否则可用集第一个(uid 排序稳定)→
/// 全耗尽则最快解禁的那个(尽力)。可单测,无 IO。`exhausted_until[uid] > now` 视为不可选。
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

// ── provider 特化契约 ────────────────────────────────────────────────

/// 续期结果分类:账号级 → 触发失败转移(标退避 + 试下一账号);基础设施级 → 直接返回。
pub enum RefreshOutcome {
    /// 网关拒 refresh / 凭证丢失等**账号级**失效(换账号可能自愈)。
    AccountLevel(String),
    /// 网络抖动 / 文件系统等**基础设施**错误(换账号无益)。
    Infra(String),
}

/// provider 特化:凭证类型 + 字段抽取 + 续期。存储/选择/失败转移由本模块通用实现。
pub trait PoolBackend {
    /// 持久化的凭证类型(每账号一套)。
    type Cred: Serialize + DeserializeOwned + Clone + Send + Sync;

    /// 存储命名空间子目录(如 `"workbuddy"` / `"qoder"`)。
    fn namespace() -> &'static str;
    /// 旧「单文件单账号」文件名(迁移读取用,如 `"qoder-oauth.json"`)。
    fn legacy_single_filename() -> &'static str;

    /// 从凭证取 uid(`None` = 缺)。
    fn cred_uid(cred: &Self::Cred) -> Option<String>;
    /// uid 兜底:从凭证内 token 的 JWT sub 解(`cred_uid` 缺时用)。
    fn uid_from_token(cred: &Self::Cred) -> Option<String>;
    /// 写回 uid 到凭证(add_account 落盘前)。
    fn set_uid(cred: &mut Self::Cred, uid: String);

    /// 取本账号设备指纹(workbuddy device_id / qoder machine_id)。
    fn cred_fingerprint(cred: &Self::Cred) -> Option<String>;
    /// 写回设备指纹到凭证。
    fn set_fingerprint(cred: &mut Self::Cred, fp: String);
    /// 新账号无指纹时生成一个(通常 v4 UUID)。
    fn new_fingerprint() -> String;
    /// 老账号/迁移的指纹兜底(通常全局稳定设备 id)。
    fn fingerprint_fallback() -> String;

    /// UI 展示用昵称。
    fn cred_nickname(cred: &Self::Cred) -> Option<String>;
    /// UI 展示用脱敏标签(取不到 → None,前端退回短 uid)。默认 None。
    fn cred_display(_cred: &Self::Cred) -> Option<String> {
        None
    }

    /// 续期指定账号并返回可用 serving token。实现内部自行 load/refresh/save 该账号
    /// (通常复用 provider 已有的 `ensure_valid_*_token`)。失败按 [`RefreshOutcome`] 分类。
    fn ensure_valid(
        http: &reqwest::Client,
        provider_id: &str,
        uid: &str,
    ) -> impl Future<Output = Result<String, RefreshOutcome>> + Send;
}

// ── 选中的服务账号 + 池摘要 ──────────────────────────────────────────

/// 选中的服务账号 —— 代理转发用。
#[derive(Debug, Clone)]
pub struct ServingAccount {
    pub uid: String,
    /// 已续期的可用 serving token。
    pub serving_token: String,
    /// 本账号专属设备指纹(`X-Device-Id` / machine_id)。
    pub fingerprint: String,
}

/// 池内账号摘要(UI / quota 守护用)。
#[derive(Debug, Clone)]
pub struct PoolAccount {
    pub uid: String,
    pub nickname: Option<String>,
    pub display: Option<String>,
    pub fingerprint: Option<String>,
    pub is_active: bool,
    /// 耗尽到期 UNIX ms(0 = 未标记);> now 即当前不可选。
    pub exhausted_until: i64,
}

// ── 存储原语(泛型 over 凭证 T + namespace)───────────────────────────

fn transfer_root() -> Result<PathBuf, PoolStorageError> {
    codex_app_transfer_registry::paths::resolve_home()
        .map(|h| h.join(".codex-app-transfer"))
        .ok_or(PoolStorageError::HomeNotSet)
}

/// provider id / uid → 安全文件名片段(只留 `[A-Za-z0-9._-]`,其余换 `_`;防 `../` 注入)。
/// 空 / 纯点退化成 `default`。
fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 某账号文件的绝对路径(供仍需 path 的 provider store wrapper 复用)。
pub fn account_file_path(
    ns: &str,
    provider_id: &str,
    uid: &str,
) -> Result<PathBuf, PoolStorageError> {
    Ok(account_path(&transfer_root()?, ns, provider_id, uid))
}

/// 旧「单文件单账号」的绝对路径(迁移读取用)。
pub fn legacy_file_path(filename: &str) -> Result<PathBuf, PoolStorageError> {
    Ok(transfer_root()?.join(filename))
}

fn account_path(root: &Path, ns: &str, provider_id: &str, uid: &str) -> PathBuf {
    // 账号文件放 `accounts/` 子目录,与同级 `_pool.json` 物理隔离(防某 uid 清洗后恰为
    // `_pool` 覆盖池态)。
    root.join(ns)
        .join(sanitize_id(provider_id))
        .join("accounts")
        .join(format!("{}.json", sanitize_id(uid)))
}

fn pool_state_path(root: &Path, ns: &str, provider_id: &str) -> PathBuf {
    root.join(ns)
        .join(sanitize_id(provider_id))
        .join("_pool.json")
}

/// atomic 写(唯一 temp + rename,unix 0600)。唯一 temp(pid + 进程内递增)防并发续期互撞。
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), PoolStorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{nonce}", std::process::id()));
    let json = serde_json::to_vec_pretty(value)?;

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::remove_file(&tmp) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(PoolStorageError::Io(e)),
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, &json)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read_json_opt<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, PoolStorageError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PoolStorageError::Io(e)),
    }
}

/// 加载某账号凭证;不存在返 `Ok(None)`。
pub fn load_account<T: DeserializeOwned>(
    ns: &str,
    provider_id: &str,
    uid: &str,
) -> Result<Option<T>, PoolStorageError> {
    read_json_opt(&account_path(&transfer_root()?, ns, provider_id, uid))
}

/// atomic 保存某账号凭证。
pub fn save_account<T: Serialize>(
    ns: &str,
    provider_id: &str,
    uid: &str,
    cred: &T,
) -> Result<(), PoolStorageError> {
    write_json_atomic(&account_path(&transfer_root()?, ns, provider_id, uid), cred)
}

/// 删除某账号凭证(不存在算成功)。
pub fn delete_account(ns: &str, provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    match std::fs::remove_file(account_path(&transfer_root()?, ns, provider_id, uid)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PoolStorageError::Io(e)),
    }
}

/// 列某 provider 池内所有账号凭证。目录不存在 → 空;单个坏文件跳过(不废整池)。
pub fn list_accounts<T: DeserializeOwned>(
    ns: &str,
    provider_id: &str,
) -> Result<Vec<T>, PoolStorageError> {
    let dir = transfer_root()?
        .join(ns)
        .join(sanitize_id(provider_id))
        .join("accounts");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(PoolStorageError::Io(e)),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".json") || name == "_pool.json" || name.contains(".tmp") {
            continue;
        }
        if let Ok(Some(cred)) = read_json_opt::<T>(&path) {
            out.push(cred);
        }
    }
    Ok(out)
}

/// 加载池态;文件不存在 → 默认空态(非错误)。
pub fn load_pool_state(ns: &str, provider_id: &str) -> Result<PoolState, PoolStorageError> {
    Ok(read_json_opt(&pool_state_path(&transfer_root()?, ns, provider_id))?.unwrap_or_default())
}

/// atomic 保存池态。
pub fn save_pool_state(
    ns: &str,
    provider_id: &str,
    state: &PoolState,
) -> Result<(), PoolStorageError> {
    write_json_atomic(&pool_state_path(&transfer_root()?, ns, provider_id), state)
}

// ── 通用池操作(泛型 over PoolBackend)────────────────────────────────

/// 代理转发入口:迁移老凭证 → 选服务账号 → 续期 → 返回 token + 该账号指纹。
/// 池空 → `NotLoggedIn`。反应式失败转移:账号级续期失败标 5min 退避并试下一账号,
/// `tried` 防 choose 全耗尽兜底又选回刚失败的账号死循环。
pub async fn select_serving_account<B: PoolBackend>(
    http: &reqwest::Client,
    provider_id: &str,
) -> Result<ServingAccount, PoolError> {
    migrate_legacy::<B>(provider_id)?;
    let accounts = list_accounts::<B::Cred>(B::namespace(), provider_id)?;
    let uids: Vec<String> = accounts.iter().filter_map(B::cred_uid).collect();
    if uids.is_empty() {
        return Err(PoolError::NotLoggedIn);
    }
    let mut pool = load_pool_state(B::namespace(), provider_id)?;
    let mut last_err = PoolError::NotLoggedIn;
    let mut tried: HashSet<String> = HashSet::new();
    loop {
        let candidates: Vec<String> = uids
            .iter()
            .filter(|u| !tried.contains(*u))
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Err(last_err);
        }
        let chosen =
            choose_serving_uid(&candidates, &pool, unix_now_ms()).ok_or(PoolError::NotLoggedIn)?;
        tried.insert(chosen.clone());
        // sticky 切换才落盘(避免每请求写 _pool.json)
        if pool.active_uid.as_deref() != Some(chosen.as_str()) {
            pool.active_uid = Some(chosen.clone());
            save_pool_state(B::namespace(), provider_id, &pool)?;
        }
        let cred = load_account::<B::Cred>(B::namespace(), provider_id, &chosen)?
            .ok_or(PoolError::NotLoggedIn)?;
        match B::ensure_valid(http, provider_id, &chosen).await {
            Ok(serving_token) => {
                let fingerprint =
                    B::cred_fingerprint(&cred).unwrap_or_else(B::fingerprint_fallback);
                return Ok(ServingAccount {
                    uid: chosen,
                    serving_token,
                    fingerprint,
                });
            }
            Err(RefreshOutcome::AccountLevel(msg)) => {
                last_err = PoolError::Account(msg);
                let until = unix_now_ms() + 5 * 60 * 1000;
                pool.exhausted_until.insert(chosen.clone(), until);
                save_pool_state(B::namespace(), provider_id, &pool)?;
                tracing::warn!(uid = %chosen, ns = B::namespace(), "[Pool] 账号续期失败 → 标 5min 退避,试下一账号");
                continue;
            }
            Err(RefreshOutcome::Infra(msg)) => return Err(PoolError::Infra(msg)),
        }
    }
}

/// 登录成功后加账号入池:分配/复用本账号指纹 → 写 `<uid>.json` → 更新池(首账号设 active;
/// 清该账号 exhausted)。返回 uid。重登同 uid = 更新(保留指纹稳定)。
pub fn add_account<B: PoolBackend>(
    provider_id: &str,
    mut cred: B::Cred,
) -> Result<String, PoolError> {
    let uid = B::cred_uid(&cred)
        .or_else(|| B::uid_from_token(&cred))
        .ok_or_else(|| PoolError::Account("uid".into()))?;
    B::set_uid(&mut cred, uid.clone());
    // 指纹:重登复用该 uid 既有账号的(设备指纹稳定),首登新生成。容忍腐败旧文件(丢弃)。
    let existing_fp = load_account::<B::Cred>(B::namespace(), provider_id, &uid)
        .ok()
        .flatten()
        .and_then(|c| B::cred_fingerprint(&c));
    let fp = B::cred_fingerprint(&cred)
        .or(existing_fp)
        .unwrap_or_else(B::new_fingerprint);
    B::set_fingerprint(&mut cred, fp);
    save_account(B::namespace(), provider_id, &uid, &cred)?;

    let mut pool = load_pool_state(B::namespace(), provider_id)?;
    if pool.active_uid.is_none() {
        pool.active_uid = Some(uid.clone());
    }
    pool.exhausted_until.remove(&uid);
    save_pool_state(B::namespace(), provider_id, &pool)?;
    Ok(uid)
}

/// 标记账号耗尽到 `until_ms`(quota 守护);`until_ms <= now` 等价清除。
pub fn set_exhausted(
    ns: &str,
    provider_id: &str,
    uid: &str,
    until_ms: i64,
) -> Result<(), PoolStorageError> {
    let mut pool = load_pool_state(ns, provider_id)?;
    if until_ms <= unix_now_ms() {
        pool.exhausted_until.remove(uid);
    } else {
        pool.exhausted_until.insert(uid.to_string(), until_ms);
    }
    save_pool_state(ns, provider_id, &pool)
}

/// 清除账号耗尽标记。
pub fn clear_exhausted(ns: &str, provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    set_exhausted(ns, provider_id, uid, 0)
}

/// 手动指定当前服务账号(UI 切换)。
pub fn set_active(ns: &str, provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    let mut pool = load_pool_state(ns, provider_id)?;
    pool.active_uid = Some(uid.to_string());
    save_pool_state(ns, provider_id, &pool)
}

/// 移除账号(UI):删账号文件 + 从池态摘除;删的是 active 则清 active。
pub fn remove_account(ns: &str, provider_id: &str, uid: &str) -> Result<(), PoolStorageError> {
    delete_account(ns, provider_id, uid)?;
    let mut pool = load_pool_state(ns, provider_id)?;
    pool.exhausted_until.remove(uid);
    if pool.active_uid.as_deref() == Some(uid) {
        pool.active_uid = None;
    }
    save_pool_state(ns, provider_id, &pool)
}

/// 列池内账号摘要(UI / quota 守护)。
pub fn list_pool<B: PoolBackend>(provider_id: &str) -> Result<Vec<PoolAccount>, PoolStorageError> {
    let _ = migrate_legacy::<B>(provider_id); // 迁移失败不致命
    let accounts = list_accounts::<B::Cred>(B::namespace(), provider_id)?;
    let pool = load_pool_state(B::namespace(), provider_id)?;
    let active = pool.active_uid.as_deref();
    Ok(accounts
        .into_iter()
        .filter_map(|c| {
            let uid = B::cred_uid(&c)?;
            Some(PoolAccount {
                is_active: active == Some(uid.as_str()),
                exhausted_until: pool.exhausted_until.get(&uid).copied().unwrap_or(0),
                display: B::cred_display(&c),
                nickname: B::cred_nickname(&c),
                fingerprint: B::cred_fingerprint(&c),
                uid,
            })
        })
        .collect())
}

/// 一次性迁移:老「单文件单账号」→ 池首账号。仅当本 provider 池为空且老单文件存在时执行,
/// 把老指纹(全局兜底)绑给该账号,迁完删老单文件。失败吞掉(不阻塞正常流程)。
pub fn migrate_legacy<B: PoolBackend>(provider_id: &str) -> Result<(), PoolStorageError> {
    if !list_accounts::<B::Cred>(B::namespace(), provider_id)?.is_empty() {
        return Ok(()); // 已有账号,不迁
    }
    let legacy_path = transfer_root()?.join(B::legacy_single_filename());
    let Some(mut cred) = read_json_opt::<B::Cred>(&legacy_path)? else {
        return Ok(()); // 无老单文件
    };
    let uid = B::cred_uid(&cred).or_else(|| B::uid_from_token(&cred));
    let Some(uid) = uid else {
        return Ok(()); // 解不出 uid 不迁
    };
    B::set_uid(&mut cred, uid.clone());
    // 老账号沿用全局指纹兜底(它一直用这个设备)。
    if B::cred_fingerprint(&cred).is_none() {
        B::set_fingerprint(&mut cred, B::fingerprint_fallback());
    }
    save_account(B::namespace(), provider_id, &uid, &cred)?;
    let mut pool = load_pool_state(B::namespace(), provider_id)?;
    pool.active_uid = Some(uid);
    save_pool_state(B::namespace(), provider_id, &pool)?;
    let _ = std::fs::remove_file(&legacy_path); // 删老单文件(失败无妨)
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
        assert_eq!(
            choose_serving_uid(&uids, &pool_with(Some("b"), &[]), 1000).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn switches_off_exhausted_active() {
        let uids = vec!["a".to_string(), "b".to_string()];
        let p = pool_with(Some("a"), &[("a", 5000)]);
        assert_eq!(choose_serving_uid(&uids, &p, 1000).as_deref(), Some("b"));
    }

    #[test]
    fn exhaustion_expires_after_until() {
        let uids = vec!["a".to_string(), "b".to_string()];
        let p = pool_with(Some("a"), &[("a", 5000)]);
        assert_eq!(choose_serving_uid(&uids, &p, 6000).as_deref(), Some("a"));
    }

    #[test]
    fn no_active_picks_first_available_sorted() {
        let uids = vec!["b".to_string(), "a".to_string()];
        assert_eq!(
            choose_serving_uid(&uids, &pool_with(None, &[]), 1000).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn all_exhausted_picks_soonest_to_recover() {
        let uids = vec!["a".to_string(), "b".to_string()];
        let p = pool_with(Some("a"), &[("a", 9000), ("b", 7000)]);
        assert_eq!(choose_serving_uid(&uids, &p, 1000).as_deref(), Some("b"));
    }

    #[test]
    fn tried_filter_prevents_deadloop_on_failover() {
        let p = pool_with(Some("a"), &[("a", 9000), ("b", 9500)]);
        let uids_all = vec!["a".to_string(), "b".to_string()];
        // 不过滤:全耗尽兜底选回刚失败的 A(死循环根源)
        assert_eq!(
            choose_serving_uid(&uids_all, &p, 1000).as_deref(),
            Some("a")
        );
        // 过滤掉 tried={a}:候选只剩 B,必须选 B
        let mut tried = HashSet::new();
        tried.insert("a".to_string());
        let candidates: Vec<String> = uids_all
            .iter()
            .filter(|u| !tried.contains(*u))
            .cloned()
            .collect();
        assert_eq!(
            choose_serving_uid(&candidates, &p, 1000).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn account_path_isolates_and_sanitizes() {
        let root = Path::new("/r");
        let a = account_path(root, "qoder", "q-login", "u-alice");
        let b = account_path(root, "qoder", "q-login", "u-bob");
        assert_ne!(a, b);
        assert!(a.ends_with("qoder/q-login/accounts/u-alice.json"));
        for raw in ["../../etc/passwd", "a/b\\c", "..", "", "/etc"] {
            let s = sanitize_id(raw);
            assert!(!s.contains('/') && !s.contains('\\') && !s.is_empty() && s != "..");
        }
        assert_eq!(sanitize_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_id(".."), "default");
    }
}
