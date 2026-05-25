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
//! - **target ID 变化 cover**:Codex.app 完全重启 → target ID 变 → 之前注册的
//!   `addScriptToEvaluateOnNewDocument` 自然失效。通过 transfer 内"重启 Codex"
//!   按钮 / auto-launch 启动时,[`crate::admin::services::desktop::process::auto_apply_theme_on_startup`]
//!   poll `DevToolsActivePort` 拿到端口后自动 re-inject。**仅 user 绕过 transfer
//!   直接启 Codex.app(Spotlight / 系统重启 / kill -9 等)时需要手动 apply 一次**。
//!
//! **资源**:5 套内置主题在 `src-tauri/resources/themes/<name>/{bg.{png,jpg},mascot.png?}`,
//! 编译时 `include_bytes!` 嵌进 binary,运行时 base64 编码注入 data URI。

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_plugin_unlocker::current_cdp_url;

/// 主题列表 — 字符串 ID 跟 `src-tauri/resources/themes/<id>/` 目录名匹配。
/// **不变量**:每条 ID 都对应一组 (bg, mascot?) 资源 + 中英显示名(`ThemeMeta.display_name_{zh,en}`,
/// Rust hardcoded,**不**走 frontend `i18n.js` keys)。
pub const THEME_IDS: &[&str] = &["carton", "changli", "azurlane", "nailin", "zani"];

/// 自定义主题 id(动态加在 [`THEME_IDS`] 之后,仅当 `custom_theme_exists()` true 时存在)。
pub const CUSTOM_THEME_ID: &str = "custom";

/// 内置主题元数据。display name 给 frontend 渲染;`has_mascot` 决定是否注入
/// 浮动看板娘(目前仅 `carton` 有)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeMeta {
    pub id: &'static str,
    pub display_name_zh: &'static str,
    pub display_name_en: &'static str,
    pub has_mascot: bool,
    /// `background-size`(`build_inject_script` 直接展开进 CSS)。**当前所有 6
    /// 套主题统一 `"cover"`**:内置 5 张 bg 已替换为 3840×3840 方图,custom 在前端
    /// 1:1 crop 后也是方图,cover 在 Codex 1900×1100 landscape viewport 下显示头部
    /// 到胸口刚好。字段保留供未来非方图 portrait 主题用(可设 `"contain"` 整图显示,
    /// letterbox 用 body `background-color` 填)。
    pub bg_fit: &'static str,
    /// `background-position`。当前都 `"center top"` 锚顶部(保头部)。cover + 方图
    /// 组合下 viewport bottom 800px 被裁掉,锚 top 保留人物上半身。
    pub bg_position: &'static str,
}

/// 自定义主题资源目录:`~/.codex-app-transfer/themes/custom/`。
pub fn custom_theme_dir() -> Option<std::path::PathBuf> {
    codex_app_transfer_registry::paths::resolve_home()
        .map(|h| h.join(".codex-app-transfer").join("themes").join("custom"))
}

fn custom_bg_path() -> Option<std::path::PathBuf> {
    custom_theme_dir().map(|d| d.join("bg.jpg"))
}

fn custom_preview_path() -> Option<std::path::PathBuf> {
    custom_theme_dir().map(|d| d.join("preview.jpg"))
}

/// User 是否上传过自定义主题图(bg.jpg 存在即视为存在)。
pub fn custom_theme_exists() -> bool {
    custom_bg_path().map(|p| p.exists()).unwrap_or(false)
}

