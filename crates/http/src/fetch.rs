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
    /// 资源是二进制 / 非文本 (图片/视频/音频/PDF/字节流等), 不下载正文 (MOC-152)。
    #[error("{0}")]
    Unsupported(String),
    /// 资源声明体积超过下载上限, 不下载 (防 OOM, MOC-152)。
    #[error("{0}")]
    TooLarge(String),
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
        let capped = cap_bytes(&body, MAX_HTML_INPUT_BYTES);
        // 先抽正文 (剥 nav/页眉/页脚/侧栏/广告), 抽取不可靠则回退整页 —— 绝不丢内容。
        match extract_main_content(&capped, url) {
            Some(main) => html_to_markdown(&main),
            None => html_to_markdown(&capped),
        }
    } else {
        body
    })
}

/// 下载体积上限 (MOC-152): 防误抓大文件把整个 body 读进内存。**靠服务器声明的 `Content-Length`**
/// 在读取**前**早退;媒体类大文件已由 [`binary_content_kind`] 按 content-type 在读取前先挡下。
/// 残余 (无 `Content-Length` 的分块巨型**文本**响应) 不在此拦, 由 HTTP client 30s 超时兜底,
/// 实际极罕见 (沿用 `resp.text()` 的 charset 感知解码, 未改流式以免非 UTF-8 中文页变乱码)。
/// 16MB 远高于正常网页 (htmd 输入另有 8MB cap)。
const MAX_DOWNLOAD_BYTES: u64 = 16 * 1024 * 1024;

/// 抽取出的正文文本下限 (MOC-152): 低于此视为抽取不可靠 (非文章页 / 误剥) → 回退整页。
const MIN_EXTRACTED_CHARS: usize = 200;

/// 命中二进制 / 非文本资源 → 返回中文类别名 (用于提示); 文本类返回 `None` (按正常抓取)。
///
/// 放行: `text/*`、`*json*`、`*xml*`、`*javascript*`、`*html*`、无 content-type (留嗅探)。
/// 其余 (image/video/audio/font/pdf/octet-stream/zip…) 一律当二进制, 不下载正文。
fn binary_content_kind(content_type: Option<&str>) -> Option<&'static str> {
    let ct = content_type?.to_ascii_lowercase();
    let ct = ct.split(';').next().unwrap_or("").trim();
    if ct.is_empty()
        || ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.contains("html")
    {
        return None;
    }
    if ct.starts_with("image/") {
        return Some("图片");
    }
    if ct.starts_with("video/") {
        return Some("视频");
    }
    if ct.starts_with("audio/") {
        return Some("音频");
    }
    if ct.starts_with("font/") || ct.contains("font") {
        return Some("字体");
    }
    if ct == "application/pdf" {
        return Some("PDF");
    }
    Some("二进制")
}

/// 读 body **前**对响应头做闸门 (MOC-152): 二进制资源 / 声明体积超限 → 不下载, 返明确提示。
/// 避免把图片/视频按 UTF-8 硬解成乱码塞给模型, 以及大文件读进内存 OOM。
fn precheck_response(
    content_type: Option<&str>,
    content_length: Option<u64>,
) -> Result<(), WebFetchError> {
    if let Some(kind) = binary_content_kind(content_type) {
        return Err(WebFetchError::Unsupported(format!(
            "该 URL 是{kind}资源 (content-type: {}), web_fetch 只抓文本 / HTML / 文本型 API, 未下载内容。",
            content_type.unwrap_or("?")
        )));
    }
    if let Some(len) = content_length {
        if len > MAX_DOWNLOAD_BYTES {
            return Err(WebFetchError::TooLarge(format!(
                "内容约 {:.1} MB, 超过下载上限 {} MB, 未抓取 —— 请抓取更具体的子页 URL。",
                len as f64 / (1024.0 * 1024.0),
                MAX_DOWNLOAD_BYTES / (1024 * 1024)
            )));
        }
    }
    Ok(())
}

