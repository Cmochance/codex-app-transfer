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
//! **资源**:11 套内置主题在 `src-tauri/resources/themes/<name>/{preview.jpg,mascot.png?}`。
//! 缩略图 `preview.jpg`(+ carton mascot)编译时 `include_bytes!` 嵌进 binary;**背景全图
//! `bg.jpg` 不再嵌入** —— 运行时 on-demand 从 storage 仓库 `img/theme/<id>.jpg` 下载 + 缓存
//! (`~/.codex-app-transfer/theme-cache/`,带进度给前端进度环),失败/离线回退缩略图当 bg。

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
pub const THEME_IDS: &[&str] = &[
    "carton", "changli", "azurlane", "nailin", "zani", "frost", "nocturne", "duet", "rose",
    "sonata", "studio",
];

/// Neutral cool-dark palette for the user `custom` theme (unknown artwork).
/// `accent: ""` → no accent override, keeps Codex's native blue.
const NEUTRAL_PALETTE: Palette = Palette {
    ink: "#f1ece4",
    ink2: "rgba(241,236,228,0.74)",
    ink3: "rgba(241,236,228,0.56)",
    ink4: "rgba(241,236,228,0.40)",
    accent: "",
    accent_soft: "",
    focus: "",
    surface: "rgba(20,20,24,0.50)",
    glass: "rgba(24,24,29,0.60)",
    glass_soft: "rgba(28,28,34,0.52)",
    glass_strong: "rgba(16,16,20,0.78)",
    border: "rgba(255,255,255,0.12)",
    border_soft: "rgba(255,255,255,0.07)",
    border_strong: "rgba(255,255,255,0.22)",
    blur: "6px",
    hover: "rgba(255,255,255,0.08)",
    selection: "rgba(255,255,255,0.14)",
    scrim_top: "rgba(8,8,10,0.26)",
    scrim_mid: "rgba(8,8,10,0.34)",
    scrim_bot: "rgba(5,5,7,0.60)",
    base_color: "#0e0e10",
};

/// 自定义主题 id(动态加在 [`THEME_IDS`] 之后,仅当 `custom_theme_exists()` true 时存在)。
pub const CUSTOM_THEME_ID: &str = "custom";

/// 每主题调色板 — 跟图片主色调匹配的暗玻璃 + 强调色(从 agent-theme 同款微调版
/// 同步而来)。所有值是直接展开进 CSS 的 color 字符串;`accent` 为空串表示该主题
/// 不覆盖强调色(保留 Codex 原生蓝)。详见 [`render_theme_css`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Palette {
    pub ink: &'static str,
    pub ink2: &'static str,
    pub ink3: &'static str,
    pub ink4: &'static str,
    pub accent: &'static str,
    pub accent_soft: &'static str,
    pub focus: &'static str,
    pub surface: &'static str,
    pub glass: &'static str,
    pub glass_soft: &'static str,
    pub glass_strong: &'static str,
    pub border: &'static str,
    pub border_soft: &'static str,
    pub border_strong: &'static str,
    pub blur: &'static str,
    pub hover: &'static str,
    pub selection: &'static str,
    pub scrim_top: &'static str,
    pub scrim_mid: &'static str,
    pub scrim_bot: &'static str,
    pub base_color: &'static str,
}

/// 内置主题元数据。display name 给 frontend 渲染;`has_mascot` 决定是否注入
/// 浮动看板娘(目前仅 `carton` 有);`palette` 是该主题专属配色。
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
    /// `background-position` — 按图片构图锚定人物头部/上半身(per-theme)。
    pub bg_position: &'static str,
    /// 该主题专属调色板(暗玻璃 + 强调色,跟图片主色匹配)。
    pub palette: Palette,
}

// ---- [MOC-272] 调色板用户覆盖(palette override)----
// 精选关键色:accent / ink / base_color / surface(→ 派生 glass 玻璃层)/ scrim(0-100 蒙版深浅)。
// 持久化在 config `settings.themePaletteOverrides[themeId]`(前端经通用 settings save 写、apply 时读)。
// 颜色一律 `#rrggbb`(取色器输出),render 前 sanitize 丢弃非法值 → 天然防 CSS 注入(无反引号)。

/// 用户对某主题调色板的覆盖(精选关键色)。字段可空 = 不覆盖、回落编译期基底;
/// 删除整个 themeId 的 override = 还原初始调色板。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaletteOverride {
    pub accent: Option<String>,
    pub ink: Option<String>,
    pub base_color: Option<String>,
    pub surface: Option<String>,
    /// 蒙版(scrim)深浅 0-100,生成黑色三层渐变;None = 用基底 scrim。
    pub scrim: Option<u8>,
}

impl PaletteOverride {
    /// 丢弃非法 `#rrggbb` 颜色 + clamp scrim;全空返 None(= 无有效覆盖)。
    fn sanitized(self) -> Option<PaletteOverride> {
        let keep = |c: Option<String>| c.filter(|s| is_hex_color(s));
        let ov = PaletteOverride {
            accent: keep(self.accent),
            ink: keep(self.ink),
            base_color: keep(self.base_color),
            surface: keep(self.surface),
            scrim: self.scrim.map(|v| v.min(100)),
        };
        let empty = ov.accent.is_none()
            && ov.ink.is_none()
            && ov.base_color.is_none()
            && ov.surface.is_none()
            && ov.scrim.is_none();
        if empty {
            None
        } else {
            Some(ov)
        }
    }
}

/// 前端调色盘编辑器回显用:某主题**有效**精选调色板(覆盖优先,否则基底,转成取色器要的 `#rrggbb`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CuratedPalette {
    /// "" = 不覆盖强调色(跟随 Codex 原生蓝)。
    pub accent: String,
    pub ink: String,
    pub base_color: String,
    pub surface: String,
    /// None = 跟随基底 scrim(编辑器滑块给默认起点)。
    pub scrim: Option<u8>,
}

fn is_hex_color(s: &str) -> bool {
    let s = s.trim();
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn hex_to_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim();
    if !is_hex_color(s) {
        return None;
    }
    Some((
        u8::from_str_radix(&s[1..3], 16).ok()?,
        u8::from_str_radix(&s[3..5], 16).ok()?,
        u8::from_str_radix(&s[5..7], 16).ok()?,
    ))
}
fn rgba(r: u8, g: u8, b: u8, a: f32) -> String {
    format!("rgba({r},{g},{b},{a:.3})")
}
/// 基底的 `rgba(r,g,b,a)` / `rgb(...)` / `#rrggbb` 取 RGB 转 `#rrggbb`(编辑器回显;丢 alpha)。
fn color_to_hex(s: &str) -> String {
    let s = s.trim();
    if is_hex_color(s) {
        return s.to_lowercase();
    }
    if let Some(inner) = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")) {
        let nums: Vec<u8> = inner
            .trim_end_matches(')')
            .split(',')
            .take(3)
            .filter_map(|p| {
                p.trim()
                    .parse::<f32>()
                    .ok()
                    .map(|f| f.round().clamp(0.0, 255.0) as u8)
            })
            .collect();
        if nums.len() == 3 {
            return format!("#{:02x}{:02x}{:02x}", nums[0], nums[1], nums[2]);
        }
    }
    "#000000".to_string()
}

