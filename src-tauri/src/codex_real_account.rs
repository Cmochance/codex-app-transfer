//! 真实 ChatGPT 账号检测(MOC-104)。
//!
//! 「真实账号 plugin 模式」的基础:判断本机是否已有可用的真实 ChatGPT 登录态
//! (`auth.json` 里 `auth_mode == "chatgpt"` 且 tokens 齐全)。当前 plugins 解锁
//! 靠 CDP 注入伪造 `setAuthMethod('chatgpt')`,没有真实 userID → Codex 启动后要
//! 重新初始化登录态(~5.8s)。真实账号模式用真 `auth.json` 取代伪造,避开代价。
//!
//! 三块能力,各自独立:
//! - **检测**([`detect`]):**纯只读** —— 只 `read` `~/.codex/auth.json` 与 transfer
//!   快照备份,不写盘、不 spawn。
//! - **token 刷新**([`refresh_if_needed`]):token 将过期才走官方 OAuth refresh,
//!   只更新 token 字段 + `last_refresh` 原子写回(非破坏)。
//! - **登录**([`start_login`] / [`cancel_login`] / [`login_status`]):调起官方
//!   `codex login`(它自己做 OAuth + 写 `~/.codex/auth.json`),非阻塞 + 可取消。
//!
//! 检测来源(用户要求:只扫官方 `.codex/auth.json` + transfer 备份,不依赖
//! 任何特殊文件夹结构):
//! 1. 官方 `~/.codex/auth.json` —— Codex 自己(或 `codex login`)写的活动凭据。
//! 2. transfer 快照备份 `~/.codex-app-transfer/codex-snapshots/active/<session>/auth.json`
//!    —— transfer apply 前会整文件备份原 `auth.json`(见 codex_integration snapshot)。
//!    用户开 transfer 后官方 `auth.json` 可能被 apply 改成 apikey 模式,原本的
//!    chatgpt 登录态此时仍保留在快照里,可据此提示"备份里有真实账号可恢复"。

use std::path::PathBuf;
use std::process::{Command, Stdio};
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
    /// transfer 快照备份(官方被改成 apikey 后,原 chatgpt 态留在这里)。
    Backup,
    /// 哪里都没找到可用的真实 chatgpt 登录态。
    None,
}

/// 真实 ChatGPT 账号检测结果(只读快照)。
#[derive(Debug, Clone, Serialize)]
pub struct RealAccountStatus {
    /// 是否检测到**可用**的真实 chatgpt 登录态(`auth_mode==chatgpt` + access/refresh token 齐)。
    pub logged_in: bool,
    /// 活动 `auth.json` 的 `auth_mode`(`chatgpt` / `apikey` / 缺失=None)。
    /// 注意:这是**官方活动文件**的模式,即便可用凭据是从 backup 检测到的也反映活动态,
    /// 便于前端区分"活动就是 chatgpt" vs "活动是 apikey、但备份里有 chatgpt"。
    pub active_auth_mode: Option<String>,
    /// chatgpt `account_id`(从被采纳的来源里取,可能缺失)。
    pub account_id: Option<String>,
    /// `logged_in=true` 时,可用凭据来自哪里。
    pub source: AuthSource,
}

