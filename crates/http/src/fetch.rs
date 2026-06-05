//! 统一 web 抓取入口 (MOC-144): 按后端档位路由抓取一个 URL, 返回页面内容。
//!
//! "联网工具" 设置选的后端 → 这里执行:
//! - [`WebFetchBackend::Auto`]   ⓪ **自动** (MOC-161, 推荐默认): curl→wreq→headless 按失败信号
//!   动态升级, per-origin 记住每站最佳档 (见 [`web_fetch_auto`])。系统代理门槛在设置页(前端)把关。
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
//!
//! ## 结构化返回值 [`WebFetchOutcome`] (MOC-181)
//! [`web_fetch`] 由 `Result<String, …>` 改为 `Result<WebFetchOutcome, …>`:除正文
//! (`content`) 外额外带出**实际命中档** (`final_tier`)、**升级历程** (`trail`)、**HTTP
//! status** (`status`)。调用方(MCP server 诊断埋点)凭此构造 cat-webfetch 链路条目,
//! 无需再次解析正文。`content` 语义与原 `String` 完全一致。

use std::time::Duration;

use thiserror::Error;

/// 抓取后端档位 (与设置项 `关闭/auto/curl/wreq/headless` 的后四档一一对应)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFetchBackend {
    /// ⓪ **自动** (MOC-161, 推荐默认): 按失败信号 curl→wreq→headless 动态升级;
    /// per-origin 记住每域最佳档, 下次直接从该档起 (省试错)。语义 = "能力天花板 = headless"。
    Auto,
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
            "auto" => Some(Self::Auto),
            "curl" => Some(Self::Curl),
            "wreq" => Some(Self::Wreq),
            "headless" => Some(Self::Headless),
            _ => None,
        }
    }

    /// 设置值字符串 (存 config 用)。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Curl => "curl",
            Self::Wreq => "wreq",
            Self::Headless => "headless",
        }
    }

    /// 该档是否"允许升到 headless"(= 可能需要 Chrome)。`Auto` / `Headless` 为真 ——
    /// 用于上层(MCP server web_search 放行 / 设置页 Chrome consent)判定。
    pub fn may_use_headless(&self) -> bool {
        matches!(self, Self::Auto | Self::Headless)
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
    /// Auto 档升级链跑完仍失败: 错误串带各档升级原因 (诊断, MOC-161 review H1)。
    #[error("{0}")]
    Auto(String),
}

/// `web_fetch` 的结构化结果 (MOC-181 诊断): 正文 + 实际命中的档 + 升级历程 + HTTP status。
/// `trail` 在**成功路径也带出** —— 空 = 单档 / auto 未升级, 非空 = auto 逐档升级原因。
#[derive(Debug, Clone)]
pub struct WebFetchOutcome {
    /// 抽取 + 转 markdown 后的正文 (非 HTML 原样透传)。
    pub content: String,
    /// 最终命中的抓取档 (auto 升级后的实际档; 单档即入参 backend)。
    pub final_tier: WebFetchBackend,
    /// auto 档逐档升级原因 (成功也带); 单档 / auto 未升级为空。
    pub trail: Vec<String>,
    /// HTTP status (curl/wreq 有; headless 无 → None)。
    pub status: Option<u16>,
}

/// 各档抓取的中间产物 (转 markdown 前) —— 带 final_tier/trail/status 供 [`web_fetch`] 组装 Outcome。
struct Fetched {
    body: String,
    is_html: bool,
    final_tier: WebFetchBackend,
    trail: Vec<String>,
    status: Option<u16>,
}

/// 按后端抓取一个 URL, 返回结构化结果 [`WebFetchOutcome`]。HTML (curl/wreq 按 content-type /
/// 嗅探判定, headless 恒 HTML) 转 markdown 后写入 `content`; 非 HTML (JSON / 纯文本) 原样透传。
///
/// 2xx 但空 body 时返回 `Ok(outcome)` 且 `outcome.content` 为空字符串 —— 上层 (MCP server)
/// 负责把"空响应"翻成对模型清晰的提示, 这里不把合法的空响应 (如 204) 当错误。
pub async fn web_fetch(
    backend: WebFetchBackend,
    url: &str,
) -> Result<WebFetchOutcome, WebFetchError> {
    // 客户端重定向跟随 (MOC-139): curl/reqwest 只跟 HTTP 3xx, **不跟 HTML meta refresh / JS
    // location** —— 这类页 (如绕 Twitter/Substack title-card feud 的跳转页) curl 拿到的是正文
    // 极少的占位页。占位页 + 解析出重定向 target → 换 URL 重抓 (防循环 + 记 trail)。
    let mut current = url.to_string();
    let mut hops: Vec<String> = Vec::new();
    let f = loop {
        let f = fetch_by_backend(backend, &current).await?;
        if f.is_html && hops.len() < MAX_CLIENT_REDIRECTS {
            let capped = cap_bytes(&f.body, MAX_HTML_INPUT_BYTES);
            // 仅"正文极少的占位页"才尝试跟随 —— 正常页含 meta refresh/JS 也不动 (避免误跳)。
            if visible_text_len(&capped) < MIN_EXTRACTED_CHARS {
                if let Some(target) = detect_client_redirect(&capped, &current) {
                    // 防自跳 / 简单循环 (A→B→A)。
                    if target != current && !hops.iter().any(|h| h.ends_with(&target)) {
                        hops.push(format!("client-redirect → {target}"));
                        current = target;
                        continue;
                    }
                }
            }
        }
        break f;
    };
    // 内容按**重定向后的最终 URL** (current) 抽取 —— base url 决定相对链接解析。
    let content = if f.is_html {
        let capped = cap_bytes(&f.body, MAX_HTML_INPUT_BYTES);
        // 先抽正文 (剥 nav/页眉/页脚/侧栏/广告), 抽取不可靠则回退整页 —— 绝不丢内容。
        match extract_main_content(&capped, &current) {
            Some(main) => html_to_markdown(&main),
            None => html_to_markdown(&capped),
        }
    } else {
        f.body
    };
    let mut trail = hops;
    trail.extend(f.trail);
    Ok(WebFetchOutcome {
        content,
        final_tier: f.final_tier,
        trail,
        status: f.status,
    })
}