/// 读 config 里某主题的 palette override(已 sanitize)。无 / 非法 → None。
pub fn palette_override_for(theme_id: &str) -> Option<PaletteOverride> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let raw = cfg
        .get("settings")?
        .get("themePaletteOverrides")?
        .get(theme_id)?;
    serde_json::from_value::<PaletteOverride>(raw.clone())
        .ok()?
        .sanitized()
}

/// 某主题的有效精选调色板(基底 + override),供 `theme/list` 给编辑器回显。
pub fn effective_curated_palette(theme_id: &str) -> CuratedPalette {
    let metas = all_themes();
    let base = metas
        .iter()
        .find(|m| m.id == theme_id)
        .map(|m| &m.palette)
        .unwrap_or(&NEUTRAL_PALETTE);
    let ov = palette_override_for(theme_id);
    let ov = ov.as_ref();
    CuratedPalette {
        // accent 保留空串语义("" = 原生蓝),不走 color_to_hex(那会变 #000000)。
        accent: ov
            .and_then(|o| o.accent.clone())
            .unwrap_or_else(|| base.accent.to_string()),
        ink: ov
            .and_then(|o| o.ink.clone())
            .unwrap_or_else(|| color_to_hex(base.ink)),
        base_color: ov
            .and_then(|o| o.base_color.clone())
            .unwrap_or_else(|| color_to_hex(base.base_color)),
        surface: ov
            .and_then(|o| o.surface.clone())
            .unwrap_or_else(|| color_to_hex(base.surface)),
        scrim: ov.and_then(|o| o.scrim),
    }
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

/// User 上传图 bytes(JPG / PNG)→ 等比 resize 到最长边 ≤2048(节约 disk +
/// base64 体积)→ JPEG 90% 写 `bg.jpg`;同步生成最长边 ≤640 的等比 preview 写
/// `preview.jpg`。正常路径前端 `openCropModal` 已按 16:9 裁切;若直接 POST raw
/// image 则后端仅缩放、不二次裁切。写入用 .tmp + rename 原子化避免半截文件。
pub fn save_custom_theme(image_bytes: &[u8]) -> Result<(), String> {
    use image::{codecs::jpeg::JpegEncoder, imageops::FilterType};

    let img = image::load_from_memory(image_bytes)
        .map_err(|e| format!("无法解析图片(请用 JPG 或 PNG): {e}"))?;

    let bg = img.resize(2048, 2048, FilterType::Lanczos3);
    let preview = img.resize(640, 640, FilterType::Lanczos3);

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
            display_name_zh: "纸箱",
            display_name_en: "Carton",
            has_mascot: true,
            bg_fit: "cover",
            bg_position: "50% 18%",
            palette: Palette {
                ink: "#f5ece1",
                ink2: "rgba(245,236,225,0.74)",
                ink3: "rgba(245,236,225,0.56)",
                ink4: "rgba(245,236,225,0.40)",
                accent: "#ff5a36",
                accent_soft: "#ff9170",
                focus: "#ff7a4f",
                surface: "rgba(30,18,16,0.50)",
                glass: "rgba(30,18,16,0.60)",
                glass_soft: "rgba(30,18,16,0.52)",
                glass_strong: "rgba(24,14,13,0.78)",
                border: "rgba(255,228,210,0.14)",
                border_soft: "rgba(255,228,210,0.07)",
                border_strong: "rgba(255,228,210,0.26)",
                blur: "6px",
                hover: "rgba(255,228,210,0.10)",
                selection: "rgba(255,228,210,0.16)",
                scrim_top: "rgba(18,9,8,0.26)",
                scrim_mid: "rgba(18,9,8,0.34)",
                scrim_bot: "rgba(18,9,8,0.60)",
                base_color: "#120908",
            },
        },
        ThemeMeta {
            id: "changli",
            display_name_zh: "长离 (Changli)",
            display_name_en: "Changli",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "50% 4%",
            palette: Palette {
                ink: "#f4ebdf",
                ink2: "rgba(244,235,223,.74)",
                ink3: "rgba(244,235,223,.56)",
                ink4: "rgba(244,235,223,.40)",
                accent: "#e08a55",
                accent_soft: "#e6b48a",
                focus: "#ffce86",
                surface: "rgba(26,18,12,.50)",
                glass: "rgba(30,21,14,.60)",
                glass_soft: "rgba(34,24,16,.52)",
                glass_strong: "rgba(22,15,10,.78)",
                border: "rgba(255,228,201,.14)",
                border_soft: "rgba(255,228,201,.07)",
                border_strong: "rgba(255,228,201,.26)",
                blur: "6px",
                hover: "rgba(255,236,210,.10)",
                selection: "rgba(255,236,210,.16)",
                scrim_top: "rgba(18,12,8,.26)",
                scrim_mid: "rgba(17,11,7,.34)",
                scrim_bot: "rgba(11,7,5,.60)",
                base_color: "#160f0a",
            },
        },
        ThemeMeta {
            id: "azurlane",
            display_name_zh: "碧蓝航线",
            display_name_en: "Azur Lane",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
            palette: Palette {
                ink: "#eef2f8",
                ink2: "rgba(238,242,248,0.74)",
                ink3: "rgba(238,242,248,0.56)",
                ink4: "rgba(238,242,248,0.40)",
                accent: "#3fb6e8",
                accent_soft: "#8fd6f2",
                focus: "#5fc8f5",
                surface: "rgba(20,28,42,0.50)",
                glass: "rgba(20,28,42,0.60)",
                glass_soft: "rgba(20,28,42,0.52)",
                glass_strong: "rgba(20,28,42,0.78)",
                border: "rgba(206,224,244,0.14)",
                border_soft: "rgba(206,224,244,0.07)",
                border_strong: "rgba(206,224,244,0.26)",
                blur: "6px",
                hover: "rgba(206,224,244,0.10)",
                selection: "rgba(206,224,244,0.16)",
                scrim_top: "rgba(12,18,28,0.50)",
                scrim_mid: "rgba(12,18,28,0.56)",
                scrim_bot: "rgba(12,18,28,0.64)",
                base_color: "#0c121c",
            },
        },
        ThemeMeta {
            id: "nailin",
            display_name_zh: "奈琳",
            display_name_en: "Nailin",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
            palette: Palette {
                ink: "#f4ebdf",
                ink2: "rgba(244,235,223,0.74)",
                ink3: "rgba(244,235,223,0.56)",
                ink4: "rgba(244,235,223,0.40)",
                accent: "#ff7a33",
                accent_soft: "#ffb27d",
                focus: "#ff8f4d",
                surface: "rgba(26,18,12,0.50)",
                glass: "rgba(26,18,12,0.60)",
                glass_soft: "rgba(26,18,12,0.52)",
                glass_strong: "rgba(26,18,12,0.78)",
                border: "rgba(247,233,218,0.14)",
                border_soft: "rgba(247,233,218,0.07)",
                border_strong: "rgba(247,233,218,0.26)",
                blur: "6px",
                hover: "rgba(247,233,218,0.10)",
                selection: "rgba(247,233,218,0.16)",
                scrim_top: "rgba(10,6,4,0.41)",
                scrim_mid: "rgba(10,6,4,0.47)",
                scrim_bot: "rgba(10,6,4,0.55)",
                base_color: "#0a0604",
            },
        },
        ThemeMeta {
            id: "zani",
            display_name_zh: "扎妮",
            display_name_en: "Zani",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
            palette: Palette {
                ink: "#f1eef2",
                ink2: "rgba(241,238,242,0.74)",
                ink3: "rgba(241,238,242,0.56)",
                ink4: "rgba(241,238,242,0.40)",
                accent: "#d83a45",
                accent_soft: "#ec7b82",
                focus: "#f25661",
                surface: "rgba(26,22,28,0.50)",
                glass: "rgba(26,22,28,0.60)",
                glass_soft: "rgba(26,22,28,0.52)",
                glass_strong: "rgba(26,22,28,0.78)",
                border: "rgba(241,238,242,0.14)",
                border_soft: "rgba(241,238,242,0.07)",
                border_strong: "rgba(241,238,242,0.26)",
                blur: "6px",
                hover: "rgba(241,238,242,0.10)",
                selection: "rgba(241,238,242,0.16)",
                scrim_top: "rgba(16,12,18,0.44)",
                scrim_mid: "rgba(16,12,18,0.50)",
                scrim_bot: "rgba(16,12,18,0.58)",
                base_color: "#100c12",
            },
        },
        ThemeMeta {
            id: "frost",
            display_name_zh: "霜银",
            display_name_en: "Frost",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "50% 8%",
            palette: Palette {
                ink: "#eef1f7",
                ink2: "rgba(238,241,247,0.74)",
                ink3: "rgba(238,241,247,0.56)",
                ink4: "rgba(238,241,247,0.40)",
                accent: "#4f6cb0",
                accent_soft: "#8fa3d8",
                focus: "#6a86cc",
                surface: "rgba(20,24,36,0.50)",
                glass: "rgba(20,24,36,0.60)",
                glass_soft: "rgba(20,24,36,0.52)",
                glass_strong: "rgba(20,24,36,0.78)",
                border: "rgba(206,214,235,0.14)",
                border_soft: "rgba(206,214,235,0.07)",
                border_strong: "rgba(206,214,235,0.26)",
                blur: "6px",
                hover: "rgba(206,214,235,0.10)",
                selection: "rgba(206,214,235,0.16)",
                scrim_top: "rgba(14,16,24,0.43)",
                scrim_mid: "rgba(14,16,24,0.49)",
                scrim_bot: "rgba(14,16,24,0.57)",
                base_color: "#0e1018",
            },
        },
        ThemeMeta {
            id: "nocturne",
            display_name_zh: "夜合",
            display_name_en: "Nocturne",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
            palette: Palette {
                ink: "#eef1f7",
                ink2: "rgba(238,241,247,0.74)",
                ink3: "rgba(238,241,247,0.56)",
                ink4: "rgba(238,241,247,0.40)",
                accent: "#7cc5d6",
                accent_soft: "#a9dde9",
                focus: "#9bd6e4",
                surface: "rgba(20,26,32,0.50)",
                glass: "rgba(20,26,32,0.60)",
                glass_soft: "rgba(20,26,32,0.52)",
                glass_strong: "rgba(15,20,25,0.78)",
                border: "rgba(205,221,228,0.14)",
                border_soft: "rgba(205,221,228,0.07)",
                border_strong: "rgba(205,221,228,0.26)",
                blur: "6px",
                hover: "rgba(205,221,228,0.10)",
                selection: "rgba(124,197,214,0.16)",
                scrim_top: "rgba(13,17,21,0.41)",
                scrim_mid: "rgba(13,17,21,0.47)",
                scrim_bot: "rgba(13,17,21,0.55)",
                base_color: "#0d1115",
            },
        },
        ThemeMeta {
            id: "duet",
            display_name_zh: "相拥",
            display_name_en: "Duet",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "50% 4%",
            palette: Palette {
                ink: "#eef1f7",
                ink2: "rgba(238,241,247,0.74)",
                ink3: "rgba(238,241,247,0.56)",
                ink4: "rgba(238,241,247,0.40)",
                accent: "#32b1b7",
                accent_soft: "#7fd4d8",
                focus: "#46c8ce",
                surface: "rgba(16,21,36,0.50)",
                glass: "rgba(16,21,36,0.60)",
                glass_soft: "rgba(16,21,36,0.52)",
                glass_strong: "rgba(16,21,36,0.78)",
                border: "rgba(238,241,247,0.14)",
                border_soft: "rgba(238,241,247,0.07)",
                border_strong: "rgba(238,241,247,0.26)",
                blur: "6px",
                hover: "rgba(238,241,247,0.10)",
                selection: "rgba(238,241,247,0.16)",
                scrim_top: "rgba(10,13,22,0.40)",
                scrim_mid: "rgba(10,13,22,0.46)",
                scrim_bot: "rgba(10,13,22,0.54)",
                base_color: "#0a0d16",
            },
        },
        ThemeMeta {
            id: "rose",
            display_name_zh: "暖玫",
            display_name_en: "Rose",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center 60%",
            palette: Palette {
                ink: "#f5ebe6",
                ink2: "rgba(245,235,230,0.74)",
                ink3: "rgba(245,235,230,0.56)",
                ink4: "rgba(245,235,230,0.40)",
                accent: "#e8475a",
                accent_soft: "#f2899a",
                focus: "#ff5c70",
                surface: "rgba(32,21,25,0.50)",
                glass: "rgba(32,21,25,0.60)",
                glass_soft: "rgba(32,21,25,0.52)",
                glass_strong: "rgba(32,21,25,0.78)",
                border: "rgba(248,224,224,0.14)",
                border_soft: "rgba(248,224,224,0.07)",
                border_strong: "rgba(248,224,224,0.26)",
                blur: "6px",
                hover: "rgba(248,224,224,0.10)",
                selection: "rgba(248,224,224,0.16)",
                scrim_top: "rgba(18,11,14,0.36)",
                scrim_mid: "rgba(18,11,14,0.42)",
                scrim_bot: "rgba(18,11,14,0.50)",
                base_color: "#120b0e",
            },
        },
        ThemeMeta {
            id: "sonata",
            display_name_zh: "琴奏",
            display_name_en: "Sonata",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
            palette: Palette {
                ink: "#eef1f7",
                ink2: "rgba(238,241,247,0.74)",
                ink3: "rgba(238,241,247,0.56)",
                ink4: "rgba(238,241,247,0.40)",
                accent: "#6e83c4",
                accent_soft: "#9aa9d8",
                focus: "#8497d4",
                surface: "rgba(20,24,38,0.50)",
                glass: "rgba(20,24,38,0.60)",
                glass_soft: "rgba(20,24,38,0.52)",
                glass_strong: "rgba(20,24,38,0.78)",
                border: "rgba(216,224,242,0.14)",
                border_soft: "rgba(216,224,242,0.07)",
                border_strong: "rgba(216,224,242,0.26)",
                blur: "6px",
                hover: "rgba(216,224,242,0.10)",
                selection: "rgba(216,224,242,0.16)",
                scrim_top: "rgba(15,18,30,0.37)",
                scrim_mid: "rgba(15,18,30,0.43)",
                scrim_bot: "rgba(15,18,30,0.51)",
                base_color: "#0f121e",
            },
        },
        ThemeMeta {
            id: "studio",
            display_name_zh: "晴室",
            display_name_en: "Studio",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center top",
            palette: Palette {
                ink: "#f4ebdf",
                ink2: "rgba(244,235,223,0.74)",
                ink3: "rgba(244,235,223,0.56)",
                ink4: "rgba(244,235,223,0.40)",
                accent: "#e0a94e",
                accent_soft: "#f0c97d",
                focus: "#f2b75a",
                surface: "rgba(28,22,16,0.50)",
                glass: "rgba(28,22,16,0.60)",
                glass_soft: "rgba(28,22,16,0.52)",
                glass_strong: "rgba(28,22,16,0.78)",
                border: "rgba(244,235,223,0.14)",
                border_soft: "rgba(244,235,223,0.07)",
                border_strong: "rgba(244,235,223,0.26)",
                blur: "6px",
                hover: "rgba(244,235,223,0.10)",
                selection: "rgba(244,235,223,0.16)",
                scrim_top: "rgba(20,15,11,0.26)",
                scrim_mid: "rgba(20,15,11,0.34)",
                scrim_bot: "rgba(20,15,11,0.60)",
                base_color: "#1b1512",
            },
        },
    ];
    if custom_theme_exists() {
        v.push(ThemeMeta {
            id: CUSTOM_THEME_ID,
            display_name_zh: "自定义",
            display_name_en: "Custom",
            has_mascot: false,
            bg_fit: "cover",
            bg_position: "center",
            palette: NEUTRAL_PALETTE,
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
    // include_bytes! 必须用字面路径,所以每条 theme 显式 match。
    // 背景全图(bg.jpg)**不再嵌入 binary** —— 改 on-demand 从 storage 仓库下载(见
    // [`ensure_bg_data_uri`]),这里只嵌**缩略图** preview.jpg(+ carton mascot)。bg_data_uri
    // 先置缩略图作离线/下载失败兜底,apply 时被 [`ensure_bg_data_uri`] 覆盖成全图。
    let (mascot, preview_bytes): (Option<(&[u8], &str)>, &[u8]) = match theme_id {
        "carton" => (
            Some((
                include_bytes!("../resources/themes/carton/mascot.png"),
                "image/png",
            )),
            include_bytes!("../resources/themes/carton/preview.jpg"),
        ),
        "changli" => (
            None,
            include_bytes!("../resources/themes/changli/preview.jpg"),
        ),
        "azurlane" => (
            None,
            include_bytes!("../resources/themes/azurlane/preview.jpg"),
        ),
        "nailin" => (
            None,
            include_bytes!("../resources/themes/nailin/preview.jpg"),
        ),
        "zani" => (None, include_bytes!("../resources/themes/zani/preview.jpg")),
        "frost" => (
            None,
            include_bytes!("../resources/themes/frost/preview.jpg"),
        ),
        "nocturne" => (
            None,
            include_bytes!("../resources/themes/nocturne/preview.jpg"),
        ),
        "duet" => (None, include_bytes!("../resources/themes/duet/preview.jpg")),
        "rose" => (None, include_bytes!("../resources/themes/rose/preview.jpg")),
        "sonata" => (
            None,
            include_bytes!("../resources/themes/sonata/preview.jpg"),
        ),
        "studio" => (
            None,
            include_bytes!("../resources/themes/studio/preview.jpg"),
        ),
        _ => return None,
    };
    let preview_uri = encode_data_uri("image/jpeg", preview_bytes);
    Some(ThemeAssets {
        bg_data_uri: preview_uri.clone(),
        mascot_data_uri: mascot.map(|(b, m)| encode_data_uri(m, b)),
        preview_data_uri: preview_uri,
    })
}

fn encode_data_uri(mime: &str, bytes: &[u8]) -> String {
    use base64::{engine::general_purpose, Engine as _};
    format!(
        "data:{mime};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    )
}

const THEME_BG_BASE: &str =
    "https://raw.githubusercontent.com/Cmochance/codex-app-transfer-storage/main/img/theme";
/// 背景大图下载封顶(实际 0.3-1.1MB,封顶防异常超大响应 OOM)。
const MAX_BG_BYTES: u64 = 8 * 1024 * 1024;

/// 内置主题背景全图本地缓存:`~/.codex-app-transfer/theme-cache/<id>.jpg`。
fn theme_cache_path(theme_id: &str) -> Option<std::path::PathBuf> {
    codex_app_transfer_registry::paths::resolve_home().map(|h| {
        h.join(".codex-app-transfer")
            .join("theme-cache")
            .join(format!("{theme_id}.jpg"))
    })
}

/// 背景大图下载进度(theme_id → (downloaded, total))。前端轮询渲染缩略图上的进度环。
fn bg_progress() -> &'static std::sync::Mutex<std::collections::HashMap<String, (u64, u64)>> {
    static P: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (u64, u64)>>> =
        std::sync::OnceLock::new();
    P.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 查某主题当前下载进度。`None` = 没在下载(已缓存或未触发)。
pub fn bg_download_progress(theme_id: &str) -> Option<(u64, u64)> {
    bg_progress().lock().ok()?.get(theme_id).copied()
}

fn set_bg_progress(theme_id: &str, downloaded: u64, total: u64) {
    if let Ok(mut m) = bg_progress().lock() {
        m.insert(theme_id.to_owned(), (downloaded, total));
    }
}

fn clear_bg_progress(theme_id: &str) {
    if let Ok(mut m) = bg_progress().lock() {
        m.remove(theme_id);
    }
}

/// 取内置主题背景全图 data URI:先查本地缓存,没有就从 storage 下载(带进度)+ 缓存;
/// 失败(离线 / 404 等)回退 `fallback`(烤进 binary 的缩略图,低清但不 break)。
async fn ensure_bg_data_uri(theme_id: &str, fallback: &str) -> String {
    let cache = theme_cache_path(theme_id);
    if let Some(p) = &cache {
        if let Ok(bytes) = std::fs::read(p) {
            return encode_data_uri("image/jpeg", &bytes);
        }
    }
    match download_bg(theme_id).await {
        Ok(bytes) => {
            if let Some(p) = &cache {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // atomic:写 tmp 再 rename —— 中途被 kill / 写错只留半截 tmp,不会让
                // 后续 ensure_bg_data_uri 把残缺 <id>.jpg 当有效缓存返回坏图。
                let tmp = p.with_extension("jpg.tmp");
                if std::fs::write(&tmp, &bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, p);
                }
            }
            encode_data_uri("image/jpeg", &bytes)
        }
        Err(_) => fallback.to_owned(),
    }
}

/// 流式下载背景全图,边下边更新进度(给前端进度环);封顶防超大响应。
async fn download_bg(theme_id: &str) -> Result<Vec<u8>, String> {
    let url = format!("{THEME_BG_BASE}/{theme_id}.jpg");
    let resp = reqwest::Client::builder()
        // connect_timeout 让被 blackhole / 屏蔽的网络快速失败 → 立刻回退缩略图,
        // 不傻等整个 total timeout(否则 apply 卡几十秒不注入任何 CSS)。
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status().as_u16()));
    }
    let total = resp.content_length().unwrap_or(0);
    if total > MAX_BG_BYTES {
        return Err(format!("bg too large: {total}"));
    }
    set_bg_progress(theme_id, 0, total);
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                clear_bg_progress(theme_id);
                return Err(e.to_string());
            }
        };
        if buf.len() as u64 + chunk.len() as u64 > MAX_BG_BYTES {
            clear_bg_progress(theme_id);
            return Err("bg exceeds cap".to_owned());
        }
        buf.extend_from_slice(&chunk);
        set_bg_progress(theme_id, buf.len() as u64, total);
    }
    clear_bg_progress(theme_id);
    Ok(buf)
}

