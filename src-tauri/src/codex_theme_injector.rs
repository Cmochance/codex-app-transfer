//! Codex Desktop UI 主题注入器(#264)。
//!
//! **跟 plugin_unlocker 独立的功能**(user 明示):
//! - plugin_unlocker:解锁 Plugins tab(setAuthMethod('chatgpt'))
//! - theme_injector:覆盖 UI CSS token 变量 + 注入背景图 + 可选浮动 mascot
//!
//! **设计选择:一次性 inject + `Page.addScriptToEvaluateOnNewDocument` 不维持 daemon**:
//! - CDP 协议的 `Page.addScriptToEvaluateOnNewDocument` 让 script 在每次 page
//!   navigation / reload 时**自动**执行,一次注入持久生效
//! - 因此不需要 daemon 持续 reinject(plugin_unlocker 那种 deeply nested race
//!   监控不需要)
//! - **limitation v1**:Codex.app 完全重启 → target ID 变 → `addScriptToEvaluateOnNewDocument`
//!   注册失效,需要 user 在 transfer Theme 页点 "Apply" 重做。后续 v2 可加 daemon
//!   监控 Codex.app target 变化自动重注入。
//!
//! **资源**:5 套内置主题在 `src-tauri/resources/themes/<name>/{bg.{png,jpg},mascot.png?}`,
//! 编译时 `include_bytes!` 嵌进 binary,运行时 base64 编码注入 data URI。

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_plugin_unlocker::current_cdp_url;

/// 主题列表 — 字符串 ID 跟 `src-tauri/resources/themes/<id>/` 目录名匹配。
/// **不变量**:每条 ID 都对应一组 (bg, mascot?) 资源 + i18n 显示名。
pub const THEME_IDS: &[&str] = &["carton", "changli", "azurlane", "nailin", "zani"];

/// 内置主题元数据。display name 给 frontend 渲染;`has_mascot` 决定是否注入
/// 浮动看板娘(目前仅 `carton` 有)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeMeta {
    pub id: &'static str,
    pub display_name_zh: &'static str,
    pub display_name_en: &'static str,
    pub has_mascot: bool,
}

/// 所有内置主题 metadata。
pub fn all_themes() -> Vec<ThemeMeta> {
    vec![
        ThemeMeta {
            id: "carton",
            display_name_zh: "Carton",
            display_name_en: "Carton",
            has_mascot: true,
        },
        ThemeMeta {
            id: "changli",
            display_name_zh: "长离",
            display_name_en: "Changli",
            has_mascot: false,
        },
        ThemeMeta {
            id: "azurlane",
            display_name_zh: "碧蓝航线",
            display_name_en: "Azur Lane",
            has_mascot: false,
        },
        ThemeMeta {
            id: "nailin",
            display_name_zh: "乃琳",
            display_name_en: "Nailin",
            has_mascot: false,
        },
        ThemeMeta {
            id: "zani",
            display_name_zh: "赞妮",
            display_name_en: "Zani",
            has_mascot: false,
        },
    ]
}

/// 主题资源(bg + 可选 mascot)— 编译时嵌进 binary 的 base64 data URI。
#[derive(Debug, Clone)]
pub struct ThemeAssets {
    pub bg_data_uri: String,
    pub mascot_data_uri: Option<String>,
}

/// 拿指定主题的资源。返回 None = 该 theme_id 不在 [`THEME_IDS`]。
pub fn load_theme_assets(theme_id: &str) -> Option<ThemeAssets> {
    // include_bytes! 必须用字面路径,所以每条 theme 显式 match
    let (bg_bytes, bg_mime, mascot): (&[u8], &str, Option<(&[u8], &str)>) = match theme_id {
        "carton" => (
            include_bytes!("../resources/themes/carton/bg.png"),
            "image/png",
            Some((
                include_bytes!("../resources/themes/carton/mascot.png"),
                "image/png",
            )),
        ),
        "changli" => (
            include_bytes!("../resources/themes/changli/bg.jpg"),
            "image/jpeg",
            None,
        ),
        "azurlane" => (
            include_bytes!("../resources/themes/azurlane/bg.jpg"),
            "image/jpeg",
            None,
        ),
        "nailin" => (
            include_bytes!("../resources/themes/nailin/bg.jpg"),
            "image/jpeg",
            None,
        ),
        "zani" => (
            include_bytes!("../resources/themes/zani/bg.jpg"),
            "image/jpeg",
            None,
        ),
        _ => return None,
    };
    Some(ThemeAssets {
        bg_data_uri: encode_data_uri(bg_mime, bg_bytes),
        mascot_data_uri: mascot.map(|(b, m)| encode_data_uri(m, b)),
    })
}

