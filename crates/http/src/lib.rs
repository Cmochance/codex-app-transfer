//! Workspace 统一 HTTP 客户端入口 (MOC-137 PoC)
//!
//! 背景: `openai.com` / `chatgpt.com` / `help.openai.com` 等域被 Cloudflare 强 JS 挑战,
//! 标准 `reqwest` 不跑 JS, 直接 403/421。本 crate 引入 `wreq` (浏览器 TLS + HTTP/2
//! 指纹伪装) 作为这些域的 client, 其余域继续走 `reqwest` 不动。
//!
//! 用法: `should_impersonate(host)` 决定走哪个 client;
//! `ImpersonatingClient::chrome_120()` 拿带 Chrome 120 指纹的 client, 然后 `.get(url).send().await`。
//!
//! ③ JS 渲染层 (MOC-143): [`headless`] 模块用 headless Chromium (CDP) 抓 ①reqwest /
//! ②wreq 都拿不到的 JS 渲染 SPA (取渲染后 DOM)。先探测系统 Chrome, 未命中按需下载
//! chrome-headless-shell。
//!
//! 统一抓取层 (MOC-144): [`fetch`] 模块的 [`web_fetch`] 按设置页"内置联网抓取工具"的
//! 档位路由: `curl`(reqwest 静态) / `wreq`(浏览器 TLS 指纹) / `headless`(Chromium CDP)。
//! 配套 `GET /api/chrome/detect` + `POST /api/chrome/ensure` 供设置页探测/按需下载 Chrome。
//! **模型侧 web_fetch tool 注入(让 Codex 真能调到该工具)作后续 PR。**
//!
//! 非目标 (后续 PR): 不取代 workspace 其余地方 (`gemini_oauth` / `adapters` /
//! `proxy_runner` / `admin/handlers`) 的 reqwest, 按 PR 逐个迁移; 不引入 Python
//! sidecar (B 阶段), 留作 5% 漏网 fallback; ③ 层接入分层 router (空骨架检测 → 升级)
//! 亦作后续 PR。

pub mod fetch;
pub mod headless;
pub mod impersonating;
pub mod router;

pub use fetch::{web_fetch, WebFetchBackend, WebFetchError};
pub use headless::{fetch_rendered_html, HeadlessBrowser, HeadlessConfig, HeadlessError};
pub use impersonating::{ImpersonatingClient, ImpersonatingError};
pub use router::{should_impersonate, IMPERSONATE_HOSTS};
