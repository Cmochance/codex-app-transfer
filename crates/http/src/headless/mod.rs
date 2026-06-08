//! ③ JS 渲染层: headless Chromium 抓 JS 渲染 SPA (MOC-143 PoC)
//!
//! ①`reqwest` 静态抓取 / ②`wreq` CF 指纹 ([`crate::ImpersonatingClient`], MOC-137)
//! 都只能拿 **初始 HTML**; JS 渲染 SPA 的初始 HTML 是空骨架, 内容由 JS 运行时填充。
//! 本层用 headless Chromium (CDP, 经 `chromiumoxide`) 真跑 JS, 取渲染后的 DOM。这是
//! "抓所有网页" 最后也最重的一层。
//!
//! ## 浏览器来源 (两路)
//! 1. **探测系统** ([`detect_system_chrome`]): 用户已装 Chrome/Edge/Chromium → 直接用, 免下载。
//! 2. **按需下载** ([`ensure_chrome_headless_shell`]): 未命中 → 拉 chrome-headless-shell
//!    (~86MB) 到 `~/.codex-app-transfer/browsers/`, 复用。**不打包进安装包** (体积)。
//!
//! ## 后台无窗口
//! headless 模式 + 独立临时 `user-data-dir` (全新 profile), **不接管用户的 Chrome、不弹窗**。
//!
//! ## 等渲染 (MOC-145 networkIdle 精确化)
//! 导航前挂 CDP `Page.lifecycleEvent` 监听 + 开 `setLifecycleEventsEnabled`, 用
//! `execute(Navigate)` 拿到本次导航的 `loaderId`, 只认该 loaderId 的 `networkIdle`
//! (= 主文档网络静默 500ms, 等价 puppeteer networkidle0)。比固定 settle 对慢 SPA /
//! 懒加载更可靠 (不漏内容); 超 [`HeadlessConfig::networkidle_timeout`] 仍未静默则回退
//! 继续 (长连接 / 轮询页不至于卡死)。idle 后再小 settle 一次收尾微任务渲染。
//!
//! ## 反检测 (MOC-152)
//! 导航前对页面启用 stealth (chromiumoxide 自带 `enable_stealth_mode_with_agent`): 抹
//! `navigator.webdriver`、伪造 `window.chrome` / plugins / WebGL vendor, 并把 UA 里的
//! `HeadlessChrome` 换回 `Chrome` —— 等价 puppeteer-extra-plugin-stealth 核心 evasion,
//! 可过**被动**指纹 / 简单 JS 挑战类 Cloudflare。
//!
//! ## 已知边界
//! - 过不了**交互式** 反爬 (Cloudflare Turnstile/DataDome 托管挑战等); 这类需真人机交互,
//!   不在轻量范围。
//! - 本层界定为 "抓 JS 渲染 SPA + 被动反爬"。

mod detect;
mod download;

pub use detect::detect_system_chrome;
pub use download::{
    chrome_headless_shell_path, ensure_chrome_headless_shell, platform_slug, PINNED_VERSION,
};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::page::{
    EventLifecycleEvent, NavigateParams, SetLifecycleEventsEnabledParams,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use thiserror::Error;
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum HeadlessError {
    #[error("浏览器探测/下载失败: {0}")]
    Download(String),
    #[error("浏览器启动失败: {0}")]
    Launch(String),
    #[error("页面抓取失败: {0}")]
    Fetch(String),
}

/// 抓取配置。
#[derive(Debug, Clone)]
pub struct HeadlessConfig {
    /// 导航 (`Page.navigate` 命令应答) 超时。
    pub nav_timeout: Duration,
    /// 等 `networkIdle` 生命周期事件的上限; 超时则回退继续 (长连接/轮询页不卡死)。
    pub networkidle_timeout: Duration,
    /// networkIdle 后再小等一次, 收尾微任务渲染 (idle 已是网络静默, 这里只补最后绘制)。
    pub render_settle: Duration,
    /// wait-for-clear 上限 (②MOC-156, 借鉴 cloakFetch `waitForChallengeCompletion`): networkIdle
    /// 后若 DOM 仍是 CF/反爬挑战页, 原地轮询同页等 stealth 解出 (CF JS challenge 在 headless+
    /// stealth 下可能自动通过), 到 marker 消失或此上限。`0` = 不等 (直接读)。**仅 headless 档**
    /// 有此能力 —— curl/wreq 不跑 JS, 它们靠换 client 升档, 没有"原地等挑战清除"一说。
    pub challenge_wait_timeout: Duration,
    /// wait-for-clear 轮询间隔 (重读 DOM 检测 challenge marker 是否消失)。
    pub challenge_poll_interval: Duration,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            nav_timeout: Duration::from_secs(30),
            networkidle_timeout: Duration::from_secs(12),
            render_settle: Duration::from_millis(250),
            // CF JS challenge 在 headless+stealth 下解出通常数秒内; 15s 给足余量又不至久卡。
            challenge_wait_timeout: Duration::from_secs(15),
            challenge_poll_interval: Duration::from_millis(1500),
        }
    }
}

