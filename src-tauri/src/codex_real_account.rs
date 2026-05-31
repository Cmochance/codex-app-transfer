//! 真实 ChatGPT 账号检测(MOC-104)。
//!
//! 「真实账号 plugin 模式」的基础:判断本机是否已有可用的真实 ChatGPT 登录态
//! (`auth.json` 里 `auth_mode == "chatgpt"` 且 tokens 齐全)。当前 plugins 解锁
//! 靠 CDP 注入伪造 `setAuthMethod('chatgpt')`,没有真实 userID → Codex 启动后要
//! 重新初始化登录态(明显的额外延迟,Windows 上可能数十秒)。真实账号模式用真
//! `auth.json` 取代伪造,避开代价。
//!
//! 能力(注意:**只有 [`detect`] 是纯只读**,其余按需写 `auth.json` —— 都「先备份
//! 再原子写、失败即中止」,非破坏):
//! - **检测**([`detect`],只读):定位本机可用的真实 chatgpt 登录态。
//! - **token 刷新**([`refresh_if_needed`]):将过期才走官方 OAuth refresh,只更新
//!   token 字段 + `last_refresh` 写回。整个 exchange 在 `AUTH_LOCK` 内串行(防
//!   single-use refresh_token 被并发双 POST → `refresh_token_reused`)。
//! - **登录**([`start_login`]/[`cancel_login`]/[`login_status`]):调起官方
//!   `codex login`(它自己做 OAuth + 写 `~/.codex/auth.json`),非阻塞 + 可取消。
//! - **导入 / 长期保留**([`import_auth`]/[`pin_current_account`]/[`forget_imported`]/
//!   [`reconcile_on_startup`]):把真实账号写进 transfer 持久镜像(`~/.codex` 之外、
//!   不受文件变动/快照轮转影响);登录成功后前端自动 pin,启动时活动文件失效则从
//!   镜像自动恢复(无需手动"切换/恢复"——单账号工具不是多账号切换器)。
//!
//! 检测来源(优先级):① 官方 `~/.codex/auth.json`(Codex 当前活动凭据)→ ② 用户
//! 显式导入/钉住的持久镜像。**不扫 apply 快照备份** —— 那些是 transfer 改配置时的
//! 内部备份(可能是数周前早已失效的旧 chatgpt),报成「你的真实账号」会误导(用户
//! 实测反馈)。「长期保留」只认用户主动登录/导入产生的镜像。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use codex_app_transfer_codex_integration::{read_auth, write_auth, CodexPaths};

/// 检测到的真实 chatgpt 凭据来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSource {
    /// 官方 `~/.codex/auth.json`(活动凭据)。
    Official,
    /// 用户导入/钉住的 transfer 持久镜像(`~/.codex-app-transfer/real-account/
    /// imported-auth.json`)—— 不受 `~/.codex` 文件变动 / 快照轮转影响,长期保留。
    Imported,
    /// 哪里都没找到可用的真实 chatgpt 登录态。
    None,
}

/// 真实 ChatGPT 账号检测结果(只读快照)。
#[derive(Debug, Clone, Serialize)]
pub struct RealAccountStatus {
    /// 是否检测到**可用**的真实 chatgpt 登录态(`auth_mode==chatgpt` + access/refresh token 齐)。
    pub logged_in: bool,
    /// 活动 `auth.json` 的 `auth_mode`(`chatgpt` / `apikey` / 缺失=None)。
    /// 注意:这是**官方活动文件**的模式,即便可用凭据是从持久镜像检测到的也反映活动态,
    /// 便于前端区分"活动就是 chatgpt" vs "活动是 apikey、但镜像里有 chatgpt"。
    pub active_auth_mode: Option<String>,
    /// chatgpt `account_id`(从被采纳的来源里取,可能缺失)。
    pub account_id: Option<String>,
    /// `logged_in=true` 时,可用凭据来自哪里。
    pub source: AuthSource,
    /// 是否存在用户导入/钉住的持久镜像(独立于 `source` —— 活动即便是 official,
    /// 镜像也可能并存)。前端据此显示「忘记导入」按钮。
    pub has_imported: bool,
    /// 最近一次刷新/启动调谐判定「真实账号已失效、refresh_token 永久无效、需重新登录」。
    /// [connector review] 持久化到可查询的 status,而非只靠一次性 `emit` 事件 —— 启动时
    /// 若前端还没注册 listener,事件会丢;前端轮询 status 时读这个字段就不会漏报失效。
    pub relogin_required: bool,
}

impl RealAccountStatus {
    fn none(active_auth_mode: Option<String>, has_imported: bool) -> Self {
        Self {
            logged_in: false,
            active_auth_mode,
            account_id: None,
            source: AuthSource::None,
            has_imported,
            relogin_required: relogin_required(),
        }
    }
}

/// [connector review] 进程级「需重新登录」标记 —— refresh/reconcile 判定 refresh_token
/// 永久失效时置真,登录/导入/成功刷新后清零。比一次性 `emit` 事件可靠:前端任何时候
/// 轮询 `status` 都能读到,不受「事件早于 listener 注册」的启动时序影响。
static RELOGIN_REQUIRED: AtomicBool = AtomicBool::new(false);

/// 读「需重新登录」标记。
pub fn relogin_required() -> bool {
    RELOGIN_REQUIRED.load(Ordering::SeqCst)
}

/// 设「需重新登录」标记(refresh/reconcile 判定失效时 true;有新鲜账号时 false)。
fn set_relogin_required(v: bool) {
    RELOGIN_REQUIRED.store(v, Ordering::SeqCst);
}