/// 主题注入状态(给前端展示)。
///
/// 序列化保留 PascalCase(serde 默认):前端 `frontend/src/pages/CodexSkinPage.vue` 状态
/// badge 用 `sObj.Applied` / `sObj.Failed` / `sObj === "Disabled"` 检
/// 查,跟枚举 variant 名严格对齐。修过一次 `rename_all = "snake_case"`
/// 让 badge 永远 falsy,见 PR #265 review。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    let mut assets =
        load_theme_assets(theme_id).ok_or_else(|| format!("unknown theme id: {theme_id}"))?;

    set_status(ThemeStatus::Applying).await;

    // 内置主题:背景全图 on-demand 下载 + 缓存(custom 用本地 disk bg,不覆盖)。
    // 此时 status 已是 Applying,前端轮询 bg-progress 在该主题缩略图上渲染进度环 + 白蒙版。
    if theme_id != CUSTOM_THEME_ID {
        assets.bg_data_uri = ensure_bg_data_uri(theme_id, &assets.preview_data_uri).await;
    }

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
    write.send(WsMessage::Text(msg.into())).await?;
    drain_until_response(&mut read, 1).await?;
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
    write.send(WsMessage::Text(msg.into())).await?;
    drain_until_response(&mut read, 1).await?;

    // 2. addScriptToEvaluateOnNewDocument — 每次 page navigate / reload 自动跑
    let script = build_inject_script(theme_id, assets);
    let (msg, _) = make_msg(
        2,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": script }),
    );
    write.send(WsMessage::Text(msg.into())).await?;
    drain_until_response(&mut read, 2).await?;

    // 3. Runtime.evaluate — 立即在当前 page 跑一次(addScriptToEvaluateOnNewDocument
    //    只对**未来**的 navigation 生效,当前 page 需要单独 evaluate)
    let (msg, _) = make_msg(
        3,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write.send(WsMessage::Text(msg.into())).await?;
    drain_until_response(&mut read, 3).await?;

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
    write.send(WsMessage::Text(msg.into())).await?;
    drain_until_response(&mut read, 1).await?;

    let _ = write.close().await;
    Ok(())
}