/// 解析出一个可用的 Chromium 二进制: 先系统探测, 未命中按需下载 chrome-headless-shell。
///
/// 探测 ([`detect_system_chrome`]) 仅判文件存在。命中后跑一次 `--version` 自检 (MOC-145):
/// 命中一个损坏 / 不可执行 / 残缺的系统 Chrome 时自检不过 → **回退按需下载**, 而不是把坏
/// 二进制透到 `launch` 阶段直接打死本次抓取。自检 ~50-100ms, 相对冷启动可忽略。
pub async fn resolve_chrome_binary() -> Result<PathBuf, HeadlessError> {
    if let Some(p) = detect_system_chrome() {
        if chrome_binary_works(&p).await {
            return Ok(p);
        }
        eprintln!(
            "[headless] 系统 Chrome 自检 (--version) 未通过, 回退按需下载: {}",
            p.display()
        );
    }
    ensure_chrome_headless_shell().await
}

/// 二进制可用性自检: 跑 `--version` (打印版本即退, 不开窗)。spawn 失败 / 非 0 退出 = 坏。
///
/// **仅 Unix**。Windows 上 GUI 版 `chrome.exe --version` 不向 console 输出、exit code 不
/// 可靠(可能误判好 Chrome → 触发无谓 ~86MB 下载), 故 Windows 跳过自检沿用旧行为(探测
/// 命中即用, 坏二进制在 launch 阶段暴露)—— 见 [`chrome_binary_works`] 的 Windows 实现。
#[cfg(not(target_os = "windows"))]
async fn chrome_binary_works(bin: &std::path::Path) -> bool {
    tokio::process::Command::new(bin)
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Windows: 跳过 `--version` 自检(行为不可靠, 见 Unix 版 doc), 信任探测结果。坏 Chrome
/// 仍会在 `launch` 阶段以 `Launch` 错误暴露(与 item 5 前一致, 无回归)。Win 真机验证待补。
#[cfg(target_os = "windows")]
async fn chrome_binary_works(_bin: &std::path::Path) -> bool {
    true
}

/// web_search gate 用(MOC-190): Chrome 是否就绪**且 launch 不会触发下载**。已下载内置 shell → ready;
/// 否则系统 Chrome 须**自检通过**(`--version`, 与 [`resolve_chrome_binary`] 同语义)才算 —— 避免
/// detect 命中 stale/损坏路径、gate 放行后 launch 自检失败 fallback `ensure_chrome_headless_shell`
/// 静默下载 86MB(chatgpt-codex P2)。
pub async fn chrome_ready_without_download() -> bool {
    if download::chrome_headless_shell_path().is_some() {
        return true;
    }
    match detect::detect_system_chrome() {
        Some(p) => chrome_binary_works(&p).await,
        None => false,
    }
}

// 临时 profile 目录序号: 同进程内多个实例不撞目录 (Chrome 同 profile 会 lock 冲突)。
static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

// ============= ③MOC-156: per-origin 持久 profile (复用 cf_clearance cookie 跳过重复挑战) =============
//
// 诚实边界: `cf_clearance` cookie **绑定 UA + IP** (CF 侧校验) —— 跨档复用必失效 (curl 发
// CHROME_UA、wreq 发 emulation 指纹、headless 发系统 Chrome, 三档 UA 不一致), 故**只 headless
// 档**做持久 profile (它 UA 固定 + 本地 IP 固定 → 同档复用有效, 且过 challenge 成本最高最值得
// 复用)。curl/wreq 不碰。

/// per-origin 持久 profile 有效期。`cf_clearance` 通常 30min~1h 有效 (CF 侧定), 超此龄的 profile
/// 既无复用价值 (clearance 已过期、下次仍要重新过挑战) 又徒增 cookie 留存, 抓取前按 dir mtime
/// 清掉重建。
const PROFILE_TTL: Duration = Duration::from_secs(3600);

/// 某 origin 的持久 profile 目录 (`~/.codex-app-transfer/webfetch-profiles/<hash>`)。无 home →
/// `None` (调用方回退临时 profile, 旧行为)。hash 用 `DefaultHasher` (固定 key, 跨进程稳定;
/// 仅作目录名去重、非密码学用途)。
fn persistent_profile_dir(origin: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let home = dirs::home_dir()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    origin.hash(&mut h);
    Some(
        home.join(".codex-app-transfer")
            .join("webfetch-profiles")
            .join(format!("{:016x}", h.finish())),
    )
}

/// 同 origin 串行锁表: Chrome 同 `user-data-dir` 同时只能一个实例 (profile lock 文件), 同 origin
/// 并发抓会撞 lock 启动失败。进程内 per-origin async Mutex 串行化 (跨进程不在范围, 同进程 stdio
/// MCP server 足够)。
fn origin_locks(
) -> &'static StdMutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static L: std::sync::OnceLock<
        StdMutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    L.get_or_init(|| StdMutex::new(std::collections::HashMap::new()))
}