/// 正文抽取 (MOC-152, dom_smoothie / readability.js 移植): 剥 nav/页眉/页脚/侧栏/广告,
/// 返回正文 article 的清洗后 HTML (交给 [`html_to_markdown`] 转 markdown)。
///
/// 返回 `None` = 抽取不可靠 / 不适用 → 调用方回退整页 (**绝不丢内容**):
/// - 非文章页 (搜索结果 / 应用 dashboard / API JSON 列表): readability `GrabFailed` —— 预期, 静默;
/// - 抽出正文过短 (< [`MIN_EXTRACTED_CHARS`]): 疑似误剥, 预期, 静默回退;
/// - 其余 `ReadabilityError` (如 `BadDocumentURL`: url 非绝对): **非预期**, eprintln 留痕便于
///   发现抽取系统性失效 (沿用本文件 eprintln 约定), 仍回退整页。
///
/// 输入体积由调用方转换**前**的 [`cap_bytes`] 8MB 字节上限兜底 (readability 见到的 HTML 必
/// ≤8MB), 故默认 config **不**额外开 dom_smoothie 的 element cap —— 开了反而会对合法大页误判
/// 回退、丢掉 PR-B 想救的大页正文。
fn extract_main_content(html: &str, url: &str) -> Option<String> {
    // new() 实际只可能返 BadDocumentURL (url 非绝对); 上游已校验, 理论不达 → 非预期, 留痕。
    let mut r = match dom_smoothie::Readability::new(html, Some(url), None) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[webfetch] 正文抽取初始化失败, 回退整页 ({url}): {e}");
            return None;
        }
    };
    match r.parse() {
        Ok(article) if article.length >= MIN_EXTRACTED_CHARS => Some(article.content.to_string()),
        Ok(_) => None, // 正文过短: 疑似非文章页, 预期回退, 静默
        Err(dom_smoothie::ReadabilityError::GrabFailed) => None, // 非文章页: 预期, 静默
        Err(e) => {
            eprintln!("[webfetch] 正文抽取失败, 回退整页 ({url}): {e}");
            None
        }
    }
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
    let content_length = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    // 读 body 前闸门: 二进制 / 超大资源不下载 (避免乱码 + OOM)。
    precheck_response(content_type.as_deref(), content_length)?;
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
    let content_length = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    // 读 body 前闸门: 二进制 / 超大资源不下载 (避免乱码 + OOM)。
    precheck_response(content_type.as_deref(), content_length)?;
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
    fn binary_content_kind_classifies() {
        // 文本类放行 (None)
        for ok in [
            "text/html; charset=utf-8",
            "application/json",
            "text/plain",
            "application/xml",
            "application/xhtml+xml",
            "application/javascript",
        ] {
            assert!(binary_content_kind(Some(ok)).is_none(), "应放行: {ok}");
        }
        assert!(binary_content_kind(None).is_none()); // 无 content-type → 留嗅探
                                                      // 二进制拦截 (Some(类别))
        assert_eq!(binary_content_kind(Some("image/png")), Some("图片"));
        assert_eq!(binary_content_kind(Some("video/mp4")), Some("视频"));
        assert_eq!(binary_content_kind(Some("audio/mpeg")), Some("音频"));
        assert_eq!(binary_content_kind(Some("application/pdf")), Some("PDF"));
        assert_eq!(binary_content_kind(Some("font/woff2")), Some("字体"));
        assert_eq!(
            binary_content_kind(Some("application/octet-stream")),
            Some("二进制")
        );
        assert_eq!(binary_content_kind(Some("application/zip")), Some("二进制"));
    }

    #[test]
    fn precheck_rejects_binary_and_oversize() {
        assert!(matches!(
            precheck_response(Some("image/png"), None),
            Err(WebFetchError::Unsupported(_))
        ));
        assert!(matches!(
            precheck_response(Some("text/html"), Some(MAX_DOWNLOAD_BYTES + 1)),
            Err(WebFetchError::TooLarge(_))
        ));
        // 文本 + 体积合规 → 放行
        assert!(precheck_response(Some("text/html; charset=utf-8"), Some(1000)).is_ok());
        assert!(precheck_response(Some("application/json"), None).is_ok());
        assert!(precheck_response(None, None).is_ok());
    }

    #[test]
    fn extract_main_falls_back_on_non_article() {
        // 空 / 过短 / 无正文骨架 → None (调用方回退整页), 且不 panic。
        assert!(extract_main_content("<div></div>", "https://example.com/x").is_none());
        assert!(extract_main_content("", "https://example.com/x").is_none());
        // 非绝对 URL → readability BadDocumentURL → None
        assert!(extract_main_content("<html><body>hi</body></html>", "not-a-url").is_none());
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
