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
//! - **token 刷新分流(transfer 自己绝不 POST 刷新)**:transfer 与源头 Codex 共享同一份
//!   single-use refresh_token,两个进程都刷会触发 `refresh_token_reused` 把账号烧死
//!   (`AUTH_LOCK` 只串行进程内、管不到外部 codex)。故刷新**只归源头**:检测获取
//!   (Official)由本机 Codex 自刷 `~/.codex/auth.json`;导入(Imported)由源那边 Codex
//!   刷、本侧 [`reconcile_on_startup`] 从源跟随重读;登录走 `codex login` 自取全新账号。
//!   [`access_token_expired`] 仅用于本地 JWT 判过期、标记 `relogin_required`,**不触发刷新**。
//! - **登录**([`start_login`]/[`cancel_login`]/[`login_status`]):调起官方
//!   `codex login`(它自己做 OAuth + 写 `~/.codex/auth.json`),非阻塞 + 可取消。
//! - **导入 / 长期保留**([`import_auth`]/[`pin_current_account`]/[`forget_imported`]/
//!   [`reconcile_on_startup`]):导入记录**源路径** + 写持久镜像快照;启动时活动文件失效
//!   则恢复 —— 优先从**活源路径**重读最新(跟随源 Codex 刷新)、源失效回落镜像快照。
//!   登录成功后前端自动 pin。单账号工具,非多账号切换器。
//!
//! 检测来源(优先级):① 官方 `~/.codex/auth.json`(Codex 当前活动凭据)→ ② 用户
//! 显式导入/钉住的持久镜像。**不扫 apply 快照备份** —— 那些是 transfer 改配置时的
//! 内部备份(可能是数周前早已失效的旧 chatgpt),报成「你的真实账号」会误导(用户
//! 实测反馈)。「长期保留」只认用户主动登录/导入产生的镜像。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// 最近一次启动调谐/检测判定「真实账号已失效、refresh_token 永久无效、需重新登录」。
    /// [connector review] 持久化到可查询的 status,而非只靠一次性 `emit` 事件 —— 启动时
    /// 若前端还没注册 listener,事件会丢;前端轮询 status 时读这个字段就不会漏报失效。
    pub relogin_required: bool,
    /// [MOC-257] 活动账号是否是「模拟(伪造)账号」(合成 auth.json,带 `cas_synthetic` 哨兵)。
    /// 前端据此显示「模拟账号」而非「真实账号」、隐藏 pin/导入(伪造账号不入持久镜像)。
    pub is_synthetic: bool,
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
            is_synthetic: active_is_synthetic(),
        }
    }
}

/// [connector review] 进程级「需重新登录」标记 —— reconcile/检测判定 refresh_token
/// 永久失效时置真,登录/导入/检测到有效账号后清零。比一次性 `emit` 事件可靠:前端任何时候
/// 轮询 `status` 都能读到,不受「事件早于 listener 注册」的启动时序影响。
static RELOGIN_REQUIRED: AtomicBool = AtomicBool::new(false);

/// [MOC-124 H-2] proxy 401 回灌时记下「被服务端撤销的 token」指纹(FNV-1a,0=指纹未知)。detect
/// 的 self-heal 据此判:active token 指纹 == 此值 → 还是那个被撤销的旧 token(exp 没过也别清
/// relogin);≠ → token 换了(app 外 codex login / 重新导入)→ 可清。
static REVOKED_TOKEN_FP: AtomicU64 = AtomicU64::new(0);

/// [MOC-124 H-2 / codex-connector P2] 是否有 proxy 401 撤销记录(**独立于指纹**)。区分两种
/// `REVOKED_TOKEN_FP==0`:① 从没 proxy 401(无撤销)→ detect 照常自愈清 stale relogin;②
/// no-bearer 401(请求无 Authorization → 指纹算成 0,有撤销但指纹未知)→ 保守保持 relogin、
/// **不**被当成「无记录」而自清。少了这个 flag,no-bearer 401 会让 detect 漏报真撤销。
static HAS_REVOCATION: AtomicBool = AtomicBool::new(false);

/// 读「需重新登录」标记。
pub fn relogin_required() -> bool {
    RELOGIN_REQUIRED.load(Ordering::SeqCst)
}

/// 设「需重新登录」标记(reconcile/检测判定失效时 true;有新鲜账号时 false)。
fn set_relogin_required(v: bool) {
    RELOGIN_REQUIRED.store(v, Ordering::SeqCst);
}

/// [MOC-124 H-2] 清 relogin 标记 + 被撤销 token 指纹记录(拿到新账号 / token 真换了时)。
/// 比裸 `set_relogin_required(false)` 多清 [`REVOKED_TOKEN_FP`],避免旧的撤销指纹残留误判。
fn clear_relogin_state() {
    set_relogin_required(false);
    REVOKED_TOKEN_FP.store(0, Ordering::SeqCst);
    HAS_REVOCATION.store(false, Ordering::SeqCst);
}

/// [MOC-124 H-2] proxy 透传探测到 chatgpt backend 上游 401(服务端 token 失效)时回灌。proxy
/// crate 不依赖 src-tauri,经 `ProxyState::with_relogin_notify` 注入的回调调到这里。`token_fp`
/// = 被撤销 token(该请求 Authorization Bearer)的指纹(跟 [`access_token_fingerprint`] 同算法)。
/// 跟本地 JWT `exp` 判定独立 —— 服务端撤销 / refresh_token 失效本地 exp 看不到,上游 401 是唯一
/// 信号。`detect()` self-heal 用指纹区分「还是这个旧 token」(保持 relogin)vs「换了新 token」
/// (清)。清零由 detect 换 token 时、或 login/import/forget 拿到新账号时做。
///
/// **只标记、不做 2xx 自愈**(codex-connector P2):并发请求乱序下撤销前的旧 2xx 会清掉撤销后的
/// 401 标记、漏报真撤销(危险)。而 chatgpt backend 的 401 = 真 token 问题、不存在「token 有效
/// 但瞬时 401」,故无需自愈;401 一律标记(误报方向安全,detect 换 token 自然清)。
///
/// **no-bearer 401**(codex-connector P2):请求无 Authorization 时 `token_fp==0`。置
/// [`HAS_REVOCATION`] 但**不**用 0 覆盖已记录的撤销指纹 —— 否则会擦掉之前真撤销 token 的指纹、
/// 让 detect 误判「无记录」自清。`HAS_REVOCATION` 让 detect 把这种「有撤销但指纹未知」保守保持。
pub fn mark_relogin_required_from_proxy(token_fp: u64) {
    HAS_REVOCATION.store(true, Ordering::SeqCst);
    if token_fp != 0 {
        REVOKED_TOKEN_FP.store(token_fp, Ordering::SeqCst);
    }
    set_relogin_required(true);
}