/// 拿 Codex Desktop 主窗口的 CDP webSocketDebuggerUrl。
/// 复用 plugin_unlocker 的 page-filter 思路:type=page + URL 含 `index.html` +
/// 不含 `avatar-overlay`(过滤宠物悬浮窗)。
/// pub(crate):MOC-204 quota injector 复用同一套 CDP 定位/收发工具。
pub(crate) async fn locate_main_window_ws(
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
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

pub(crate) fn make_msg(id: u64, method: &str, params: Value) -> (String, u64) {
    let body = json!({ "id": id, "method": method, "params": params }).to_string();
    (body, id)
}

/// drain CDP messages until we receive the response with the matching `expected_id`.
/// 等到 id==expected_id 的 CDP 响应(CDP 可能先发 event(无 `id` 字段)再发 response,
/// 所以必须 loop 跳过 event)。检查 response 的 `error` 字段,有错就返 Err。
/// overall_timeout = 8s,每条 read 最多等 500ms。
///
/// 返回 `Runtime.evaluate` 结果值(`result.result.value`,returnByValue 下是真实 JS 返回值;
/// 无则 None)—— caller 可 `?` 忽略,也可读回(MOC-230:回读活动 conversationId)。
pub(crate) async fn drain_until_response(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
    expected_id: u64,
) -> Result<Option<serde_json::Value>, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("CDP response timeout for id={}", expected_id));
        }
        match tokio::time::timeout(Duration::from_millis(500), read.next()).await {
            Ok(Some(Ok(WsMessage::Text(t)))) => {
                let val: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // event(no id) → skip
                if val.get("id").is_none() {
                    continue;
                }
                if val["id"].as_u64() != Some(expected_id) {
                    continue;
                }
                if let Some(err) = val.get("error") {
                    return Err(format!("CDP error for id={}: {}", expected_id, err));
                }
                if let Some(exception) = val.get("result").and_then(|r| r.get("exceptionDetails")) {
                    return Err(format!(
                        "CDP exception for id={}: {}",
                        expected_id, exception
                    ));
                }
                // Runtime.evaluate 结果:result.result.value(returnByValue 下为真实 JS 值)
                return Ok(val
                    .get("result")
                    .and_then(|r| r.get("result"))
                    .and_then(|r| r.get("value"))
                    .cloned());
            }
            Ok(Some(Ok(WsMessage::Binary(b)))) => {
                let _ = b;
                continue;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(format!("CDP read error: {}", e));
            }
            Ok(None) => {
                return Err("CDP connection closed".into());
            }
            Err(_) => {
                continue;
            }
        }
    }
}

