//! web_search (MOC-12 P0): DuckDuckGo HTML SSR 搜索, 给 cat-webfetch 提供 `web_search` 工具。
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

/// 默认返回结果上限(MOC-190: 8→15, 8 条太少、模型常因信息不足反复换词搜;每条只是标题+URL+短摘要、
/// 15 条不占多少 context)。
pub const DEFAULT_MAX_RESULTS: usize = 15;
/// 结果上限硬顶(防模型传超大值撑爆 context;MOC-190: 20→30)。
const MAX_RESULTS_CAP: usize = 30;

/// 每页结果基准条数(Bing first= 翻页步长 + DDG 单页约值)。
const PAGE_SIZE: usize = 10;

/// 搜索 `query` 第 `page` 页, 返回结构化结果列表。固定走 headless(见模块注释)。
///
/// - `max_results` 截到 `[1, 30]`(`0` 视作 1, `>30` 截到 30)—— 防模型传超大值撑爆 context。
/// - `page` 1-indexed(`0` 视作 1)。**page 1**: DDG + Bing 并行 merge(覆盖最全);**page ≥ 2**:
///   仅 Bing `first=` 深页 —— DDG html GET `s=` 实测**不翻页**(返回同页, 需 POST+vqd token),
///   故深页交给 Bing(MOC-215 step2 分页机制 headless 实测)。让模型"再搜"取**新结果**而非重复。
pub async fn web_search(
    query: &str,
    max_results: usize,
    page: usize,
) -> Result<Vec<SearchResult>, WebSearchError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(WebSearchError::NoResults);
    }
    let max = max_results.clamp(1, MAX_RESULTS_CAP);
    let page = page.max(1);
    // MOC-215 step1+2: DDG + Bing **并行抓取后合并去重**(此前 Bing 仅在 DDG 0 结果时兜底)。两家
    // 索引覆盖不同, 合并提升全面性;并行(非串行)故 wall-time ≈ max(单家)。翻页(step2):Bing GET
    // `first=` 实测有效, DDG GET `s=` 实测不翻页 → DDG 仅 page 1 贡献、深页只 Bing。no_wait: DDG
    // 202 anomaly 硬拦截不自动解(白等 15s), 靠 anomaly marker 自判 Blocked。任一家抓取失败非致命。
    let want_ddg = page == 1;
    let ddg_url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(q)
    );
    let bing_first = (page - 1) * PAGE_SIZE + 1;
    let bing_url = format!(
        "https://www.bing.com/search?q={}&first={}",
        urlencoding::encode(q),
        bing_first
    );
    let (ddg_fetch, bing_fetch) = tokio::join!(
        async {
            if want_ddg {
                Some(crate::headless::fetch_rendered_html_no_wait(&ddg_url).await)
            } else {
                None // 深页 DDG GET 不翻页, 不抓(省一次 headless)
            }
        },
        crate::headless::fetch_rendered_html_no_wait(&bing_url),
    );
    let mut ddg_results = Vec::new();
    let mut ddg_anomaly = false;
    if let Some(ref ddg_r) = ddg_fetch {
        match ddg_r {
            // 各家先取到硬顶, 合并去重后再截 max(避免单家前 max 把另一家挤掉)。
            Ok(html) => {
                ddg_results = parse_ddg_html(html, MAX_RESULTS_CAP);
                if ddg_results.is_empty() {
                    ddg_anomaly = has_anomaly_markers(html);
                }
            }
            Err(e) => eprintln!("[web_search] DDG 抓取失败: {e}"),
        }
    }
    let bing_results = match &bing_fetch {
        Ok(html) => parse_bing_html(html, MAX_RESULTS_CAP),
        Err(e) => {
            eprintln!("[web_search] Bing 抓取失败: {e}");
            Vec::new()
        }
    };
    let merged = merge_dedup(ddg_results, bing_results, max);
    if !merged.is_empty() {
        return Ok(merged);
    }
    // 无合并结果: DDG anomaly → Blocked(退避重试);抓取层失败(headless 故障)→ 透传 Fetch 错
    // (? 语义);否则 NoResults(该换查询)。block/NoResults remediation 相反, 必须分开报
    // (silent-failure-hunter MOC-12 review)。
    if ddg_anomaly {
        return Err(WebSearchError::Blocked);
    }
    // page>1 时 ddg_fetch=None(没抓 DDG), 只看 Bing;page 1 时 DDG 也没成功(None 不会出现,
    // 但 Some(Err))→ Bing 也失败才透传(DDG Ok-but-empty + Bing Ok-empty 落 NoResults)。
    if !matches!(&ddg_fetch, Some(Ok(_))) {
        if let Err(e) = bing_fetch {
            return Err(e.into());
        }
    }
    Err(WebSearchError::NoResults)
}