/// 取某 origin 的串行锁 (持有期间独占该 origin 的持久 profile)。
async fn lock_origin(origin: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut m = origin_locks().lock().unwrap_or_else(|e| e.into_inner());
        m.entry(origin.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}

/// 过期 (dir mtime 超 [`PROFILE_TTL`]) 的持久 profile 删掉重建 (clearance 已失效, 留着无益 +
/// 减少 cookie 留存)。best-effort: 取不到 mtime / 删失败都忽略 (大不了复用旧 profile)。
fn evict_if_stale(profile: &Path) {
    let stale = std::fs::metadata(profile)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age > PROFILE_TTL)
        .unwrap_or(false);
    if stale {
        let _ = std::fs::remove_dir_all(profile);
    }
}

/// 一个 launched headless 浏览器实例 (持有进程 + handler task), **可复用** 抓多个 URL。
///
/// 复用避免每次冷启动 (~1s+)。生命周期: [`Self::close`] 优雅关闭 (关浏览器 + 等子进程
/// 退出 + 收 handler + 清 profile); `Drop` 兜底 (abort handler + 清 profile)。
pub struct HeadlessBrowser {
    browser: Browser,
    handler_task: JoinHandle<()>,
    /// CDP handler 退出死因 (出错 break 时记下), 供 fetch 失败时拼进错误定位根因。
    handler_err: Arc<StdMutex<Option<String>>>,
    profile_dir: PathBuf,
    /// `true` = per-origin 持久 profile (③MOC-156): close/Drop **不清** profile_dir (跨调用复用
    /// `cf_clearance`)。`false` = 临时 profile, 清掉。
    persist_profile: bool,
    config: HeadlessConfig,
}

impl HeadlessBrowser {
    /// 启动 (探测/下载 chrome + launch headless + 独立临时 profile), 默认配置。
    pub async fn launch() -> Result<Self, HeadlessError> {
        Self::launch_with(HeadlessConfig::default()).await
    }

    pub async fn launch_with(config: HeadlessConfig) -> Result<Self, HeadlessError> {
        let chrome = resolve_chrome_binary().await?;
        Self::launch_with_binary(chrome, config).await
    }

    /// 用 **指定** 的 chrome 二进制 launch (跳过探测/下载)。**临时** profile (每次全新,
    /// close/Drop 清掉)。用于强制指定浏览器来源 (如真机验收用按需下载的 chrome-headless-shell)。
    pub async fn launch_with_binary(
        chrome: impl AsRef<std::path::Path>,
        config: HeadlessConfig,
    ) -> Result<Self, HeadlessError> {
        // 独立临时 user-data-dir: 全新 profile, 不接管用户 Chrome、不弹窗。
        let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let profile_dir =
            std::env::temp_dir().join(format!("cat-headless-{}-{seq}", std::process::id()));
        Self::launch_inner(chrome, config, profile_dir, false).await
    }