/// [MOC-124 H-2] 算 auth.json 的 `tokens.access_token` 的 FNV-1a 64 指纹,跟 proxy 侧
/// `authorization_token_fingerprint` **同算法**(对 raw token、同 offset basis + prime),
/// 用于 detect self-heal 判 active token 是否就是被 proxy 401 标记撤销的那个。空 token → 0。
fn access_token_fingerprint(auth: &Value) -> u64 {
    let token = auth
        .get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if token.is_empty() {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// [MOC-124 H-2] detect self-heal 见有效 Official token 时**是否该清** relogin。
/// - `!has_revocation`(从没 proxy 401)→ 清(detect 照常自愈 detect-None 设的 stale relogin)。
/// - 有撤销 + 指纹未知(`revoked_fp==0`,no-bearer 401,codex P2)→ **不清**(保守保持,避免被当
///   成「无记录」漏报真撤销)。
/// - 有撤销 + active 指纹 ≠ 被撤销的(token 换了:app 外 login / 重新导入)→ 清。
/// - 有撤销 + 指纹相同(还是那个被服务端撤销、本地 exp 没过的旧 token)→ **不清**,否则会抹掉
///   proxy 401 探测、H-2 形同无效(本 PR 的 BLOCKER)。
fn should_clear_relogin(active_token_fp: u64, revoked_fp: u64, has_revocation: bool) -> bool {
    if !has_revocation {
        return true;
    }
    revoked_fp != 0 && active_token_fp != revoked_fp
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
    parse_chatgpt_tokens(v)
}

/// [MOC-178] 只校验 chatgpt tokens 有效(access/refresh 非空),**不看 auth_mode**。供
/// 「账号可用性」(`detect` 新口径)用:清除真实账号切 apikey 后 tokens 仍在活动文件 →
/// 账号状态仍应「获取成功」、用户可再开真实账号模式。`parse_chatgpt_auth` 复用它再叠
/// auth_mode==chatgpt 判定,供「活动当前就是 chatgpt」(relay 生效 / reconcile)用。
fn parse_chatgpt_tokens(v: &Value) -> Option<ChatgptAuth> {
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

/// [MOC-104 导入分流] 记录「导入来源路径」的 metadata 文件(跟镜像同目录)。导入时
/// 记下用户选的源文件绝对路径;`reconcile_on_startup` 据此**从源重读最新 token**
/// (活源:另一个在跑的 Codex 的 auth.json 被那边刷新 → transfer 跟随、自己不刷新),
/// 源不存在/不可读时回落到镜像快照(静态导入)。两种导入形态统一覆盖。
fn imported_source_path_file(paths: &CodexPaths) -> PathBuf {
    paths
        .app_home
        .join("real-account")
        .join("imported-source.json")
}

/// 读「导入来源路径」(无记录 / 文件坏 → None)。
fn read_imported_source_path(paths: &CodexPaths) -> Option<PathBuf> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(imported_source_path_file(paths)).ok()?)
            .ok()?;
    v.get("source_path")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

/// 写「导入来源路径」metadata(`None` = 清除记录,如 pin 当前账号无外部源)。best-effort:
/// 记录失败不该让导入整体失败(镜像 + 活动已落盘),只 warn。
fn write_imported_source_path(paths: &CodexPaths, source_path: Option<&str>) {
    let file = imported_source_path_file(paths);
    match source_path {
        Some(p) => {
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let body = serde_json::json!({ "source_path": p }).to_string();
            if let Err(e) = std::fs::write(&file, body) {
                tracing::warn!("[RealAccount] 记录导入来源路径失败(忽略): {e}");
            }
        }
        None => {
            let _ = std::fs::remove_file(&file);
        }
    }
}

/// 定位**当前**可用的真实 chatgpt 账号:① 官方活动 `~/.codex/auth.json` → ② 用户
/// 显式导入/钉住的持久镜像。**不扫 apply 快照备份** —— 那些是 transfer 改配置时
/// 的内部备份(可能是几周前早已失效的旧 chatgpt),报成「你的真实账号」会误导
/// 用户、让活动是 apikey 的人以为账号被改(用户实测反馈)。「长期保留」只认用户
/// 主动登录/导入产生的镜像。[`detect`] / reconcile 共用,口径一致。只读。
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

/// [MOC-178] 定位有效 chatgpt **tokens**(不看 auth_mode):① 活动 auth.json 的 tokens 有效
/// → Official;② 镜像有效 → Imported。供 `detect` 的「账号可用性」用 —— 清除真实账号切
/// apikey 后活动 auth_mode=apikey 但 tokens 还在,仍定位得到(账号仍可用、可再开)。
fn locate_chatgpt_tokens(paths: &CodexPaths) -> Option<LocatedChatgptAuth> {
    if let Ok(v) = read_auth(&paths.auth_json) {
        if let Some(parsed) = parse_chatgpt_tokens(&v) {
            return Some(LocatedChatgptAuth {
                path: paths.auth_json.clone(),
                source: AuthSource::Official,
                value: v,
                account_id: parsed.account_id,
            });
        }
    }
    let mirror = imported_mirror_path(paths);
    if mirror.is_file() {
        if let Ok(v) = read_auth(&mirror) {
            if let Some(parsed) = parse_chatgpt_tokens(&v) {
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
/// [MOC-178 codex P2] 找「有效(非空 + 本地 JWT 未过期)」chatgpt token:活动有效 → Official;
/// 活动无效/过期但镜像有效 → Imported;都无效 → None。区别于 `locate_chatgpt_tokens`(只判非空、
/// 不判过期,供 reconcile「有 token 可保留」用):本函数供 detect 判「账号当前可用」——过期 token
/// 不算可用(transfer 不刷新 token,过期只能靠重登 / 重新导入恢复)。只读。
fn locate_valid_chatgpt_tokens(paths: &CodexPaths) -> Option<LocatedChatgptAuth> {
    let valid = |v: &Value| -> bool {
        let access = v
            .get("tokens")
            .and_then(|t| t.get("access_token"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        parse_chatgpt_tokens(v).is_some()
            && !access_token_expired(access, chrono::Utc::now().timestamp())
    };
    if let Ok(v) = read_auth(&paths.auth_json) {
        if valid(&v) {
            let account_id = parse_chatgpt_tokens(&v).and_then(|c| c.account_id);
            return Some(LocatedChatgptAuth {
                path: paths.auth_json.clone(),
                source: AuthSource::Official,
                value: v,
                account_id,
            });
        }
    }
    let mirror = imported_mirror_path(paths);
    if mirror.is_file() {
        if let Ok(v) = read_auth(&mirror) {
            if valid(&v) {
                let account_id = parse_chatgpt_tokens(&v).and_then(|c| c.account_id);
                return Some(LocatedChatgptAuth {
                    path: mirror,
                    source: AuthSource::Imported,
                    value: v,
                    account_id,
                });
            }
        }
    }
    None
}

pub fn detect() -> RealAccountStatus {
    let Ok(paths) = CodexPaths::from_home_env() else {
        // 连 home 都解析不到 —— 当作"没有",不 panic。
        return RealAccountStatus::none(None, false);
    };
    let active_mode = active_auth_mode(&paths);
    let has_imported = read_imported_mirror(&paths).is_some();
    // [MOC-178] 账号可用性认 token(活动或镜像有**有效**chatgpt token,含本地 JWT 未过期),不看
    // auth_mode —— 清除切 apikey 后 tokens 仍在且未过期 → 账号状态仍「获取成功」、用户可再开。
    match locate_valid_chatgpt_tokens(&paths) {
        Some(found) => {
            // [connector review 自愈] 活动文件就是有效真实 chatgpt = 账号当前确实可用 → 清掉可能
            // stale 的「需重新登录」标记(覆盖 app 外重新 codex login / 直接恢复活动文件场景)。
            // [MOC-124 H-2] 但**只在 token 真换了**时清:proxy 401 回灌记下了被撤销 token 的指纹,
            // 若 active 还是那个旧 token(指纹相同、本地 exp 没过)说明服务端撤销仍在,清了等于抹掉
            // proxy 探测(detect 用 local-exp 判有效、看不到撤销);指纹不同 = app 外 login / 重新
            // 导入换了新 token → 才清。`REVOKED_TOKEN_FP==0`(无撤销记录)照常清,保持原自愈语义。
            if found.source == AuthSource::Official
                && should_clear_relogin(
                    access_token_fingerprint(&found.value),
                    REVOKED_TOKEN_FP.load(Ordering::SeqCst),
                    HAS_REVOCATION.load(Ordering::SeqCst),
                )
            {
                clear_relogin_state();
            }
            RealAccountStatus {
                logged_in: true,
                active_auth_mode: active_mode,
                account_id: found.account_id,
                source: found.source,
                has_imported,
                relogin_required: relogin_required(),
                is_synthetic: active_is_synthetic(),
            }
        }
        None => {
            // [MOC-178 codex P2] 没有**有效**token。但若有 token(非空、只是过期 —— locate_chatgpt_tokens
            // 认非空)→ 标记需重登:清除后 tokens 留活动文件、随时间过期,不标记会让 dashboard 报账号
            // 可用 + offer enable,enable 时 activate 才 reject expired(UI vs reality 不一致)。
            if locate_chatgpt_tokens(&paths).is_some() {
                set_relogin_required(true);
            }
            RealAccountStatus::none(active_mode, has_imported)
        }
    }
}

use base64::Engine;
/// 提前于真实过期点判失效(skew),避免 in-flight 请求恰好撞 401。
const EXPIRY_SKEW_SECONDS: i64 = 300;

/// reconcile / import 的账号检测结果(transfer 分流后**绝不刷新**,故名 `ReconcileOutcome`;
/// 只表示检测/恢复的判定,不含"刷新成功"态)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ReconcileOutcome {
    /// 没有可用的真实 chatgpt 账号(官方活动 + 持久镜像都没有)。
    NoAccount,
    /// access_token 本地 JWT 未到期(或无法解析 → 保守视作有效),账号可用。
    StillValid { source: AuthSource },
    /// 真实账号**不可用**:本地 JWT 已过期 / 镜像废 token —— 需要重新登录。上层据此
    /// 自动关「自动解锁」开关 + emit 事件提示用户重登。
    ReloginRequired { source: AuthSource },
    /// [MOC-178] 用户主动关了真实账号模式(flag=false),但活动可能被退出 restore 写回 chatgpt。
    /// caller 据此 apply 切 apikey 收敛回关闭态(保留 tokens),在 daemon 决策前执行。
    ForceDisable { had_valid_token: bool },
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

/// [MOC-104 review P1/I-3] 串行化 import / pin / forget / reconcile 对 auth.json + 持久镜像
/// 的整个「读 → 判定 → 备份 → 写活动 → 写镜像」序列,防并发入口交错写互相覆盖。
/// **异步** mutex —— 锁内跨多次 `.await`(文件 IO),不能用只锁同步段的 std mutex。
/// 注:transfer 分流后**不在锁内做任何刷新网络 POST**(刷新归源头 Codex —— transfer 与其
/// 共享 single-use refresh_token,自己刷会触发 `refresh_token_reused` 烧账号,openai/codex#7144)。
static AUTH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
fn import_locked(
    paths: &CodexPaths,
    value: &Value,
    source_path: Option<&str>,
) -> Result<(), String> {
    // 先恢复到活动(覆盖前先备份)—— 任一步失败直接返回,镜像保持原样不被污染。
    backup_active_auth(paths, "preimport")?;
    write_auth(&paths.auth_json, value).map_err(|e| format!("写活动 auth.json 失败: {e}"))?;
    // 活动已成功更新后,才提交长期保留的持久镜像。
    let mirror = imported_mirror_path(paths);
    if let Some(parent) = mirror.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("镜像目录创建失败: {e}"))?;
    }
    write_auth(&mirror, value).map_err(|e| format!("写持久镜像失败: {e}"))?;
    // [MOC-104 导入分流] 记录/清除导入来源路径:文件导入记下源路径(reconcile 从源
    // 跟随刷新);pin 当前账号无外部源传 None(清记录,纯快照)。best-effort,不阻断导入。
    write_imported_source_path(paths, source_path);
    Ok(())
}

/// [MOC-104 req] 从**文件路径**导入真实 chatgpt auth(活源 / 静态文件统一入口)。读源
/// 文件 → 校验可用 chatgpt → 写持久镜像快照 + **记录源路径** + 恢复到活动(先备份)。
/// **不刷新** token(分流:刷新归源头);按本地 JWT exp 判过期设 relogin 标记。记下源路
/// 径后,`reconcile_on_startup` 可在启动时从**活源**重读最新(跟随那边 Codex 刷新);源
/// 失效/移除则回落到此处写的快照。前端用 Tauri dialog 选文件、把绝对路径传进来。
pub async fn import_auth(source_path: String) -> Result<(), String> {
    let content = std::fs::read_to_string(&source_path)
        .map_err(|e| format!("读导入源文件失败({source_path}): {e}"))?;
    let value: Value =
        serde_json::from_str(&content).map_err(|e| format!("导入源不是合法 JSON: {e}"))?;
    if parse_chatgpt_auth(&value).is_none() {
        return Err(
            "不是可用的 chatgpt auth.json(需 auth_mode=chatgpt + access/refresh token)".to_owned(),
        );
    }
    // [connector review] 导入**不刷新** token;先按本地 JWT exp 判过期 —— 过期则**拒绝导入、
    // 不激活**(不让过期账号覆盖当前可用活动 + 镜像;否则 import_locked 已写活动,reconcile 之后
    // 还会从过期镜像恢复,等于默默激活了死账号)。有效 token 才落盘激活。
    let access = value
        .get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if access.is_empty() || access_token_expired(access, chrono::Utc::now().timestamp()) {
        set_relogin_required(true);
        return Err(
            "导入文件的登录态已过期,请重新导出最新 auth.json 或改用「登录真实账号」".to_owned(),
        );
    }
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    import_locked(&paths, &value, Some(&source_path))?;
    clear_relogin_state(); // [MOC-124 H-2] 有效账号导入成功,清失效标记 + 撤销指纹
    Ok(())
}

/// 钉住当前检测到的真实账号(官方活动 auth.json)进持久镜像。
/// [review #5] locate + 写全程持 `AUTH_LOCK`,避免锁外读到 stale 值、随后被并发
/// reconcile/import 抢先改写 auth.json,导致 pin 钉到被覆盖前的旧值。
pub async fn pin_current_account() -> Result<(), String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let located = locate_chatgpt_auth(&paths).ok_or("未检测到可钉住的真实 chatgpt 账号")?;
    // pin 钉的是 Official 活动账号(源即 ~/.codex,reconcile 已优先读 Official)→ 无外部源,
    // 传 None(纯快照保留 + 清掉旧 source 记录),避免 reconcile 误从 ~/.codex 绕一圈重读。
    import_locked(&paths, &located.value, None)
}

/// 忘记导入的真实账号(删持久镜像)= 退出"真实账号长期生效"。删镜像后启动不再
/// 自动恢复。删除已不存在的镜像视作成功(幂等)。
/// [review #1] 持 `AUTH_LOCK`,避免与 in-flight reconcile/import 竞态(删了之后 reconcile
/// 的 `write_auth` 又把镜像重建出来 → 已"忘记"的账号复活)。
pub async fn forget_imported() -> Result<bool, String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let mirror = imported_mirror_path(&paths);
    let had_mirror = mirror.is_file();
    if had_mirror {
        std::fs::remove_file(&mirror).map_err(|e| format!("删持久镜像失败: {e}"))?;
        // [MOC-104 导入分流] 镜像删了,导入来源路径记录也一并清(否则 reconcile 还会从旧
        // 源路径重读、把已"忘记"的账号复活)。
        write_imported_source_path(&paths, None);
    }
    // [MOC-124 H-2 / codex-connector P2] **不**在这里清 relogin / 撤销指纹 —— forget_imported
    // 只删导入镜像、**保留活动 auth.json tokens**(见下 MOC-178)。若活动 token 正是被服务端 401
    // 撤销的那个,清掉撤销状态会让它在重新启用真账号时被 detect 当 healthy 呈现(漏报撤销)。交给
    // detect 自然处理:活动 token 有效(指纹不同 / 无撤销记录)→ self-heal 清 relogin;还是被撤销
    // 的那个(指纹相同)→ 保持提示重登。比硬清更正确(detect 的指纹对比本就区分这两种)。
    //
    // [MOC-178] 不在这里删/改活动 auth.json —— 删整个文件会丢 tokens(退出 restore 只恢复
    // MANAGED 的 auth_mode/OPENAI_API_KEY、tokens 恢复不回 → 残缺)。停用真实账号(让 toggle
    // 关 + Codex 原生不显示 plugins)改由 forget_handler apply 当前 provider 强制 non-relay
    // 完成:写 auth_mode=apikey 但**保留 tokens**,退出 restore 才能写回 chatgpt + tokens 完整恢复。
    Ok(had_mirror)
}

/// [MOC-178] 开真实账号模式:把活动 auth.json 写回 `auth_mode=chatgpt` + 有效 tokens,使
/// 后续 apply relay 的 gate(`active_is_real_chatgpt_now`)通过、Codex 原生显示 plugins。
/// 优先用活动现存 tokens(清除切 apikey 后 tokens 仍在 → 只需改 auth_mode + 删 apikey key);
/// 活动无有效 token 则从持久镜像恢复整份。先备份再写。持 `AUTH_LOCK`。返回是否成功激活
/// (有可用 token);无可用 token → `Ok(false)`(caller 提示重登)。
pub async fn activate_real_account() -> Result<bool, String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    // 已是真实 chatgpt → no-op 成功。
    if active_is_real_chatgpt(&paths) {
        return Ok(true);
    }
    // 活动有有效 chatgpt tokens(但 auth_mode 非 chatgpt,如清除后的 apikey)→ 只改 auth_mode。
    // [codex-review P2] 必须连本地 JWT 过期一起判 —— parse_chatgpt_tokens 只校验非空,清除后保留的
    // token 会随时间过期;不检查会激活 expired token + 报 Ok(true),前端显示「已开启」但 runtime 全
    // 401,还会 shadow 掉下面可能有效的镜像(本分支 early-return 在镜像分支前)。过期则 fall through
    // 到镜像分支(下面会判过期再决定是否恢复)。
    if let Ok(mut v) = read_auth(&paths.auth_json) {
        if parse_chatgpt_tokens(&v).is_some() {
            let access = v
                .get("tokens")
                .and_then(|t| t.get("access_token"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !access_token_expired(access, chrono::Utc::now().timestamp()) {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("auth_mode".into(), Value::String("chatgpt".into()));
                    obj.remove("OPENAI_API_KEY");
                }
                backup_active_auth(&paths, "preactivate")?;
                write_auth(&paths.auth_json, &v).map_err(|e| format!("写回 chatgpt 失败: {e}"))?;
                return Ok(true);
            }
        }
    }
    // 活动无 token → 从持久镜像恢复整份(镜像有效且本地 JWT 未过期)。
    if let Some(v) = read_imported_mirror(&paths) {
        let access = v
            .get("tokens")
            .and_then(|t| t.get("access_token"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !access.is_empty() && !access_token_expired(access, chrono::Utc::now().timestamp()) {
            backup_active_auth(&paths, "preactivate")?;
            write_auth(&paths.auth_json, &v).map_err(|e| format!("从镜像恢复失败: {e}"))?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// [MOC-178 codex P2] 关真实账号模式的 auth 兜底:直接改活动 auth.json `auth_mode=apikey`
/// (保留 tokens),**不依赖 provider config**。forget / enable 失败回滚走的 sync 路径依赖
/// active provider(无 provider(默认 activeProvider null)/ apply 失败 → sync success:false、
/// 活动仍 chatgpt),用本函数兜底确保活动不留 chatgpt(否则 Codex 仍显示 plugins、跟 flag=false
/// 不一致,要等下次启动 ForceDisable 才纠)。持 `AUTH_LOCK`。返回是否执行了切换(活动本就
/// 非 chatgpt → `Ok(false)` no-op)。
pub async fn deactivate_real_account() -> Result<bool, String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let Ok(mut v) = read_auth(&paths.auth_json) else {
        return Ok(false);
    };
    if v.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Ok(false);
    }
    if let Some(obj) = v.as_object_mut() {
        obj.insert("auth_mode".into(), Value::String("apikey".into()));
    }
    backup_active_auth(&paths, "predeactivate")?;
    write_auth(&paths.auth_json, &v).map_err(|e| format!("切 apikey 失败: {e}"))?;
    Ok(true)
}

// ── [MOC-257] 模拟(伪造)账号:合成合规 auth.json + 开关 ────────────────────────
//
// 无真实 ChatGPT 账号时的「强制解锁」新实现:不再靠 CDP 改渲染层 authMethod(只改 UI、auth.json
// 仍 apikey → 通信层不自洽,插件跑不通),而是写一份**合规但伪造**的 auth.json(auth_mode=chatgpt
// + 合成 JWT),让 Codex 原生走 chatgpt 路径(原生显示 Plugins、CLI 原生发 `/backend-api/*`);这些
// 账号/插件请求由 proxy 截断逐条伪造 200(见 `codex_app_transfer_proxy::fake_account`),不透传真
// chatgpt.com(伪造 token 会被上游 401)。relay 装配(`active_is_real_chatgpt_now` → 写
// `chatgpt_base_url`→proxy)整套复用,无需新增 managed key。

/// 模拟账号的固定哨兵 account_id —— detect / 前端据此区分「伪造」vs「真实」账号。
pub const SYNTHETIC_ACCOUNT_ID: &str = "cas-synthetic-0000-0000-0000-000000000000";

/// 活动 auth.json 是否是模拟(伪造)账号 —— 顶层 `cas_synthetic==true` 哨兵。home 解析失败 → false。只读。
pub fn active_is_synthetic() -> bool {
    CodexPaths::from_home_env()
        .ok()
        .and_then(|p| read_auth(&p.auth_json).ok())
        .and_then(|v| v.get("cas_synthetic").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// 合成 JWT(`base64url(header).base64url(payload).<占位签名>`)。resolver 只验形状不验签
/// (`crates/proxy/src/resolver.rs::is_chatgpt_access_token`),故占位签名足够;`exp` 由 caller
/// 放进 payload(远未来),让本地 JWT exp 判定不过期、且 Codex CLI 不触发 refresh。
fn synth_jwt(payload: &Value) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap_or_default());
    format!("{header}.{body}.cas-synthetic-sig")
}

/// 构造一份合规但伪造的 chatgpt `auth.json`。`exp` 设远未来(10 年)杜绝 Codex CLI 触发 token
/// 刷新(假 refresh_token 刷新必失败);带 `cas_synthetic` 哨兵供识别。
fn build_synthetic_auth() -> Value {
    let now = chrono::Utc::now();
    let exp = now.timestamp() + 10 * 365 * 86400;
    let access = synth_jwt(&serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": SYNTHETIC_ACCOUNT_ID },
        "iss": "https://auth.openai.com",
        "exp": exp,
    }));
    let id_token = synth_jwt(&serde_json::json!({
        "https://api.openai.com/profile": { "email": "plugins@local.codex-app-transfer" },
        "email": "plugins@local.codex-app-transfer",
        "exp": exp,
    }));
    serde_json::json!({
        "OPENAI_API_KEY": null,
        "auth_mode": "chatgpt",
        "cas_synthetic": true,
        "last_refresh": now.to_rfc3339(),
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": "cas-synthetic-refresh-token",
            "account_id": SYNTHETIC_ACCOUNT_ID,
        }
    })
}

/// [MOC-257] 开模拟账号模式:把活动 auth.json 写成合成伪造账号(先备份)。幂等:活动已是合成
/// 账号则 no-op(不每次 churn auth.json + 触发 Codex 重读)。持 `AUTH_LOCK`。caller 随后 apply
/// relay(写 `chatgpt_base_url`→proxy)。
///
/// **防误伤**:活动有**真实 chatgpt tokens**(无哨兵)→ 拒绝,不覆盖真账号(应改用真实账号模式)。
/// [MOC-257 bot P1] 用 `parse_chatgpt_tokens`(只看 tokens 非空、**不看 auth_mode**)判定,而非
/// `parse_chatgpt_auth`(要 auth_mode==chatgpt):用户关真实账号模式后,活动是 `auth_mode=apikey` 但
/// **保留真 tokens**(MOC-178 设计,供退出 restore 恢复);若只判 auth_mode==chatgpt 会漏判这种态、
/// 合成写覆盖掉唯一一份可恢复的真 tokens(disable 又不自动 restore 备份)→ 用户丢真账号。
pub async fn activate_fake_account() -> Result<(), String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    if let Ok(v) = read_auth(&paths.auth_json) {
        if v.get("cas_synthetic").and_then(Value::as_bool) == Some(true) {
            // 我们自己的合成账号。**完整**(auth_mode==chatgpt)才 no-op;[MOC-257 真机] 哨兵在但
            // auth_mode 被改残(如被另一个非 fake-aware build 的 reconcile 切成 apikey)→ fall through
            // 重写干净合成账号自愈(否则 Codex auth_mode=apikey 不显示 plugins、relay gate 也不过)。
            if v.get("auth_mode").and_then(Value::as_str) == Some("chatgpt") {
                return Ok(());
            }
        } else if parse_chatgpt_tokens(&v).is_some() {
            // 非合成 + 有真实 chatgpt tokens(无哨兵,含「已切 apikey 但保留 tokens」态)→ 拒绝覆盖。
            return Err(
                "检测到 ChatGPT 登录态(tokens),模拟账号会覆盖它;请改用「真实账号模式」,或先在真实账号面板彻底清除"
                    .to_owned(),
            );
        }
    }
    backup_active_auth(&paths, "prefake")?;
    write_auth(&paths.auth_json, &build_synthetic_auth())
        .map_err(|e| format!("写合成 auth.json 失败: {e}"))?;
    Ok(())
}

// ── [MOC-257 三态] 插件解锁三态(off / synthetic / real)+ 真账号 stash ──────────────
//
// 三态选择器统一 off / 模拟账号 / 真实账号,取代旧 autoUnlockCodexPlugins(CDP,已废弃)+
// fakeAccountModeEnabled + realAccountModeEnabled。非 off 一律写 chatgpt_base_url(不按账号类型
// gate)。off 持久化:把真账号 auth.json **整文件** stash 走,退出/切回 real 时整文件还原 ——
// 因现有快照 restore 只补 auth.json 的 managed key(auth_mode/OPENAI_API_KEY)、**不恢复 tokens**,
// 直接移走真账号会丢 tokens,故用专用 stash 保全。

/// 三态枚举。`resolve_plugin_unlock_mode` 由持久键 + 真账号检测推导。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUnlockMode {
    Off,
    Synthetic,
    Real,
}

/// 真账号「被位移暂存区」:synthetic/off 占用活动 auth.json 时,把真账号整文件移到这里(保 tokens),
/// 切回 real / 退出 / 启动 self-heal 前整文件还原。`~/.codex` 之外,不被 Codex 改、不被快照轮转。
fn real_account_stash_path(paths: &CodexPaths) -> PathBuf {
    paths
        .app_home
        .join("plugin-unlock")
        .join("stashed-real-auth.json")
}

/// stash 是否存在(暂存着一份被位移的真账号)。只读。
pub fn real_account_stash_exists() -> bool {
    CodexPaths::from_home_env()
        .map(|p| real_account_stash_path(&p).is_file())
        .unwrap_or(false)
}

/// 是否「本地有真账号可用」:活动 auth.json 有真实 chatgpt tokens(非合成),或 stash 里存着真账号。
/// 供三态默认推导(键缺失 → 有真账号则 real、否则 synthetic)。只读。
pub fn has_real_account() -> bool {
    let Ok(paths) = CodexPaths::from_home_env() else {
        return false;
    };
    let is_real = |v: &Value| {
        v.get("cas_synthetic").and_then(Value::as_bool) != Some(true)
            && parse_chatgpt_tokens(v).is_some()
    };
    let active_real = read_auth(&paths.auth_json)
        .ok()
        .is_some_and(|v| is_real(&v));
    // [MOC-257 review] 也认导入/钉住的镜像 + 导入活源(source_path 还在 + 可读):活动被删 / 换成 apikey
    // 时,真账号只在镜像或活源里(import/pin 过、reconcile 会从它恢复)。否则这些老用户 set(real) 被拒、
    // 默认推导降级 synthetic。
    let mirror_real = read_imported_mirror(&paths).is_some_and(|v| is_real(&v));
    let source_real = read_imported_source_path(&paths)
        .and_then(|sp| std::fs::read_to_string(&sp).ok())
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .is_some_and(|v| is_real(&v));
    active_real || mirror_real || source_real || real_account_stash_exists()
}

/// [MOC-257 review] 一个 auth.json `Value` 是否是**当前可用的真实** chatgpt 账号:非合成、access/refresh
/// 非空、不是被服务端 401 撤销的那个 token(指纹比对)、且 access_token 本地 JWT 未过期。只读、无副作用。
/// 供 `real_account_usable`(降级判定)+ `restore_stashed_real_auth_impl`(活动是否比 stash 新/好)复用。
fn auth_value_real_and_usable(v: &Value) -> bool {
    if v.get("cas_synthetic").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if parse_chatgpt_tokens(v).is_none() {
        return false;
    }
    // 就是被服务端 401 撤销的那个 token → 不可用(token 已换 → 指纹不同 → 放行)。
    let revoked_fp = REVOKED_TOKEN_FP.load(Ordering::SeqCst);
    let has_revocation = HAS_REVOCATION.load(Ordering::SeqCst);
    if has_revocation && revoked_fp != 0 && access_token_fingerprint(v) == revoked_fp {
        return false;
    }
    let access = v
        .get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    !access_token_expired(access, chrono::Utc::now().timestamp())
}

/// [MOC-257] 真账号当前是否**实际可用**(供「real 档不可用则降级 synthetic」+ 前端展示)。可用 = 活动 /
/// 导入活源 / 导入镜像 / stash 任一有可用真账号(`auth_value_real_and_usable`)。
///
/// [MOC-257 review] **不**盲目早返 `relogin_required()`(进程级粘滞标记)—— app 外重登换新 token 后标记
/// 一时未清会误降级;改逐源直接判(过期 + 撤销指纹)。真正清标记由 `detect()` 的指纹自愈做(这里只读)。
pub fn real_account_usable() -> bool {
    let Ok(paths) = CodexPaths::from_home_env() else {
        return false;
    };
    // 有撤销但指纹未知(proxy 算不出被撤 token 指纹)→ 保守判全部不可用(对齐 detect「有撤销但指纹未知
    // 就保持 relogin」,见 `mark_relogin_required_from_proxy` 注释)。
    if HAS_REVOCATION.load(Ordering::SeqCst) && REVOKED_TOKEN_FP.load(Ordering::SeqCst) == 0 {
        return false;
    }
    if read_auth(&paths.auth_json)
        .ok()
        .is_some_and(|v| auth_value_real_and_usable(&v))
    {
        return true;
    }
    // [MOC-257 review] 导入活源(source_path 还在 + 可读 + chatgpt + 可用)——对齐 reconcile_on_startup:
    // 镜像快照过期但活源已被那边 Codex 刷新时不误降级(否则 degrade 后 real-only 的 reconcile 不跑、活源
    // 永不被恢复)。
    if read_imported_source_path(&paths)
        .and_then(|sp| std::fs::read_to_string(&sp).ok())
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .is_some_and(|v| auth_value_real_and_usable(&v))
    {
        return true;
    }
    // 导入/钉住的镜像也算(未过期才算可用)。
    if read_imported_mirror(&paths).is_some_and(|v| auth_value_real_and_usable(&v)) {
        return true;
    }
    read_auth(&real_account_stash_path(&paths))
        .ok()
        .is_some_and(|v| auth_value_real_and_usable(&v))
}

/// 解析当前**生效**三态:持久键优先;键缺失 → 按可用真账号推导。
/// [MOC-257] **real 档失效降级**:持久 real 但账号**实际不可用**(过期/服务端撤销)→ 降级 Synthetic
/// (用户要求);持久仍 real,账号恢复可用(重登)后下次 resolve 自动升回 real。键缺失同理(有可用
/// 真账号→Real、否则→Synthetic)。只读。
pub fn resolve_plugin_unlock_mode() -> PluginUnlockMode {
    match crate::admin::handlers::settings::read_plugin_unlock_mode().as_deref() {
        Some("off") => PluginUnlockMode::Off,
        Some("synthetic") => PluginUnlockMode::Synthetic,
        // real(显式)+ 键缺失(默认):都按「可用真账号→Real,否则降级 Synthetic」。
        _ => {
            if real_account_usable() {
                PluginUnlockMode::Real
            } else {
                PluginUnlockMode::Synthetic
            }
        }
    }
}

/// [MOC-257 review] 把 `path`(若存在)改名归档到 `<app_home>/real-account/discarded-stash-<ns>.json`,
/// 覆盖前留可恢复副本(**不直删** —— 可能是不同真账号,直删 = 丢账号,违反「覆盖前必备份」不变量)。
/// 返回归档后 `path` 是否**已不存在**(`true` = 已移走 / 本就不存在 → 可安全覆盖;`false` = 归档失败、
/// 文件仍在 → caller **不应**覆盖以免丢账号)。
#[must_use]
fn archive_existing_auth_file(paths: &CodexPaths, path: &std::path::Path) -> bool {
    if !path.is_file() {
        return true; // 本就不存在
    }
    let archive_dir = paths.app_home.join("real-account");
    if let Err(e) = std::fs::create_dir_all(&archive_dir) {
        tracing::warn!("[PluginUnlock] 建归档目录失败(保留旧文件不直删): {e}");
        return false;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let archived = archive_dir.join(format!("discarded-stash-{ts}.json"));
    match std::fs::rename(path, &archived) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("[PluginUnlock] 归档旧 auth 文件失败(保留不直删): {e}");
            false
        }
    }
}

/// 把活动**真账号**整文件移到 stash 保全(保 tokens),供切到 synthetic/off 前调用。**只动真账号**
/// (有 chatgpt tokens、非合成);合成账号 / apikey / 无 auth.json → 留着不动(各自由 `activate_fake_account`
/// 覆盖、`clear_active_auth_file`(off)清、或本就空)。持 `AUTH_LOCK`。
pub async fn stash_displaced_real_auth() -> Result<(), String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let Ok(v) = read_auth(&paths.auth_json) else {
        return Ok(()); // 无 auth.json
    };
    if v.get("cas_synthetic").and_then(Value::as_bool) == Some(true) {
        return Ok(()); // 合成账号 → 不动(synthetic 的 activate no-op / off 的 clear 各自处理)
    }
    if parse_chatgpt_tokens(&v).is_none() {
        return Ok(()); // apikey 无真 tokens → 不进 stash(off 会清 / synthetic 会覆盖)
    }
    // 真账号 → 整文件移到 stash 保全。
    let stash = real_account_stash_path(&paths);
    if let Some(parent) = stash.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建 stash 目录失败: {e}"))?;
    }
    // [MOC-257 review] stash 已存在另一真账号(A 已 stash → app 外 login B 成活动 → 再 stash B)→ 先归档
    // 旧 stash 再覆盖,否则 rename 直接顶掉 A 丢账号(macOS/Linux rename 静默替换);也兼顾 Windows
    // (rename 目标存在会失败)。**归档失败(旧 stash 仍在)→ 中止**,绝不 rename 覆盖丢掉账号 A。
    if !archive_existing_auth_file(&paths, &stash) {
        return Err(
            "旧 stash 归档失败,已中止以免覆盖丢账号;请检查 ~/.codex-app-transfer 目录权限后重试"
                .to_owned(),
        );
    }
    std::fs::rename(&paths.auth_json, &stash).map_err(|e| format!("移真账号到 stash 失败: {e}"))?;
    Ok(())
}