/// 按后端抓一次 (不含客户端重定向跟随) —— 供 [`web_fetch`] 的重定向 loop 每跳调用。
async fn fetch_by_backend(backend: WebFetchBackend, url: &str) -> Result<Fetched, WebFetchError> {
    match backend {
        // Auto: curl→wreq→headless 按失败信号自动升级 + per-origin 复用 (MOC-161)。
        WebFetchBackend::Auto => web_fetch_auto(url).await,
        WebFetchBackend::Curl => single_tier(WebFetchBackend::Curl, url).await,
        WebFetchBackend::Wreq => single_tier(WebFetchBackend::Wreq, url).await,
        // headless 渲染后的 page.content() 恒为完整 HTML 文档 (无 HTTP status)。
        WebFetchBackend::Headless => Ok(Fetched {
            body: crate::headless::fetch_rendered_html(url).await?,
            is_html: true,
            final_tier: WebFetchBackend::Headless,
            trail: Vec::new(),
            status: None,
        }),
    }
}

/// 下载体积上限 (MOC-152): 防误抓大文件把整个 body 读进内存。**靠服务器声明的 `Content-Length`**
/// 在读取**前**早退;媒体类大文件已由 [`binary_content_kind`] 按 content-type 在读取前先挡下。
/// 残余 (无 `Content-Length` 的分块巨型**文本**响应) 不在此拦, 由 HTTP client 30s 超时兜底,
/// 实际极罕见 (沿用 `resp.text()` 的 charset 感知解码, 未改流式以免非 UTF-8 中文页变乱码)。
/// 16MB 远高于正常网页 (htmd 输入另有 8MB cap)。
const MAX_DOWNLOAD_BYTES: u64 = 16 * 1024 * 1024;

/// 抽取出的正文文本下限 (MOC-152): 低于此视为抽取不可靠 (非文章页 / 误剥) → 回退整页。
const MIN_EXTRACTED_CHARS: usize = 200;

/// 客户端重定向 (meta refresh / JS location) 最大跟随跳数 (MOC-139, 防循环)。
const MAX_CLIENT_REDIRECTS: usize = 3;

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

/// 一次原始抓取的产物 (curl/wreq tier): 供单档判 status / Auto 档判升级信号。
struct RawFetch {
    status: u16,
    body: String,
    is_html: bool,
    /// CF 挑战 header (`cf-mitigated`) 是否出现 —— Auto 档据此升级 (MOC-161)。
    cf_challenge_header: bool,
}

/// 单档 (Curl/Wreq) 抓取: raw 抓 → 非 2xx 即 Err (保持原单档语义, 不自动升级)。
async fn single_tier(tier: WebFetchBackend, url: &str) -> Result<Fetched, WebFetchError> {
    let raw = fetch_raw(tier, url).await?;
    if !(200..300).contains(&raw.status) {
        return Err(tier_http_error(tier, raw.status));
    }
    Ok(Fetched {
        body: raw.body,
        is_html: raw.is_html,
        final_tier: tier,
        trail: Vec::new(),
        status: Some(raw.status),
    })
}

/// 按 tier (Curl/Wreq) 原始抓取, **不判 status** (status 判定交调用方: 单档→Err, Auto→升级信号)。
/// Headless / Auto 不走这里。
async fn fetch_raw(tier: WebFetchBackend, url: &str) -> Result<RawFetch, WebFetchError> {
    match tier {
        WebFetchBackend::Curl => fetch_curl_raw(url).await,
        WebFetchBackend::Wreq => fetch_wreq_raw(url).await,
        WebFetchBackend::Headless | WebFetchBackend::Auto => {
            unreachable!("fetch_raw 只用于 Curl/Wreq tier")
        }
    }
}

/// tier 对应的 HTTP 错误 (单档非 2xx)。
fn tier_http_error(tier: WebFetchBackend, status: u16) -> WebFetchError {
    let msg = format!("HTTP {status}");
    match tier {
        WebFetchBackend::Wreq => WebFetchError::Wreq(msg),
        _ => WebFetchError::Curl(msg),
    }
}

/// ① reqwest 静态 GET。返回 RawFetch (不判 status)。
async fn fetch_curl_raw(url: &str) -> Result<RawFetch, WebFetchError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| WebFetchError::Curl(format!("建 client 失败: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| WebFetchError::Curl(e.to_string()))?;
    let status = resp.status().as_u16();
    let cf_challenge_header = resp.headers().get("cf-mitigated").is_some();
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
    Ok(RawFetch {
        status,
        body,
        is_html,
        cf_challenge_header,
    })
}

/// ② wreq 浏览器 TLS 指纹 (Chrome 120)。返回 RawFetch (不判 status)。
async fn fetch_wreq_raw(url: &str) -> Result<RawFetch, WebFetchError> {
    let client =
        crate::ImpersonatingClient::chrome_120().map_err(|e| WebFetchError::Wreq(e.to_string()))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| WebFetchError::Wreq(e.to_string()))?;
    let status = resp.status().as_u16();
    let cf_challenge_header = resp.headers().get("cf-mitigated").is_some();
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
    Ok(RawFetch {
        status,
        body,
        is_html,
        cf_challenge_header,
    })
}

// ===================== MOC-161 Auto 档: 升级链 + 信号检测 + per-origin 复用 =====================

/// Auto 档升级链顺序 (低→高成本)。
const AUTO_TIERS: [WebFetchBackend; 3] = [
    WebFetchBackend::Curl,
    WebFetchBackend::Wreq,
    WebFetchBackend::Headless,
];

