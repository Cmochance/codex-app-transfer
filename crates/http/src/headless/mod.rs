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
pub use download::{ensure_chrome_headless_shell, platform_slug, PINNED_VERSION};

use std::path::PathBuf;
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
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            nav_timeout: Duration::from_secs(30),
            networkidle_timeout: Duration::from_secs(12),
            render_settle: Duration::from_millis(250),
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

// 临时 profile 目录序号: 同进程内多个实例不撞目录 (Chrome 同 profile 会 lock 冲突)。
static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

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

    /// 用 **指定** 的 chrome 二进制 launch (跳过探测/下载)。用于强制指定浏览器来源
    /// (如真机验收要用按需下载的 chrome-headless-shell 抓取, 绕开系统 Chrome)。
    pub async fn launch_with_binary(
        chrome: impl AsRef<std::path::Path>,
        config: HeadlessConfig,
    ) -> Result<Self, HeadlessError> {
        let chrome = chrome.as_ref();

        // 独立临时 user-data-dir: 全新 profile, 不接管用户 Chrome、不弹窗。
        let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let profile_dir =
            std::env::temp_dir().join(format!("cat-headless-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&profile_dir)
            .map_err(|e| HeadlessError::Launch(format!("建临时 profile 失败: {e}")))?;

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
            // fallback: 读不到真实 UA 时用全 crate 统一的 CHROME_UA (147, 见 impersonating 模块
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
        let html = page
            .content()
            .await
            .map_err(|e| HeadlessError::Fetch(format!("取 content 失败: {e}")))?;

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
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

impl Drop for HeadlessBrowser {
    fn drop(&mut self) {
        // 同步 Drop 不能 await; abort handler + 清 profile。chromiumoxide 的 Browser
        // 自身 Drop 会尝试 kill child 进程 (避免僵尸)。优雅路径走 close()。
        self.handler_task.abort();
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

/// 便捷一次性抓取: 自起自清 (无复用)。用于偶发抓取。
pub async fn fetch_rendered_html(url: &str) -> Result<String, HeadlessError> {
    let browser = HeadlessBrowser::launch().await?;
    let result = browser.fetch_rendered_html(url).await;
    browser.close().await;
    result
}