/// Modular, design-token-driven Codex reskin. Surface tokens become per-theme
/// translucent glass (so token consumers like the collapsed-sidebar fly-out stay
/// opaque) while the big surface ELEMENTS are forced transparent so the bg image
/// shows; overlay modules get per-module frosted glass; the runtime `--color-*`
/// layer (settings cards, buttons, text, icons, hover) is overridden; the resize
/// handle + panel pseudo-elements are neutralised; the top-of-content fade is
/// forced always-on. Per-theme values come from [`ThemeMeta::palette`]; the
/// structure is synced from the tuned agent-theme injector. Legacy
/// codex-app-transfer selectors (browser-mode surfaces, panel dividers) are
/// merged in so older / browser windows stay covered. Placeholders filled below.
const CODEX_CSS_TEMPLATE: &str = r#":root{
--cl-ink:__INK__;--cl-ink-2:__INK2__;--cl-ink-3:__INK3__;--cl-ink-4:__INK4__;
--cl-surface:__SURFACE__;--cl-glass:__GLASS__;--cl-glass-soft:__GLASS_SOFT__;--cl-glass-strong:__GLASS_STRONG__;
--cl-border:__BORDER__;--cl-border-soft:__BORDER_SOFT__;--cl-border-strong:__BORDER_STRONG__;
--cl-blur:__BLUR__;--cl-hover:__HOVER__;--cl-selection:__SELECTION__;
--cl-scrim-top:__SCRIM_TOP__;--cl-scrim-mid:__SCRIM_MID__;--cl-scrim-bot:__SCRIM_BOT__;
}
html{
color-scheme:dark !important;
--color-token-main-surface-primary:var(--cl-surface) !important;
--color-token-side-bar-background:var(--cl-glass) !important;
--vscode-sideBar-background:var(--cl-glass) !important;
--color-token-editor-background:transparent !important;
--vscode-editor-background:transparent !important;
--color-token-terminal-background:rgba(0,0,0,.5) !important;
--color-token-bg-primary:var(--cl-glass) !important;
--color-token-bg-secondary:var(--cl-glass-soft) !important;
--color-token-bg-tertiary:var(--cl-glass-soft) !important;
--color-token-bg-fog:var(--cl-surface) !important;
--color-token-dropdown-background:var(--cl-glass-strong) !important;
--vscode-dropdown-background:var(--cl-glass-strong) !important;
--color-token-menu-background:var(--cl-glass-strong) !important;
--vscode-menu-background:var(--cl-glass-strong) !important;
--color-token-input-background:var(--cl-glass-soft) !important;
--vscode-input-background:var(--cl-glass-soft) !important;
--color-token-text-code-block-background:rgba(0,0,0,.40) !important;
--vscode-textCodeBlock-background:rgba(0,0,0,.40) !important;
--color-token-text-preformat-background:rgba(0,0,0,.40) !important;
--color-token-diff-surface:rgba(255,255,255,.04) !important;
--color-background-surface:var(--cl-surface) !important;
--color-background-surface-under:transparent !important;
--codex-base-surface:var(--cl-surface) !important;
--color-background-panel:var(--cl-glass-strong) !important;
--color-background-elevated-primary:var(--cl-glass-strong) !important;
--color-background-elevated-primary-opaque:var(--cl-glass-strong) !important;
--color-background-elevated-secondary:var(--cl-glass-soft) !important;
--color-background-elevated-secondary-opaque:var(--cl-glass-soft) !important;
--color-background-editor-opaque:var(--cl-glass-soft) !important;
--color-background-control:var(--cl-glass-soft) !important;
--color-background-control-opaque:var(--cl-glass-strong) !important;
--color-token-foreground:var(--cl-ink) !important;
--vscode-foreground:var(--cl-ink) !important;
--color-token-text-primary:var(--cl-ink) !important;
--color-token-text-secondary:var(--cl-ink-2) !important;
--color-token-text-tertiary:var(--cl-ink-3) !important;
--color-token-description-foreground:var(--cl-ink-3) !important;
--vscode-descriptionForeground:var(--cl-ink-3) !important;
--color-token-disabled-foreground:var(--cl-ink-4) !important;
--color-token-icon-foreground:var(--cl-ink-2) !important;
--vscode-icon-foreground:var(--cl-ink-2) !important;
--color-text-foreground:var(--cl-ink) !important;
--color-text-foreground-secondary:var(--cl-ink-2) !important;
--color-text-foreground-tertiary:var(--cl-ink-3) !important;
--color-text-button-secondary:var(--cl-ink) !important;
--color-text-button-tertiary:var(--cl-ink-3) !important;
--codex-base-ink:var(--cl-ink) !important;
--color-icon-primary:var(--cl-ink) !important;
--color-icon-secondary:var(--cl-ink-2) !important;
--color-icon-tertiary:var(--cl-ink-3) !important;
--color-token-border:var(--cl-border) !important;
--color-token-border-default:var(--cl-border) !important;
--color-token-border-light:var(--cl-border-soft) !important;
--color-token-border-heavy:var(--cl-border) !important;
--color-border:var(--cl-border) !important;
--color-border-light:var(--cl-border-soft) !important;
--color-border-heavy:var(--cl-border) !important;
--color-token-list-hover-background:var(--cl-hover) !important;
--color-token-list-active-selection-background:var(--cl-selection) !important;
--color-token-list-active-selection-foreground:var(--cl-ink) !important;
--color-token-toolbar-hover-background:var(--cl-hover) !important;
--vscode-list-hoverBackground:var(--cl-hover) !important;
--vscode-list-activeSelectionBackground:var(--cl-selection) !important;
--vscode-toolbar-hoverBackground:var(--cl-hover) !important;
--color-background-button-secondary-hover:var(--cl-hover) !important;
--color-background-button-tertiary-hover:var(--cl-hover) !important;
--color-token-scrollbar-slider-background:var(--cl-border) !important;
--color-token-scrollbar-slider-hover-background:var(--cl-border-strong) !important;
}
html.electron-light,html.electron-dark,html{
background:__BASECOLOR__ url('__HERO__') __POS__ / __FIT__ no-repeat fixed !important;
}
body{background:transparent !important;}
/* .main-surface 用类选择器、不加 main 元素限定:Codex settings/archive 页该容器是
   div.main-surface(整页无 main 元素),元素限定会漏匹配 → 主面板留半透明+圆角 →
   hover 局部重绘撕裂(MOC-247)。聊天页同名容器一并透明化是预期。
   注:本模板整体嵌进 JS 模板字符串(style.textContent = ...),禁止出现反引号。 */
