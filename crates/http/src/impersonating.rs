//! 浏览器 TLS 指纹 HTTP 客户端 (基于 `wreq`)
//!
//! 走 `Emulation::Chrome147` 伪装出 Chrome 147 浏览器指纹 (TLS 客户端 hello +
//! HTTP/2 SETTINGS + headers), 用于通过 Cloudflare 的 JS 挑战。
//!
//! ## 浏览器身份版本统一 (MOC-186)
//! 三层抓取 (curl / wreq / headless) 的"声称浏览器版本"必须一致, 否则同一 origin 升级时
//! 身份漂移 (CF 先看到 Chrome A 的 TLS 指纹、再看到 Chrome B 的 UA) 反而可疑。
//! [`CHROME_MAJOR`] / [`CHROME_UA`] 是全 crate 单一事实源:
//! - ② wreq 层: `Emulation::Chrome147` (TLS + HTTP/2 + `sec-ch-ua` 由 wreq-util 按版本注入);
//! - ① curl 层 ([`crate::fetch`]): 无 TLS 指纹, 用 [`CHROME_UA`] 过 UA 黑名单粗筛;
//! - ③ headless 层 ([`crate::headless`]): 优先用真实系统 Chrome UA, 读不到时 fallback [`CHROME_UA`]。
//!
//! **升级 Chrome 版本时三处一起改**: `Emulation::ChromeNNN` + [`CHROME_MAJOR`] + [`CHROME_UA`] 版本号。
//! 注: wreq-util 3.0.0-rc.11 的 `Emulation` 枚举上限为 `Chrome147`, 再新需升 crate
//! (pre-release, 有 CF 指纹回归风险, 须 CI `cf-canary-on-deps` 验证)。
//!
//! PoC 范围: 只暴露 `get` / `post` 两个常用入口, 不实现完整 reqwest API 镜像。
//! 后续 PR 根据使用面补 `request` / `header` / `body` 等。

use std::time::Duration;

use thiserror::Error;
use wreq::Client;
use wreq_util::Emulation;

/// 全 crate 统一的"声称 Chrome 大版本" (见模块注释的三层身份统一约定)。
pub const CHROME_MAJOR: u32 = 147;

/// 全 crate 统一的 Chrome UA (macOS, 与 [`CHROME_MAJOR`] 同步)。curl 档 / headless fallback 复用,
/// 与 wreq `Emulation::Chrome147` 注入的 UA 版本号一致 —— 三层声称同一浏览器版本, 避免升级时身份漂移。
pub const CHROME_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";

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
    /// Chrome 147 指纹 (wreq-util rc.11 `Emulation` 枚举上限; 见模块注释的版本统一约定)。
    ///
    /// 配套: 30s 总超时 / 10s connect 超时 / 走 workspace rustls roots。
    pub fn chrome() -> Result<Self, ImpersonatingError> {
        let inner = Client::builder()
            .emulation(Emulation::Chrome147)
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
            .field("emulation", &"Chrome147")
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