/// 归一化 URL 做去重: 去尾斜杠 + 去 fragment + 小写(host 大小写不敏感;path 小写是务实过宽
/// 匹配, 极少 case-sensitive path 碰撞、可接受)。**保留 query**(不同 query = 不同页, 不能并)。
fn norm_url(u: &str) -> String {
    let no_frag = u.trim().split('#').next().unwrap_or("");
    no_frag.trim_end_matches('/').to_ascii_lowercase()
}

/// 合并 DDG + Bing 结果: **轮流交错取**(DDG, Bing, DDG, Bing…), 按归一化 URL 去重, 截到 max。
/// 交错而非拼接 → 两家各自靠前的高质结果都能进前列(单家排第 max+1 的不会被另一家前 max 挤掉),
/// 覆盖面最大化。两家其一空(抓取失败/被拦)时退化为另一家结果。
fn merge_dedup(ddg: Vec<SearchResult>, bing: Vec<SearchResult>, max: usize) -> Vec<SearchResult> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(max);
    let mut a = ddg.into_iter();
    let mut b = bing.into_iter();
    let (mut a_done, mut b_done) = (false, false);
    while out.len() < max && !(a_done && b_done) {
        match a.next() {
            Some(r) => {
                if seen.insert(norm_url(&r.url)) {
                    out.push(r);
                }
            }
            None => a_done = true,
        }
        if out.len() >= max {
            break;
        }
        match b.next() {
            Some(r) => {
                if seen.insert(norm_url(&r.url)) {
                    out.push(r);
                }
            }
            None => b_done = true,
        }
    }
    out
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