    /// 用 **per-origin 持久** profile launch (③MOC-156): profile 跨调用保留 (Chrome 落盘的
    /// cookie 含 `cf_clearance`), close/Drop **不清** —— 下次同 origin 复用、跳过重复 CF 挑战。
    /// 调用方 (顶层 [`fetch_rendered_html`]) 负责 per-origin 串行 + TTL 清理。
    pub async fn launch_persistent(
        profile_dir: impl Into<PathBuf>,
        config: HeadlessConfig,
    ) -> Result<Self, HeadlessError> {
        let chrome = resolve_chrome_binary().await?;
        Self::launch_inner(chrome, config, profile_dir.into(), true).await
    }

    /// launch 内核: 建 profile dir + launch headless + spawn handler。`persist_profile` = 该
    /// profile 是否持久 (true 则 close/Drop 不清, 见 [`Self::persist_profile`])。
    async fn launch_inner(
        chrome: impl AsRef<std::path::Path>,
        config: HeadlessConfig,
        profile_dir: PathBuf,
        persist_profile: bool,
    ) -> Result<Self, HeadlessError> {
        let chrome = chrome.as_ref();
        std::fs::create_dir_all(&profile_dir)
            .map_err(|e| HeadlessError::Launch(format!("建 profile 失败: {e}")))?;

        // headless 是 0.9.1 默认 (HeadlessMode::True); 这里只加常规无头 args。
        let cfg = BrowserConfig::builder()
            .chrome_executable(chrome)
            .user_data_dir(&profile_dir)
            .no_sandbox()
            .arg("--disable-gpu")
            .args([
                "--disable-dev-shm-usage",
                "--hide-scrollbars",
                "--disable-extensions",
                // 反检测 (MOC-152): 关 AutomationControlled blink 特性, 浏览器层抹掉
                // navigator.webdriver (与每页注入的 stealth 脚本互补)。
                "--disable-blink-features=AutomationControlled",
            ])
            .build()
            .map_err(|e| HeadlessError::Launch(format!("BrowserConfig build 失败: {e}")))?;

        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| HeadlessError::Launch(format!("Browser::launch 失败: {e}")))?;