#root > *,.app-shell,.app-shell-main,.main-surface,.app-shell-main-content-viewport,.app-shell-main-content-frame,[class~="electron:bg-token-main-surface-primary"]{background-color:transparent !important;}
html .main-surface{border-radius:0 !important;}
/* 可读性 scrim 折进 #root(normal-flow;再叠 position:fixed 层会让 backdrop-filter
   采样时 Page.captureScreenshot 死锁)。从 agent-theme 同步 3 层复合方案 —— 旧单层
   linear(mid 仅 ~0.34)对亮壁纸太弱、文字被壁纸透射压住:(1)顶部 ~42% 阻尼带平衡
   亮发/脸高光;(2)居中对话列焦点处(~50%/47%)径向加强,让正文落在更暗的值上;
   (3)基线 linear,mid 上提到 44% + 底部用 color-mix 在最深 --cl-scrim-bot 上加深。
   同 --cl-scrim-* 旋钮,只叠得更强;各主题 alpha 按壁纸亮度在 palette 里逐套校准。 */
#root{
background:
 linear-gradient(180deg,color-mix(in srgb,var(--cl-scrim-bot) 58%,transparent) 0%,transparent 42%),
 radial-gradient(135% 92% at 50% 47%,color-mix(in srgb,var(--cl-scrim-bot) 54%,transparent) 0%,transparent 70%),
 linear-gradient(180deg,var(--cl-scrim-top) 0%,var(--cl-scrim-mid) 44%,color-mix(in srgb,var(--cl-scrim-bot) 86%,transparent) 100%) !important;
}
html .app-shell-left-panel{
background:var(--cl-glass) !important;border-right:none !important;
-webkit-backdrop-filter:blur(var(--cl-blur)) saturate(118%);backdrop-filter:blur(var(--cl-blur)) saturate(118%);
}
html aside.fixed.bottom-0.left-0{
background:var(--cl-glass-strong) !important;
-webkit-backdrop-filter:blur(calc(var(--cl-blur) + 4px)) saturate(120%);backdrop-filter:blur(calc(var(--cl-blur) + 4px)) saturate(120%);
border:1px solid var(--cl-border-soft);box-shadow:0 14px 44px rgba(0,0,0,.55);
}
html .relative.flex.flex-col[class*="input-background"]{
background:var(--cl-glass-soft) !important;border:1px solid var(--cl-border-strong) !important;
box-shadow:0 10px 28px rgba(0,0,0,.5),inset 0 1px 0 rgba(255,255,255,.08) !important;
-webkit-backdrop-filter:blur(calc(var(--cl-blur) + 4px)) saturate(120%);backdrop-filter:blur(calc(var(--cl-blur) + 4px)) saturate(120%);
}
html [role="dialog"],html [role="menu"],html [role="listbox"],html [data-radix-menu-content],html .dialog-layout,html [data-browser-comment-editor-surface],html .bg-token-dropdown-background{
background-color:var(--cl-glass-strong) !important;
-webkit-backdrop-filter:blur(calc(var(--cl-blur) + 6px)) saturate(120%);backdrop-filter:blur(calc(var(--cl-blur) + 6px)) saturate(120%);
box-shadow:0 8px 24px rgba(0,0,0,.40) !important;
}
html .app-shell-left-panel::before,html .app-shell-left-panel::after,html .app-shell-main::before,html .app-shell-main::after,html .main-surface::before,html .main-surface::after,html .thread-root::before,html .thread-root::after,html .composer-root::before,html .composer-root::after,html .editor-container::before,html .editor-container::after{
background:transparent !important;background-image:none !important;box-shadow:none !important;mask:none !important;-webkit-mask:none !important;-webkit-mask-image:none !important;mask-image:none !important;filter:none !important;
}
html [role="separator"][aria-orientation="vertical"],html .sidebar-resize-handle-line,html [data-panel-resize-handle],html [data-panel-resize-handle-id],html [data-panel-group],html [data-resize-handle],html .split-pane-divider,html .app-shell-divider,html .resize-handle,html .resizable-handle{
background:transparent !important;background-image:none !important;box-shadow:none !important;border:none !important;
}
html .app-shell-main-content-top-fade{opacity:1 !important;}
html [container-name="home-main-content"]{text-shadow:0 1px 16px rgba(0,0,0,.6),0 0 2px rgba(0,0,0,.45);}
html .text-token-text-tertiary,html [class*="placeholder"]{text-shadow:0 1px 8px rgba(0,0,0,.6),0 0 2px rgba(0,0,0,.5);}
html [container-name="home-main-content"] [role="list"] [role="listitem"]{
background-color:rgba(0,0,0,.22);
-webkit-backdrop-filter:blur(var(--cl-blur)) saturate(115%);backdrop-filter:blur(var(--cl-blur)) saturate(115%);
text-shadow:0 1px 6px rgba(0,0,0,.55),0 0 1px rgba(0,0,0,.45);
}
html .vscode-markdown code,html .vscode-markdown pre,html .monaco-editor{text-shadow:none !important;}
__ACCENT_BLOCK__"#;

