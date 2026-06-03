//! 浏览器 TLS 指纹 HTTP 客户端 (基于 `wreq`)
//!
//! 走 `Emulation::Chrome120` 伪装出 Chrome 120 浏览器指纹 (TLS 客户端 hello +
//! HTTP/2 SETTINGS + headers), 用于通过 Cloudflare 的 JS 挑战。
//!
//! PoC 范围: 只暴露 `get` / `post` 两个常用入口, 不实现完整 reqwest API 镜像。
//! 后续 PR 根据使用面补 `request` / `header` / `body` 等。

use std::time::Duration;

use thiserror::Error;
use wreq::Client;
use wreq_util::Emulation;

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
    /// Chrome 120 指纹 (2024-01 发布的稳定 major)
    ///
    /// 配套: 30s 总超时 / 10s connect 超时 / 走 workspace rustls roots。
    pub fn chrome_120() -> Result<Self, ImpersonatingError> {
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
