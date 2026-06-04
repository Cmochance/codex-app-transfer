//! web_search (MOC-12 P0): DuckDuckGo HTML SSR 搜索, 给 cat-webfetch MCP 提供 `web_search` 工具。
//!
//! ## 为什么需要
//! Codex.app 每轮发 OpenAI server-side `web_search`, 但 chat-completions provider
//! (MiniMax / DeepSeek / GLM / Kimi 非搜索套餐…)上游不支持 → adapter `convert_web_search_tool`
//! drop。模型失去原生搜索 → 退化到自己抓搜索引擎页 / 猜 URL(真机实测 12 抓 2 成功, 17%,
//! 见 MOC-12)。本模块把"唯一可靠路径 DuckDuckGo"固化成一个工具, 模型直接拿结构化结果列表。
//!
//! ## 为什么固定 headless
//! DDG html 版(`html.duckduckgo.com/html/`)是 SSR + 经典 `.result__a` 结构, 但前置 JS
//! challenge: 裸 reqwest / wreq Chrome120 指纹(含完整浏览器 header / POST / lite 版)**全部
//! 被 202 anomaly 拦**(spike 实测 6 变体全灭) —— 必须 headless 真跑 JS 才放行。故 web_search
//! 内部固定走 [`crate::headless`], **不跟随** web_fetch 的 curl/wreq/headless 档位设置。
//!
//! ## 解析
//! `.result__a`(title + href)/ `.result__snippet`(摘要)。href 形如
//! `//duckduckgo.com/l/?uddg=<urlencoded 真实 URL>&rut=...` → 解码 `uddg` 参数拿真实落地 URL。
//! DDG 把广告也塞进 `.result__a`(解码后是 `duckduckgo.com/y.js?ad_provider=…` 跳转)→ 过滤。
//!
//! 上游参考: `duckduckgo_search`(py, `.result__a`/`.result__snippet`/uddg 解码模式)。
//! anomaly 页检测 / 广告过滤按本项目 spike 实测自行实现。

use dom_query::Document;
use thiserror::Error;

/// 单条搜索结果(给模型: 据此再 `web_fetch(url)` 取正文 —— 两段式)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Error)]
pub enum WebSearchError {
    /// DDG 反爬挑战页(202 anomaly): headless 也被拦(出口 IP 信誉 / 频率)。
    #[error("DuckDuckGo 反爬拦截(anomaly 验证页)—— 出口 IP 可能被风控或请求过频, 请稍后重试")]
    Blocked,
    /// 抓取失败(headless 启动 / 导航 / 超时 / 无 Chrome)。透传 [`crate::headless::HeadlessError`]
    /// 结构(与 sibling `WebFetchError::Headless` 一致), 保留具体失败类别便于诊断。
    #[error("搜索页抓取失败: {0}")]
    Fetch(#[from] crate::headless::HeadlessError),
    /// 抓到页但无有效结果(查询无果 / DDG 结构变化)。
    #[error("无搜索结果(查询无果或页面结构变化)")]
    NoResults,
}

/// 默认返回结果上限。
pub const DEFAULT_MAX_RESULTS: usize = 8;
/// 结果上限硬顶(防模型传超大值撑爆 context)。
const MAX_RESULTS_CAP: usize = 20;

/// 搜索 `query`, 返回结构化结果列表。固定走 DDG html 版 + headless(见模块注释)。
///
/// `max_results` 截到 `[1, 20]`(`0` 视作 1, `>20` 截到 20)—— 防模型传超大值撑爆 context。
pub async fn web_search(
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, WebSearchError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(WebSearchError::NoResults);
    }
    let max = max_results.clamp(1, MAX_RESULTS_CAP);
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(q)
    );
    let html = crate::headless::fetch_rendered_html(&url).await?;
    let results = parse_ddg_html(&html, max);
    if results.is_empty() {
        // 解析出 0 条结果元素: 区分"反爬拦截"(202 anomaly 页, 该退避重试)与"真无结果 / 结构
        // 变化"(该换查询)—— 两者 remediation 相反, 必须分开报(避免把 block 误报成 NoResults
        // 让模型去改查询而非退避; silent-failure-hunter MOC-12 review)。判定基于"无结果元素 +
        // anomaly 文案", 不再 substring 匹配 result__a(它会被 inline CSS/JS 里的同名 token 干扰)。
        return Err(if has_anomaly_markers(&html) {
            WebSearchError::Blocked
        } else {
            WebSearchError::NoResults
        });
    }
    Ok(results)
}