/// [MOC-257 三态 OFF] 清掉活动 `~/.codex/auth.json`(确保 .codex 无 auth.json)。真账号应已先经
/// `stash_displaced_real_auth` 移走;这里删剩下的合成 / apikey 残留。无文件 → no-op。持 `AUTH_LOCK`。
pub async fn clear_active_auth_file() -> Result<(), String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    if paths.auth_json.is_file() {
        std::fs::remove_file(&paths.auth_json)
            .map_err(|e| format!("删活动 auth.json 失败: {e}"))?;
    }
    Ok(())
}

/// 从 stash 整文件还原真账号到 `~/.codex/auth.json`(纯文件逻辑,**无锁**,供 sync/async 复用)。
/// stash 不存在 → `Ok(false)`。**活动已是真账号**(app 外新 login、比 stash 更新)→ 把 stash
/// **改名归档**(不直删:可能是不同账号,直删 = 丢账号,违反「覆盖前必备份」不变量)、不覆盖活动,
/// 返 `Ok(false)`。否则整文件还原、返 `Ok(true)`。
fn restore_stashed_real_auth_impl(paths: &CodexPaths) -> Result<bool, String> {
    let stash = real_account_stash_path(paths);
    if !stash.is_file() {
        return Ok(false);
    }
    // [MOC-257 review] 活动是否「更新且**可用**的真账号」——必须含过期/撤销判定:活动只是**过期的**真
    // chatgpt 残留(auth_mode=chatgpt、tokens 在但已过期)时,不能当成「更新真账号」把有效的 stash 归档掉、
    // 回退到过期 active(切 real 会失败)。用 `auth_value_real_and_usable`(非合成 + 非空 + 未撤销 + 未过期)。
    let active_usable = read_auth(&paths.auth_json)
        .ok()
        .is_some_and(|v| auth_value_real_and_usable(&v));
    if active_usable {
        // 活动已是更新且可用的真账号 → 不覆盖;stash 那份也是真账号(含 tokens),改名归档(不直删:
        // 可能不同账号 → 直删丢账号),留可恢复副本。归档失败这里不阻断:活动 + stash 都保住、下次重试。
        let _ = archive_existing_auth_file(paths, &stash);
        return Ok(false);
    }
    // 活动非可用真账号(synthetic / apikey / 过期残留)→ 删掉再 rename:Windows `rename` 目标已存在会
    // 失败(real 还原直接报错、回不去 real);macOS/Linux 虽会替换,显式删保证跨平台一致。被删的是
    // 占位 / 过期残留(stash 里有更值得保留的),安全。
    let _ = std::fs::remove_file(&paths.auth_json);
    std::fs::rename(&stash, &paths.auth_json)
        .map_err(|e| format!("从 stash 还原真账号失败: {e}"))?;
    Ok(true)
}

