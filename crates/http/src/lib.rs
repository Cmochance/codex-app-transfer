//! Workspace 统一 HTTP 客户端入口 (MOC-137 PoC)
//!
//! 背景: `openai.com` / `chatgpt.com` / `help.openai.com` 等域被 Cloudflare 强 JS 挑战,
//! 标准 `reqwest` 不跑 JS, 直接 403/421。本 crate 引入 `wreq` (浏览器 TLS + HTTP/2
//! 指纹伪装) 作为这些域的 client, 其余域继续走 `reqwest` 不动。
//!
//! 用法: `should_impersonate(host)` 决定走哪个 client;
//! `ImpersonatingClient::chrome_120()` 拿带 Chrome 120 指纹的 client, 然后 `.get(url).send().await`。
//!
//! 非目标 (后续 PR): 不取代 workspace 其余地方 (`gemini_oauth` / `adapters` /
//! `proxy_runner` / `admin/handlers`) 的 reqwest, 按 PR 逐个迁移; 不引入 Python
//! sidecar (B 阶段), 留作 5% 漏网 fallback。

pub mod impersonating;
pub mod router;

pub use impersonating::{ImpersonatingClient, ImpersonatingError};
pub use router::{should_impersonate, IMPERSONATE_HOSTS};