/// DDG 反爬挑战页文案标记(仅在解析出 0 条结果时调用, 用于区分"被拦"与"真无结果")。
/// 不再 substring 匹配 result__a —— 调用方已按解析出的**结果元素数**判定有无结果, 这里只看
/// anomaly 文案, 避免 inline CSS/JS 里的 `result__a` token 把 block 页误判成"无结果"。
fn has_anomaly_markers(html: &str) -> bool {
    let lower = html.to_lowercase();
    lower.contains("anomaly")
        || lower.contains("if this error persists")
        || lower.contains("bots use duckduckgo")
}

/// 解析 DDG html 版结果: 遍历 `.result` 容器, 取 `.result__a`(title+href)+ `.result__snippet`,
/// uddg 解码真实 URL, 过滤广告 / 无效条目, 取前 `max` 条。
fn parse_ddg_html(html: &str, max: usize) -> Vec<SearchResult> {
    let doc = Document::from(html);
    let mut out = Vec::new();
    for node in doc.select("div.result").iter() {
        let a = node.select("a.result__a");
        if a.length() == 0 {
            continue; // .result--no-result / 分隔容器等
        }
        let href = a.attr("href").map(|s| s.to_string()).unwrap_or_default();
        let url = match decode_href(&href) {
            Some(u) => u,
            None => {
                // 空 / 相对 / 编码坏: 解不出 URL。非空 href 解码失败 = 真实结果被丢(疑 DDG 结构
                // 变化), 留 stderr 痕(stdout 留给 MCP 帧)便于发现; 空 href 不告警(无信息量)。
                if !href.is_empty() {
                    eprintln!("[web_search] 跳过无法解码的结果 href: {href}");
                }
                continue;
            }
        };
        if is_ad(&url) {
            continue; // 广告(y.js / aclick): 预期过滤, 静默跳过(与"解码失败"区分开)。
        }
        let title = collapse_ws(a.text().as_ref());
        if title.is_empty() {
            continue;
        }
        let snippet = collapse_ws(node.select("a.result__snippet").text().as_ref());
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
        if out.len() >= max {
            break;
        }
    }
    out
}

/// DDG 链接解码: href=`//duckduckgo.com/l/?uddg=<enc>&rut=...` → 解码 `uddg` 真实 URL;
/// 直接 http(s) href 原样返回; 空 / 相对 / 编码损坏 → `None`。
/// **不判广告** —— 广告过滤交给 [`is_ad`](在 decoded URL 上判), 以便调用方区分"广告预期跳过"
/// 与"解码失败异常"(后者留痕), 二者不再共用一条静默 `continue`(silent-failure-hunter review)。
fn decode_href(href: &str) -> Option<String> {
    if let Some(rest) = href.split("uddg=").nth(1) {
        let enc = rest.split('&').next().unwrap_or("");
        Some(urlencoding::decode(enc).ok()?.into_owned())
    } else if href.starts_with("http://") || href.starts_with("https://") {
        Some(href.to_string())
    } else {
        None
    }
}

/// 广告链接判定(DDG 把广告塞进 `.result__a`, 解码后是 y.js / bing aclick 跳转)。
fn is_ad(url: &str) -> bool {
    url.contains("duckduckgo.com/y.js")
        || url.contains("ad_provider=")
        || url.contains("ad_domain=")
        || url.contains(".bing.com/aclick")
}