/// 解析 Bing 搜索结果 (⑤ MOC-186 后备引擎): 遍历 `li.b_algo` organic 容器, 取 `h2 a` 的 title +
/// href (容器内首个 `a` 常是缩略图, 故锚定 `h2 a`), 经 [`decode_bing_href`] 把 `ck/a` 跳转解成
/// 真实 URL, `.b_caption` 作摘要。spike 实测 headless 抓 Bing 拿 10 条干净结果 (无反爬拦截), 作
/// DDG 被拦/无果时的后备 (见 [`web_search`])。
fn parse_bing_html(html: &str, max: usize) -> Vec<SearchResult> {
    let doc = Document::from(html);
    let mut out = Vec::new();
    for node in doc.select("li.b_algo").iter() {
        let a = node.select("h2 a");
        if a.length() == 0 {
            continue; // 非 organic 块 (图片/视频/问答卡) 无 h2 a, 跳过。
        }
        let href = a.attr("href").map(|s| s.to_string()).unwrap_or_default();
        let url = match decode_bing_href(&href) {
            Some(u) => u,
            None => {
                // 容器有 h2 a、href 非空却解不出 = 疑 Bing ck/a 编码变化, 真实结果被丢 → 留 stderr 痕
                // (对齐 parse_ddg_html), 便于发现解码器过期; 空 href 不告警(图片/特殊块, 无信息量)。
                if !href.is_empty() {
                    eprintln!("[web_search] 跳过无法解码的 Bing href: {href}");
                }
                continue;
            }
        };
        let title = collapse_ws(a.text().as_ref());
        if title.is_empty() {
            continue;
        }
        let snippet = collapse_ws(node.select(".b_caption").text().as_ref());
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

/// 解 Bing organic 链接 (⑤ MOC-186)。Bing 的 `h2 a` href 多为
/// `bing.com/ck/a?...&u=a1<base64url>&ntb=1` 跳转 (非直链) —— 取 `u` 参数、去 `a1` 前缀、base64url
/// 解码拿真实落地 URL。少数已是直链则原样返回; 非 http / 解码失败 / 解出非 http (图片/视频等特殊块)
/// → `None` (调用方跳过)。
fn decode_bing_href(href: &str) -> Option<String> {
    if !(href.starts_with("http://") || href.starts_with("https://")) {
        return None;
    }
    if !href.contains("/ck/a") {
        return Some(href.to_string()); // 已是直链。
    }
    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
    // 按 query 参数边界取 `u`(不能用字面 split("u=") —— 会被早于真 u 参数、含 "u=" 子串的 key
    // 如 `&menu=1` 错切到错误段)。从 `?` 后按 `&` 拆 kv, 找 key 恰为 `u` 的值。
    let u = href
        .split('?')
        .nth(1)?
        .split('&')
        .find_map(|kv| kv.strip_prefix("u="))?;
    let b64 = u.strip_prefix("a1").unwrap_or(u);
    let decoded = String::from_utf8(BASE64_URL_SAFE_NO_PAD.decode(b64).ok()?).ok()?;
    (decoded.starts_with("http://") || decoded.starts_with("https://")).then_some(decoded)
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

    fn sr(url: &str) -> SearchResult {
        SearchResult {
            title: url.to_owned(),
            url: url.to_owned(),
            snippet: String::new(),
        }
    }

    #[test]
    fn merge_dedup_interleaves_and_dedupes() {
        // ddg[0] 与 bing[0] 是同 URL(差尾斜杠)→ 去重;交错取剩余。
        let ddg = vec![sr("https://a.com/"), sr("https://b.com")];
        let bing = vec![sr("https://a.com"), sr("https://c.com")];
        let m = merge_dedup(ddg, bing, 10);
        let urls: Vec<&str> = m.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(m.len(), 3, "got {urls:?}");
        // a.com 去重后只一条
        assert_eq!(
            m.iter().filter(|r| norm_url(&r.url) == "https://a.com").count(),
            1
        );
        assert!(urls.contains(&"https://b.com"));
        assert!(urls.contains(&"https://c.com"));
    }

    #[test]
    fn merge_dedup_truncates_to_max_and_handles_empty_engine() {
        let ddg = vec![sr("https://1"), sr("https://2"), sr("https://3")];
        // Bing 空(抓取失败/被拦)→ 退化为 ddg, 截到 max=2。
        let m = merge_dedup(ddg, vec![], 2);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].url, "https://1");
        assert_eq!(m[1].url, "https://2");
    }

    #[test]
    fn norm_url_strips_trailing_slash_and_fragment() {
        assert_eq!(norm_url("https://X.com/Path/#sec"), "https://x.com/path");
        assert_eq!(norm_url("https://x.com"), "https://x.com");
        // query 保留(不同 query 不能并)
        assert_eq!(norm_url("https://x.com/?q=1"), "https://x.com/?q=1");
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

    // 最小 Bing 结构 (⑤): 1 ck/a 跳转结果 + 1 直链结果 + 1 图片块(无 h2 a, 应跳过)。
    const BING_FIXTURE: &str = r##"<html><body><ol id="b_results">
      <li class="b_algo"><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;u=a1aHR0cHM6Ly9leGFtcGxlLmNvbS9h&amp;ntb=1">Example Result</a></h2>
        <div class="b_caption"><p>An example snippet.</p></div></li>
      <li class="b_algo"><h2><a href="https://direct.example.org/page">Direct Link</a></h2>
        <div class="b_caption"><p>Direct snippet.</p></div></li>
      <li class="b_algo"><a class="thumb" href="/images/search?view=x">img</a></li>
    </ol></body></html>"##;

    #[test]
    fn parses_bing_and_decodes_ck_redirect() {
        let r = parse_bing_html(BING_FIXTURE, 10);
        // ck/a 跳转 + 直链各 1 条; 图片块(无 h2 a)跳过。
        assert_eq!(r.len(), 2, "got: {r:?}");
        assert_eq!(r[0].url, "https://example.com/a"); // ck/a base64url 解码
        assert_eq!(r[0].title, "Example Result");
        assert_eq!(r[0].snippet, "An example snippet.");
        assert_eq!(r[1].url, "https://direct.example.org/page"); // 直链原样
    }

    #[test]
    fn decode_bing_href_variants() {
        // ck/a 跳转 → base64url 解码
        assert_eq!(
            decode_bing_href("https://www.bing.com/ck/a?!&&u=a1aHR0cHM6Ly9leGFtcGxlLmNvbS9h&ntb=1"),
            Some("https://example.com/a".to_string())
        );
        // 直链原样
        assert_eq!(
            decode_bing_href("https://direct.org/x"),
            Some("https://direct.org/x".to_string())
        );
        // 相对/非 http → None
        assert_eq!(decode_bing_href("/images/search"), None);
        // 早于真 u 参数、含 "u=" 子串的 key(menu=1)不被错切 —— 按 query 边界严格匹配 u。
        assert_eq!(
            decode_bing_href(
                "https://www.bing.com/ck/a?menu=1&u=a1aHR0cHM6Ly9leGFtcGxlLmNvbS9h&ntb=1"
            ),
            Some("https://example.com/a".to_string())
        );
    }

    /// 端到端真机(需网络 + headless Chrome): 手动
    /// `cargo test -p codex-app-transfer-http --ignored live_ddg` 跑。CI 不跑(无 headless)。
    /// spike 已证 fetch_rendered_html 能过 DDG 拿 11 条 .result__a; 本测验证全链路解码 / 过滤。
    #[tokio::test]
    #[ignore = "real network + headless Chrome"]
    async fn live_ddg_search() {
        let r = web_search("openai chatgpt plus pricing", 5, 1).await;
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

    /// ⑤ live (MOC-186): 验证后备引擎 Bing 的 [`parse_bing_html`] 真机解析正确。手动:
    /// `cargo test -p codex-app-transfer-http live_bing_parse -- --ignored --nocapture`。
    /// spike 三引擎对比结论: Bing `li.b_algo` 拿 10 条干净直链结果 (Brave 结果掺图片块 + networkIdle
    /// 超时, Startpage selector 0 命中) → 选 Bing 作 DDG 后备。
    #[tokio::test]
    #[ignore = "real network + headless Chrome"]
    async fn live_bing_parse() {
        let url = format!(
            "https://www.bing.com/search?q={}",
            urlencoding::encode("openai chatgpt plus pricing")
        );
        let html = crate::headless::fetch_rendered_html(&url)
            .await
            .expect("fetch bing");
        let results = parse_bing_html(&html, 8);
        eprintln!("bing parsed {} results:", results.len());
        for r in &results {
            let snip: String = r.snippet.chars().take(100).collect();
            eprintln!("  title={}\n    url={}\n    snip={snip}", r.title, r.url);
        }
        assert!(!results.is_empty(), "expected bing organic results");
        assert!(
            results.iter().all(|r| r.url.starts_with("http")),
            "all urls must be absolute direct links"
        );
        assert!(
            results.iter().all(|r| !r.url.contains("bing.com/ck/a")),
            "ck/a redirects must be decoded to real landing URLs"
        );
    }
}