        // handler stream 必须持续 poll, 否则 CDP 不前进。出错时记下死因再 break, 否则
        // 后续 new_page 只会拿到泛化的 "channel closed", 真因 (浏览器崩了/CDP 错) 丢失。
        let handler_err: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let handler_err_w = Arc::clone(&handler_err);
        let handler_task = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if let Err(e) = ev {
                    if let Ok(mut slot) = handler_err_w.lock() {
                        *slot = Some(e.to_string());
                    }
                    break;
                }
            }
        });

        Ok(Self {
            browser,
            handler_task,
            handler_err,
            profile_dir,
            persist_profile,
            config,
        })
    }

    /// 抓一个 URL, 返回渲染后 (JS 执行后) 的完整 HTML。复用本实例 (开新 tab)。
    ///
    /// 等渲染走 networkIdle (见模块注释): 导航**前**挂 lifecycle 监听 → `Navigate` 拿
    /// loaderId → 只认该 loaderId 的 `networkIdle`, 超时回退。避免 `new_page(url)` 直接
    /// 导航时 idle 事件抢在监听挂上前发生而漏掉 (瞬时页) → 空等到超时。
    pub async fn fetch_rendered_html(&self, url: &str) -> Result<String, HeadlessError> {
        // 先开空白页 (about:blank), 不直接导航到目标 —— 留出挂监听的窗口。
        let page = match self.browser.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => {
                // new_page 失败常因 handler 已退出 (channel closed); 拼上死因定位根因。
                let root = self.handler_err.lock().ok().and_then(|g| g.clone());
                let msg = match root {
                    Some(r) => format!("new_page 失败: {e} (handler 已退出, 根因: {r})"),
                    None => format!("new_page 失败: {e}"),
                };
                return Err(HeadlessError::Fetch(msg));
            }
        };

        // 反检测 (MOC-152): 导航**前**在 about:blank 上启用 stealth —— 抹 navigator.webdriver、
        // 伪造 window.chrome / plugins / WebGL vendor (chromiumoxide 自带, 等价
        // puppeteer-extra-plugin-stealth 核心 evasion, 经 addScriptToEvaluateOnNewDocument
        // 对随后导航的目标文档生效)。同时把 UA 里的 HeadlessChrome 换成 Chrome (沿用真实
        // Chrome 版本, 不留 headless 标记 / 老版本号被被动反爬识破)。best-effort: 失败仅降低
        // 过墙率, 不阻断本次抓取。诚实边界: 仍过不了交互式 Turnstile / DataDome。
        let ua = page
            .evaluate("navigator.userAgent")
            .await
            .ok()
            .and_then(|r| r.into_value::<String>().ok())
            .map(|ua| ua.replace("HeadlessChrome", "Chrome"))
            // fallback: 读不到真实 UA 时用全 crate 统一的 CHROME_UA (120, 见 impersonating 模块
            // 版本统一约定) —— 与 wreq 指纹层声称同版本, 不留老版本号被被动反爬识破 (MOC-186)。
            .unwrap_or_else(|| crate::impersonating::CHROME_UA.to_string());
        if let Err(e) = page.enable_stealth_mode_with_agent(&ua).await {
            eprintln!("[headless] 启用 stealth 失败 (继续抓取): {e}");
        }

        // 导航前: 开 lifecycle 事件 + 挂 networkIdle 监听 (顺序关键, 见方法 doc)。
        page.execute(SetLifecycleEventsEnabledParams::new(true))
            .await
            .map_err(|e| HeadlessError::Fetch(format!("开 lifecycle 事件失败: {e}")))?;
        let mut lifecycle = page
            .event_listener::<EventLifecycleEvent>()
            .await
            .map_err(|e| HeadlessError::Fetch(format!("挂 lifecycle 监听失败: {e}")))?;

        // 导航到目标; 拿本次导航的 loaderId 以过滤 networkIdle (排除 about:blank 等噪声)。
        let nav = tokio::time::timeout(
            self.config.nav_timeout,
            page.execute(NavigateParams::new(url.to_string())),
        )
        .await
        .map_err(|_| HeadlessError::Fetch("导航超时".into()))?
        .map_err(|e| HeadlessError::Fetch(format!("Navigate 失败: {e}")))?;
        if let Some(err) = &nav.result.error_text {
            return Err(HeadlessError::Fetch(format!("导航被拒: {err}")));
        }
        let nav_loader = nav.result.loader_id.clone();

        // 两段式等渲染。`Page.navigate` 只发起导航(commit 即返回), 不等 load —— 故不能
        // 只用短的 networkidle_timeout 兜底, 否则慢页(load 耗时 > networkidle_timeout 但
        // < nav_timeout)会在 load 前就超时, 读到半文档 / about:blank, 回归旧 wait_for_navigation
        // 行为(codex-connector P2)。
        //
        // 只认本次导航 loaderId 的事件(排除 about:blank 等噪声; nav_loader 为 None 时不早退,
        // 靠超时兜底 —— 跨文档导航理论上不会 None, CDP 仅 same-document 省略 loaderId)。
        let loader_matches = |ev: &EventLifecycleEvent| nav_loader.as_ref() == Some(&ev.loader_id);

        // Phase A:等文档 load(地板), cap nav_timeout。networkIdle 蕴含已 load, 先到也算完成。
        // 返回 true = 已 idle(整体完成), false = 仅 load(进 Phase B 等 idle)。
        let already_idle = {
            let phase_a = async {
                while let Some(ev) = lifecycle.next().await {
                    if !loader_matches(&ev) {
                        continue;
                    }
                    match ev.name.as_str() {
                        "networkIdle" => return true,
                        "load" | "DOMContentLoaded" => return false,
                        _ => {}
                    }
                }
                false
            };
            match tokio::time::timeout(self.config.nav_timeout, phase_a).await {
                Ok(idle) => idle,
                Err(_) => {
                    // nav_timeout 内连 load 都没等到 → 放弃, 直接读(best-effort, 同旧 nav 超时)。
                    eprintln!(
                        "[headless] 导航/加载在 {}s 内未完成, 回退读当前 DOM: {url}",
                        self.config.nav_timeout.as_secs()
                    );
                    true
                }
            }
        };

        // Phase B:load 后再等 networkIdle(主文档网络静默 500ms), cap networkidle_timeout。
        // 此时读到的至少是 load 完成的文档(不会半文档/about:blank)。
        if !already_idle {
            let wait_idle = async {
                while let Some(ev) = lifecycle.next().await {
                    if ev.name == "networkIdle" && loader_matches(&ev) {
                        return;
                    }
                }
            };
            if tokio::time::timeout(self.config.networkidle_timeout, wait_idle)
                .await
                .is_err()
            {
                // 超时回退是预期 best-effort(长连接/轮询页本就永不 idle); load 已完成, 留痕便于排查。
                eprintln!(
                    "[headless] networkIdle 超时 ({}s) 未静默, 回退读当前 DOM (load 已完成): {url}",
                    self.config.networkidle_timeout.as_secs()
                );
            }
        }

        // idle 后小 settle 收尾微任务渲染 (idle 已网络静默, 这里只补最后绘制)。
        if !self.config.render_settle.is_zero() {
            tokio::time::sleep(self.config.render_settle).await;
        }

        // 渲染后 DOM: page.content() 返回当前序列化文档 (= outerHTML 等价)。
        let mut html = page
            .content()
            .await
            .map_err(|e| HeadlessError::Fetch(format!("取 content 失败: {e}")))?;

        // wait-for-clear (②MOC-156, 借鉴 cloakFetch `waitForChallengeCompletion`): 若渲染后 DOM
        // 仍是 CF/反爬挑战页, 原地轮询同页等 stealth 解出 —— CF 的 JS challenge (`Just a moment`)
        // 在 headless+stealth 下常数秒内自动通过, 此时 networkIdle 已过但 DOM 还停在挑战页, 不等
        // 就把挑战页当正文返回。轮询重读 content 到 marker 消失或 challenge_wait_timeout。仅
        // headless 能这么做 (curl/wreq 不跑 JS, 见 `HeadlessConfig::challenge_wait_timeout` doc)。
        if !self.config.challenge_wait_timeout.is_zero() && crate::fetch::is_challenge_body(&html) {
            eprintln!(
                "[headless] 渲染后仍是挑战页, 原地等 stealth 解 (≤{}s): {url}",
                self.config.challenge_wait_timeout.as_secs()
            );
            let poll = async {
                loop {
                    tokio::time::sleep(self.config.challenge_poll_interval).await;
                    match page.content().await {
                        // 挑战已解 (marker 消失) → 用清除后的 DOM。
                        Ok(c) if !crate::fetch::is_challenge_body(&c) => return Some(c),
                        // 仍是挑战 → 继续等。
                        Ok(_) => continue,
                        // 读 content 失败 (页面崩/导航) → 放弃轮询, 留旧 html。
                        Err(_) => return None,
                    }
                }
            };
            match tokio::time::timeout(self.config.challenge_wait_timeout, poll).await {
                Ok(Some(cleared)) => html = cleared,
                // 超时仍未清 / 轮询读失败 → 落到下面 surface error。
                _ => eprintln!(
                    "[headless] 挑战页 {}s 内未清除: {url}",
                    self.config.challenge_wait_timeout.as_secs()
                ),
            }
            // wait-for-clear 后**仍是挑战页**(交互式 Turnstile/DataDome 过不了) → headless 层
            // **自己 surface error**, 不把挑战页当正文返回。chatgpt-codex review P2: 直选
            // `WebFetchBackend::Headless` 档(`fetch.rs` web_fetch)/ 任何 public
            // `fetch_rendered_html` caller 都没有 Auto 路径的 `last_usable` 兜底, 靠上层查
            // challenge 会漏(只 web_fetch_auto 查了)。在 headless 层判失败覆盖**所有** caller;
            // Auto 路径收到 Err 自动走其 `last_usable` 非破坏回退。no_wait(challenge_wait_timeout
            // =0)不进本块, search 仍拿到 anomaly 页 html 自判 Blocked。
            if crate::fetch::is_challenge_body(&html) {
                let _ = page.close().await;
                return Err(HeadlessError::Fetch(format!(
                    "CF/反爬挑战页 {}s 内未清除(交互式挑战 headless+stealth 过不了)",
                    self.config.challenge_wait_timeout.as_secs()
                )));
            }
        }

        // 关掉这个 tab (释放), 浏览器进程留着复用。
        let _ = page.close().await;
        Ok(html)
    }

    /// 显式优雅关闭 (best-effort): 关浏览器 + 等子进程退出 + 收 handler + 清 profile。
    /// 不返回 Result —— 进程正确性由 chromiumoxide 的 `Browser` Drop 兜底 (kill child),
    /// 这里各步失败仅影响清理彻底度, 不影响调用方已拿到的结果。
    pub async fn close(mut self) {
        let _ = self.browser.close().await;
        let _ = self.browser.wait().await;
        self.handler_task.abort();
        // 持久 profile (③MOC-156) 跨调用复用, 不清; 临时 profile 清掉。
        if !self.persist_profile {
            let _ = std::fs::remove_dir_all(&self.profile_dir);
        }
    }
}