impl RealAccountStatus {
    fn none(active_auth_mode: Option<String>) -> Self {
        Self {
            logged_in: false,
            active_auth_mode,
            account_id: None,
            source: AuthSource::None,
        }
    }
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

/// 按"官方活动 → transfer 备份"的优先级定位**可用**的真实 chatgpt `auth.json`。
/// 这是 [`detect`] 与刷新共用的单一入口,保证两者口径一致(同一个文件)。只读。
fn locate_chatgpt_auth(paths: &CodexPaths) -> Option<LocatedChatgptAuth> {
    // 1) 官方活动 auth.json。
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
    // 2) transfer 快照备份 `<active>/<session>/auth.json`。
    let entries = std::fs::read_dir(&paths.active_snapshots_dir).ok()?;
    for entry in entries.flatten() {
        let auth_path = entry.path().join("auth.json");
        if !auth_path.is_file() {
            continue;
        }
        if let Ok(v) = read_auth(&auth_path) {
            if let Some(parsed) = parse_chatgpt_auth(&v) {
                return Some(LocatedChatgptAuth {
                    path: auth_path,
                    source: AuthSource::Backup,
                    value: v,
                    account_id: parsed.account_id,
                });
            }
        }
    }
    None
}

/// 读官方活动 `auth.json` 的 `auth_mode`(不存在/坏 → None)。检测结果里单独
/// 报告活动模式,便于前端区分"活动就是 chatgpt" vs "活动 apikey、备份有 chatgpt"。
fn active_auth_mode(paths: &CodexPaths) -> Option<String> {
    read_auth(&paths.auth_json)
        .ok()?
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// 检测真实 chatgpt 账号:按"官方活动 → transfer 备份"定位可用凭据(见
/// [`locate_chatgpt_auth`])。纯只读,绝不写盘 / spawn。
pub fn detect() -> RealAccountStatus {
    let Ok(paths) = CodexPaths::from_home_env() else {
        // 连 home 都解析不到 —— 当作"没有",不 panic。
        return RealAccountStatus::none(None);
    };
    let active_mode = active_auth_mode(&paths);
    match locate_chatgpt_auth(&paths) {
        Some(found) => RealAccountStatus {
            logged_in: true,
            active_auth_mode: active_mode,
            account_id: found.account_id,
            source: found.source,
        },
        None => RealAccountStatus::none(active_mode),
    }
}

// ── Token 刷新(MOC-104 req#2)─────────────────────────────────────────
//
// 真实 chatgpt token 会过期。用户要求每次启动刷新真实账号(含 transfer 备份里
// 的那份)的 token 避免过期。刷新走官方 OAuth refresh_token 流(常量与请求格式
// 借鉴 Codex_Account_Switch `shared/runtime/chatgpt_api.rs`,README 已致谢):
//   POST https://auth.openai.com/oauth/token
//        grant_type=refresh_token&refresh_token=<rt>&client_id=<id>
// 响应 {access_token, id_token?, refresh_token?};只更新 tokens.{access,refresh,
// id} + 顶层 last_refresh,其它字段透传(非破坏)。刷的是 [`locate_chatgpt_auth`]
// 定位到的那份文件(官方活动 or 备份),与检测口径一致。

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
    /// 没有可刷新的真实 chatgpt 账号(官方 + 备份都没有)。
    NoAccount,
    /// access_token 还没到期(或无法解析过期时间 → 保守视作有效),跳过刷新。
    StillValid { source: AuthSource },
    /// 刷新成功并写回。
    Refreshed { source: AuthSource },
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

/// 启动时刷新真实 chatgpt 账号的 token(若将过期)。定位官方/备份里那份真实
/// chatgpt `auth.json` → 检查 access_token 是否将过期 → 走 refresh_token 流 →
/// 原子写回(`write_auth`,0o600,非 token 字段透传)。
///
/// 非破坏:只更新 token 字段;无真实账号 / token 仍有效时不写盘。任何步骤失败
/// 都返回 `Err`(由 caller 决定吞掉还是上报),不会留下半写状态(`write_auth` 原子)。
pub async fn refresh_if_needed(client: &reqwest::Client) -> Result<RefreshOutcome, String> {
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    let Some(located) = locate_chatgpt_auth(&paths) else {
        return Ok(RefreshOutcome::NoAccount);
    };
    let source = located.source;
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
        return Err(format!("OAuth refresh 返回 {status}: {body}"));
    }
    let parsed: OAuthRefreshResponse = resp
        .json()
        .await
        .map_err(|e| format!("解析 OAuth refresh 响应失败: {e}"))?;

    let now_iso = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    apply_refresh_response(&mut auth, &parsed, &now_iso)?;
    write_auth(&located.path, &auth).map_err(|e| format!("写回 auth.json 失败: {e}"))?;
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

/// 覆盖当前 `~/.codex/auth.json` 前先整文件备份到 app_home,被覆盖后用户仍可
/// 恢复 —— 非破坏(`feedback_no_silent_destructive_fallback`)。`suffix` 区分场景
/// (prelogin / preactivate)。best-effort:备份失败只 warn,不挡主流程。
fn backup_active_auth(paths: &CodexPaths, suffix: &str) {
    if !paths.auth_json.is_file() {
        return;
    }
    let backup_dir = paths.app_home.join("real-account");
    if let Err(e) = std::fs::create_dir_all(&backup_dir) {
        tracing::warn!("[RealAccount] 备份目录创建失败: {e}");
        return;
    }
    let backup = backup_dir.join(format!("auth-{suffix}-backup.json"));
    if let Err(e) = std::fs::copy(&paths.auth_json, &backup) {
        tracing::warn!("[RealAccount] 备份 auth.json({suffix})失败: {e}");
    }
}

/// [MOC-104 req#5 时序] 把 transfer 备份里检测到的真实 chatgpt 账号**恢复到活动**
/// `~/.codex/auth.json`(场景:开 transfer 后 apply 把活动文件改成 apikey,真实
/// chatgpt 态落到快照备份;用户想让 Codex 重新用回真实账号)。
///
/// 时序安全(先备份再写):① 定位备份里的可用 chatgpt;② 若活动文件已经就是同一
/// 真实账号(source=Official)则无需操作;③ 覆盖活动文件前先备份当前活动文件;
/// ④ 用 `write_auth` 原子写回(0o600)。任何步骤失败前不动活动文件。
pub fn activate_backup_to_active() -> Result<AuthSource, String> {
    let paths = CodexPaths::from_home_env().map_err(|e| format!("解析 home 失败: {e}"))?;
    activate_with_paths(&paths)
}

/// [`activate_backup_to_active`] 的可注入路径内层,便于单测。
fn activate_with_paths(paths: &CodexPaths) -> Result<AuthSource, String> {
    let located = locate_chatgpt_auth(paths).ok_or("未检测到可恢复的真实 chatgpt 账号")?;
    match located.source {
        // 活动文件已经是真实 chatgpt,无需恢复。
        AuthSource::Official => Ok(AuthSource::Official),
        AuthSource::Backup => {
            backup_active_auth(paths, "preactivate");
            write_auth(&paths.auth_json, &located.value)
                .map_err(|e| format!("恢复真实账号到活动 auth.json 失败: {e}"))?;
            Ok(AuthSource::Backup)
        }
        AuthSource::None => Err("未检测到可恢复的真实 chatgpt 账号".to_owned()),
    }
}

/// 启动 `codex login`(非阻塞)。已在进行中则返回 Err。
pub fn start_login() -> Result<(), String> {
    let mut g = LOGIN.lock().map_err(|_| "登录状态锁中毒".to_owned())?;
    if g.running {
        return Err("登录已在进行中".to_owned());
    }
    let codex = resolve_codex_cli().ok_or("未找到 codex CLI;请确认已安装 Codex Desktop")?;
    if let Ok(paths) = CodexPaths::from_home_env() {
        backup_active_auth(&paths, "prelogin");
    }
    // 不覆盖 CODEX_HOME → codex login 写真实 `~/.codex/auth.json`,登录后即生效。
    let child = Command::new(&codex)
        .arg("login")
        .stdout(Stdio::piped())
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
        let mut g = match LOGIN.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.running = false;
        g.pid = None;
        g.last = match result {
            Ok(out) if out.status.success() => LoginState::Succeeded,
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
pub fn cancel_login() -> bool {
    let mut g = match LOGIN.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if !g.running {
        return false;
    }
    g.cancel_requested = true;
    if let Some(pid) = g.pid {
        #[cfg(unix)]
        let _ = Command::new("kill").arg(pid.to_string()).status();
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    true
}

/// 当前登录流程状态(前端轮询)。
pub fn login_status() -> LoginState {
    LOGIN
        .lock()
        .map(|g| g.last.clone())
        .unwrap_or(LoginState::Idle)
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
    fn locate_falls_back_to_backup_when_active_is_apikey() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        // 活动文件是 apikey(transfer apply 改写后的常态)
        write_json(&paths.auth_json, &json!({"auth_mode": "apikey"}));
        // 备份里一个 apikey session + 一个 chatgpt session
        write_json(
            &paths.active_snapshots_dir.join("sess-a").join("auth.json"),
            &json!({"auth_mode": "apikey"}),
        );
        write_json(
            &paths.active_snapshots_dir.join("sess-b").join("auth.json"),
            &chatgpt_auth(),
        );
        let found = locate_chatgpt_auth(&paths).expect("备份里应找到 chatgpt");
        assert_eq!(found.source, AuthSource::Backup);
        assert!(found.path.ends_with("sess-b/auth.json"));

        // detect() 同口径:报 backup、活动模式 apikey
        // (detect 读 from_home_env,这里直接验 locate + active_auth_mode 组合即可)
        assert_eq!(active_auth_mode(&paths).as_deref(), Some("apikey"));
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
    fn activate_promotes_backup_chatgpt_to_active_with_prebackup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        // 活动 apikey + 备份 chatgpt
        write_json(
            &paths.auth_json,
            &json!({"auth_mode": "apikey", "OPENAI_API_KEY": "cas_x"}),
        );
        write_json(
            &paths.active_snapshots_dir.join("sess").join("auth.json"),
            &chatgpt_auth(),
        );
        let source = activate_with_paths(&paths).unwrap();
        assert_eq!(source, AuthSource::Backup);
        // 活动文件现在是 chatgpt
        let active = read_auth(&paths.auth_json).unwrap();
        assert_eq!(active["auth_mode"], "chatgpt");
        assert_eq!(active["tokens"]["account_id"], "acct_123");
        // 覆盖前备份了原 apikey 活动文件(时序安全)
        let prebackup = paths
            .app_home
            .join("real-account")
            .join("auth-preactivate-backup.json");
        assert!(prebackup.is_file(), "覆盖前应备份原活动 auth.json");
        let backed = read_auth(&prebackup).unwrap();
        assert_eq!(backed["auth_mode"], "apikey", "备份的是覆盖前的 apikey 态");
    }

    #[test]
    fn activate_noop_when_active_already_official_chatgpt() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.auth_json, &chatgpt_auth());
        assert_eq!(activate_with_paths(&paths).unwrap(), AuthSource::Official);
    }

    #[test]
    fn activate_errs_when_no_real_account() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.auth_json, &json!({"auth_mode": "apikey"}));
        assert!(activate_with_paths(&paths).is_err());
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
