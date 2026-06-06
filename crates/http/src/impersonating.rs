//! 浏览器 TLS 指纹 HTTP 客户端 (基于 `wreq`)
//!
//! 走 `Emulation::Chrome131` 伪装出 Chrome 131 浏览器指纹 (TLS 客户端 hello +
//! HTTP/2 SETTINGS + headers), 用于通过 Cloudflare 的 JS 挑战。
//!
//! ## 版本选择 (MOC-186): 为什么是 131 而非更新的 147
//! 实测 (CI `cf-canary-on-deps`, 数据中心 IP) wreq-util rc.11 的 `Emulation::Chrome147` 指纹
//! **过不了** chatgpt.com 的 CF (403);而 `Chrome131` (headless 层本就用的 UA 版本) 稳定通过。
//! 指纹时新性让位于"确实过 CF"这一硬约束 —— 升级 emulation 版本前**必须** CI cf-canary 验证新
//! 版本仍过 CF (住宅 IP 本地 canary 是假阳性)。
//!
//! ## 三层身份统一
//! 三层抓取 (curl / wreq / headless) 声称同一浏览器版本, 避免同一 origin 升级时身份漂移
//! (CF 先看到 Chrome A 的 TLS 指纹、再看到 Chrome B 的 UA 反而可疑)。[`CHROME_MAJOR`] /
//! [`CHROME_UA`] 是全 crate 单一事实源:
//! - wreq 层: `Emulation::Chrome131` (TLS + HTTP/2 + `sec-ch-ua` 由 wreq-util 按版本注入);
//! - curl 层 ([`crate::fetch`]): 无 TLS 指纹, 用 [`CHROME_UA`] 过 UA 黑名单粗筛;
//! - headless 层 ([`crate::headless`]): 优先真实系统 Chrome UA, 读不到时 fallback [`CHROME_UA`]。
//!
//! **升级版本时三处 (`Emulation::ChromeNNN` + [`CHROME_MAJOR`] + [`CHROME_UA`]) 一起改 + 过 cf-canary。**
//!
//! PoC 范围: 只暴露 `get` / `post` 两个常用入口, 不实现完整 reqwest API 镜像。
//! 后续 PR 根据使用面补 `request` / `header` / `body` 等。

use std::time::Duration;

use thiserror::Error;
use wreq::Client;
use wreq_util::Emulation;

/// 全 crate 统一的"声称 Chrome 大版本" (见模块注释的三层身份统一约定 + 版本选择)。
pub const CHROME_MAJOR: u32 = 131;

/// 全 crate 统一的 Chrome UA (macOS, 与 [`CHROME_MAJOR`] 同步)。curl 档 / headless fallback 复用,
/// 与 wreq `Emulation::Chrome131` 注入的 UA 版本号一致 —— 三层声称同一浏览器版本, 避免升级时身份漂移。
pub const CHROME_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Error)]
pub enum ImpersonatingError {
    #[error("wreq client build failed: {0}")]
    Build(String),
    #[error("wreq request error: {0}")]
    Request(String),
}

/// 浏览器指纹 HTTP 客户端 (轻量包装, 内部存一个 `wreq::Client`)
#[derive(Clone)]
pub struct ImpersonatingClient {
    inner: Client,
}

impl ImpersonatingClient {
    /// Chrome 131 指纹 (实测过 CF 的版本; 见模块注释的版本选择约定)。
    ///
    /// 配套: 30s 总超时 / 10s connect 超时 / 走 workspace rustls roots。
    pub fn chrome() -> Result<Self, ImpersonatingError> {
        let inner = Client::builder()
            .emulation(Emulation::Chrome131)
            // wreq 默认不跟随重定向,而本 client 要替代 reqwest(默认跟随)。保持
            // limited(10) 同 reqwest 行为,否则 call site 迁移到 301/302 页会拿到
            // 跳转响应而非最终资源(#358 chatgpt review P2)。
            .redirect(wreq::redirect::Policy::limited(10))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ImpersonatingError::Build(e.to_string()))?;
        Ok(Self { inner })
    }

    pub fn get(&self, url: &str) -> ImpersonatingRequestBuilder<'_> {
        ImpersonatingRequestBuilder {
            inner: self.inner.get(url),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn post(&self, url: &str) -> ImpersonatingRequestBuilder<'_> {
        ImpersonatingRequestBuilder {
            inner: self.inner.post(url),
            _phantom: std::marker::PhantomData,
        }
    }

    /// 拿到底层 `wreq::Client` (供需要完整 API 的高级用户)
    pub fn raw(&self) -> &Client {
        &self.inner
    }
}

impl std::fmt::Debug for ImpersonatingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImpersonatingClient")
            .field("emulation", &"Chrome131")
            .finish_non_exhaustive()
    }
}

/// `ImpersonatingClient::get/post` 返回的 request builder
pub struct ImpersonatingRequestBuilder<'a> {
    inner: wreq::RequestBuilder,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> ImpersonatingRequestBuilder<'a> {
    pub fn header(self, key: &str, value: &str) -> Self {
        Self {
            inner: self.inner.header(key, value),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn body(self, body: impl Into<wreq::Body>) -> Self {
        Self {
            inner: self.inner.body(body),
            _phantom: std::marker::PhantomData,
        }
    }

    pub async fn send(self) -> Result<wreq::Response, ImpersonatingError> {
        self.inner
            .send()
            .await
            .map_err(|e| ImpersonatingError::Request(e.to_string()))
    }
}