fn encode_data_uri(mime: &str, bytes: &[u8]) -> String {
    use base64::{engine::general_purpose, Engine as _};
    format!(
        "data:{mime};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    )
}

/// 主题注入状态(给前端展示)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeStatus {
    /// 未启用(transfer settings.codexUiThemeEnabled = false 或没 apply 过)
    Disabled,
    /// 正在 connect / inject
    Applying,
    /// 已注入(指定主题)
    Applied { theme_id: String },
    /// 注入失败
    Failed { error: String },
}

/// 全局状态 — 跟前端 status 查询共享。
static THEME_STATUS: RwLock<Option<ThemeStatus>> = RwLock::const_new(None);

/// 拿当前主题注入状态。None = 还没初始化(等同 Disabled)。
pub async fn get_status() -> ThemeStatus {
    THEME_STATUS
        .read()
        .await
        .clone()
        .unwrap_or(ThemeStatus::Disabled)
}

async fn set_status(new: ThemeStatus) {
    let mut g = THEME_STATUS.write().await;
    if g.as_ref() != Some(&new) {
        tracing::info!("[CodexTheme] status: {:?} → {:?}", g.as_ref(), new);
        *g = Some(new);
    }
}

/// 应用主题:CDP connect → addScriptToEvaluateOnNewDocument(持久跨 reload)+
/// Runtime.evaluate(立即生效) → disconnect。
///
/// `theme_id` 必须在 [`THEME_IDS`] 内;否则返 `Err`。
/// Codex.app 没启动 / CDP 未开 → 返 `Err`(caller 决定 retry 还是报 user)。
pub async fn apply_theme(theme_id: &str) -> Result<(), String> {
    let assets =
        load_theme_assets(theme_id).ok_or_else(|| format!("unknown theme id: {theme_id}"))?;

    set_status(ThemeStatus::Applying).await;

    match run_apply(theme_id, &assets).await {
        Ok(()) => {
            set_status(ThemeStatus::Applied {
                theme_id: theme_id.to_owned(),
            })
            .await;
            Ok(())
        }
        Err(e) => {
            set_status(ThemeStatus::Failed {
                error: e.to_string(),
            })
            .await;
            Err(e.to_string())
        }
    }
}