/// User 上传图 bytes(JPG / PNG)→ **中心 crop 方形**(safety net:正常路径前端
/// `openCropModal` 已 1:1 crop,这里 crop 已是方图时 no-op;兜底直接 POST raw
/// image 不走 modal 的场景)→ resize 到 max 2048(节约 disk + base64 体积)→
/// JPEG 90% 写 `bg.jpg`;同步生成 640px preview 写 `preview.jpg`。写入用 .tmp
/// + rename 原子化避免半截文件。
pub fn save_custom_theme(image_bytes: &[u8]) -> Result<(), String> {
    use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, GenericImageView};

    let img = image::load_from_memory(image_bytes)
        .map_err(|e| format!("无法解析图片(请用 JPG 或 PNG): {e}"))?;
    let (w, h) = img.dimensions();
    let side = w.min(h);
    let x = (w - side) / 2;
    let y = (h - side) / 2;
    let cropped = img.crop_imm(x, y, side, side);

    let bg = cropped.resize(2048, 2048, FilterType::Lanczos3);
    let preview = cropped.resize(640, 640, FilterType::Lanczos3);

    let dir = custom_theme_dir().ok_or_else(|| "无法解析 home 目录".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;

    fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        let encoder = JpegEncoder::new_with_quality(&mut buf, quality);
        img.to_rgb8()
            .write_with_encoder(encoder)
            .map_err(|e| format!("JPEG encode 失败: {e}"))?;
        Ok(buf)
    }

    let bg_bytes = encode_jpeg(&bg, 90)?;
    let preview_bytes = encode_jpeg(&preview, 85)?;

    let bg_final = dir.join("bg.jpg");
    let preview_final = dir.join("preview.jpg");
    let bg_tmp = dir.join("bg.jpg.tmp");
    let preview_tmp = dir.join("preview.jpg.tmp");
    std::fs::write(&bg_tmp, &bg_bytes).map_err(|e| format!("写 bg 失败: {e}"))?;
    std::fs::rename(&bg_tmp, &bg_final).map_err(|e| format!("rename bg 失败: {e}"))?;
    std::fs::write(&preview_tmp, &preview_bytes).map_err(|e| format!("写 preview 失败: {e}"))?;
    std::fs::rename(&preview_tmp, &preview_final)
        .map_err(|e| format!("rename preview 失败: {e}"))?;

    Ok(())
}

/// 删除 user 上传的自定义主题:rm `bg.jpg` + `preview.jpg`(同时清理可能残留
/// 的 .tmp 文件)。文件不存在视为已删除返 Ok(幂等)。
pub fn delete_custom_theme() -> Result<(), String> {
    let dir = match custom_theme_dir() {
        Some(d) => d,
        None => return Ok(()),
    };
    if !dir.exists() {
        return Ok(());
    }
    for name in ["bg.jpg", "preview.jpg", "bg.jpg.tmp", "preview.jpg.tmp"] {
        let p = dir.join(name);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| format!("删除 {name} 失败: {e}"))?;
        }
    }
    Ok(())
}

fn load_custom_theme_assets() -> Option<ThemeAssets> {
    let bg_bytes = std::fs::read(custom_bg_path()?).ok()?;
    let preview_bytes = std::fs::read(custom_preview_path()?).ok()?;
    Some(ThemeAssets {
        bg_data_uri: encode_data_uri("image/jpeg", &bg_bytes),
        mascot_data_uri: None,
        preview_data_uri: encode_data_uri("image/jpeg", &preview_bytes),
    })
}

/// 所有主题 metadata。内置 5 套固定;`custom` 仅当 user 上传过(disk 文件存在)
/// 时追加在末尾。
pub fn all_themes() -> Vec<ThemeMeta> {
    let mut v = vec![
        ThemeMeta {
            id: "carton",
            display_name_zh: "Carton",
            display_name_en: "Carton",
            has_mascot: true,
            bg_fit: "cover",
            bg_position: "center top",
        },
        ThemeMeta {
            id: "changli",
            display_name_zh: "长离",
            display_name_en: "Changli",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
        },
        ThemeMeta {
            id: "azurlane",
            display_name_zh: "碧蓝航线",
            display_name_en: "Azur Lane",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
        },
        ThemeMeta {
            id: "nailin",
            display_name_zh: "乃琳",
            display_name_en: "Nailin",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
        },
        ThemeMeta {
            id: "zani",
            display_name_zh: "赞妮",
            display_name_en: "Zani",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
        },
    ];
    if custom_theme_exists() {
        v.push(ThemeMeta {
            id: CUSTOM_THEME_ID,
            display_name_zh: "自定义",
            display_name_en: "Custom",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
        });
    }
    v
}

