//! 主题系统:7 主题(default/green/orange/gray/dark/white)+ dark 内置变体。
//!
//! 颜色字面量从 `frontend/css/style.css` 17 个 CSS `--xxx` 变量逐字搬过来。
//! W2 起步只实现 **default + dark** 两套色板加一个切换通路,W3 把剩余 5 套
//! 填充并做 A/B 截图给用户审(决策点 W2-A)。

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};

#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ThemeName {
    Default,
    Green,
    Orange,
    Gray,
    Dark,
    White,
}

impl ThemeName {
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::Green,
        Self::Orange,
        Self::Gray,
        Self::Dark,
        Self::White,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Green => "Green",
            Self::Orange => "Orange",
            Self::Gray => "Gray",
            Self::Dark => "Dark",
            Self::White => "White",
        }
    }
    pub fn palette(self) -> Palette {
        match self {
            Self::Default => Palette::DEFAULT,
            Self::Green => Palette::GREEN,
            Self::Orange => Palette::ORANGE,
            Self::Gray => Palette::GRAY,
            Self::Dark => Palette::DARK,
            Self::White => Palette::WHITE,
        }
    }
}
impl Default for ThemeName {
    fn default() -> Self {
        Self::Default
    }
}

/// 17 字段映射 style.css 17 个 `--xxx` 变量。
#[derive(Copy, Clone, Debug)]
pub struct Palette {
    pub app_bg: Color32,
    pub surface: Color32,
    pub soft_surface: Color32,
    pub line: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub primary: Color32,
    pub primary_soft: Color32,
    pub success: Color32,
    pub success_soft: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub shadow_alpha: u8, // 控制 shadow 强度,简化 CSS box-shadow 一个数值
    pub radius: f32,
    pub is_dark: bool,
}

impl Palette {
    /// `:root` 默认主题(浅色蓝主基调,与 style.css 第 1-15 行一致)
    pub const DEFAULT: Self = Self {
        app_bg: rgb(0xf6, 0xf8, 0xfb),
        surface: rgb(0xff, 0xff, 0xff),
        soft_surface: rgb(0xee, 0xf3, 0xf9),
        line: rgb(0xdb, 0xe3, 0xef),
        text: rgb(0x0f, 0x17, 0x2a),
        muted: rgb(0x47, 0x55, 0x69),
        primary: rgb(0x25, 0x63, 0xeb),
        primary_soft: rgb(0xdb, 0xea, 0xfe),
        success: rgb(0x11, 0xb5, 0x6d),
        success_soft: rgb(0xe8, 0xf9, 0xf1),
        danger: rgb(0xef, 0x38, 0x3f),
        warning: rgb(0xf0, 0xbd, 0x12),
        shadow_alpha: 20,
        radius: 18.0,
        is_dark: false,
    };

    /// 暗色(style.css 第 14-25 行 dark mode 变量)
    pub const DARK: Self = Self {
        app_bg: rgb(0x0f, 0x17, 0x2a),
        surface: rgb(0x17, 0x20, 0x33),
        soft_surface: rgb(0x1e, 0x29, 0x3b),
        line: rgb(0x33, 0x41, 0x55),
        text: rgb(0xf1, 0xf5, 0xf9),
        muted: rgb(0xcb, 0xd5, 0xe1),
        primary: rgb(0x3b, 0x82, 0xf6),
        primary_soft: Color32::from_rgba_premultiplied(59, 130, 246, 46),
        success: rgb(0x11, 0xb5, 0x6d),
        success_soft: Color32::from_rgba_premultiplied(17, 181, 109, 41),
        danger: rgb(0xef, 0x38, 0x3f),
        warning: rgb(0xf0, 0xbd, 0x12),
        shadow_alpha: 80,
        radius: 18.0,
        is_dark: true,
    };

    // W2 占位:其它 5 套等 W3 决策点再调精。先复用 DEFAULT/DARK 让切换链通,
    // 视觉 A/B 时填充各自调色板。

    /// 绿色系(W3 填充)
    pub const GREEN: Self = Self {
        primary: rgb(0x10, 0x99, 0x59),
        primary_soft: rgb(0xd1, 0xfa, 0xe5),
        ..Self::DEFAULT
    };
    /// 橙色系(W3 填充)
    pub const ORANGE: Self = Self {
        primary: rgb(0xea, 0x77, 0x0a),
        primary_soft: rgb(0xff, 0xed, 0xd5),
        ..Self::DEFAULT
    };
    /// 灰系(W3 填充)
    pub const GRAY: Self = Self {
        primary: rgb(0x52, 0x52, 0x52),
        primary_soft: rgb(0xe5, 0xe5, 0xe5),
        muted: rgb(0x73, 0x73, 0x73),
        ..Self::DEFAULT
    };
    /// 纯白系(W3 填充)
    pub const WHITE: Self = Self {
        app_bg: rgb(0xff, 0xff, 0xff),
        surface: rgb(0xff, 0xff, 0xff),
        soft_surface: rgb(0xfa, 0xfa, 0xfa),
        line: rgb(0xe5, 0xe5, 0xe5),
        ..Self::DEFAULT
    };
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// 把 Palette 应用到 egui::Style/Visuals。
pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    v.dark_mode = p.is_dark;
    v.window_fill = p.app_bg;
    v.panel_fill = p.app_bg;
    v.faint_bg_color = p.soft_surface;
    v.extreme_bg_color = p.surface;
    v.code_bg_color = p.soft_surface;
    v.override_text_color = Some(p.text);
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_rgba_premultiplied(0, 0, 0, p.shadow_alpha),
    };
    let r = p.radius.round().clamp(0.0, 255.0) as u8;
    v.window_corner_radius = CornerRadius::same(r);
    v.menu_corner_radius = CornerRadius::same((r as f32 * 0.66).round() as u8);

    // 主操作色 → primary
    v.selection.bg_fill = p.primary;
    v.selection.stroke = Stroke::new(1.0, p.primary);
    v.hyperlink_color = p.primary;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;

    // 按钮 / 控件背景层
    v.widgets.noninteractive.bg_fill = p.surface;
    v.widgets.noninteractive.weak_bg_fill = p.soft_surface;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.inactive.bg_fill = p.soft_surface;
    v.widgets.inactive.weak_bg_fill = p.soft_surface;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.hovered.bg_fill = p.primary_soft;
    v.widgets.hovered.weak_bg_fill = p.primary_soft;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.primary);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.primary);
    v.widgets.active.bg_fill = p.primary;
    v.widgets.active.weak_bg_fill = p.primary;
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.primary);

    ctx.set_style(style);
}