/// per-origin 最佳后端缓存 (进程内): 某 origin 升到某档成功就记住, 下次该 origin 直接从该档起,
/// 省掉重复的低档试错。仅进程生命周期 (MCP server 重启清空), 不持久化 —— 站点反爬策略会变,
/// 重启重新探测更安全。
fn origin_profiles() -> &'static std::sync::Mutex<std::collections::HashMap<String, WebFetchBackend>>
{
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, WebFetchBackend>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 取 URL 的 origin (`scheme://host`) 作 per-origin cache key。无法解析 → None (不缓存)。
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if scheme.is_empty() || host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// 记住某 origin 的成功档 (per-origin 复用)。origin 无法解析则不记。
/// **headless 降一档缓存成 wreq** (chatgpt-codex review): 同 origin 不同 URL 难度不同 —— JS app
/// 页要 headless, 但 raw API/text endpoint 用 curl/wreq 就够 (非 HTML 2xx)。若缓存 headless 当起始
/// 档, 下次该 origin 的 API URL 会直接走 headless → JSON 被渲染成 browser HTML 破坏结构。缓存 wreq
/// 既省 curl 试错、又让 API URL 走 wreq 正确拿非 HTML 2xx; app 页从 wreq 起仍会升 headless。
fn remember_origin(origin: &Option<String>, tier: WebFetchBackend) {
    let cache_tier = if tier == WebFetchBackend::Headless {
        WebFetchBackend::Wreq
    } else {
        tier
    };
    if let Some(o) = origin {
        if let Ok(mut m) = origin_profiles().lock() {
            m.insert(o.clone(), cache_tier);
        }
    }
}

/// Auto 档: 从 per-origin 记住的档 (默认 curl) 起, 按失败信号 curl→wreq→headless 升级;
/// 成功后记住该 origin 的成功档。返回 [`Fetched`] (带 final_tier + 升级 trail + status)。
async fn web_fetch_auto(url: &str) -> Result<Fetched, WebFetchError> {
    let origin = origin_of(url);
    // 起始档 = 该 origin 上次成功的档 (没记录则 curl)。
    let start = origin
        .as_ref()
        .and_then(|o| {
            origin_profiles()
                .lock()
                .ok()
                .and_then(|m| m.get(o).copied())
        })
        .unwrap_or(WebFetchBackend::Curl);
    let start_idx = AUTO_TIERS.iter().position(|&t| t == start).unwrap_or(0);

    // 记每档为何升级, 最终失败时拼进错误 —— 否则只看到"headless 失败", 不知 curl/wreq 为何升级
    // (review H1)。
    let mut trail: Vec<String> = Vec::new();
    // 升级链里最后一个非空 body —— headless 失败时回退它, 避免把 curl/wreq 已拿到的内容变成
    // Auto error(启发式升级如 is_js_shell 可能误判短静态页, chatgpt-codex review)。
    let mut last_usable: Option<Fetched> = None;
    for &tier in &AUTO_TIERS[start_idx..] {
        if tier == WebFetchBackend::Headless {
            // headless 是终极兜底 (跑 JS), 无"升级信号"可言 —— 成功即返回, 失败即整体失败。
            match crate::headless::fetch_rendered_html(url).await {
                Ok(body) => {
                    remember_origin(&origin, tier);
                    return Ok(Fetched {
                        body,
                        is_html: true,
                        final_tier: tier,
                        trail,
                        status: None,
                    });
                }
                Err(e) => {
                    // headless 失败但升级链中有非空 body → 回退它(非破坏性降级)。不 remember
                    // headless(它失败了), 也不记那档(它被判过升级信号, 记了下次还从那升)。
                    if let Some(f) = last_usable.take() {
                        return Ok(f);
                    }
                    return Err(WebFetchError::Auto(format!(
                        "升级历程 [{}] 后 headless 仍失败: {e}",
                        trail.join("; ")
                    )));
                }
            }
        }
        match fetch_raw(tier, url).await {
            // 不可恢复 (4xx 死链/权限除反爬 + 5xx 无挑战的真服务器故障): 换 client、headless 渲染
            // 错误页都没意义, 直接报 HTTP 错误 (不当成功, 对齐单档 curl/wreq 行为, chatgpt-codex)。
            Ok(raw) if is_unrecoverable(&raw) => return Err(tier_http_error(tier, raw.status)),
            // 拿到可用结果 (无升级信号) → 记住该档并返回。
            Ok(raw) if !needs_upgrade(&raw) => {
                remember_origin(&origin, tier);
                return Ok(Fetched {
                    body: raw.body,
                    is_html: raw.is_html,
                    final_tier: tier,
                    trail,
                    status: Some(raw.status),
                });
            }
            // 有升级信号 (反爬 / 空 / JS 骨架) → 记原因, 升级到下一档。
            Ok(raw) => {
                let is_shell = raw.is_html && is_js_shell(&raw.body);
                trail.push(format!(
                    "{tier:?}(status={} cf_hdr={} 空={} 骨架={is_shell})",
                    raw.status,
                    raw.cf_challenge_header,
                    raw.body.trim().is_empty(),
                ));
                // 留作 headless 失败时的回退 —— 判据见 worth_fallback(仅非标准 mount 兜底的不确定
                // 启发式; 确认 app shell / 反爬 / 空 / 非-200 排除)。后到的合格档覆盖前面的。
                if worth_fallback(&raw) {
                    last_usable = Some(Fetched {
                        body: raw.body,
                        is_html: raw.is_html,
                        final_tier: tier,
                        trail: trail.clone(),
                        status: Some(raw.status),
                    });
                }
                continue;
            }
            // 二进制 / 超大: 升级也没用 (内容本身不抓), 直接返回。
            Err(e @ (WebFetchError::Unsupported(_) | WebFetchError::TooLarge(_))) => return Err(e),
            // 其他硬错误 (连接 / 超时): 记原因, 升级试下一档。
            Err(e) => {
                trail.push(format!("{tier:?} 错误: {e}"));
                continue;
            }
        }
    }
    // 升级链跑完仍无可用结果 (headless 分支总会 return, 正常不达此)。
    Err(WebFetchError::Auto(format!(
        "所有后端均失败: {}",
        trail.join("; ")
    )))
}

/// auto 升级时, 该档 raw 是否值得留作 headless 失败的回退 body。**仅"非标准 mount 兜底"这种不确定
/// 启发式** —— `has_app_bundle_script`(MOC-183)可能把有内容的短静态页误判成空壳, 误升后若 headless
/// 起不来, 回退它好过把 curl 已拿到的成功 body 变 Auto error(破坏性降级)。但以下一并排除:
/// - **标准框架 mount**(`has_spa_skeleton`)是**确认的 app shell**: headless 失败应保留 Auto error 让
///   调用方知道 headless 依赖坏了; 回退未渲染空壳既无内容、又掩盖故障(chatgpt-codex review 第 2 轮)。
/// - 反爬挑战页(结构也像 shell)/ 空 body / 非-200 是**确定**失败信号, 回退会把挑战页/空当内容误导模型。
/// `visible_text < MIN` 复刻 `is_js_shell` 的 gate(只对真"可见文本极少"的页回退)。
fn worth_fallback(raw: &RawFetch) -> bool {
    raw.status == 200
        && raw.is_html
        && !raw.cf_challenge_header
        && !raw.body.trim().is_empty()
        && !is_challenge_body(&raw.body)
        && visible_text_len(&raw.body) < MIN_EXTRACTED_CHARS
        && !has_spa_skeleton(&raw.body)
        && has_app_bundle_script(&raw.body)
}

/// 这次抓取是否"不可恢复"—— 换 client 或 headless 渲染错误页都救不了, Auto 直接报错(不升级、不当
/// 成功)。两类:
/// ① **4xx 除 403/429**(404 不存在 / 405 GET 不允许 / 422 请求错 / 410 已删 / 451 封禁…… headless
///    渲染这些的错误页只会被模型当成功摘要);403/429 是反爬/限流, headless+stealth 可能过, 排除。
/// ② **5xx 但无挑战信号**(500/502 真服务器故障, headless 渲染 500 页当成功无意义)。带 CF 挑战信号
///    的 5xx(常见 503 challenge)**不算** —— headless 可能过挑战拿真内容 (chatgpt-codex review)。
fn is_unrecoverable(raw: &RawFetch) -> bool {
    let s = raw.status;
    if (400..500).contains(&s) && !matches!(s, 403 | 429) {
        return true;
    }
    (500..600).contains(&s) && !raw.cf_challenge_header && !is_challenge_body(&raw.body)
}

/// Auto 档: 这次 (curl/wreq) 抓取是否需要升级到更高档。命中任一信号即升级:
/// ① 非 2xx (反爬 403 / 限流 429 / 5xx); ② CF 挑战 (header 或 body 标记);
/// ③ 空 body; ④ JS 空骨架 (HTML 但剥 script 后几乎无可见文本 → 内容靠 JS 渲染)。
fn needs_upgrade(raw: &RawFetch) -> bool {
    // 非 HTML (JSON / text API): 所有 2xx 都算成功 —— 201/202/206 是合法 API 状态, 且升 headless
    // 会把 JSON 序列化成 browser document 破坏结构 (chatgpt-codex review); 只有非 2xx 才升级。
    if !raw.is_html {
        return !(200..300).contains(&raw.status);
    }
    // 以下针对 HTML: 只 200 算"正常拿到内容"; 202 等对 HTML GET 异常 (反爬常用 202 软拦, 见 DDG
    // anomaly), 升级 (review M1: 旧 `!(200..300)` 放过 HTML 202 反爬页当成功)。
    if raw.status != 200 {
        return true;
    }
    if raw.cf_challenge_header || is_challenge_body(&raw.body) {
        return true;
    }
    if raw.body.trim().is_empty() {
        return true;
    }
    is_js_shell(&raw.body)
}

/// body 是否是反爬挑战 / 软拦截页 (CF + 通用反爬, 只看前 4KB)。命中即 Auto 升级。
/// CF 的 `just a moment` 锚到 `<title>` (避免讨论 CF 的正文误判, review M2); `challenge-platform`
/// / `_cf_chl_opt` 是 CF 专有 token; 另加通用反爬 (DDG anomaly / 限流 / "enable javascript" /
/// DataDome / PerimeterX) marker (review M1: 非 CF 软拦截此前 slip through 当成功)。
fn is_challenge_body(body: &str) -> bool {
    let head: String = body
        .chars()
        .take(4096)
        .collect::<String>()
        .to_ascii_lowercase();
    // CF 专有
    head.contains("challenge-platform")
        || head.contains("cf-browser-verification")
        || head.contains("_cf_chl_opt")
        || head.contains("<title>just a moment")
        // 通用反爬 / 软拦截
        || head.contains("bots use duckduckgo")
        || head.contains("unusual traffic")
        || head.contains("enable javascript and cookies")
        || head.contains("datadome")
        || head.contains("px-captcha")
        || head.contains("are you a robot")
}

/// JS 空骨架判定: HTML 文档但剥掉 script/style + 标签后可见文本极少 (典型 SPA 初始 HTML:
/// `<div id="root"></div>` + bundle script, 内容全靠 JS 运行时填充)。阈值复用正文下限。
///
/// **已知边界 (review H2)**: 内容塞进 `<script id="__NEXT_DATA__">` 等 data island 的 SSR-light
/// 页, 剥 script 后可见文本也偏少 → 判 shell → 升 headless。这**多花一次 headless 但结果正确**
/// (headless 渲染出完整 DOM), per-origin cache 后续复用, 故按启发式接受不做 data-island 特判
/// (特判反会漏真 CSR 空壳——它同样带 __NEXT_DATA__)。
fn is_js_shell(html: &str) -> bool {
    // 仅"可见文本少"不够 —— 短静态页(status 页 / 小 snippet)也少, 但 curl 已成功, 升 headless
    // 是无谓浪费。要求可见文本极少 **且** 有"内容靠 JS 渲染"的信号才判 shell。
    if visible_text_len(html) >= MIN_EXTRACTED_CHARS {
        return false;
    }
    // ① 标准框架 mount + script(精确覆盖 React/Vue/Next/Nuxt/Angular)。
    // ② MOC-183 兜底: 非标准 mount 但有 bundle 脚本(ESM `type=module` / 外部 `.js`) —— 实测
    //    mouseless.click / doscienceto.it 这类被原 mount 白名单漏判, headless 能抓到内容
    //    (vtext 163→1722 / 21→126)。静态短页(status/snippet)不引 module/bundle.js → 不误升,
    //    保住"避免误升静态短页"的原 trade-off。
    has_spa_skeleton(html) || has_app_bundle_script(html)
}

/// "加载 JS 应用"的 bundle 脚本特征 (MOC-183): ESM `<script type=module>` 或外部 `.js`/`.mjs`
/// 引用 —— 现代构建工具 (vite/webpack/rollup) 产物。用于 [`is_js_shell`] 的非标准 mount 兜底:
/// **仅在可见文本已极少时才查** (调用点已 gate), 故"正文里恰好提到 .js"的误判窗口极小。
fn has_app_bundle_script(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    if !lower.contains("<script") {
        return false;
    }
    // ESM `type=module` 声明(容忍等号空格 + 引号变体) 或外部 .js/.mjs bundle 引用。
    has_module_type_attr(&lower) || has_js_bundle_url(&lower)
}

/// `lower`(已小写)是否有 `type` 属性值 = `module` —— ESM script 声明。容忍**等号周围空格**
/// (`type = "module"`) + 三种引号(双 / 单 / 无), 对齐 [`id_attr_matches`] 的宽容解析(chatgpt-codex
/// review 第 3 轮:exact substring 漏 spaced 变体)。`type` 前须非 ident 字符(避免 `mimetype` /
/// `datatype` 误命中), `module` 后须是引号 / 空白 / `>` / `/`(避免 `module-x` 误命中)。
fn has_module_type_attr(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("type") {
        let start = from + pos;
        from = start + 4;
        if start > 0 {
            let p = bytes[start - 1];
            if p.is_ascii_alphanumeric() || p == b'_' || p == b'-' {
                continue;
            }
        }
        let rest = lower[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
        if let Some(after) = rest.strip_prefix("module") {
            if after.is_empty() || after.starts_with(['"', '\'', ' ', '>', '/', '\t', '\n', '\r']) {
                return true;
            }
        }
    }
    false
}

/// `lower`(已小写)是否含 `.js`/`.mjs` 后接 URL 结束符的子串 —— 外部 JS bundle 引用。结束符:
/// 引号 / query `?` / fragment `#` / 空白 / `>` / 串尾。避免 `.json`/`.jsx`(后接字母)误命中,
/// 容忍 cache-bust query(`app.js?v=123`, chatgpt-codex)。
fn has_js_bundle_url(lower: &str) -> bool {
    for pat in [".mjs", ".js"] {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(pat) {
            let end = from + rel + pat.len();
            from = end;
            match lower[end..].chars().next() {
                None => return true,
                Some(c)
                    if c == '"'
                        || c == '\''
                        || c == '?'
                        || c == '#'
                        || c == '>'
                        || c.is_ascii_whitespace() =>
                {
                    return true
                }
                _ => {}
            }
        }
    }
    false
}

/// SPA 骨架特征: 已知框架挂载点 (root/app/__next/__nuxt/ng-app/reactroot) + 页面挂着 `<script`
/// (bundle / data island)。覆盖主流 React/Vue/Next/Nuxt/Angular; 非标准挂载点的小众 SPA 会漏判
/// (退化当短静态页放行, 拿到的空壳交模型自行判断), 优于把短静态页误升 headless。
fn has_spa_skeleton(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let has_mount = id_attr_matches(&lower, "root")
        || id_attr_matches(&lower, "app")
        || id_attr_matches(&lower, "__next")
        || id_attr_matches(&lower, "__nuxt")
        || lower.contains("ng-app")
        || lower.contains("data-reactroot");
    has_mount && lower.contains("<script")
}

/// `id` 属性值是否命中 `mount`。容忍 `id="x"` / `id='x'` / `id=x` 及**等号周围空格**
/// (`id = "x"`,模板 / 手写 HTML 常见, chatgpt-codex review 第 7 轮)。边界校验:`id` 前一字符
/// 须非 ident (避免 `grid`/`void`/`width` 误命中)、`mount` 后须是引号 / 空白 / `>` / `/`
/// (避免 `rootxxx` 误命中)。入参须已 lowercase。
fn id_attr_matches(lower: &str, mount: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("id") {
        let start = from + pos;
        from = start + 2;
        if start > 0 {
            let p = bytes[start - 1];
            if p.is_ascii_alphanumeric() || p == b'_' || p == b'-' {
                continue;
            }
        }
        let rest = lower[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
        if let Some(after) = rest.strip_prefix(mount) {
            if after.is_empty() || after.starts_with(['"', '\'', ' ', '>', '/']) {
                return true;
            }
        }
    }
    false
}

/// 粗算 HTML 去掉 `<script>`/`<style>` 块 + 所有标签后的可见非空白字符数 (启发式, char-safe,
/// 不求精确 DOM)。用于 [`is_js_shell`]。
fn visible_text_len(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    let mut out = 0usize;
    let mut i = 0usize;
    let n = html.len();
    while i < n {
        if lower[i..].starts_with("<script") {
            match lower[i..].find("</script>") {
                Some(rel) => i += rel + "</script>".len(),
                None => break,
            }
            continue;
        }
        if lower[i..].starts_with("<style") {
            match lower[i..].find("</style>") {
                Some(rel) => i += rel + "</style>".len(),
                None => break,
            }
            continue;
        }
        if html.as_bytes()[i] == b'<' {
            match html[i..].find('>') {
                Some(rel) => i += rel + 1,
                None => break,
            }
            continue;
        }
        // 标签外的可见字符 (char-safe 推进)。
        let ch = html[i..].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            out += 1;
        }
        i += ch.len_utf8();
    }
    out
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

/// HTML 客户端重定向 (meta refresh / JS location) 的目标 URL (MOC-139), resolve 为绝对 URL。
/// curl/reqwest 只跟 HTTP 3xx, 不跟这两类 → 占位页跟随重抓的判据。调用点已 gate "正文极少"。
fn detect_client_redirect(html: &str, base_url: &str) -> Option<String> {
    let target = parse_meta_refresh(html).or_else(|| parse_js_location(html))?;
    resolve_url(&target, base_url)
}

/// 解析 `<meta http-equiv="refresh" content="0; url=TARGET">` 的 TARGET (大小写/引号/空格容忍)。
fn parse_meta_refresh(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<meta") {
        let start = from + rel;
        let end = lower[start..]
            .find('>')
            .map(|e| start + e + 1)
            .unwrap_or(html.len());
        from = end;
        let tag_lower = &lower[start..end];
        if !(tag_lower.contains("http-equiv") && tag_lower.contains("refresh")) {
            continue;
        }
        // **锚定 content 属性值后**再找 url= —— 避免 `data-url=` 等其他属性在 content 前被误匹配
        // (chatgpt-codex review)。ascii lowercase 不改字节长度, lower 偏移可直接用于原 html 取值。
        let Some(cpos) = tag_lower.find("content") else {
            continue;
        };
        let region = start + cpos;
        if let Some(u) = lower[region..end].find("url=") {
            let val = html[region + u + 4..end]
                .trim_start_matches(['"', '\'', ' '])
                .split(['"', '\'', '>', ' '])
                .next()
                .unwrap_or("")
                .trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// 解析 JS 跳转 `window.location.replace('T')` / `location.assign('T')` / `location.href='T'`
/// / `location='T'` 的目标 (取第一个命中)。URL 须以 `http` / `/` 开头 (排除非跳转的 location 读取)。
fn parse_js_location(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    for pat in [
        "location.replace(",
        "location.assign(",
        "location.href",
        ".location=",
    ] {
        let Some(rel) = lower.find(pat) else {
            continue;
        };
        let after = &html[rel + pat.len()..];
        // `location.href` 须是**赋值**(单 `=`)不是**比较**(`==`)—— 排除 `if(location.href=='x')`
        // 这类读取(chatgpt-codex review)。replace(/assign( 是调用、.location= 已含 =, 不受此限。
        if pat == "location.href" {
            let t = after.trim_start();
            if !t.starts_with('=') || t.starts_with("==") {
                continue;
            }
        }
        // pat 后找第一个引号, 取引号内 URL。
        let Some(q) = after.find(['"', '\'']) else {
            continue;
        };
        let quote = after.as_bytes()[q] as char;
        let rest = &after[q + 1..];
        if let Some(e) = rest.find(quote) {
            let url = rest[..e].trim();
            if url.starts_with("http") || url.starts_with('/') {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// 相对 URL → 绝对 (用 `base_url` 解析; 已是绝对则规范化原样返回)。
fn resolve_url(target: &str, base_url: &str) -> Option<String> {
    reqwest::Url::parse(base_url)
        .ok()?
        .join(target)
        .ok()
        .map(|u| u.to_string())
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
    fn parse_meta_refresh_extracts_url() {
        // meta refresh(lcamtuf 型)
        let h = "<html><meta http-equiv='refresh' content='0; url=https://sub.example.com/p/x'><body></body></html>";
        assert_eq!(
            parse_meta_refresh(h).as_deref(),
            Some("https://sub.example.com/p/x")
        );
        // 大写 + 双引号 + 相对路径
        let h2 = "<META HTTP-EQUIV=\"Refresh\" CONTENT=\"5; URL=/relative/path\">";
        assert_eq!(parse_meta_refresh(h2).as_deref(), Some("/relative/path"));
        // data-url= 在 content 前不应被误匹配(chatgpt-codex review): 取 content 内的 url
        assert_eq!(
            parse_meta_refresh(
                "<meta http-equiv='refresh' data-url='junk' content='0;url=https://e.com/real'>"
            )
            .as_deref(),
            Some("https://e.com/real")
        );
        // 非 refresh meta → None
        assert_eq!(parse_meta_refresh("<meta charset='utf-8'><p>hi</p>"), None);
    }

    #[test]
    fn parse_js_location_extracts_url() {
        assert_eq!(
            parse_js_location("<script>window.location.replace('https://e.com/a')</script>")
                .as_deref(),
            Some("https://e.com/a")
        );
        assert_eq!(
            parse_js_location("<script>location.href = \"/path\"</script>").as_deref(),
            Some("/path")
        );
        assert_eq!(
            parse_js_location("<script>document.location='https://e.com/b'</script>").as_deref(),
            Some("https://e.com/b")
        );
        // 读取 location(无引号 URL)→ None, 不误跳
        assert_eq!(
            parse_js_location("<script>var x = location.href;</script>"),
            None
        );
        // location.href == 比较(非赋值)不当跳转目标(chatgpt-codex review)
        assert_eq!(
            parse_js_location("<script>if(location.href=='https://old.com/x'){}</script>"),
            None
        );
    }

    #[test]
    fn detect_client_redirect_resolves() {
        // 相对 URL → 用 base 解析为绝对
        assert_eq!(
            detect_client_redirect(
                "<meta http-equiv='refresh' content='0;url=/p/x'>",
                "https://lcamtuf.coredump.cx/blog/conway/"
            )
            .as_deref(),
            Some("https://lcamtuf.coredump.cx/p/x")
        );
        // 绝对 URL 原样(规范化)
        assert_eq!(
            detect_client_redirect(
                "<meta http-equiv='refresh' content='0;url=https://sub.example.com/p'>",
                "https://lcamtuf.coredump.cx/blog/"
            )
            .as_deref(),
            Some("https://sub.example.com/p")
        );
        // 正常页(无重定向)→ None
        assert_eq!(
            detect_client_redirect("<html><body>正常内容</body></html>", "https://e.com"),
            None
        );
    }

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
            WebFetchBackend::Auto,
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
        assert_eq!(WebFetchBackend::parse("AUTO"), Some(WebFetchBackend::Auto));
        // 关闭 / 未知 → None
        assert_eq!(WebFetchBackend::parse("off"), None);
        assert_eq!(WebFetchBackend::parse("关闭"), None);
        assert_eq!(WebFetchBackend::parse(""), None);
    }

    #[test]
    fn may_use_headless_flag() {
        assert!(WebFetchBackend::Auto.may_use_headless());
        assert!(WebFetchBackend::Headless.may_use_headless());
        assert!(!WebFetchBackend::Curl.may_use_headless());
        assert!(!WebFetchBackend::Wreq.may_use_headless());
    }

    #[test]
    fn origin_of_extracts_scheme_host() {
        assert_eq!(
            origin_of("https://example.com/a/b?q=1#x"),
            Some("https://example.com".to_string())
        );
        assert_eq!(
            origin_of("http://sub.host.org:8080/p"),
            Some("http://sub.host.org:8080".to_string())
        );
        assert_eq!(origin_of("not-a-url"), None);
        assert_eq!(origin_of("https://"), None);
    }

    #[test]
    fn needs_upgrade_signals() {
        let html_ok = format!("<html><body>{}</body></html>", "正文内容很长".repeat(50));
        // 正常 2xx + 有正文 → 不升级
        assert!(!needs_upgrade(&RawFetch {
            status: 200,
            body: html_ok.clone(),
            is_html: true,
            cf_challenge_header: false,
        }));
        // 非 2xx → 升级
        assert!(needs_upgrade(&RawFetch {
            status: 403,
            body: html_ok.clone(),
            is_html: true,
            cf_challenge_header: false,
        }));
        // 202 软拦 (DDG anomaly 等) → 升级 (review M1)
        assert!(needs_upgrade(&RawFetch {
            status: 202,
            body: html_ok.clone(),
            is_html: true,
            cf_challenge_header: false,
        }));
        // CF header → 升级
        assert!(needs_upgrade(&RawFetch {
            status: 200,
            body: html_ok.clone(),
            is_html: true,
            cf_challenge_header: true,
        }));
        // 通用反爬 body (DDG anomaly) → 升级 (review M1)
        assert!(needs_upgrade(&RawFetch {
            status: 200,
            body: "<html><body>bots use duckduckgo too</body></html>".to_string(),
            is_html: true,
            cf_challenge_header: false,
        }));
        // 空 body → 升级
        assert!(needs_upgrade(&RawFetch {
            status: 200,
            body: "   ".to_string(),
            is_html: true,
            cf_challenge_header: false,
        }));
        // 非 HTML (JSON API) 即使短也不当 JS 骨架 → 不升级
        assert!(!needs_upgrade(&RawFetch {
            status: 200,
            body: "{\"k\":1}".to_string(),
            is_html: false,
            cf_challenge_header: false,
        }));
        // 非 HTML 202 (合法 API Accepted) → 不升级 (chatgpt-codex: 保留 API 响应, 不升 headless)
        assert!(!needs_upgrade(&RawFetch {
            status: 202,
            body: "{\"job\":\"queued\"}".to_string(),
            is_html: false,
            cf_challenge_header: false,
        }));
        // 非 HTML 非 2xx → 升级 (连接级问题, 试别的 client)
        assert!(needs_upgrade(&RawFetch {
            status: 500,
            body: "err".to_string(),
            is_html: false,
            cf_challenge_header: false,
        }));
    }

    #[test]
    fn unrecoverable_status_not_escalated() {
        let raw = |status: u16, body: &str| RawFetch {
            status,
            body: body.to_string(),
            is_html: true,
            cf_challenge_header: false,
        };
        // 4xx 除 403/429 → 不可恢复 (含原白名单漏的 405/422/406/409)
        for s in [400, 401, 404, 405, 406, 409, 410, 422, 451] {
            assert!(is_unrecoverable(&raw(s, "err")), "{s} 应不可恢复");
        }
        // 5xx 无挑战信号 → 不可恢复 (真服务器故障, 别让 headless 渲染 500 页当成功)
        for s in [500, 502, 504] {
            assert!(
                is_unrecoverable(&raw(s, "Internal Server Error")),
                "{s} 无挑战应不可恢复"
            );
        }
        // 反爬(403) / 限流(429) / 2xx → 可恢复, 升级
        for s in [200, 202, 403, 429] {
            assert!(!is_unrecoverable(&raw(s, "ok")), "{s} 不该当不可恢复");
        }
        // 5xx 带 CF 挑战信号 (503 challenge) → 可恢复 (headless 可能过挑战)
        assert!(
            !is_unrecoverable(&raw(
                503,
                "<html><head><title>just a moment</title></head><body>challenge-platform</body></html>"
            )),
            "503 带挑战信号不该当不可恢复"
        );
    }

    #[test]
    fn detects_challenge_body() {
        // CF: just a moment 锚在 title
        assert!(is_challenge_body(
            "<html><head><title>Just a moment...</title></head></html>"
        ));
        assert!(is_challenge_body("<script>challenge-platform/x</script>"));
        // 通用反爬 (review M1)
        assert!(is_challenge_body("<body>Unusual traffic detected</body>"));
        assert!(is_challenge_body("bots use duckduckgo too"));
        // 普通页不误判 (review M2: 正文提 just a moment 但不在 title)
        assert!(!is_challenge_body(
            "<html><body>just a moment in history was discussed.</body></html>"
        ));
        assert!(!is_challenge_body("<html><body>normal page</body></html>"));
    }

    #[test]
    fn detects_js_shell() {
        // SPA 空骨架: 挂载点 + 一堆 script, 可见正文极少 → shell
        let shell = "<html><body><div id=\"root\"></div>\
            <script>var a=1;var b=2;</script><script src=\"/bundle.js\"></script></body></html>";
        assert!(is_js_shell(shell), "SPA 空骨架应判为 shell");
        // 单引号挂载点变体也应判 shell (chatgpt-codex: 容忍引号种类)
        assert!(
            is_js_shell(
                "<html><body><div id='root'></div><script src=/b.js></script></body></html>"
            ),
            "单引号挂载点也应判 shell"
        );
        // 等号周围空格变体也应判 shell (chatgpt-codex 第 7 轮: 容忍 id = "root")
        assert!(
            is_js_shell(
                "<html><body><div id = \"root\"></div><script src=/b.js></script></body></html>"
            ),
            "空格挂载点变体也应判 shell"
        );
        // 边界: grid / rootless 等不应被误当挂载点 (id_attr_matches 的前后边界校验)
        assert!(
            !is_js_shell(
                "<html><body><div class=\"grid rootless\">x</div><script>a()</script></body></html>"
            ),
            "grid/rootless 不应误判为挂载点"
        );
        // 有真实长正文 → 不是 shell
        let real = format!(
            "<html><body><article>{}</article></body></html>",
            "这是一篇有实际内容的文章正文。".repeat(40)
        );
        assert!(!is_js_shell(&real), "有正文不应判为 shell");
        // 短静态页 (无 SPA 挂载点) 即使可见文本少也不判 shell (review: 别误升 headless)
        assert!(
            !is_js_shell("<html><body><p>OK</p></body></html>"),
            "短静态页(无挂载点)不应判为 shell"
        );
        // 有挂载点但无 script → 不算 SPA shell (has_script 必要)
        assert!(
            !is_js_shell("<html><body><div id=\"root\"></div></body></html>"),
            "无 script 的挂载点不算 SPA shell"
        );

        // MOC-183: 非标准 mount 但有 bundle 脚本(实测 mouseless.click / doscienceto.it 型,
        // 原 mount 白名单漏判, headless 实测能抓到内容)。
        assert!(
            is_js_shell(
                "<html><body><div id=\"app-container\"></div>\
                 <script type=\"module\" src=\"/assets/index-abc.js\"></script></body></html>"
            ),
            "非标准 mount + ESM module 脚本应判 shell"
        );
        assert!(
            is_js_shell(
                "<html><body><main id=\"wrap\"></main>\
                 <script src=\"/static/bundle.abc.js\"></script></body></html>"
            ),
            "非标准 mount + 外部 .js bundle 应判 shell"
        );
        // 边界(不误升): 静态短页 + 纯 inline script(无 module / 无 .js bundle)→ 不判 shell。
        assert!(
            !is_js_shell("<html><body><p>Status: OK</p><script>track();</script></body></html>"),
            "静态短页 + inline script(无 bundle)不应误升"
        );
        // devin review: inline 单引号 `type='module'`(无 .js/.mjs)也应判 shell。
        assert!(
            is_js_shell(
                "<html><body><div id=\"x\"></div>\
                 <script type='module'>import {a} from '/a'; a.mount('#x')</script></body></html>"
            ),
            "单引号 type='module' 应判 shell"
        );
        // chatgpt-codex review: cache-bust 的 `.js?v=123` bundle 也应判 shell(.js 后跟 ?)。
        assert!(
            is_js_shell(
                "<html><body><main id=\"wrap\"></main>\
                 <script src=\"/assets/app.js?v=123\"></script></body></html>"
            ),
            "cache-bust .js?v= bundle 应判 shell"
        );
        // 边界: `.json` / `.jsx`(后接字母)不应被当 .js bundle 误命中。
        assert!(
            !is_js_shell(
                "<html><body><p>x</p><script>fetch(\"/data.json\")</script></body></html>"
            ),
            ".json 不应误命中 .js bundle"
        );
        // chatgpt-codex review 第 3 轮: 等号周围空格 `type = "module"`(对齐 id_attr_matches 容忍)。
        assert!(
            is_js_shell(
                "<html><body><div id=\"q\"></div>\
                 <script type = \"module\">import {a} from '/a'; a.mount('#q')</script></body></html>"
            ),
            "等号空格 type = \"module\" 应判 shell"
        );
        // 边界: `mimetype`(type 前是字母)不应误命中 type=module。
        assert!(
            !is_js_shell(
                "<html><body><p>x</p><script>var mimetype=moduleX;</script></body></html>"
            ),
            "mimetype 不应误命中 type=module"
        );
        // 已知 trade-off(MOC-183): 静态短页若引外部 `.js`(如 analytics)会被判 shell → auto 升
        // headless。**chatgpt-codex review 后已加兜底**: headless 失败时 web_fetch_auto 回退升级
        // 链最后一个非空 body(见 last_usable), 不再把 curl 成功变 Auto error。误升仅多花一次
        // headless(成功时结果不差), 这类页 MOC-152 证明罕见。
    }

    #[test]
    fn worth_fallback_only_nonstandard_mount_heuristic() {
        let mk = |status: u16, cf: bool, body: &str| RawFetch {
            status,
            body: body.to_string(),
            is_html: true,
            cf_challenge_header: cf,
        };
        // 非标准 mount + bundle(不确定启发式, 可能误判有内容短静态页) → 留作回退。
        let nonstd = "<html><body><main id=\"x\"></main>\
            <script type=\"module\" src=\"/a.js\"></script></body></html>";
        assert!(worth_fallback(&mk(200, false, nonstd)));
        // 标准框架 mount(确认 app shell)→ **不留**: headless 失败应保留 error, 别回退未渲染空壳
        // 掩盖 headless 依赖故障(chatgpt-codex review 第 2 轮)。
        assert!(
            !worth_fallback(&mk(
                200,
                false,
                "<html><body><div id=\"root\"></div>\
                 <script src=\"/app.js\"></script></body></html>"
            )),
            "标准 mount 确认 shell 不应回退(保留 headless error)"
        );
        // CF 挑战页(非标准 mount + 结构像 shell, 但 is_challenge_body 命中)→ 不留。
        assert!(
            !worth_fallback(&mk(
                200,
                false,
                "<html><head><title>just a moment</title></head><body><div id=\"cf-wrap\"></div>\
                 <script type=\"module\" src=\"/cf.js\"></script>challenge-platform</body></html>"
            )),
            "CF 挑战页不应回退"
        );
        assert!(!worth_fallback(&mk(200, true, nonstd)), "cf header 不应留");
        assert!(!worth_fallback(&mk(200, false, "   ")), "空 body 不应留");
        assert!(!worth_fallback(&mk(202, false, nonstd)), "非-200 不应留");
        // 内容页(vtext 够 → 非 shell)即便带 module 也不留。
        let content = format!(
            "<html><body><article>{}</article><script type=\"module\"></script></body></html>",
            "正文内容".repeat(60)
        );
        assert!(!worth_fallback(&mk(200, false, &content)), "内容页不应留");
    }

    /// 端到端真机 (网络 + headless): Auto 档抓 DDG (curl/wreq 必被 202 反爬, 应自动升到 headless
    /// 拿到结果)。手动 `cargo test -p codex-app-transfer-http --ignored live_auto` 跑; CI 不跑。
    #[tokio::test]
    #[ignore = "real network + headless Chrome"]
    async fn live_auto_escalates_to_headless() {
        let r = web_fetch(
            WebFetchBackend::Auto,
            "https://html.duckduckgo.com/html/?q=rust",
        )
        .await;
        eprintln!("auto fetch len: {:?}", r.as_ref().map(|o| o.content.len()));
        let outcome = r.expect("auto 应升到 headless 并成功");
        eprintln!(
            "final_tier={:?} trail={:?}",
            outcome.final_tier, outcome.trail
        );
        assert!(
            !outcome.content.trim().is_empty(),
            "expected non-empty content"
        );
    }

    /// 端到端真机 (MOC-139): 抓客户端重定向页 (lcamtuf 绕 Substack feud 的 meta refresh + JS
    /// location 跳转页), 应跟随到 substack 目标拿到真内容。手动 `--ignored live_client_redirect`。
    #[tokio::test]
    #[ignore = "real network"]
    async fn live_client_redirect_follows() {
        let o = web_fetch(
            WebFetchBackend::Auto,
            "https://lcamtuf.coredump.cx/blog/conway/",
        )
        .await
        .expect("应成功");
        eprintln!("trail={:?}", o.trail);
        eprintln!(
            "content len={} 头200={}",
            o.content.len(),
            &o.content[..o.content.len().min(200)]
        );
        assert!(
            o.trail.iter().any(|t| t.contains("client-redirect")),
            "应跟随客户端重定向"
        );
        assert!(o.content.len() > 1000, "应拿到 substack 真内容(非占位页)");
    }
}