/// 重载 Codex Desktop 当前 page(走 CDP `Page.reload`)。Theme 用
/// `addScriptToEvaluateOnNewDocument` 注册的脚本会在重载后**自动**触发,
/// 等于"全页强刷"应用主题(也可用来快速验证 inject 是否完整生效)。
pub async fn reload_codex_page() -> Result<(), String> {
    match run_reload().await {
        Ok(()) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

async fn run_reload() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_url = locate_main_window_ws().await?;
    let (ws_stream, _) = connect_async(&ws_url).await?;
    let (mut write, mut read) = ws_stream.split();
    let (msg, _) = make_msg(1, "Page.reload", json!({ "ignoreCache": true }));
    write.send(WsMessage::Text(msg)).await?;
    drain_one(&mut read).await;
    let _ = write.close().await;
    Ok(())
}

/// 清除主题:CDP connect → 注入 removal script(移除 style + mascot DOM)+
/// Runtime.evaluate 立即生效 → disconnect。
///
/// **不**清 `Page.addScriptToEvaluateOnNewDocument` 注册(CDP 没暴露 list 接口
/// 拿 identifier;一次性 transient 移除够 v1,Codex.app 重启 target ID 变了
/// 之前注册自然失效)。
pub async fn clear_theme() -> Result<(), String> {
    match run_clear().await {
        Ok(()) => {
            set_status(ThemeStatus::Disabled).await;
            Ok(())
        }
        Err(e) => {
            set_status(ThemeStatus::Failed {
                error: e.to_string(),
            })
            .await;
            Err(e.to_string())
        }
    }
}

async fn run_apply(
    theme_id: &str,
    assets: &ThemeAssets,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_url = locate_main_window_ws().await?;
    let (ws_stream, _) = connect_async(&ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    // 1. enable Page domain(addScriptToEvaluateOnNewDocument 需要)
    let (msg, _) = make_msg(1, "Page.enable", json!({}));
    write.send(WsMessage::Text(msg)).await?;
    drain_one(&mut read).await;

    // 2. addScriptToEvaluateOnNewDocument — 每次 page navigate / reload 自动跑
    let script = build_inject_script(theme_id, assets);
    let (msg, _) = make_msg(
        2,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": script }),
    );
    write.send(WsMessage::Text(msg)).await?;
    drain_one(&mut read).await;

    // 3. Runtime.evaluate — 立即在当前 page 跑一次(addScriptToEvaluateOnNewDocument
    //    只对**未来**的 navigation 生效,当前 page 需要单独 evaluate)
    let (msg, _) = make_msg(
        3,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write.send(WsMessage::Text(msg)).await?;
    drain_one(&mut read).await;

    let _ = write.close().await;
    Ok(())
}

async fn run_clear() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_url = locate_main_window_ws().await?;
    let (ws_stream, _) = connect_async(&ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    let script = REMOVE_THEME_SCRIPT;
    let (msg, _) = make_msg(
        1,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write.send(WsMessage::Text(msg)).await?;
    drain_one(&mut read).await;

    let _ = write.close().await;
    Ok(())
}

/// 拿 Codex Desktop 主窗口的 CDP webSocketDebuggerUrl。
/// 复用 plugin_unlocker 的 page-filter 思路:type=page + URL 含 `index.html` +
/// 不含 `avatar-overlay`(过滤宠物悬浮窗)。
async fn locate_main_window_ws() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = current_cdp_url();
    let resp = reqwest::get(&url).await?;
    if !resp.status().is_success() {
        return Err(format!("CDP /json/list returned {}", resp.status()).into());
    }
    let pages: Vec<Value> = resp.json().await?;
    let main = pages
        .iter()
        .find(|p| {
            let url = p.get("url").and_then(Value::as_str).unwrap_or("");
            let ptype = p.get("type").and_then(Value::as_str).unwrap_or("");
            ptype == "page" && url.contains("index.html") && !url.contains("avatar-overlay")
        })
        .ok_or("no main page (index.html) found in CDP /json/list")?;
    main.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(|s| s.to_owned())
        .ok_or_else(|| "webSocketDebuggerUrl missing".into())
}

fn make_msg(id: u64, method: &str, params: Value) -> (String, u64) {
    let body = json!({ "id": id, "method": method, "params": params }).to_string();
    (body, id)
}

/// drain 一帧 — `addScriptToEvaluateOnNewDocument` / `Runtime.evaluate` 的响应
/// 直接丢弃,我们不解析(theme inject 没有"成功/失败" 二态需要识别,只要 CDP
/// 没报错 frame 就行)。带 1s 超时避免永久阻塞。
async fn drain_one(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
) {
    let _ = tokio::time::timeout(Duration::from_secs(2), read.next()).await;
}

/// 构造注入 script — CSS variable 覆盖 + 背景图 + 可选 mascot。
///
/// **CSS 借鉴 user 本地 `~/alysechen/github/codex-theme/launcher.js` 手搓**
/// (user 明示无需致谢,非上游借鉴)。token 变量名跟 Codex Desktop UI 框架内部
/// 一致(`--color-token-*` 系列),改它们等于 hot reskin。
fn build_inject_script(theme_id: &str, assets: &ThemeAssets) -> String {
    let bg = &assets.bg_data_uri;
    let mascot_block = match &assets.mascot_data_uri {
        Some(m) => format!(
            r#"
    /* Floating Mascot (carton 主题专属) */
    .cat-theme-mascot {{
      position: fixed;
      bottom: 15px;
      right: 15px;
      width: 150px;
      height: 150px;
      background-image: url('{m}');
      background-size: contain;
      background-repeat: no-repeat;
      background-position: bottom right;
      z-index: 9999;
      pointer-events: none;
      transition: transform 0.4s cubic-bezier(0.175, 0.885, 0.32, 1.275), opacity 0.3s ease;
      opacity: 0.85;
      filter: drop-shadow(0 4px 12px rgba(0,0,0,0.35));
    }}
"#
        ),
        None => String::new(),
    };

    let mascot_js = if assets.mascot_data_uri.is_some() {
        r#"
  // Mount mascot + rAF-throttled distance-based micro-animation
  if (!document.getElementById('cat-theme-mascot')) {
    var m = document.createElement('div');
    m.id = 'cat-theme-mascot';
    m.className = 'cat-theme-mascot';
    document.body.appendChild(m);
    var lx = 0, ly = 0, rafPending = false;
    window.addEventListener('mousemove', function(e) {
      lx = e.clientX; ly = e.clientY;
      if (rafPending) return;
      rafPending = true;
      requestAnimationFrame(function() {
        rafPending = false;
        var el = document.getElementById('cat-theme-mascot');
        if (!el) return;
        var rect = el.getBoundingClientRect();
        var d = Math.hypot(lx - (rect.left + rect.width/2), ly - (rect.top + rect.height/2));
        if (d < 180) { el.style.transform = 'translateY(-10px) scale(1.08)'; el.style.opacity = '1'; }
        else { el.style.transform = 'none'; el.style.opacity = '0.85'; }
      });
    }, { passive: true });
  }
"#
    } else {
        ""
    };

    format!(
        r#"
(function() {{
  // codex-app-transfer theme inject (#264). theme={theme_id}
  // 幂等:用固定 id 的 <style>,二次注入直接 short-circuit
  if (document.getElementById('cat-theme-style')) return;

  var style = document.createElement('style');
  style.id = 'cat-theme-style';
  style.setAttribute('data-cat-theme', '{theme_id}');
  style.textContent = `
    body {{
      background-image: linear-gradient(rgba(22, 13, 13, 0.45), rgba(22, 13, 13, 0.45)), url('{bg}') !important;
      background-size: cover !important;
      background-position: center top !important;
      background-repeat: no-repeat !important;
      background-attachment: fixed !important;
    }}
    #root, .app-shell, .app-shell-main, main.main-surface {{ background: transparent !important; }}

    :root {{
      --color-token-main-surface-primary: rgba(22, 13, 13, 0.65) !important;
      --color-token-bg-primary: rgba(18, 10, 10, 0.7) !important;
      --color-token-side-bar-background: rgba(14, 6, 6, 0.75) !important;
      --color-token-editor-background: rgba(22, 12, 12, 0.45) !important;
      --color-token-input-background: rgba(255, 200, 200, 0.08) !important;
      --color-background-surface: rgba(22, 13, 13, 0.65) !important;
      --color-background-panel: rgba(22, 13, 13, 0.65) !important;
      --color-background-elevated-primary: rgba(22, 13, 13, 0.65) !important;
      --color-background-elevated-primary-opaque: rgba(22, 13, 13, 0.65) !important;
      --color-background-elevated-secondary: rgba(22, 13, 13, 0.65) !important;
      --color-background-elevated-secondary-opaque: rgba(22, 13, 13, 0.65) !important;
      --color-background-control: rgba(22, 13, 13, 0.65) !important;
      --color-background-control-opaque: rgba(22, 13, 13, 0.65) !important;
      --color-token-bg-fog: rgba(22, 13, 13, 0.65) !important;
      --color-token-dropdown-background: rgba(22, 13, 13, 0.65) !important;
      --color-token-border: rgba(230, 70, 70, 0.18) !important;
      --color-token-border-heavy: rgba(230, 70, 70, 0.28) !important;
      --color-token-border-light: rgba(230, 70, 70, 0.1) !important;
      --color-border: rgba(230, 70, 70, 0.18) !important;
      --color-border-heavy: rgba(230, 70, 70, 0.28) !important;
      --color-border-light: rgba(230, 70, 70, 0.1) !important;
      --color-token-foreground: #fcfcfc !important;
      --color-token-text-primary: #fcfcfc !important;
      --color-token-text-secondary: rgba(250, 240, 240, 0.75) !important;
      --color-text-foreground: #fcfcfc !important;
      --color-text-foreground-secondary: rgba(250, 240, 240, 0.75) !important;
      --color-text-foreground-tertiary: rgba(250, 240, 240, 0.5) !important;
      --color-text-button-primary: #fcfcfc !important;
      --color-text-button-secondary: #fcfcfc !important;
      --color-text-button-tertiary: rgba(250, 240, 240, 0.75) !important;
      --color-icon-primary: #fcfcfc !important;
      --color-icon-secondary: rgba(250, 240, 240, 0.75) !important;
      --color-icon-tertiary: rgba(250, 240, 240, 0.5) !important;
      --color-token-primary: #ff4747 !important;
      --color-token-link: #ff4747 !important;
      --color-token-text-link-foreground: #ff4747 !important;
      --color-token-focus-border: #ffd700 !important;
      --color-token-scrollbar-slider-background: rgba(230, 70, 70, 0.2) !important;
      --color-token-scrollbar-slider-hover-background: rgba(230, 70, 70, 0.4) !important;
      --color-token-list-hover-background: rgba(230, 70, 70, 0.15) !important;
      --color-background-button-secondary-hover: rgba(230, 70, 70, 0.2) !important;
      --color-background-button-tertiary-hover: rgba(230, 70, 70, 0.1) !important;
    }}

    .app-shell-left-panel, .composer-root, .thread-root, .editor-container, .dialog-layout,
    [role="menu"], [role="listbox"], [role="dialog"], [data-radix-menu-content],
    [data-browser-comment-editor-surface], .bg-token-dropdown-background {{
      background-color: rgba(22, 13, 13, 0.65) !important;
      backdrop-filter: blur(12px) saturate(125%) !important;
      -webkit-backdrop-filter: blur(12px) saturate(125%) !important;
      border: 1px solid rgba(230, 70, 70, 0.18) !important;
    }}

    .app-shell-left-panel, .composer-root, .thread-root, .editor-container, .dialog-layout,
    [data-browser-comment-editor-surface] {{
      box-shadow: none !important;
      mask: none !important; -webkit-mask: none !important;
      mask-image: none !important; -webkit-mask-image: none !important;
    }}

    [role="menu"], [role="listbox"], [role="dialog"], [data-radix-menu-content], .bg-token-dropdown-background {{
      box-shadow: 0 8px 24px 0 rgba(0, 0, 0, 0.4) !important;
    }}

    .app-shell-left-panel {{ border-right: none !important; }}

    .app-shell-left-panel::before, .app-shell-left-panel::after, .thread-root::before, .thread-root::after,
    .composer-root::before, .composer-root::after, .editor-container::before, .editor-container::after,
    .app-shell-main::before, .app-shell-main::after {{
      background: transparent !important; background-image: none !important;
      box-shadow: none !important; mask: none !important; -webkit-mask: none !important; filter: none !important;
    }}

    [data-panel-resize-handle], [data-panel-resize-handle-id], [data-panel-group], [data-resize-handle],
    [role="separator"], .split-pane-divider, .app-shell-divider, .resize-handle, .resizable-handle {{
      background: transparent !important; background-image: none !important;
      box-shadow: none !important; border: none !important;
    }}
    {mascot_block}
  `;
  document.head.appendChild(style);
  {mascot_js}
}})();
"#
    )
}

/// 移除主题 script — 把 cat-theme-style + cat-theme-mascot 从 DOM 拆掉。
const REMOVE_THEME_SCRIPT: &str = r#"
(function() {
  var s = document.getElementById('cat-theme-style');
  if (s) s.remove();
  var m = document.getElementById('cat-theme-mascot');
  if (m) m.remove();
})();
"#;

/// 设置面板里 `codexUiThemeEnabled` (开关) + `codexUiTheme` (选定的 theme id) 读取。
#[derive(Debug, Clone, Default)]
pub struct ThemeSettings {
    pub enabled: bool,
    pub theme_id: Option<String>,
}

/// 从 `settings` JSON 读出 [`ThemeSettings`]。缺字段 → enabled=false, theme=None。
pub fn read_settings(settings: &Value) -> ThemeSettings {
    let enabled = settings
        .get("codexUiThemeEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let theme_id = settings
        .get("codexUiTheme")
        .and_then(Value::as_str)
        .map(|s| s.to_owned())
        .filter(|s| THEME_IDS.contains(&s.as_str()));
    ThemeSettings { enabled, theme_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_ids_match_all_themes_metadata() {
        let metas: Vec<&str> = all_themes().iter().map(|m| m.id).collect();
        let ids: Vec<&str> = THEME_IDS.iter().copied().collect();
        assert_eq!(metas, ids, "THEME_IDS 必须跟 all_themes() 严格对齐");
    }

    #[test]
    fn load_assets_for_every_known_theme() {
        for id in THEME_IDS {
            let assets = load_theme_assets(id).expect("known theme should load");
            assert!(
                assets.bg_data_uri.starts_with("data:image/"),
                "{id} bg must be data URI: {}",
                &assets.bg_data_uri[..30]
            );
        }
    }

    #[test]
    fn carton_has_mascot_others_dont() {
        for theme in all_themes() {
            let assets = load_theme_assets(theme.id).unwrap();
            assert_eq!(
                assets.mascot_data_uri.is_some(),
                theme.has_mascot,
                "{} has_mascot mismatch",
                theme.id
            );
        }
    }

    #[test]
    fn unknown_theme_returns_none() {
        assert!(load_theme_assets("nonexistent").is_none());
    }

    #[test]
    fn build_inject_script_embeds_theme_id_marker() {
        let assets = load_theme_assets("changli").unwrap();
        let script = build_inject_script("changli", &assets);
        assert!(script.contains("theme=changli"));
        assert!(script.contains("data-cat-theme"));
        assert!(script.contains("cat-theme-style"));
    }

    #[test]
    fn build_inject_script_includes_mascot_only_for_carton() {
        let carton = load_theme_assets("carton").unwrap();
        let carton_script = build_inject_script("carton", &carton);
        assert!(carton_script.contains("cat-theme-mascot"));

        let changli = load_theme_assets("changli").unwrap();
        let changli_script = build_inject_script("changli", &changli);
        assert!(!changli_script.contains("cat-theme-mascot"));
    }

    #[test]
    fn read_settings_defaults_to_disabled() {
        let s = read_settings(&json!({}));
        assert!(!s.enabled);
        assert_eq!(s.theme_id, None);
    }

    #[test]
    fn read_settings_extracts_valid_theme_only() {
        let s = read_settings(&json!({
            "codexUiThemeEnabled": true,
            "codexUiTheme": "carton",
        }));
        assert!(s.enabled);
        assert_eq!(s.theme_id, Some("carton".to_owned()));

        // unknown theme id 被 filter 掉(防 settings 文件被手改 typo)
        let s = read_settings(&json!({
            "codexUiThemeEnabled": true,
            "codexUiTheme": "nonexistent",
        }));
        assert!(s.enabled);
        assert_eq!(s.theme_id, None);
    }
}