/// Accent-cohesion rules — emitted only when the theme's palette declares an accent.
const CODEX_ACCENT_BLOCK: &str = r#":root{--cl-accent:__ACCENT__;--cl-accent-soft:__ACCENT_SOFT__;--cl-focus:__FOCUS__;}
html{
--codex-base-accent:var(--cl-accent) !important;
--color-accent-blue:var(--cl-accent) !important;
--color-text-accent:var(--cl-accent) !important;
--color-icon-accent:var(--cl-accent) !important;
--color-token-primary:var(--cl-accent) !important;
--color-token-link:var(--cl-accent) !important;
--color-token-text-link-foreground:var(--cl-accent) !important;
--vscode-textLink-foreground:var(--cl-accent) !important;
--color-token-focus-border:var(--cl-focus) !important;
--color-border-focus:var(--cl-focus) !important;
--color-background-accent:color-mix(in srgb,var(--cl-accent) 18%,transparent) !important;
--color-background-accent-hover:color-mix(in srgb,var(--cl-accent) 24%,transparent) !important;
--color-background-accent-active:color-mix(in srgb,var(--cl-accent) 28%,transparent) !important;
}
html button[data-testid="composer-send-button"],html .composer-send-button{color:var(--cl-accent) !important;border-color:color-mix(in srgb,var(--cl-accent) 30%,transparent) !important;}
html button[data-testid="composer-send-button"]:not(:disabled),html .composer-send-button:not(:disabled){background:color-mix(in srgb,var(--cl-accent) 30%,transparent) !important;}
html button[data-testid="composer-send-button"]:not(:disabled) svg,html .composer-send-button:not(:disabled) svg{color:var(--cl-accent) !important;opacity:1 !important;}
::selection{background:color-mix(in srgb,var(--cl-accent) 30%,transparent);}"#;

/// Fill [`CODEX_CSS_TEMPLATE`] from a theme's [`Palette`] + background data URI.
fn render_theme_css(meta: Option<&ThemeMeta>, bg: &str, ov: Option<&PaletteOverride>) -> String {
    let (p, pos, fit) = match meta {
        Some(m) => (&m.palette, m.bg_position, m.bg_fit),
        None => (&NEUTRAL_PALETTE, "center top", "cover"),
    };
    // [MOC-272] 精选关键色:override 优先,否则基底常量。非精选角色(ink2-4 / border* / blur /
    // hover / selection)恒用基底。颜色已 sanitize 为 `#rrggbb`,无反引号 → 安全嵌入 CSS。
    let ink = ov
        .and_then(|o| o.ink.as_deref())
        .unwrap_or(p.ink)
        .to_string();
    let base_color = ov
        .and_then(|o| o.base_color.as_deref())
        .unwrap_or(p.base_color)
        .to_string();
    // accent:override 设则 accent/soft/focus 同色(启用 accent block);否则用基底三件。
    let (accent, accent_soft, focus) = match ov.and_then(|o| o.accent.as_deref()) {
        Some(a) => (a.to_string(), a.to_string(), a.to_string()),
        None => (
            p.accent.to_string(),
            p.accent_soft.to_string(),
            p.focus.to_string(),
        ),
    };
    // surface:override(hex)派生 surface + 3 层 glass(沿用 NEUTRAL 的 alpha 梯度,玻璃层一致);否则基底。
    let (surface, glass, glass_soft, glass_strong) =
        match ov.and_then(|o| o.surface.as_deref()).and_then(hex_to_rgb) {
            Some((r, g, b)) => (
                rgba(r, g, b, 0.50),
                rgba(r, g, b, 0.60),
                rgba(r, g, b, 0.52),
                rgba(r, g, b, 0.78),
            ),
            None => (
                p.surface.to_string(),
                p.glass.to_string(),
                p.glass_soft.to_string(),
                p.glass_strong.to_string(),
            ),
        };
    // scrim:override(0-100)→ 黑色三层渐变(深浅);否则基底三件。
    let (scrim_top, scrim_mid, scrim_bot) = match ov.and_then(|o| o.scrim) {
        Some(v) => {
            let f = (v.min(100) as f32) / 100.0;
            (
                rgba(0, 0, 0, f * 0.26),
                rgba(0, 0, 0, f * 0.40),
                rgba(0, 0, 0, f * 0.62),
            )
        }
        None => (
            p.scrim_top.to_string(),
            p.scrim_mid.to_string(),
            p.scrim_bot.to_string(),
        ),
    };
    let accent_block = if accent.is_empty() {
        String::new()
    } else {
        CODEX_ACCENT_BLOCK
            .replace("__ACCENT_SOFT__", &accent_soft)
            .replace("__FOCUS__", &focus)
            .replace("__ACCENT__", &accent)
    };
    CODEX_CSS_TEMPLATE
        .replace("__HERO__", bg)
        .replace("__BASECOLOR__", &base_color)
        .replace("__POS__", pos)
        .replace("__FIT__", fit)
        .replace("__INK2__", p.ink2)
        .replace("__INK3__", p.ink3)
        .replace("__INK4__", p.ink4)
        .replace("__INK__", &ink)
        .replace("__SURFACE__", &surface)
        .replace("__GLASS_STRONG__", &glass_strong)
        .replace("__GLASS_SOFT__", &glass_soft)
        .replace("__GLASS__", &glass)
        .replace("__BORDER_STRONG__", p.border_strong)
        .replace("__BORDER_SOFT__", p.border_soft)
        .replace("__BORDER__", p.border)
        .replace("__BLUR__", p.blur)
        .replace("__HOVER__", p.hover)
        .replace("__SELECTION__", p.selection)
        .replace("__SCRIM_TOP__", &scrim_top)
        .replace("__SCRIM_MID__", &scrim_mid)
        .replace("__SCRIM_BOT__", &scrim_bot)
        .replace("__ACCENT_BLOCK__", &accent_block)
}