impl Drop for HeadlessBrowser {
    fn drop(&mut self) {
        // 同步 Drop 不能 await; abort handler + 清 profile。chromiumoxide 的 Browser
        // 自身 Drop 会尝试 kill child 进程 (避免僵尸)。优雅路径走 close()。
        self.handler_task.abort();
        // 持久 profile (③MOC-156) 不清 (跨调用复用 cf_clearance)。
        if !self.persist_profile {
            let _ = std::fs::remove_dir_all(&self.profile_dir);
        }
    }
}

/// 便捷一次性抓取(**web_fetch 用**)。③MOC-156: 优先 per-origin 持久 profile(复用 `cf_clearance`
/// 跳过重复 CF 挑战),同 origin 串行 + TTL 清过期;无法解析 origin / 无 home → 回退临时 profile。
/// 含 ②wait-for-clear(默认 config): 遇 CF JS 挑战页原地等 stealth 解。
pub async fn fetch_rendered_html(url: &str) -> Result<String, HeadlessError> {
    fetch_rendered_html_inner(url, HeadlessConfig::default()).await
}

/// 便捷抓取(**web_search 用**): 同上但 **跳过 ②wait-for-clear**(`challenge_wait_timeout = 0`)。
/// DDG/Bing 的反爬是**硬拦截**(202 anomaly / 出口 IP 信誉),不是会自动解出的 CF JS challenge ——
/// 原地等只白等满 15s;且 `web_search` 靠 anomaly marker 自己判 `Blocked` + 后备 Bing(它**要**拿到
/// anomaly 页 html, 不能被当挑战页判失败, code-reviewer review)。仍用持久 profile(无害)。
pub async fn fetch_rendered_html_no_wait(url: &str) -> Result<String, HeadlessError> {
    let config = HeadlessConfig {
        challenge_wait_timeout: Duration::ZERO,
        ..HeadlessConfig::default()
    };
    fetch_rendered_html_inner(url, config).await
}

