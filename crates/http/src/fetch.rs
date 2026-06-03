//! 统一 web 抓取入口 (MOC-144): 按后端档位路由抓取一个 URL, 返回页面内容。
//!
//! "联网工具" 设置选的后端 → 这里执行:
//! - [`WebFetchBackend::Curl`]   ① `reqwest` 静态 GET (不跑 JS, 最快, 拿初始 HTML)
//! - [`WebFetchBackend::Wreq`]   ② [`crate::ImpersonatingClient`] 浏览器 TLS 指纹 (绕 CF JS 挑战)
//! - [`WebFetchBackend::Headless`] ③ [`crate::headless`] headless Chromium (跑 JS, 取渲染后 DOM)
//!
//! "关闭" 档不在这里 (关闭 = 根本不暴露抓取工具, 由上层判定)。返回的是页面原始/渲染后
//! 内容字符串; HTML→markdown 提取 (类 Claude WebFetch 的 Turndown 层) 留后续。

use std::time::Duration;

use thiserror::Error;

/// 抓取后端档位 (与设置项 `关闭/curl/wreq/headless` 的后三档一一对应)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFetchBackend {
    /// ① `reqwest` 静态 GET (不跑 JS)。
    Curl,
    /// ② `wreq` 浏览器 TLS 指纹 (绕 Cloudflare JS 挑战)。
    Wreq,
    /// ③ headless Chromium (跑 JS, 取渲染后 DOM)。
    Headless,
}

impl WebFetchBackend {
    /// 解析设置字符串。`off`/`关闭`/未知 → `None` (不抓取)。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "curl" => Some(Self::Curl),
            "wreq" => Some(Self::Wreq),
            "headless" => Some(Self::Headless),
            _ => None,
        }
    }

    /// 设置值字符串 (存 config 用)。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Curl => "curl",
            Self::Wreq => "wreq",
            Self::Headless => "headless",
        }
    }
}

#[derive(Debug, Error)]
pub enum WebFetchError {
    #[error("curl(reqwest) 抓取失败: {0}")]
    Curl(String),
    #[error("wreq 抓取失败: {0}")]
    Wreq(String),
    #[error("headless 抓取失败: {0}")]
    Headless(#[from] crate::headless::HeadlessError),
}

/// 按后端抓取一个 URL, 返回页面内容 (curl/wreq=初始 HTML, headless=渲染后 DOM)。
pub async fn web_fetch(backend: WebFetchBackend, url: &str) -> Result<String, WebFetchError> {
    match backend {
        WebFetchBackend::Curl => fetch_curl(url).await,
        WebFetchBackend::Wreq => fetch_wreq(url).await,
        WebFetchBackend::Headless => crate::headless::fetch_rendered_html(url)
            .await
            .map_err(Into::into),
    }
}

/// ① reqwest 静态 GET。
async fn fetch_curl(url: &str) -> Result<String, WebFetchError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| WebFetchError::Curl(format!("建 client 失败: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| WebFetchError::Curl(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(WebFetchError::Curl(format!("HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| WebFetchError::Curl(e.to_string()))
}

/// ② wreq 浏览器 TLS 指纹 (Chrome 120)。
async fn fetch_wreq(url: &str) -> Result<String, WebFetchError> {
    let client =
        crate::ImpersonatingClient::chrome_120().map_err(|e| WebFetchError::Wreq(e.to_string()))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| WebFetchError::Wreq(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(WebFetchError::Wreq(format!("HTTP {status}")));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| WebFetchError::Wreq(e.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_and_off() {
        for b in [
            WebFetchBackend::Curl,
            WebFetchBackend::Wreq,
            WebFetchBackend::Headless,
        ] {
            assert_eq!(WebFetchBackend::parse(b.as_str()), Some(b));
        }
        // 大小写 / 空白容忍
        assert_eq!(
            WebFetchBackend::parse(" Headless "),
            Some(WebFetchBackend::Headless)
        );
        // 关闭 / 未知 → None
        assert_eq!(WebFetchBackend::parse("off"), None);
        assert_eq!(WebFetchBackend::parse("关闭"), None);
        assert_eq!(WebFetchBackend::parse(""), None);
    }
}