/// 从 stash 整文件还原真账号(**异步**,持 `AUTH_LOCK`)。供 orchestrator(切到 real)用。
pub async fn restore_stashed_real_auth() -> Result<bool, String> {
    let _guard = AUTH_LOCK.lock().await;
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    restore_stashed_real_auth_impl(&paths)
}

/// [MOC-257 review] 从 stash 还原真账号的**同步**版(无 `AUTH_LOCK` / 不 `block_on`)。供 Tauri
/// setup(startup self-heal)/ exit 闭包(非 async 上下文)用 —— exit 时 async runtime 可能正在
/// shutdown,`block_on` 异步锁会 panic(panic 在 exit 闭包会 abort 进程、跳过紧随的
/// `restore_codex_if_enabled`);且 exit 进程独占、startup self-heal 在任何 auth task spawn 之前,
/// 均无并发,无需 AUTH_LOCK。失败由 caller `tracing::error!` 留痕(对齐 restore_codex_if_enabled)。
pub fn restore_stashed_real_auth_blocking() -> Result<bool, String> {
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    restore_stashed_real_auth_impl(&paths)
}

/// [MOC-104 req#5 启动调谐] 启动时(**绝不刷新 token**,见模块级分流说明):① 活动
/// `~/.codex/auth.json` 已是有效真实 chatgpt → 共用、原样不动(本机 Codex 自维护);
/// ② 活动失效(被 apply 改 apikey / 登出 / 清掉)且用户导入过账号 → 恢复:优先从
/// **活源路径**重读最新(跟随源 Codex 刷新)、源失效回落镜像快照,先备份再写。**只对
/// 用户显式导入/钉住的账号自动恢复**,不抢别的活动文件(避免误覆盖代理 apikey)。
/// 选中那份本地 JWT 已过期 → 标记 relogin、不写废 token。best-effort。
pub async fn reconcile_on_startup(mode_enabled: Option<bool>) -> Result<ReconcileOutcome, String> {
    // [review #2] 有 codex login 正在进行 → 跳过调谐,别跟 codex login 抢写 auth.json。
    if matches!(login_status(), LoginState::Running) {
        tracing::info!("[RealAccount] 启动调谐跳过:codex login 进行中");
        return Ok(ReconcileOutcome::NoAccount);
    }
    // [MOC-104 分流] transfer **不再**在启动时 POST 刷新 token —— 刷新权交给源头 Codex:
    // 检测获取(Official)由本机 Codex 自刷新 `~/.codex/auth.json`;导入(Imported)由源那边
    // 的 Codex 刷新。transfer 与 Codex 是**两个进程**、共享同一份 single-use refresh_token,
    // 双方都刷必触发 `refresh_token_reused` 把账号烧死(`AUTH_LOCK` 只串行 transfer 进程内、
    // 管不到外部 codex 进程 —— 实测 5月30 的 token 正因 transfer 每次启动刷新跟 Codex 撞而
    // 失效)。故启动只做「检测 + 必要时从导入镜像恢复」,**绝不主动刷新**;唯一拿新 token
    // 的入口是 transfer 内「登录」(`start_login` → codex login 自己换全新账号)。
    let _guard = AUTH_LOCK.lock().await;
    if matches!(login_status(), LoginState::Running) {
        tracing::info!("[RealAccount] reconcile 跳过:codex login 进行中");
        return Ok(ReconcileOutcome::NoAccount);
    }
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;

    // [MOC-178] 用户主动关了真实账号模式(flag=false)→ 即便退出 restore 把活动写回 chatgpt,
    // 也收敛回 apikey(由 caller apply,在 daemon 决策前),且**不**从镜像恢复活动(尊重关闭意图)。
    if mode_enabled == Some(false) {
        let had = active_is_real_chatgpt(&paths) || locate_chatgpt_tokens(&paths).is_some();
        return Ok(ReconcileOutcome::ForceDisable {
            had_valid_token: had,
        });
    }

    // 活动已是真实 chatgpt → 共用、绝不动(Codex 自维护这份,transfer 只读跟随、不覆盖)。
    if active_is_real_chatgpt(&paths) {
        return Ok(ReconcileOutcome::StillValid {
            source: AuthSource::Official,
        });
    }

    // 活动非真实 chatgpt(apikey / 登出 / 空)→ 从用户导入的账号恢复(不刷新)。两种导入形态:
    //   ① 活源:记录的 source_path 还在 + 可读 + 是 chatgpt → 用源**最新**(跟随那边 Codex
    //      刷新),并顺手把它同步进镜像快照(源将来移除/失效时快照是最后一次可用账号);
    //   ② 静态文件 / 源已移除失效 → 回落到镜像快照。
    // 两者都**不 POST 刷新**;选中那份 access_token 本地 JWT 过期 → 标记 relogin、不写废
    // token(否则恢复到活动只会让 chatgpt backend 全 401,不如保留可用配置 + 提示重登)。
    let from_source = read_imported_source_path(&paths)
        .and_then(|sp| std::fs::read_to_string(&sp).ok())
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .filter(|v| parse_chatgpt_auth(v).is_some());
    let (chosen, from_live_source) = match from_source {
        Some(v) => (v, true),
        None => match read_imported_mirror(&paths) {
            Some(v) => (v, false),
            None => return Ok(ReconcileOutcome::NoAccount),
        },
    };
    let origin = if from_live_source {
        "导入源路径(活源跟随)"
    } else {
        "镜像快照"
    };
    let access = chosen
        .get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if access.is_empty() || access_token_expired(access, chrono::Utc::now().timestamp()) {
        set_relogin_required(true);
        tracing::warn!(
            "[RealAccount] 导入账号 token 本地已过期({origin}),不恢复废 token,标记需重新登录"
        );
        return Ok(ReconcileOutcome::ReloginRequired {
            source: AuthSource::Imported,
        });
    }
    backup_active_auth(&paths, "prereconcile")?;
    write_auth(&paths.auth_json, &chosen)
        .map_err(|e| format!("启动恢复导入账号到活动失败: {e}"))?;
    // 活源读到的最新内容同步进镜像快照(源日后移除/失效时,快照即最后一次可用账号)。
    if from_live_source {
        let _ = write_auth(&imported_mirror_path(&paths), &chosen);
    }
    tracing::info!("[RealAccount] 启动调谐:活动非真实账号,已从{origin}恢复(不刷新)");
    Ok(ReconcileOutcome::StillValid {
        source: AuthSource::Imported,
    })
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
                clear_relogin_state(); // [MOC-124 H-2] 登录成功 = 拿到新鲜账号,清失效标记 + 撤销指纹
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

    // [MOC-124 H-2 BLOCKER + codex P2] detect self-heal 决策矩阵:被撤销旧 token 保持、token
    // 换了才清、无撤销照常清、no-bearer 401(指纹未知)保守保持。
    #[test]
    fn should_clear_relogin_decision_matrix() {
        let revoked = access_token_fingerprint(&json!({"tokens": {"access_token": "tok_revoked"}}));
        let fresh = access_token_fingerprint(&json!({"tokens": {"access_token": "tok_fresh"}}));
        // 无撤销记录(has_revocation=false)→ 清(detect 照常自愈 detect-None 设的 stale relogin)
        assert!(should_clear_relogin(revoked, 0, false));
        assert!(should_clear_relogin(revoked, revoked, false));
        // 有撤销 + 还是被撤销的旧 token(指纹相同)→ 不清(保持) ← BLOCKER 核心
        assert!(!should_clear_relogin(revoked, revoked, true));
        // 有撤销 + token 换了(app 外 login / 重新导入)→ 清
        assert!(should_clear_relogin(fresh, revoked, true));
        // 有撤销 + 指纹未知(no-bearer 401,revoked_fp==0)→ 不清(保守保持) ← codex P2
        assert!(!should_clear_relogin(revoked, 0, true));
        assert!(!should_clear_relogin(fresh, 0, true));
    }

    // [MOC-124 H-2] auth.json access_token 指纹:稳定、不同 token 不同、缺/空 → 0。
    // 跟 proxy 侧 authorization_token_fingerprint 同算法 → 同 token 同指纹(跨 crate 比对成立)。
    #[test]
    fn access_token_fingerprint_stable_and_distinct() {
        let a = json!({"tokens": {"access_token": "tok_abc"}});
        let b = json!({"tokens": {"access_token": "tok_xyz"}});
        assert_eq!(access_token_fingerprint(&a), access_token_fingerprint(&a));
        assert_ne!(access_token_fingerprint(&a), access_token_fingerprint(&b));
        assert_ne!(access_token_fingerprint(&a), 0);
        assert_eq!(access_token_fingerprint(&json!({})), 0);
        assert_eq!(
            access_token_fingerprint(&json!({"tokens": {"access_token": ""}})),
            0
        );
    }

    // [MOC-124 H-2] proxy 401 回灌:标记需重登 + 记被撤销 token 指纹;清零由 clear 显式做(不做
    // 2xx 自愈,见 mark_relogin_required_from_proxy doc)。detect 的「换 token 才清」决策由纯 fn
    // should_clear_relogin 测试覆盖。用全局 state,开头/结尾 clear 复位(本测试是唯一碰这俩的)。
    #[test]
    fn mark_relogin_required_from_proxy_sets_flag_and_records_fp() {
        let a = 1111u64;
        clear_relogin_state();
        assert!(!relogin_required());

        // 401(token A 撤销)→ 标记需重登 + 记指纹 A + has_revocation
        mark_relogin_required_from_proxy(a);
        assert!(relogin_required());
        assert_eq!(REVOKED_TOKEN_FP.load(Ordering::SeqCst), a);
        assert!(HAS_REVOCATION.load(Ordering::SeqCst));
        // [codex P2] 随后 no-bearer 401(fp=0)置 has_revocation 但**不覆盖**已记录的指纹 A
        mark_relogin_required_from_proxy(0);
        assert_eq!(
            REVOKED_TOKEN_FP.load(Ordering::SeqCst),
            a,
            "no-bearer 401 不该擦掉指纹 A"
        );
        assert!(HAS_REVOCATION.load(Ordering::SeqCst));
        // 显式清(等价 login/import/forget 拿到新账号)→ 全清
        clear_relogin_state();
        assert!(!relogin_required());
        assert_eq!(REVOKED_TOKEN_FP.load(Ordering::SeqCst), 0);
        assert!(!HAS_REVOCATION.load(Ordering::SeqCst));
    }
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
        import_locked(&paths, &chatgpt_auth(), None).unwrap();
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

    // [MOC-257] 合成伪造 auth.json 必须:① 满足 parse_chatgpt_auth(触发 relay gate)②带 cas_synthetic
    // 哨兵 ③access_token 远未来不判过期 ④是 3 段 JWT + payload 含 chatgpt_account_id(镜像 proxy
    // resolver::is_chatgpt_access_token 的放行条件,等价「真配置验证」——合成 token 一定被 proxy 放行)。
    #[test]
    fn synthetic_auth_is_wellformed_chatgpt() {
        use base64::Engine;
        let v = build_synthetic_auth();
        assert!(
            parse_chatgpt_auth(&v).is_some(),
            "应满足 relay gate 的 chatgpt 判定"
        );
        assert_eq!(v.get("cas_synthetic").and_then(Value::as_bool), Some(true));
        let access = v["tokens"]["access_token"].as_str().unwrap();
        assert!(
            !access_token_expired(access, chrono::Utc::now().timestamp()),
            "远未来 exp 不应判过期(否则 detect 会标 relogin)"
        );
        let parts: Vec<&str> = access.split('.').collect();
        assert_eq!(parts.len(), 3, "access_token 应是 3 段 JWT");
        assert!(!parts[2].is_empty(), "签名段非空(resolver 形状校验要求)");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .unwrap();
        let pv: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(
            pv["https://api.openai.com/auth"]["chatgpt_account_id"].as_str(),
            Some(SYNTHETIC_ACCOUNT_ID),
            "payload 必须含非空 chatgpt_account_id(resolver 放行条件)"
        );
    }

    // ── [MOC-257] 三态 stash 原语单测 ────────────────────────────────────────

    /// stash 存在 + 活动非真账号(合成/apikey/无)→ 整文件还原到活动、stash 消费。
    #[test]
    fn restore_stash_moves_to_active_when_active_not_real() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&real_account_stash_path(&paths), &chatgpt_auth());
        // 活动是合成账号 → 应被 stash 真账号覆盖。
        write_json(
            &paths.auth_json,
            &json!({"auth_mode": "chatgpt", "cas_synthetic": true,
                    "tokens": {"access_token": "syn", "refresh_token": "syn"}}),
        );
        assert!(restore_stashed_real_auth_impl(&paths).unwrap(), "应还原");
        assert!(!real_account_stash_path(&paths).is_file(), "stash 应消费");
        let restored = read_auth(&paths.auth_json).unwrap();
        assert_eq!(
            restored.get("cas_synthetic"),
            None,
            "活动应是真账号(无哨兵)"
        );
        assert!(
            parse_chatgpt_tokens(&restored).is_some(),
            "活动应有真 tokens"
        );
    }

    /// [review 数据安全] 活动已是**更新真账号**(app 外 login)→ 不覆盖;旧 stash **改名归档**
    /// (不直删,留可恢复副本),返 false。
    #[test]
    fn restore_archives_stash_when_active_already_real() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.auth_json, &chatgpt_auth()); // 活动真账号(更新)
        write_json(&real_account_stash_path(&paths), &chatgpt_auth()); // stash 旧真账号
        assert!(
            !restore_stashed_real_auth_impl(&paths).unwrap(),
            "活动已是真账号 → 不还原"
        );
        assert!(
            !real_account_stash_path(&paths).is_file(),
            "stash 应被移走(归档)"
        );
        let archived = std::fs::read_dir(paths.app_home.join("real-account"))
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("discarded-stash-")
            })
            .count();
        assert_eq!(archived, 1, "应有一份归档副本(不直删,防丢账号)");
    }

    /// 无 stash → no-op 返 false。
    #[test]
    fn restore_noop_when_no_stash() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        assert!(!restore_stashed_real_auth_impl(&paths).unwrap());
    }

    /// [review] 活动只是**过期的**真账号残留 → 不当成「更新真账号」归档掉有效 stash,而是还原有效 stash。
    #[test]
    fn restore_prefers_stash_when_active_real_is_expired() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        let now = chrono::Utc::now().timestamp();
        // 活动:过期真 chatgpt 残留(auth_mode=chatgpt、tokens 在但 access 已过期)。
        write_json(
            &paths.auth_json,
            &json!({"auth_mode": "chatgpt",
                    "tokens": {"access_token": make_jwt_with_exp(now - 3600), "refresh_token": "r1"}}),
        );
        // stash:有效真账号(未过期)。
        write_json(
            &real_account_stash_path(&paths),
            &json!({"auth_mode": "chatgpt",
                    "tokens": {"access_token": make_jwt_with_exp(now + 86400), "refresh_token": "r2"}}),
        );
        assert!(
            restore_stashed_real_auth_impl(&paths).unwrap(),
            "过期活动残留 → 应还原有效 stash(而非归档掉好 stash)"
        );
        assert!(!real_account_stash_path(&paths).is_file(), "stash 应消费");
        let active = read_auth(&paths.auth_json).unwrap();
        assert_eq!(
            active["tokens"]["refresh_token"], "r2",
            "活动应换成 stash 里的有效账号"
        );
    }
}