/// 顶层抓取内核: 持久 / 临时 profile 分流 + 指定 `config`(决定是否 wait-for-clear)。
async fn fetch_rendered_html_inner(
    url: &str,
    config: HeadlessConfig,
) -> Result<String, HeadlessError> {
    // 持久 profile: 仅当能解析 origin 且能定位 home 时启用(否则回退临时 profile)。
    let persistent = crate::fetch::origin_of(url)
        .and_then(|origin| persistent_profile_dir(&origin).map(|p| (origin, p)));
    if let Some((origin, profile)) = persistent {
        // 同 origin 串行: 持有锁期间独占该 profile (Chrome 同 user-data-dir 不能并发)。
        let _guard = lock_origin(&origin).await;
        evict_if_stale(&profile); // TTL: clearance 过期的 profile 删掉重建。
        let browser = HeadlessBrowser::launch_persistent(profile, config).await?;
        let result = browser.fetch_rendered_html(url).await;
        browser.close().await; // persist_profile=true → 不清 profile (跨调用复用)。
        return result;
    }
    // 回退: 临时 profile, 自起自清 (无 origin / 无 home)。
    let browser = HeadlessBrowser::launch_with(config).await?;
    let result = browser.fetch_rendered_html(url).await;
    browser.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ③MOC-156: per-origin 持久 profile 目录按 origin **稳定且隔离** —— 同 origin 两次调用必
    /// 同目录 (跨调用复用 `cf_clearance` 的前提), 不同 origin 必不同目录 (per-origin 隔离, 不串
    /// cookie)。`DefaultHasher` 固定 key 保证跨进程稳定。
    #[test]
    fn persistent_profile_dir_stable_and_per_origin() {
        let a1 = persistent_profile_dir("https://example.com");
        let a2 = persistent_profile_dir("https://example.com");
        let b = persistent_profile_dir("https://other.example.org");
        assert_eq!(a1, a2, "同 origin 两次应同目录 (稳定复用)");
        assert_ne!(a1, b, "不同 origin 应不同目录 (per-origin 隔离)");
        if let Some(p) = a1 {
            assert!(
                p.to_string_lossy().contains("webfetch-profiles"),
                "profile 应落在 webfetch-profiles 根下"
            );
        }
    }
}