/// 主题资源 — 编译时嵌进 binary 的 base64 data URI。
///
/// - `bg_data_uri`:**原始大背景图**,inject 到 Codex Desktop body 用。
/// - `mascot_data_uri`:carton 主题专属漂浮立绘。
/// - `preview_data_uri`:**Theme 页缩略图**(实际 Codex Desktop 主题应用效果
///   截图,左侧 sidebar 已 GaussianBlur 防隐私泄露),~40KB / 张。
#[derive(Debug, Clone)]
pub struct ThemeAssets {
    pub bg_data_uri: String,
    pub mascot_data_uri: Option<String>,
    pub preview_data_uri: String,
}

/// 拿指定主题的资源。返回 None = 该 theme_id 既不在 [`THEME_IDS`] 也没有
/// 对应的 custom disk 资源。`custom` 走 [`load_custom_theme_assets`] disk 读路径,
/// 其他走 `include_bytes!` 静态嵌入。
pub fn load_theme_assets(theme_id: &str) -> Option<ThemeAssets> {
    if theme_id == CUSTOM_THEME_ID {
        return load_custom_theme_assets();
    }
    // include_bytes! 必须用字面路径,所以每条 theme 显式 match
    let (bg_bytes, bg_mime, mascot, preview_bytes): (&[u8], &str, Option<(&[u8], &str)>, &[u8]) =
        match theme_id {
            "carton" => (
                include_bytes!("../resources/themes/carton/bg.jpg"),
                "image/jpeg",
                Some((
                    include_bytes!("../resources/themes/carton/mascot.png"),
                    "image/png",
                )),
                include_bytes!("../resources/themes/carton/preview.jpg"),
            ),
            "changli" => (
                include_bytes!("../resources/themes/changli/bg.jpg"),
                "image/jpeg",
                None,
                include_bytes!("../resources/themes/changli/preview.jpg"),
            ),
            "azurlane" => (
                include_bytes!("../resources/themes/azurlane/bg.jpg"),
                "image/jpeg",
                None,
                include_bytes!("../resources/themes/azurlane/preview.jpg"),
            ),
            "nailin" => (
                include_bytes!("../resources/themes/nailin/bg.jpg"),
                "image/jpeg",
                None,
                include_bytes!("../resources/themes/nailin/preview.jpg"),
            ),
            "zani" => (
                include_bytes!("../resources/themes/zani/bg.jpg"),
                "image/jpeg",
                None,
                include_bytes!("../resources/themes/zani/preview.jpg"),
            ),
            _ => return None,
        };
    Some(ThemeAssets {
        bg_data_uri: encode_data_uri(bg_mime, bg_bytes),
        mascot_data_uri: mascot.map(|(b, m)| encode_data_uri(m, b)),
        preview_data_uri: encode_data_uri("image/jpeg", preview_bytes),
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
///
/// **503 / connection refused 友好处理**(#264 user 反馈):Codex.app
/// reload 中 / debug port 短暂不可用是预期 transient,自动 retry 1 次 +
/// 200ms 延迟。仍失败则返友好 hint,user 可以重试或先确认 Codex 在跑。
pub async fn reload_codex_page() -> Result<(), String> {
    match run_reload().await {
        Ok(()) => Ok(()),
        Err(_) => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            match run_reload().await {
                Ok(()) => Ok(()),
                Err(e) => Err(format!(
                    "Codex Desktop CDP 暂不可达({e}) — 确认 Codex.app 正在运行,稍后重试"
                )),
            }
        }
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
/// **不**清 `Page.addScriptToEvaluateOnNewDocument` 注册:`addScriptToEvaluateOnNewDocument`
/// 调用本身返回 identifier,实现选择不存(简单 + transient remove 够 v1)。
///
/// ⚠️ **副作用**:注册仍存活,Codex 任何 page navigation / reload 都会再跑一次 IIFE
/// 把主题装回来。**只有 user 完整 quit Codex.app(target ID 变)注册才彻底失效**。
/// v1 user 关 toggle → clear 立即视觉清除,如果在 Codex 内手动 reload 会"复发",
/// 但 transfer 不会主动 reload Codex,所以 v1 流程下不可见。
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
    // **CDP_PORT=0 sentinel 检查**:Codex 启动后 0–15s 内,或 DevToolsActivePort
    // 永远没出现的极端 case,`current_cdp_url()` 会返 `http://127.0.0.1:0/...`,
    // 直接走 reqwest 会冒出"tcp connect error: Cannot assign requested address"
    // 这种 OS 原始错误,user 看不懂根因。这里 early-return 友好错误。
    use crate::codex_plugin_unlocker::CDP_PORT;
    if CDP_PORT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return Err(
            "CDP 端口尚未就绪 — Codex Desktop 可能还在启动中,稍候重试 / 或确认 Codex.app 正在运行"
                .into(),
        );
    }
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
    // 按 theme 查 bg_fit / bg_position(portrait 图用 contain 不裁头)
    let (bg_fit, bg_position) = all_themes()
        .into_iter()
        .find(|m| m.id == theme_id)
        .map(|m| (m.bg_fit, m.bg_position))
        .unwrap_or(("cover", "center top"));
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
  // **切换主题语义**:进来先 cleanup 旧 style + mascot(如有),再 inject 新的。
  // 早期版本用 `if (existing) return` short-circuit,导致从主题 A apply 到主题 B
  // 时 Runtime.evaluate 因 'cat-theme-style' 已存在直接 return,新主题没注入 —
  // user 必须 reload Codex page 才能切换。改成 remove-then-create 后,apply
  // 调用即立即切到新主题(单步生效)。
  var oldStyle = document.getElementById('cat-theme-style');
  if (oldStyle) oldStyle.remove();
  var oldMascot = document.getElementById('cat-theme-mascot');
  if (oldMascot) oldMascot.remove();

  var style = document.createElement('style');
  style.id = 'cat-theme-style';
  style.setAttribute('data-cat-theme', '{theme_id}');
  style.textContent = `
    body {{
      background-color: #1a1010 !important;
      background-image: linear-gradient(rgba(22, 13, 13, 0.45), rgba(22, 13, 13, 0.45)), url('{bg}') !important;
      background-size: cover, {bg_fit} !important;
      background-position: center, {bg_position} !important;
      background-repeat: no-repeat, no-repeat !important;
      background-attachment: fixed, fixed !important;
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
      backdrop-filter: blur(4px) saturate(120%) !important;
      -webkit-backdrop-filter: blur(4px) saturate(120%) !important;
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
        .filter(|s| {
            THEME_IDS.contains(&s.as_str()) || (s == CUSTOM_THEME_ID && custom_theme_exists())
        });
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
        // 验证 mascot CSS rule + mount block 只出现在 carton script,changli 等无 mascot
        // 主题没有。**不**直接 grep "cat-theme-mascot" 字符串 — 切换主题语义改造后
        // 所有 script 头都有 `getElementById('cat-theme-mascot')` cleanup line(用于
        // 切到非 mascot 主题时移除旧 carton mascot),仅 mascot CSS rule 跟 mount
        // 代码是 carton 专属。
        let carton = load_theme_assets("carton").unwrap();
        let carton_script = build_inject_script("carton", &carton);
        assert!(carton_script.contains(".cat-theme-mascot {"));
        assert!(carton_script.contains("m.className = 'cat-theme-mascot'"));

        let changli = load_theme_assets("changli").unwrap();
        let changli_script = build_inject_script("changli", &changli);
        assert!(!changli_script.contains(".cat-theme-mascot {"));
        assert!(!changli_script.contains("m.className = 'cat-theme-mascot'"));
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
