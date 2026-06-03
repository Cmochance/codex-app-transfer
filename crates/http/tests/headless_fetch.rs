//! 真机集成测试: headless Chromium 真跑 JS, 验证拿到渲染后 DOM (非空骨架)。
//!
//! 默认 `#[ignore]` (需 chrome 二进制 — 系统已装或触发按需下载, CI 无浏览器会挂)。
//! 运行: `cargo test -p codex-app-transfer-http --test headless_fetch -- --include-ignored --nocapture`
//!
//! 用 `data:` URL 自包含, 不依赖外部网络 (只需 chrome 二进制); 初始 HTML 骨架里
//! 目标节点为空, JS 运行后才填入标记文本 → 渲染后 HTML 含标记即证明 "真跑了 JS、
//! 拿的是 post-render DOM"。MOC-143 "真 React 站 vs reqwest 空骨架" 对比见真机验收记录。

use codex_app_transfer_http::{fetch_rendered_html, HeadlessBrowser};

/// 核心验收: JS 注入的内容必须出现在渲染后 HTML 里。
#[tokio::test]
#[ignore = "需要 chrome 二进制 (系统已装或触发按需下载)"]
async fn renders_js_injected_content() {
    // 初始骨架: #app 为空; JS 运行后填入标记文本 (= SPA 行为的最小复现)。
    let url = "data:text/html,<html><body><div id=app></div>\
               <script>document.getElementById('app').innerHTML='RENDERED_BY_JS_OK'</script>\
               </body></html>";
    let html = fetch_rendered_html(url).await.expect("headless fetch");
    eprintln!("rendered html len = {}", html.len());
    assert!(
        html.contains("RENDERED_BY_JS_OK"),
        "渲染后 HTML 未含 JS 注入内容 — 没拿到 post-render DOM:\n{html}"
    );
}

/// 复用验证: 同一实例连抓两次都成功 (浏览器进程复用, 不每次冷启动)。
#[tokio::test]
#[ignore = "需要 chrome 二进制"]
async fn reuses_browser_for_multiple_fetches() {
    let browser = HeadlessBrowser::launch().await.expect("launch");
    for marker in ["FIRST_OK", "SECOND_OK"] {
        let url = format!(
            "data:text/html,<div id=a></div>\
             <script>document.getElementById('a').innerHTML='{marker}'</script>"
        );
        let html = browser.fetch_rendered_html(&url).await.expect("fetch");
        assert!(html.contains(marker), "复用抓取缺内容 {marker}");
    }
    browser.close().await;
}

/// 下载路闭环 + 复用不重下: 强制按需下载 chrome-headless-shell, 验证落盘 / 路径含 pin
/// 版本 / 第二次复用不重下 / 用下载的二进制真抓到渲染 DOM (= "未命中" 路完整 OK)。
/// quarantine 实测在测试外用 `xattr` 命令查 (见 PR / MOC-143 验收记录)。
#[tokio::test]
#[ignore = "需要网络: 下载 ~86MB chrome-headless-shell"]
async fn downloads_and_uses_headless_shell() {
    use codex_app_transfer_http::headless::{ensure_chrome_headless_shell, PINNED_VERSION};
    use codex_app_transfer_http::HeadlessConfig;

    // 第一次: 触发下载 + 解压。
    let t0 = std::time::Instant::now();
    let bin = ensure_chrome_headless_shell()
        .await
        .expect("下载 chrome-headless-shell");
    eprintln!(
        "download+extract took {:?}, bin = {}",
        t0.elapsed(),
        bin.display()
    );
    assert!(bin.is_file(), "下载的二进制不存在");
    assert!(
        bin.to_string_lossy().contains(PINNED_VERSION),
        "路径未含 pin 版本 {PINNED_VERSION}"
    );

    // 第二次: 复用不重下 (marker 命中 → 秒回)。
    let t1 = std::time::Instant::now();
    let bin2 = ensure_chrome_headless_shell().await.expect("复用");
    let reuse = t1.elapsed();
    eprintln!("reuse took {reuse:?}");
    assert_eq!(bin, bin2, "复用返回路径不一致");
    assert!(reuse.as_millis() < 1000, "复用不应重下 (耗时 {reuse:?})");

    // 用下载的二进制真抓 (下载路完整闭环): JS 注入内容出现。
    let browser = HeadlessBrowser::launch_with_binary(&bin, HeadlessConfig::default())
        .await
        .expect("用下载的二进制 launch");
    let url = "data:text/html,<div id=x></div>\
               <script>document.getElementById('x').innerHTML='SHELL_RENDER_OK'</script>";
    let html = browser.fetch_rendered_html(url).await.expect("fetch");
    assert!(
        html.contains("SHELL_RENDER_OK"),
        "下载的二进制未拿到渲染 DOM"
    );
    browser.close().await;
}
