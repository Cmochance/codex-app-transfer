//! 浏览器 TLS 指纹 HTTP 客户端 (基于 `wreq`)
//!
//! 走 `Emulation::Chrome120` 伪装出 Chrome 120 浏览器指纹 (TLS 客户端 hello +
//! HTTP/2 SETTINGS + headers), 用于通过 Cloudflare 的 JS 挑战。
//!
//! ## 版本选择 (MOC-186): 为什么固定 Chrome120 而非更新的版本
//! 实测 (CI `cf-canary-on-deps`, 数据中心 IP, 对比 main 基线): wreq-util rc.11 的
//! `Emulation::Chrome120` 在 help.openai.com **过 CF (200)**, 而后加的更新版本 `Chrome131` /
//! `Chrome147` 同条件 **403 (指纹回归)** —— wreq-util 新增的版本指纹反而过不了 CF。指纹时新性
//! 让位于"确实过 CF"这一硬约束。**升级 emulation 版本前必须 CI cf-canary 对比 main 基线验证**:
//! 以 **help.openai.com 为试金石** (chatgpt.com 对所有版本都 403 是 GitHub DC IP 被 CF 整体封的
//! 环境噪声, 家宽不受影响, 不能用它判指纹)。
//!
//! ## 三层身份: [`CHROME_UA`] 给 curl 复用
//! curl 档 ([`crate::fetch`]) 无 TLS 指纹, 用 [`CHROME_UA`] (与本 emulation 同版本号的 Chrome
//! UA) 过 UA 黑名单粗筛; headless 层优先用真实系统 Chrome UA, 读不到时 fallback [`CHROME_UA`]。
//! 三处声称同一浏览器版本, 避免身份漂移。**改版本时 `Emulation::ChromeNNN` + [`CHROME_MAJOR`]
//! + [`CHROME_UA`] 一起改 + 过 cf-canary。**
//!
//! PoC 范围: 只暴露 `get` / `post` 两个常用入口, 不实现完整 reqwest API 镜像。
//! 后续 PR 根据使用面补 `request` / `header` / `body` 等。

use std::time::Duration;

use thiserror::Error;
use wreq::Client;
use wreq_util::Emulation;

/// 全 crate 统一的"声称 Chrome 大版本" (见模块注释的版本选择: 实测唯一稳定过 CF 的版本)。
pub const CHROME_MAJOR: u32 = 120;

/// 全 crate 统一的 Chrome UA (macOS, 与 [`CHROME_MAJOR`] 同步)。curl 档 / headless fallback 复用,
/// 与 wreq `Emulation::Chrome120` 注入的 UA 版本号一致 —— 三层声称同一浏览器版本, 避免身份漂移。
pub const CHROME_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

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
    /// Chrome 120 指纹 (实测唯一稳定过 CF 的 wreq-util rc.11 版本; 见模块注释的版本选择)。
    ///
    /// 配套: 30s 总超时 / 10s connect 超时 / 走 workspace rustls roots。
    pub fn chrome() -> Result<Self, ImpersonatingError> {
        let inner = Client::builder()
            .emulation(Emulation::Chrome120)
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
            .field("emulation", &"Chrome120")
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