/// 活动 `~/.codex/auth.json` 当前是否就是可用的真实 chatgpt(决定「插件解锁是否走原生
/// 路径、无需 CDP daemon」—— 解耦的核心判据,借鉴 CodexPlusPlus relay 模式:有 chatgpt
/// 登录态则 Codex 原生显示 plugins,不打 CDP 注入)。home 解析失败 → false。只读。
pub fn active_is_real_chatgpt_now() -> bool {
    CodexPaths::from_home_env()
        .map(|p| active_is_real_chatgpt(&p))
        .unwrap_or(false)
}

/// 从一个 `auth.json` Value 判断是否是**可用**的 chatgpt 登录态。
/// 可用 = `auth_mode=="chatgpt"` 且 `tokens.{access_token,refresh_token}` 均非空。
/// 返回 `account_id`(可能为 None)。
fn parse_chatgpt_auth(v: &Value) -> Option<ChatgptAuth> {
    if v.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return None;
    }
    let tokens = v.get("tokens").and_then(Value::as_object)?;
    let nonempty = |key: &str| {
        tokens
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    // refresh_token 是刷新续期的前提;access_token 是当下能用的前提。两者缺一
    // 则视作不可用(残缺/登出中),不报 logged_in,避免误导上层去"用"它。
    if !nonempty("access_token") || !nonempty("refresh_token") {
        return None;
    }
    Some(ChatgptAuth {
        account_id: tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

struct ChatgptAuth {
    account_id: Option<String>,
}

/// 定位到的真实 chatgpt `auth.json`:文件路径 + 来源 + 已解析的整个 Value +
/// 顺手取出的 `account_id`。刷新用 `path`(刷哪个文件)+ `value`(透传非 token
/// 字段);`detect` 用 `account_id`,避免再 parse 一遍(review N-1)。
struct LocatedChatgptAuth {
    path: std::path::PathBuf,
    source: AuthSource,
    value: Value,
    account_id: Option<String>,
}

/// transfer 持久镜像路径(用户导入/钉住的真实账号,`~/.codex` 之外、不被快照
/// 轮转 / 切账号 / apply 改写影响)。
fn imported_mirror_path(paths: &CodexPaths) -> PathBuf {
    paths
        .app_home
        .join("real-account")
        .join("imported-auth.json")
}

/// 定位**当前**可用的真实 chatgpt 账号:① 官方活动 `~/.codex/auth.json` → ② 用户
/// 显式导入/钉住的持久镜像。**不扫 apply 快照备份** —— 那些是 transfer 改配置时
/// 的内部备份(可能是几周前早已失效的旧 chatgpt),报成「你的真实账号」会误导
/// 用户、让活动是 apikey 的人以为账号被改(用户实测反馈)。「长期保留」只认用户
/// 主动登录/导入产生的镜像。[`detect`] / refresh / reconcile 共用,口径一致。只读。
fn locate_chatgpt_auth(paths: &CodexPaths) -> Option<LocatedChatgptAuth> {
    // ① 官方活动 auth.json(Codex 当前真在用的那份)。
    if let Ok(v) = read_auth(&paths.auth_json) {
        if let Some(parsed) = parse_chatgpt_auth(&v) {
            return Some(LocatedChatgptAuth {
                path: paths.auth_json.clone(),
                source: AuthSource::Official,
                value: v,
                account_id: parsed.account_id,
            });
        }
    }
    // ② 用户导入/钉住的持久镜像(长期保留的真相源)。
    let mirror = imported_mirror_path(paths);
    if mirror.is_file() {
        if let Ok(v) = read_auth(&mirror) {
            if let Some(parsed) = parse_chatgpt_auth(&v) {
                return Some(LocatedChatgptAuth {
                    path: mirror,
                    source: AuthSource::Imported,
                    value: v,
                    account_id: parsed.account_id,
                });
            }
        }
    }
    None
}

/// 读官方活动 `auth.json` 的 `auth_mode`(不存在/坏 → None)。检测结果里单独
/// 报告活动模式,便于前端区分"活动就是 chatgpt" vs "活动 apikey、镜像有 chatgpt"。
fn active_auth_mode(paths: &CodexPaths) -> Option<String> {
    read_auth(&paths.auth_json)
        .ok()?
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// 检测真实 chatgpt 账号:按"官方活动 → 持久镜像"定位可用凭据(见
/// [`locate_chatgpt_auth`])。纯只读,绝不写盘 / spawn。
pub fn detect() -> RealAccountStatus {
    let Ok(paths) = CodexPaths::from_home_env() else {
        // 连 home 都解析不到 —— 当作"没有",不 panic。
        return RealAccountStatus::none(None, false);
    };
    let active_mode = active_auth_mode(&paths);
    let has_imported = read_imported_mirror(&paths).is_some();
    match locate_chatgpt_auth(&paths) {
        Some(found) => {
            // [connector review 自愈] 活动文件本身是真实 chatgpt 且 access_token 未过期
            // (本地 JWT exp 判断,无网络)= 账号当前确实可用 → 清掉可能 stale 的
            // 「需重新登录」标记。覆盖用户在 app 外重新 `codex login` / 直接恢复活动文件、
            // 不经 import/login/refresh 入口的场景。只在有「确实有效」的本地证据时清:
            // access_token 过期则不清(避免把真失效误报成「获取成功」)。
            if found.source == AuthSource::Official {
                let access = found
                    .value
                    .get("tokens")
                    .and_then(|t| t.get("access_token"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !access.is_empty()
                    && !access_token_expired(access, chrono::Utc::now().timestamp())
                {
                    set_relogin_required(false);
                }
            }
            RealAccountStatus {
                logged_in: true,
                active_auth_mode: active_mode,
                account_id: found.account_id,
                source: found.source,
                has_imported,
                relogin_required: relogin_required(),
            }
        }
        None => RealAccountStatus::none(active_mode, has_imported),
    }
}

// ── Token 刷新(MOC-104 req#2)─────────────────────────────────────────
//
// 真实 chatgpt token 会过期。用户要求每次启动刷新真实账号的 token 避免过期。刷新
// 走官方 OAuth refresh_token 流(常量与请求格式借鉴 Codex_Account_Switch
// `shared/runtime/chatgpt_api.rs`,致谢见 ACKNOWLEDGEMENTS.md):
//   POST https://auth.openai.com/oauth/token
//        grant_type=refresh_token&refresh_token=<rt>&client_id=<id>
// 响应 {access_token, id_token?, refresh_token?};只更新 tokens.{access,refresh,
// id} + 顶层 last_refresh,其它字段透传(非破坏)。刷的是 [`locate_chatgpt_auth`]
// 定位到的那份文件(官方活动 or 持久镜像),与检测口径一致。

use base64::Engine;

/// 官方 ChatGPT OAuth issuer。
const OPENAI_ISSUER: &str = "https://auth.openai.com";
/// ChatGPT desktop / Codex CLI 公开 OAuth client id(借鉴 Codex_Account_Switch
/// chatgpt_api.rs:100,该处注明已对照官方 codex 与 codex-switcher 验证)。
const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 提前于真实过期点刷新,避免 in-flight 请求恰好撞 401。
const EXPIRY_SKEW_SECONDS: i64 = 300;

/// 一次刷新尝试的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RefreshOutcome {
    /// 没有可刷新的真实 chatgpt 账号(官方活动 + 持久镜像都没有)。
    NoAccount,
    /// access_token 还没到期(或无法解析过期时间 → 保守视作有效),跳过刷新。
    StillValid { source: AuthSource },
    /// 刷新成功并写回。
    Refreshed { source: AuthSource },
    /// 真实账号**不可用**:refresh_token 失效 / 已被用过 / invalid_grant —— 续期无望,
    /// 需要重新登录。上层据此自动关「自动解锁」开关 + 停 daemon + 提示用户重登。
    ReloginRequired { source: AuthSource },
}

/// 刷新失败信息是否表示"需要重新登录"(refresh_token 永久失效,不是瞬时网络错)。
/// 借鉴 Codex_Account_Switch `chatgpt_api.rs::looks_like_relogin_required` 的签名集。
fn looks_like_relogin_required(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("token_invalidated")
        || m.contains("refresh_token_reused")
        || m.contains("invalid_grant")
        || m.contains("authentication token has been invalidated")
        || m.contains("refresh token has already been used")
        || m.contains("please try signing in again")
        || m.contains("please log out and sign in again")
}

/// OAuth refresh 响应:只取我们要写回 auth.json 的字段。
#[derive(serde::Deserialize)]
struct OAuthRefreshResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// 解析 JWT 的 payload(第二段,base64url no-pad)。失败返 None。
fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// access_token(JWT)是否已过期或将在 skew 内过期。无法解析 = 保守视作**未**过期
/// (让服务器用 401 告知,避免拿不准就乱刷把 refresh_token 烧了)。
fn access_token_expired(access_token: &str, now_unix: i64) -> bool {
    match jwt_payload(access_token).and_then(|p| p.get("exp").and_then(Value::as_i64)) {
        Some(exp) => exp <= now_unix + EXPIRY_SKEW_SECONDS,
        None => false,
    }
}

/// 把刷新响应应用到整个 auth.json 的 Value:只动 `tokens.{access_token,
/// refresh_token,id_token}` + 顶层 `last_refresh`,其它字段透传不动。响应里
/// 没返回的 refresh_token / id_token 保留旧值(OAuth 不一定每次都轮换)。
///
/// [MOC-104 review I-1] 顶层 / `tokens` 不是 object 时返回 `Err` 而非静默跳过 ——
/// 绝不能在"没真正写入新 token"的情况下让 caller 走到 `write_auth` + 报 Refreshed
/// (那会把"没刷成"伪装成"刷成功",真 token 过期后用户被登出)。正常路径下
/// `locate_chatgpt_auth` 已保证结构合法,这是防御性硬化。
fn apply_refresh_response(
    auth: &mut Value,
    resp: &OAuthRefreshResponse,
    now_iso: &str,
) -> Result<(), String> {
    let obj = auth
        .as_object_mut()
        .ok_or("auth.json 顶层不是 JSON object,拒绝写回")?;
    let tokens = obj
        .entry("tokens".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or("auth.json 的 tokens 字段不是 object,拒绝写回")?;
    tokens.insert(
        "access_token".to_owned(),
        Value::String(resp.access_token.clone()),
    );
    if let Some(rt) = resp.refresh_token.as_ref().filter(|s| !s.is_empty()) {
        tokens.insert("refresh_token".to_owned(), Value::String(rt.clone()));
    }
    if let Some(idt) = resp.id_token.as_ref().filter(|s| !s.is_empty()) {
        tokens.insert("id_token".to_owned(), Value::String(idt.clone()));
    }
    obj.insert("last_refresh".to_owned(), Value::String(now_iso.to_owned()));
    Ok(())
}

/// [MOC-104 review P1/I-3] 串行化对 auth.json 的整个 refresh exchange + activate
/// 写回。**异步** mutex —— 因为 refresh 的网络 POST 必须在锁内(ChatGPT refresh_token
/// 是单次使用,两个并发 caller 各 POST 同一 token 会触发 `refresh_token_reused`
/// 把账号卡死,openai/codex#7144),不能像 std mutex 那样只锁同步写、把网络放锁外。
/// 锁内可跨 `.await`,故用 `tokio::sync::Mutex`。
static AUTH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 启动时刷新真实 chatgpt 账号的 token(若将过期)。定位官方活动 / 持久镜像里
/// 那份真实 chatgpt `auth.json` → 检查 access_token 是否将过期 → 走 refresh_token 流 →
/// 原子写回(`write_auth`,0o600,非 token 字段透传)。
///
/// 非破坏:只更新 token 字段;无真实账号 / token 仍有效时不写盘。任何步骤失败
/// 都返回 `Err`(由 caller 决定吞掉还是上报),不会留下半写状态(`write_auth` 原子)。
/// [P1] **整个 exchange 在 `AUTH_LOCK` 内串行**:定位 → 判过期 → POST → 写回都持锁,
/// 第二个并发 caller 等锁后重新定位发现 token 已刷新(未过期)→ StillValid,不会
/// 拿同一 single-use refresh_token 再 POST 一次。
pub async fn refresh_if_needed(client: &reqwest::Client) -> Result<RefreshOutcome, String> {
    let _guard = AUTH_LOCK.lock().await;
    // [connector review] codex login 是外部进程,直接写 `~/.codex/auth.json` 且**不**
    // 走 AUTH_LOCK。若它正在进行,refresh 刷的是登录前的旧账号,写回会盖掉 login
    // 即将/已经写入的新账号 —— 直接跳过,让 login 当唯一权威写者(reconcile 也有
    // 同样前置守卫;这里再设一道,覆盖 manual refresh / import 后刷新等其它入口)。
    if matches!(login_status(), LoginState::Running) {
        tracing::info!("[RealAccount] refresh 跳过:codex login 进行中");
        return Ok(RefreshOutcome::NoAccount);
    }
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let Some(located) = locate_chatgpt_auth(&paths) else {
        return Ok(RefreshOutcome::NoAccount);
    };
    let source = located.source;
    let target = located.path;
    let mut auth = located.value;

    let refresh_token = auth
        .get("tokens")
        .and_then(|t| t.get("refresh_token"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if refresh_token.is_empty() {
        return Err("auth.json 缺 refresh_token,无法刷新".to_owned());
    }
    let access_token = auth
        .get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let now = chrono::Utc::now();
    if !access_token_expired(access_token, now.timestamp()) {
        // 持锁期间已是最新;第二个并发 caller 走到这里就 StillValid,不重复 POST。
        set_relogin_required(false); // 账号当前有效
        return Ok(RefreshOutcome::StillValid { source });
    }

    let resp = client
        .post(format!("{OPENAI_ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", OPENAI_OAUTH_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|e| format!("OAuth refresh 请求失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // refresh_token 永久失效(已用过 / invalid_grant 等)→ 续期无望,报
        // ReloginRequired 让上层关「自动解锁」+ 提示重登,而非当瞬时错误吞掉。
        if looks_like_relogin_required(&format!("{status} {body}")) {
            tracing::warn!("[RealAccount] refresh 失败 = 需重新登录: {status} {body}");
            set_relogin_required(true); // 持久标记失效,前端轮询 status 即可读到
            return Ok(RefreshOutcome::ReloginRequired { source });
        }
        return Err(format!("OAuth refresh 返回 {status}: {body}"));
    }
    let parsed: OAuthRefreshResponse = resp
        .json()
        .await
        .map_err(|e| format!("解析 OAuth refresh 响应失败: {e}"))?;

    let now_iso = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    apply_refresh_response(&mut auth, &parsed, &now_iso)?;
    // [connector review] OAuth POST 期间用户可能点了登录(login 不走 AUTH_LOCK)。
    // 写回前再查一次:login 已在跑就别把刚刷新的旧账号写回去盖掉 login 的新账号。
    // check 与 write 之间仍有微秒级窗口(外部进程无法纳入 AUTH_LOCK),但竞态窗已
    // 从整个 POST 时长(~1-2s)缩到一次写调用内。刚消费的 refresh_token 随之丢弃没
    // 关系 —— login 在换全新账号,旧账号即将被取代。
    if matches!(login_status(), LoginState::Running) {
        tracing::info!("[RealAccount] refresh 写回跳过:codex login 在 OAuth POST 期间启动");
        return Ok(RefreshOutcome::NoAccount);
    }
    write_auth(&target, &auth).map_err(|e| format!("写回 auth.json 失败: {e}"))?;

    // [review #4] 若刷的是 Official 活动文件、而持久镜像里是同一账号(镜像当前
    // refresh_token == 我们刚用掉的那个),把新 token 同步进镜像 —— 否则镜像一直
    // 留着已花掉的单次 refresh_token,日后活动文件失效从镜像恢复时下一次刷新必
    // token-reuse 失败,"长期生效"承诺破功。
    if source == AuthSource::Official {
        let mirror = imported_mirror_path(&paths);
        if mirror != target && mirror.is_file() {
            if let Ok(mut mv) = read_auth(&mirror) {
                let mirror_rt = mv
                    .get("tokens")
                    .and_then(|t| t.get("refresh_token"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if mirror_rt == refresh_token {
                    // [review #5] 镜像同步是 best-effort:不能用 `?` 把这里的错误冒泡
                    // 成整个 refresh 失败 —— 主活动文件这时已成功刷新写回,
                    // 镜像写坏只 warn,不影响 Refreshed 结果 / 后续启动调谐。
                    match apply_refresh_response(&mut mv, &parsed, &now_iso) {
                        Ok(()) => {
                            if let Err(e) = write_auth(&mirror, &mv) {
                                tracing::warn!("[RealAccount] 同步刷新 token 到持久镜像失败: {e}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("[RealAccount] 持久镜像结构异常,跳过 token 同步: {e}")
                        }
                    }
                }
            }
        }
    }
    set_relogin_required(false); // 刷新成功 = 账号恢复有效
    Ok(RefreshOutcome::Refreshed { source })
}

// ── 登录:调起官方 codex login(MOC-104 req#3)────────────────────────
//
// 用户在 transfer 内点"登录" → 后台 spawn 官方 `codex login`(它自己做 ChatGPT
// OAuth 并把真实 auth.json 写到 `~/.codex`)→ 前端轮询 detect() 看是否登录成功。
// 不自建 OpenAI OAuth(轻、稳),复用官方流程。借鉴 Codex_Account_Switch
// `mac/runtime/process.rs::run_codex_login` + `login_cancel.rs`(README 待致谢)。
//
// codex login 是交互式(开浏览器等回调),会阻塞到完成/超时,所以**不能**在 HTTP
// handler 里同步 await —— spawn 到后台线程 reap,前端轮询 [`login_status`]。

/// 解析官方 codex CLI 二进制路径。macOS 优先 Codex.app 内置 `Contents/Resources/
/// codex`(可靠,不受用户 shell 里 `codex` 函数/别名干扰),回退 PATH 扫描。
fn resolve_codex_cli() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut apps = vec![PathBuf::from("/Applications/Codex.app")];
        if let Some(home) = std::env::var_os("HOME") {
            apps.push(PathBuf::from(home).join("Applications").join("Codex.app"));
        }
        for app in apps {
            let cli = app.join("Contents").join("Resources").join("codex");
            if cli.is_file() {
                return Some(cli);
            }
        }
    }
    // PATH 扫描(各平台兜底):直接找 PATH 目录下的 `codex` 可执行文件,绕开
    // 用户 shell 里可能定义的 `codex` 函数(那个不在 PATH 上、也不是文件)。
    let exe = if cfg!(target_os = "windows") {
        "codex.exe"
    } else {
        "codex"
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 登录流程状态(前端轮询)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "message")]
pub enum LoginState {
    /// 没有进行中的登录(初始/上次结束后清空)。
    Idle,
    /// `codex login` 进行中(用户应在弹出的浏览器里完成授权)。
    Running,
    /// 登录成功(`codex login` 0 退出)。
    Succeeded,
    /// 登录失败,附 stderr/原因。
    Failed(String),
    /// 用户取消(cancel 杀掉了进程)。
    Cancelled,
}

struct LoginShared {
    running: bool,
    /// 进行中 `codex login` 子进程 pid(用于 cancel 杀进程)。
    pid: Option<u32>,
    /// cancel 已请求 —— reap 时据此把非零退出标记为 Cancelled 而非 Failed。
    cancel_requested: bool,
    last: LoginState,
}

static LOGIN: Mutex<LoginShared> = Mutex::new(LoginShared {
    running: false,
    pid: None,
    cancel_requested: false,
    last: LoginState::Idle,
});

/// [MOC-104 review N-1] 取 LOGIN 锁,锁中毒时恢复内部值 —— 不 panic、也不把异常
/// 静默退化成 Idle/false(那会让前端以为"没在登录"、按钮点了没反应)。
fn login_lock() -> std::sync::MutexGuard<'static, LoginShared> {
    LOGIN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 覆盖当前 `~/.codex/auth.json` 前先整文件备份到 app_home,被覆盖后用户仍可恢复。
///
/// [MOC-104 review B-1] **硬前置,非 best-effort**:备份失败返回 `Err`,调用方据此
/// 中止覆盖,绝不"备份没成功还照样覆盖活动文件"(那是 `feedback_no_silent_
/// destructive_fallback` 禁止的破坏性降级)。活动文件不存在 = 无需备份,返 Ok。
/// [review I-2] 文件名带 unix 时间戳,连续多次操作不互相覆盖备份(防丢失放大)。
fn backup_active_auth(paths: &CodexPaths, suffix: &str) -> Result<(), String> {
    if !paths.auth_json.is_file() {
        return Ok(());
    }
    let backup_dir = paths.app_home.join("real-account");
    std::fs::create_dir_all(&backup_dir).map_err(|e| format!("备份目录创建失败: {e}"))?;
    // [review I-2] 用纳秒,避免同一秒内两次同 suffix 操作覆盖彼此的备份(秒级粒度
    // 会让"覆盖前先备份"的唯一恢复副本丢失)。
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let backup = backup_dir.join(format!("auth-{suffix}-{ts}.json"));
    std::fs::copy(&paths.auth_json, &backup)
        .map_err(|e| format!("备份活动 auth.json 失败: {e}"))?;
    Ok(())
}

/// 当前活动 `~/.codex/auth.json` 是否已经是可用的真实 chatgpt(决定是否需要恢复)。
fn active_is_real_chatgpt(paths: &CodexPaths) -> bool {
    read_auth(&paths.auth_json)
        .ok()
        .as_ref()
        .and_then(parse_chatgpt_auth)
        .is_some()
}

/// 读持久镜像里的可用 chatgpt(无 / 非 chatgpt → None)。
fn read_imported_mirror(paths: &CodexPaths) -> Option<Value> {
    let mirror = imported_mirror_path(paths);
    let v = read_auth(&mirror).ok()?;
    parse_chatgpt_auth(&v).map(|_| v)
}

/// import 内层(**假设 caller 已持 `AUTH_LOCK`**):备份活动 → 写活动 → 提交持久镜像。
///
/// [connector review] 顺序是「先成功更新活动文件,再提交持久镜像」:若活动备份/写失败,
/// 镜像还没动,不会留下「导入失败却有镜像、下次启动 reconcile 把它当成已保留账号恢复
/// 到活动」的幽灵态。反序(先写镜像)在活动写失败时会留下孤儿镜像。
fn import_locked(paths: &CodexPaths, value: &Value) -> Result<(), String> {
    // 先恢复到活动(覆盖前先备份)—— 任一步失败直接返回,镜像保持原样不被污染。
    backup_active_auth(paths, "preimport")?;
    write_auth(&paths.auth_json, value).map_err(|e| format!("写活动 auth.json 失败: {e}"))?;
    // 活动已成功更新后,才提交长期保留的持久镜像。
    let mirror = imported_mirror_path(paths);
    if let Some(parent) = mirror.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("镜像目录创建失败: {e}"))?;
    }
    write_auth(&mirror, value).map_err(|e| format!("写持久镜像失败: {e}"))?;
    Ok(())
}

/// [MOC-104 req] 导入一份真实 chatgpt auth(文件导入)。校验是可用 chatgpt → 写进
/// 持久镜像(`~/.codex` 之外不受文件变动影响)→ 同时恢复到活动文件(先备份)。
pub async fn import_auth(value: Value) -> Result<(), String> {
    if parse_chatgpt_auth(&value).is_none() {
        return Err(
            "不是可用的 chatgpt auth.json(需 auth_mode=chatgpt + access/refresh token)".to_owned(),
        );
    }
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    import_locked(&paths, &value)?;
    set_relogin_required(false); // 刚导入一份(校验过的)真实账号,清掉失效标记
    Ok(())
}

/// 钉住当前检测到的真实账号(官方活动 auth.json)进持久镜像。
/// [review #5] locate + 写全程持 `AUTH_LOCK`,避免锁外读到 stale 值、随后被并发
/// refresh 抢先轮换 token,导致 pin 用过期 access + 已花掉的 refresh 覆盖刚刷新的。
pub async fn pin_current_account() -> Result<(), String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let located = locate_chatgpt_auth(&paths).ok_or("未检测到可钉住的真实 chatgpt 账号")?;
    import_locked(&paths, &located.value)
}

/// 忘记导入的真实账号(删持久镜像)= 退出"真实账号长期生效"。删镜像后启动不再
/// 自动恢复。删除已不存在的镜像视作成功(幂等)。
/// [review #1] 持 `AUTH_LOCK`,避免与 in-flight refresh 竞态(删了之后 refresh
/// 的 `write_auth` 又把镜像重建出来 → 已"忘记"的账号复活)。
pub async fn forget_imported() -> Result<bool, String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let mirror = imported_mirror_path(&paths);
    if !mirror.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&mirror).map_err(|e| format!("删持久镜像失败: {e}"))?;
    Ok(true)
}

/// [MOC-104 req#5 启动调谐] 启动时:① 刷新真实账号 token(保鲜);② 若用户导入过
/// 持久镜像、且活动 `~/.codex/auth.json` 已不是有效真实账号(被 apply 改 apikey /
/// 登出 / 清掉)→ 从镜像恢复到活动(先备份)。**只对用户显式导入/钉住的持久镜像
/// 自动恢复**,其它来源不自动抢活动文件(避免误覆盖代理模式的 apikey)。best-effort。
pub async fn reconcile_on_startup(client: &reqwest::Client) -> Result<RefreshOutcome, String> {
    // [review #2] 有 codex login 正在进行(用户在启动窗口内点了登录)→ 跳过调谐。
    // 否则会跟 codex login 抢写 `~/.codex/auth.json`:reconcile 刷的是登录前的旧
    // 账号,可能盖掉 codex login 刚写入的新账号。登录成功后前端会自动 pin/刷新。
    if matches!(login_status(), LoginState::Running) {
        tracing::info!("[RealAccount] 启动调谐跳过:codex login 进行中");
        return Ok(RefreshOutcome::NoAccount);
    }
    // 先刷新(refresh_if_needed 内部持 AUTH_LOCK)。
    let outcome = refresh_if_needed(client).await?;
    // [devin review] refresh 判定 refresh_token 永久失效 → 镜像里是同一份失效 token,
    // 恢复到活动只会把当前可用的 apikey 配置覆盖成坏掉的 chatgpt 凭据,毫无好处
    // (账号已死,用户必须重登)。跳过恢复,保留可用配置 + 由上层提示重登。
    if matches!(outcome, RefreshOutcome::ReloginRequired { .. }) {
        return Ok(outcome);
    }
    // 再看是否要把导入镜像恢复到活动。
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    if active_is_real_chatgpt(&paths) {
        return Ok(outcome); // 活动已是真实账号,不动。
    }
    if let Some(mirror_value) = read_imported_mirror(&paths) {
        backup_active_auth(&paths, "prereconcile")?;
        write_auth(&paths.auth_json, &mirror_value)
            .map_err(|e| format!("启动恢复导入账号到活动失败: {e}"))?;
        tracing::info!("[RealAccount] 启动调谐:活动文件非真实账号,已从导入镜像恢复");
    }
    Ok(outcome)
}

/// 启动 `codex login`(非阻塞)。已在进行中则返回 Err。
pub fn start_login() -> Result<(), String> {
    let mut g = login_lock();
    if g.running {
        return Err("登录已在进行中".to_owned());
    }
    let codex = resolve_codex_cli().ok_or("未找到 codex CLI;请确认已安装 Codex Desktop")?;
    // [I-1/B-1] codex login 会整文件重写 ~/.codex/auth.json;覆盖前先备份当前活动
    // 文件,备份失败即中止登录(非破坏)—— 不能让"换账号"丢掉原账号且无备份。
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    backup_active_auth(&paths, "prelogin")?;
    // 不覆盖 CODEX_HOME → codex login 写真实 `~/.codex/auth.json`,登录后即生效。
    // [N-2] stdout 丢弃(只靠 stderr 做失败摘要),避免用户长时间不完成 OAuth 时
    // codex login 往 stdout 刷日志写满 pipe 缓冲反卡住自己。
    let child = Command::new(&codex)
        .arg("login")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 codex login 失败: {e}"))?;
    g.pid = Some(child.id());
    g.running = true;
    g.cancel_requested = false;
    g.last = LoginState::Running;
    drop(g);

    // 后台线程 reap:wait_with_output 阻塞到 codex login 完成/被杀,记录结果。
    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let mut g = login_lock();
        g.running = false;
        g.pid = None;
        g.last = match result {
            Ok(out) if out.status.success() => {
                set_relogin_required(false); // 登录成功 = 拿到新鲜账号
                LoginState::Succeeded
            }
            Ok(out) => {
                if g.cancel_requested {
                    LoginState::Cancelled
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    LoginState::Failed(if stderr.is_empty() {
                        "codex login 非零退出".to_owned()
                    } else {
                        stderr
                    })
                }
            }
            Err(e) => LoginState::Failed(format!("等待 codex login 失败: {e}")),
        };
    });
    Ok(())
}

/// 取消进行中的登录(杀 `codex login` 进程)。返回是否有进行中的登录被取消。
///
/// [I-5 已知窗口] 用裸 pid kill;若进程刚自然退出、reap 线程还没清 `pid` 时取消,
/// 理论上可能 kill 到一个已回收/被复用的 pid。窗口是微秒级(reap 返回到拿锁清
/// pid 之间),概率极低;cancel_requested 标记保证即便误杀也只是把本次标记为
/// Cancelled。彻底免疫需持有 Child 句柄,当前架构 Child 在 reap 线程,留待后续。
pub fn cancel_login() -> bool {
    // [I-4] 锁内只读 pid + 置标记,kill 移到锁外执行 —— taskkill 可能阻塞数百 ms,
    // 不能卡住 status 轮询 / reap 线程拿同一把锁。
    let pid = {
        let mut g = login_lock();
        if !g.running {
            return false;
        }
        g.cancel_requested = true;
        g.pid
    };
    if let Some(pid) = pid {
        #[cfg(unix)]
        let kill = Command::new("kill").arg(pid.to_string()).status();
        #[cfg(windows)]
        let kill = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
        // [I-4] kill 失败不再静默吞 —— 留痕便于排查"点了取消但登录还在跑"。
        if let Err(e) = kill {
            tracing::warn!("[RealAccount] 取消登录 kill pid={pid} 失败: {e}");
        }
    }
    true
}

/// 当前登录流程状态(前端轮询)。锁中毒时恢复内部值,不静默退化成 Idle(N-1)。
pub fn login_status() -> LoginState {
    login_lock().last.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn chatgpt_auth() -> Value {
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "acc_xxx",
                "refresh_token": "ref_xxx",
                "id_token": "id_xxx",
                "account_id": "acct_123"
            },
            "last_refresh": "2026-05-31T00:00:00Z"
        })
    }

    #[test]
    fn parses_valid_chatgpt_auth() {
        let parsed = parse_chatgpt_auth(&chatgpt_auth()).expect("应识别为可用 chatgpt");
        assert_eq!(parsed.account_id.as_deref(), Some("acct_123"));
    }

    #[test]
    fn apikey_mode_is_not_chatgpt() {
        let v = json!({ "auth_mode": "apikey", "OPENAI_API_KEY": "cas_x" });
        assert!(parse_chatgpt_auth(&v).is_none());
    }

    #[test]
    fn chatgpt_missing_refresh_token_is_unusable() {
        let v = json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "acc_xxx" }
        });
        assert!(
            parse_chatgpt_auth(&v).is_none(),
            "缺 refresh_token 不能续期,视作不可用"
        );
    }

    #[test]
    fn chatgpt_empty_token_is_unusable() {
        let v = json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "  ", "refresh_token": "ref_xxx" }
        });
        assert!(
            parse_chatgpt_auth(&v).is_none(),
            "空白 access_token 视作不可用"
        );
    }

    #[test]
    fn empty_object_is_not_chatgpt() {
        assert!(parse_chatgpt_auth(&json!({})).is_none());
    }

    /// 在 tmp home 下写一份 auth.json(官方活动 or 某个备份 session)。
    fn write_json(path: &Path, v: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string(v).unwrap()).unwrap();
    }

    #[test]
    fn locate_prefers_official_chatgpt() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.auth_json, &chatgpt_auth());
        let found = locate_chatgpt_auth(&paths).expect("官方有 chatgpt 应命中");
        assert_eq!(found.source, AuthSource::Official);
        assert_eq!(found.path, paths.auth_json);
    }

    #[test]
    fn locate_ignores_snapshot_backups() {
        // 用户反馈:不能把 apply 快照备份里的旧 chatgpt 报成「你的真实账号」。
        // 活动是 apikey、镜像不存在、快照里有 chatgpt → locate 应返回 None。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.auth_json, &json!({"auth_mode": "apikey"}));
        write_json(
            &paths.active_snapshots_dir.join("sess-b").join("auth.json"),
            &chatgpt_auth(),
        );
        write_json(
            &paths.recovery_snapshots_dir.join("old").join("auth.json"),
            &chatgpt_auth(),
        );
        assert!(
            locate_chatgpt_auth(&paths).is_none(),
            "快照备份里的 chatgpt 不应被当成当前真实账号"
        );
        assert_eq!(active_auth_mode(&paths).as_deref(), Some("apikey"));
    }

    #[test]
    fn locate_finds_imported_mirror_when_active_apikey() {
        // 但用户显式导入的镜像应被认出(长期保留的真相源)。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.auth_json, &json!({"auth_mode": "apikey"}));
        write_json(&imported_mirror_path(&paths), &chatgpt_auth());
        let found = locate_chatgpt_auth(&paths).expect("镜像应被认出");
        assert_eq!(found.source, AuthSource::Imported);
    }

    #[test]
    fn locate_none_when_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        assert!(locate_chatgpt_auth(&paths).is_none());
    }

    fn make_jwt_with_exp(exp: i64) -> String {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({ "exp": exp })).unwrap());
        format!("header.{body}.sig")
    }

    #[test]
    fn access_token_expired_detects_past_and_skew() {
        let now = 1_000_000_000_i64;
        // 已过期
        assert!(access_token_expired(&make_jwt_with_exp(now - 10), now));
        // 在 skew(300s)窗口内 → 视作"将过期",要刷
        assert!(access_token_expired(&make_jwt_with_exp(now + 100), now));
        // 远未过期
        assert!(!access_token_expired(&make_jwt_with_exp(now + 10_000), now));
        // 不可解析 → 保守视作未过期
        assert!(!access_token_expired("not-a-jwt", now));
    }

    #[test]
    fn apply_refresh_updates_tokens_and_preserves_other_fields() {
        let mut auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "old_acc",
                "refresh_token": "old_ref",
                "id_token": "old_id",
                "account_id": "acct_keep"
            },
            "some_other_field": "keep_me"
        });
        let resp = OAuthRefreshResponse {
            access_token: "new_acc".to_owned(),
            id_token: Some("new_id".to_owned()),
            refresh_token: None, // 不轮换 → 保留旧值
        };
        apply_refresh_response(&mut auth, &resp, "2026-05-31T12:00:00Z").unwrap();
        let t = &auth["tokens"];
        assert_eq!(t["access_token"], "new_acc");
        assert_eq!(t["id_token"], "new_id");
        assert_eq!(
            t["refresh_token"], "old_ref",
            "响应没返回则保留旧 refresh_token"
        );
        assert_eq!(t["account_id"], "acct_keep", "account_id 透传不动");
        assert_eq!(auth["last_refresh"], "2026-05-31T12:00:00Z");
        assert_eq!(auth["some_other_field"], "keep_me", "无关字段透传不动");
    }

    #[test]
    fn login_state_serializes_with_tag_and_message() {
        assert_eq!(
            serde_json::to_value(LoginState::Running).unwrap(),
            json!({ "state": "running" })
        );
        assert_eq!(
            serde_json::to_value(LoginState::Failed("boom".to_owned())).unwrap(),
            json!({ "state": "failed", "message": "boom" })
        );
        assert_eq!(
            serde_json::to_value(LoginState::Cancelled).unwrap(),
            json!({ "state": "cancelled" })
        );
    }

    #[test]
    fn import_locked_writes_mirror_active_and_prebackup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        // 原活动是 apikey(代理模式常态)
        write_json(
            &paths.auth_json,
            &json!({"auth_mode": "apikey", "OPENAI_API_KEY": "cas_x"}),
        );
        import_locked(&paths, &chatgpt_auth()).unwrap();
        // 持久镜像写了 chatgpt(长期保留的真相源)
        assert!(
            read_imported_mirror(&paths).is_some(),
            "镜像应有可用 chatgpt"
        );
        assert_eq!(
            read_auth(&imported_mirror_path(&paths)).unwrap()["auth_mode"],
            "chatgpt"
        );
        // 活动文件也恢复成 chatgpt
        assert_eq!(read_auth(&paths.auth_json).unwrap()["auth_mode"], "chatgpt");
        // 覆盖活动前备份了原 apikey(时序安全,文件名带时间戳)
        let prebackup = std::fs::read_dir(paths.app_home.join("real-account"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("auth-preimport-"))
            })
            .expect("import 前应备份原活动 auth.json");
        assert_eq!(read_auth(&prebackup).unwrap()["auth_mode"], "apikey");
    }

    #[test]
    fn relogin_required_matches_permanent_failures_only() {
        assert!(looks_like_relogin_required(
            "400 {\"error\":\"invalid_grant\"}"
        ));
        assert!(looks_like_relogin_required(
            "refresh token has already been used"
        ));
        assert!(looks_like_relogin_required("token_invalidated"));
        // 瞬时/网络错误不算需重登
        assert!(!looks_like_relogin_required("500 internal server error"));
        assert!(!looks_like_relogin_required("timed out"));
    }

    #[test]
    fn apply_refresh_errs_when_not_object() {
        // [I-1] 顶层 / tokens 非 object → Err,绝不静默吞(否则把没刷成报成功)
        let resp = OAuthRefreshResponse {
            access_token: "x".to_owned(),
            id_token: None,
            refresh_token: None,
        };
        let mut not_obj = json!("i am a string");
        assert!(apply_refresh_response(&mut not_obj, &resp, "t").is_err());
        let mut bad_tokens = json!({ "auth_mode": "chatgpt", "tokens": "nope" });
        assert!(apply_refresh_response(&mut bad_tokens, &resp, "t").is_err());
    }
}
