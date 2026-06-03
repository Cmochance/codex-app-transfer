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
//! ## 已知边界 (PoC)
//! - 等渲染用 `wait_for_navigation` (load) + 固定 settle 延迟; chromiumoxide 0.9.1 无
//!   内建 networkIdle helper, 精确化 (监听 CDP `Page.lifecycleEvent` 'networkIdle') 留 followup。
//! - 不对抗主动反爬 (Cloudflare Turnstile/DataDome 等); 本层界定为 "抓 JS 渲染 SPA"。
//! - 接入分层 router (检测空骨架 → 升级 ③) 作后续 PR; 本 PoC 只打通抓取能力。

mod detect;
mod download;

pub use detect::detect_system_chrome;
pub use download::{ensure_chrome_headless_shell, platform_slug, PINNED_VERSION};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

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
    /// 导航 (load 事件) 超时。
    pub nav_timeout: Duration,
    /// load 后额外等待 JS 渲染落定的时间 (networkIdle 的务实替代)。
    pub render_settle: Duration,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            nav_timeout: Duration::from_secs(30),
            render_settle: Duration::from_millis(1500),
        }
    }
}

/// 解析出一个可用的 Chromium 二进制: 先系统探测, 未命中按需下载 chrome-headless-shell。
///
/// 注: 探测 ([`detect_system_chrome`]) 仅判文件存在, 不验证可执行/CDP 可用。命中一个
/// 损坏的系统 Chrome 时会直接返回它, 在 `launch` 阶段以 `Launch` 错误暴露 (不会静默
/// 成功), 但不会自动回退到按需下载。followup: launch 失败时 fallback 到按需下载。
pub async fn resolve_chrome_binary() -> Result<PathBuf, HeadlessError> {
    if let Some(p) = detect_system_chrome() {
        return Ok(p);
    }
    ensure_chrome_headless_shell().await
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
            std::env::temp_dir().join(format!("cas-headless-{}-{seq}", std::process::id()));
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
    pub async fn fetch_rendered_html(&self, url: &str) -> Result<String, HeadlessError> {
        let page = match self.browser.new_page(url).await {
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

        // 等 load 事件 (0.9.1 无内建 networkIdle helper)。
        tokio::time::timeout(self.config.nav_timeout, page.wait_for_navigation())
            .await
            .map_err(|_| HeadlessError::Fetch("导航超时".into()))?
            .map_err(|e| HeadlessError::Fetch(format!("wait_for_navigation 失败: {e}")))?;

        // load 后等 JS 渲染落定 (SPA 内容由 JS 填充, load 时往往还没填完)。
        tokio::time::sleep(self.config.render_settle).await;

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
