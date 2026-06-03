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
    /// 最近一次启动调谐/检测判定「真实账号已失效、refresh_token 永久无效、需重新登录」。
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

/// [connector review] 进程级「需重新登录」标记 —— reconcile/检测判定 refresh_token
/// 永久失效时置真,登录/导入/检测到有效账号后清零。比一次性 `emit` 事件可靠:前端任何时候
/// 轮询 `status` 都能读到,不受「事件早于 listener 注册」的启动时序影响。
static RELOGIN_REQUIRED: AtomicBool = AtomicBool::new(false);

/// 读「需重新登录」标记。
pub fn relogin_required() -> bool {
    RELOGIN_REQUIRED.load(Ordering::SeqCst)
}

/// 设「需重新登录」标记(reconcile/检测判定失效时 true;有新鲜账号时 false)。
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
            // 不经 import/login/reconcile 入口的场景。只在有「确实有效」的本地证据时清:
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
    set_relogin_required(false); // 有效账号导入成功,清失效标记
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
    if !mirror.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&mirror).map_err(|e| format!("删持久镜像失败: {e}"))?;
    // [MOC-104 导入分流] 镜像删了,导入来源路径记录也一并清(否则 reconcile 还会从旧
    // 源路径重读、把已"忘记"的账号复活)。
    write_imported_source_path(&paths, None);
    // [connector review] 清除账号后不再有可重登的保留账号 → 清掉「需重新登录」标记,
    // 否则 status 仍带 relogin_required=true,UI 继续提示「账号已失效」要重登。
    set_relogin_required(false);
    Ok(true)
}

/// [MOC-104 req#5 启动调谐] 启动时(**绝不刷新 token**,见模块级分流说明):① 活动
/// `~/.codex/auth.json` 已是有效真实 chatgpt → 共用、原样不动(本机 Codex 自维护);
/// ② 活动失效(被 apply 改 apikey / 登出 / 清掉)且用户导入过账号 → 恢复:优先从
/// **活源路径**重读最新(跟随源 Codex 刷新)、源失效回落镜像快照,先备份再写。**只对
/// 用户显式导入/钉住的账号自动恢复**,不抢别的活动文件(避免误覆盖代理 apikey)。
/// 选中那份本地 JWT 已过期 → 标记 relogin、不写废 token。best-effort。
pub async fn reconcile_on_startup() -> Result<ReconcileOutcome, String> {
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
}