/// 构造注入 script — per-theme CSS(见 [`render_theme_css`])+ 可选 mascot,
/// 包进 IIFE。沿用 `cat-theme-style` id + remove-then-create 切换语义。
fn build_inject_script(theme_id: &str, assets: &ThemeAssets) -> String {
    let metas = all_themes();
    let meta = metas.iter().find(|m| m.id == theme_id);
    // [MOC-272] 注入时读该主题的用户 palette override(config settings.themePaletteOverrides),
    // 合并到基底再渲 CSS。无 override → 完全用编译期基底(= 还原初始)。
    let ov = palette_override_for(theme_id);
    let css = render_theme_css(meta, &assets.bg_data_uri, ov.as_ref());

    let mascot_block = match &assets.mascot_data_uri {
        Some(m) => format!(
            r#"
    /* Floating Mascot (carton 主题专属) */
    .cat-theme-mascot {{
      position: fixed; bottom: 15px; right: 15px; width: 150px; height: 150px;
      background-image: url('{m}'); background-size: contain; background-repeat: no-repeat;
      background-position: bottom right; z-index: 9999; pointer-events: none;
      transition: transform 0.4s cubic-bezier(0.175, 0.885, 0.32, 1.275), opacity 0.3s ease;
      opacity: 0.85; filter: drop-shadow(0 4px 12px rgba(0,0,0,0.35));
    }}
"#
        ),
        None => String::new(),
    };

    let mascot_js = if assets.mascot_data_uri.is_some() {
        r#"
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
  // codex-app-transfer modular theme inject. theme={theme_id}
  // remove-then-create so apply 即时切换(不需 reload)。
  var oldStyle = document.getElementById('cat-theme-style');
  if (oldStyle) oldStyle.remove();
  var oldMascot = document.getElementById('cat-theme-mascot');
  if (oldMascot) oldMascot.remove();

  var style = document.createElement('style');
  style.id = 'cat-theme-style';
  style.setAttribute('data-cat-theme', '{theme_id}');
  style.textContent = `{css}
{mascot_block}`;
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
        // 跳过 custom — `all_themes()` 在 user 上传过自定义主题时会追加
        // 一条 `CUSTOM_THEME_ID` 项,本机有 ~/.codex-app-transfer/themes/
        // custom/bg.jpg 时直接比较会让测试随机挂。
        let metas: Vec<&str> = all_themes()
            .iter()
            .map(|m| m.id)
            .filter(|id| *id != CUSTOM_THEME_ID)
            .collect();
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
    fn per_theme_palette_and_converged_structure_in_script() {
        // 1) EVERY built-in theme must build a complete script —— 无未填占位符(`__`)
        //    + converged 引擎标记(runtime --color-* 层 / top-fade / 透明 main /
        //    style id)。覆盖全 11 套,关闭「未测主题可能带畸形 palette / 占位符 ship
        //    green」的防御 gap(code-reviewer MOC-97 IMPORTANT)。
        for id in THEME_IDS {
            let assets = load_theme_assets(id).unwrap_or_else(|| panic!("{id} assets must load"));
            let script = build_inject_script(id, &assets);
            assert!(!script.contains("__"), "{id} has unfilled placeholder");
            assert!(
                script.contains("--color-background-panel:var(--cl-glass-strong)"),
                "{id} missing settings-card override"
            );
            assert!(
                script.contains("--color-text-foreground:var(--cl-ink)"),
                "{id} missing runtime text override"
            );
            assert!(
                script.contains("app-shell-main-content-top-fade"),
                "{id} missing top-fade rule"
            );
            assert!(
                script.contains(".main-surface") && script.contains("cat-theme-style"),
                "{id} missing transparent main / style id"
            );
        }
        // 1.5) CSS 整体嵌进 build_inject_script 的 JS 模板字符串(`style.textContent = `...``),
        //      模板里任何反引号都会截断字符串 → 整个注入脚本 SyntaxError、主题 CSS 全不生效
        //      (MOC-247 review 实证)。守住 CSS 源里不得出现反引号。
        assert!(
            !CODEX_CSS_TEMPLATE.contains('`'),
            "CODEX_CSS_TEMPLATE must not contain backticks (embedded in a JS template literal)"
        );
        assert!(
            !CODEX_ACCENT_BLOCK.contains('`'),
            "CODEX_ACCENT_BLOCK must not contain backticks (embedded in a JS template literal)"
        );
        // 2) 代表性 spot-check:每套 thread 自己**独立**的 accent(证 per-theme palette
        //    接线正确,不是共享 accent)。
        for (id, accent) in [
            ("changli", "#e08a55"),
            ("frost", "#4f6cb0"),
            ("carton", "#ff5a36"),
            ("nocturne", "#7cc5d6"),
            ("rose", "#e8475a"),
        ] {
            let assets = load_theme_assets(id).unwrap();
            let script = build_inject_script(id, &assets);
            assert!(script.contains(accent), "{id} missing its accent {accent}");
        }
        // accent-less custom theme omits the accent block (keeps native blue)
        let custom_css = render_theme_css(None, "data:image/jpeg;base64,AA==", None);
        assert!(!custom_css.contains("composer-send-button"));
        assert!(custom_css.contains("--cl-surface"));
    }

    #[test]
    fn render_applies_palette_override() {
        // [MOC-272] override 的精选关键色合并进 CSS。
        let metas = all_themes();
        let carton = metas.iter().find(|m| m.id == "carton").unwrap();
        let ov = PaletteOverride {
            accent: Some("#abcdef".into()),
            ink: Some("#123456".into()),
            base_color: None,
            surface: Some("#0a0b0c".into()),
            scrim: Some(50),
        };
        let css = render_theme_css(Some(carton), "data:,x", Some(&ov));
        assert!(
            css.contains("--cl-accent:#abcdef"),
            "override accent applied"
        );
        assert!(css.contains("#123456"), "override ink applied");
        // surface(#0a0b0c = 10,11,12)派生玻璃层(带 alpha 梯度)。
        assert!(
            css.contains("rgba(10,11,12,0.50"),
            "override surface→glass derived"
        );
        // scrim=50 → 黑色半透明渐变(非基底 carton 的彩色 scrim)。
        assert!(
            css.contains("rgba(0,0,0,"),
            "override scrim → black gradient"
        );
    }

    #[test]
    fn palette_override_sanitize_drops_invalid_and_empty() {
        // [MOC-272] 非法 hex 丢弃、scrim clamp、全空 → None(= 还原)。
        let ov = PaletteOverride {
            accent: Some("red".into()),
            ink: Some("#aabbcc".into()),
            base_color: None,
            surface: None,
            scrim: Some(200),
        }
        .sanitized()
        .unwrap();
        assert!(ov.accent.is_none(), "invalid hex 'red' dropped");
        assert_eq!(ov.ink.as_deref(), Some("#aabbcc"));
        assert_eq!(ov.scrim, Some(100), "scrim clamped to 100");
        assert!(
            PaletteOverride::default().sanitized().is_none(),
            "empty → None"
        );
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
