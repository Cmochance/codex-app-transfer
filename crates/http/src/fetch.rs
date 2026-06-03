//! 统一 web 抓取入口 (MOC-144): 按后端档位路由抓取一个 URL, 返回页面内容。
//!
//! "联网工具" 设置选的后端 → 这里执行:
//! - [`WebFetchBackend::Curl`]   ① `reqwest` 静态 GET (不跑 JS, 最快, 拿初始 HTML)
//! - [`WebFetchBackend::Wreq`]   ② [`crate::ImpersonatingClient`] 浏览器 TLS 指纹 (绕 CF JS 挑战)
//! - [`WebFetchBackend::Headless`] ③ [`crate::headless`] headless Chromium (跑 JS, 取渲染后 DOM)
//!
//! "关闭" 档不在这里 (关闭 = 根本不暴露抓取工具, 由上层判定)。
//!
//! ## HTML→markdown (MOC-145)
//! HTML 内容统一经 [`html_to_markdown`] (htmd, Turndown 思路) 转 markdown 后返回: 比原始
//! HTML 省 token、更干净。判定走 content-type (curl/wreq 有响应头) + body 嗅探兜底
//! (headless 渲染后恒 HTML)。非 HTML (JSON / 纯文本 API 响应) 原样透传, 不破坏结构。

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

/// 按后端抓取一个 URL, 返回页面内容。HTML (curl/wreq 按 content-type / 嗅探判定,
/// headless 恒 HTML) 转 markdown 返回; 非 HTML (JSON / 纯文本) 原样透传。
///
/// 2xx 但空 body 时返回 `Ok("")` —— 上层 (MCP server) 负责把"空响应"翻成对模型清晰的
/// 提示, 这里不把合法的空响应 (如 204) 当错误。
pub async fn web_fetch(backend: WebFetchBackend, url: &str) -> Result<String, WebFetchError> {
    let (body, is_html) = match backend {
        WebFetchBackend::Curl => fetch_curl(url).await?,
        WebFetchBackend::Wreq => fetch_wreq(url).await?,
        // headless 渲染后的 page.content() 恒为完整 HTML 文档。
        WebFetchBackend::Headless => (crate::headless::fetch_rendered_html(url).await?, true),
    };
    Ok(if is_html {
        html_to_markdown(&cap_bytes(&body, MAX_HTML_INPUT_BYTES))
    } else {
        body
    })
}

/// htmd 转换前的 HTML 输入字节上限。htmd 对完整 DOM **无深度上限地递归** walk, 病态大页 /
/// 深嵌套页可能 OOM 或栈溢出 —— 栈溢出是 abort, `catch_unwind` 抓不住, 会杀掉整个 MCP
/// server 进程(违背"单次抓取失败不杀 server")。转换前截到此上限兜底。8MB 远高于输出
/// 100k 字符上限(markdown 比 HTML 密, 正常页根本到不了这层截断), 仅防对抗/异常巨页。
const MAX_HTML_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// 按字节上限截断(就近退到 char 边界), 未超则零拷贝借用。
fn cap_bytes(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.len() <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(s[..end].to_string())
}

/// ① reqwest 静态 GET。返回 (body, is_html)。
async fn fetch_curl(url: &str) -> Result<(String, bool), WebFetchError> {
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
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = resp
        .text()
        .await
        .map_err(|e| WebFetchError::Curl(e.to_string()))?;
    let is_html = is_html_response(content_type.as_deref(), &body);
    Ok((body, is_html))
}

/// ② wreq 浏览器 TLS 指纹 (Chrome 120)。返回 (body, is_html)。
async fn fetch_wreq(url: &str) -> Result<(String, bool), WebFetchError> {
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
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| WebFetchError::Wreq(e.to_string()))?;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let is_html = is_html_response(content_type.as_deref(), &body);
    Ok((body, is_html))
}

/// 是否按 HTML 处理 (→ 转 markdown)。content-type 权威: 明确非 HTML (JSON/纯文本) 即
/// 不转, 避免破坏结构化响应; 无 content-type 时才嗅探 body 兜底 (headless 不走这里)。
fn is_html_response(content_type: Option<&str>, body: &str) -> bool {
    match content_type {
        Some(ct) => {
            let ct = ct.to_ascii_lowercase();
            ct.contains("text/html") || ct.contains("application/xhtml")
        }
        None => looks_like_html(body),
    }
}

/// body 嗅探: trim 后**开头**即典型 HTML 文档标记才判 HTML。仅在缺 content-type 时用。
/// 用 starts_with (锚定文档头) 而非 contains —— 否则 JSON 字符串里含 `<html>` 会误判。
fn looks_like_html(body: &str) -> bool {
    let head: String = body
        .trim_start()
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.starts_with("<head")
        || head.starts_with("<body")
}

/// HTML→markdown (htmd, Turndown 思路)。剥 script/style/noscript/svg 噪声; 转换失败或
/// 转出空 (纯 JS 骨架等) → 回退原 HTML, 绝不丢内容。
fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "svg"])
        .build();
    match converter.convert(html) {
        Ok(md) if !md.trim().is_empty() => md,
        // 转出空 (纯 JS 骨架等) 是预期, 静默回退原文。
        Ok(_) => html.to_string(),
        // 转换器真报错是非预期: 留 stderr 痕迹以便发现 htmd 对某类 HTML 的系统性失败。
        Err(e) => {
            eprintln!("[webfetch] html→markdown 转换失败, 回退原 HTML: {e}");
            html.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_markdown_basic_and_skip() {
        let html = "<html><head><style>.x{color:red}</style></head>\
            <body><h1>Title</h1><p>Hello <b>world</b></p>\
            <script>var leak='SHOULD_NOT_APPEAR';</script></body></html>";
        let md = html_to_markdown(html);
        assert!(md.contains("Title"), "缺标题: {md}");
        assert!(md.contains("world"), "缺正文: {md}");
        // script/style 内容必须被剥掉
        assert!(!md.contains("SHOULD_NOT_APPEAR"), "script 泄漏: {md}");
        assert!(!md.contains("color:red"), "style 泄漏: {md}");
    }

    #[test]
    fn html_to_markdown_empty_falls_back_to_raw() {
        // 转出空时回退原文, 不丢内容。
        let raw = "<div></div>";
        let out = html_to_markdown(raw);
        assert!(!out.is_empty());
    }

    #[test]
    fn is_html_by_content_type_and_sniff() {
        // content-type 权威
        assert!(is_html_response(Some("text/html; charset=utf-8"), "{}"));
        assert!(is_html_response(Some("application/xhtml+xml"), ""));
        assert!(!is_html_response(
            Some("application/json"),
            "<html>fake</html>"
        ));
        assert!(!is_html_response(Some("text/plain"), "<html>"));
        // 无 content-type → 嗅探
        assert!(is_html_response(None, "  <!DOCTYPE html><html></html>"));
        assert!(is_html_response(None, "<HTML><body>x</body></HTML>"));
        assert!(!is_html_response(None, "{\"k\": \"<html> in a string\"}"));
        assert!(!is_html_response(None, "plain text"));
    }

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