/// 折叠连续空白(含换行)为单空格并 trim —— DDG 的 title/snippet 常含换行缩进。
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 最小 DDG html 版结构 fixture: 1 广告(y.js)+ 2 真实结果 + 1 无结果容器。
    const FIXTURE: &str = r##"<!DOCTYPE html><html><body>
      <div class="result result--ad results_links_deep">
        <div class="links_main">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_provider%3Dbingv7aa%26ad_domain%3Dexample.com&amp;rut=xxx">Sponsored AskGPT</a>
          <a class="result__snippet" href="//x">广告摘要</a>
        </div>
      </div>
      <div class="result results_links results_links_deep web-result">
        <div class="links_main">
          <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fopenai.com%2Fchatgpt%2Fpricing&amp;rut=aaa">OpenAI ChatGPT Pricing</a></h2>
          <a class="result__snippet" href="//y">ChatGPT Plus
            is $20 per month.</a>
        </div>
      </div>
      <div class="result results_links web-result">
        <div class="links_main">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fhelp.openai.com%2Fen%2Farticles%2F123&amp;rut=bbb">Help Center</a>
          <a class="result__snippet" href="//z">How billing works</a>
        </div>
      </div>
      <div class="result result--no-result">No results.</div>
    </body></html>"##;

    #[test]
    fn parses_real_results_and_filters_ads() {
        let r = parse_ddg_html(FIXTURE, 10);
        // 广告(y.js)被过滤, 无结果容器跳过 → 只剩 2 条真实结果。
        assert_eq!(r.len(), 2, "got: {r:?}");
        assert_eq!(r[0].title, "OpenAI ChatGPT Pricing");
        assert_eq!(r[0].url, "https://openai.com/chatgpt/pricing");
        assert_eq!(r[0].snippet, "ChatGPT Plus is $20 per month."); // 换行折叠
        assert_eq!(r[1].url, "https://help.openai.com/en/articles/123");
    }

    #[test]
    fn respects_max_results() {
        let r = parse_ddg_html(FIXTURE, 1);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://openai.com/chatgpt/pricing");
    }

    #[test]
    fn decodes_uddg_and_passthrough() {
        assert_eq!(
            decode_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=z"),
            Some("https://example.com/a?b=1".to_string())
        );
        // 直链原样
        assert_eq!(
            decode_href("https://example.org/x"),
            Some("https://example.org/x".to_string())
        );
        // 空 / 相对 / 未知 → None(解码失败, 由调用方留痕)
        assert_eq!(decode_href("/relative"), None);
        assert_eq!(decode_href(""), None);
    }

    #[test]
    fn filters_ad_urls() {
        assert!(is_ad("https://duckduckgo.com/y.js?ad_provider=bingv7aa"));
        assert!(is_ad("https://www.bing.com/aclick?ld=abc"));
        assert!(!is_ad("https://openai.com/pricing"));
        // decode_href 只解码(不自过滤), 解出广告 URL → 由 is_ad 判定为广告。
        let ad = decode_href(
            "//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_provider%3Dx&rut=z",
        );
        assert_eq!(
            ad.as_deref(),
            Some("https://duckduckgo.com/y.js?ad_provider=x")
        );
        assert!(is_ad(&ad.unwrap()));
    }

    #[test]
    fn detects_anomaly_markers() {
        assert!(has_anomaly_markers(
            "<html><body>If this error persists, please let us know. anomaly</body></html>"
        ));
        // 普通结果页文案(无 anomaly 词) → 不算被拦
        assert!(!has_anomaly_markers(
            r#"<a class="result__a" href="x">t</a><a class="result__snippet">s</a>"#
        ));
    }

    /// 端到端真机(需网络 + headless Chrome): 手动
    /// `cargo test -p codex-app-transfer-http --ignored live_ddg` 跑。CI 不跑(无 headless)。
    /// spike 已证 fetch_rendered_html 能过 DDG 拿 11 条 .result__a; 本测验证全链路解码 / 过滤。
    #[tokio::test]
    #[ignore = "real network + headless Chrome"]
    async fn live_ddg_search() {
        let r = web_search("openai chatgpt plus pricing", 5).await;
        eprintln!("live web_search: {r:#?}");
        let results = r.expect("web_search should succeed on live network");
        assert!(!results.is_empty(), "expected >=1 result");
        assert!(
            results[0].url.starts_with("http"),
            "url must be absolute (uddg decoded): {}",
            results[0].url
        );
        assert!(!results[0].title.is_empty());
        // 广告应被过滤: 不应出现 y.js / aclick 跳转。
        assert!(results
            .iter()
            .all(|r| !r.url.contains("duckduckgo.com/y.js")));
    }
}
