//! eframe::App 主体。W2:左侧 nav + 中心 page + 主题/语言活态切换。

use eframe::egui;

use crate::i18n::{lookup_owned, Locale};
use crate::page::{self, Page};
use crate::theme::{self, ThemeName};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct App {
    pub active_page: Page,
    pub locale: Locale,
    pub theme: ThemeName,
    /// 上一帧应用的主题,用于检测切换并 ctx.set_style
    #[serde(skip)]
    last_applied_theme: Option<ThemeName>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            active_page: Page::Dashboard,
            locale: Locale::Zh,
            theme: ThemeName::Default,
            last_applied_theme: None,
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 系统中文字体探测(W2 stub):macOS 走 PingFang.ttc,
        // Windows 走 msyh.ttc,Linux 走 wqy-microhei。失败就用 egui 默认。
        // W7 切到 bundled font;现阶段 demo 测可以跑就行。
        try_install_system_cjk_font(&cc.egui_ctx);

        if let Some(storage) = cc.storage {
            if let Some(saved) = eframe::get_value::<App>(storage, eframe::APP_KEY) {
                return saved;
            }
        }
        Self::default()
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 主题切换 → set_style
        if self.last_applied_theme != Some(self.theme) {
            theme::apply(ctx, &self.theme.palette());
            self.last_applied_theme = Some(self.theme);
        }

        // 顶栏(W2 占位:暂只放标题与版本号)
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.heading("Codex App Transfer");
                ui.weak(format!(
                    "v{} · {}",
                    env!("CARGO_PKG_VERSION"),
                    self.theme.label()
                ));
            });
        });

        // 左侧 nav
        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(180.0)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                for &page in Page::ALL {
                    let label = lookup_owned(self.locale, page.nav_key());
                    if ui
                        .selectable_label(self.active_page == page, label)
                        .clicked()
                    {
                        self.active_page = page;
                    }
                }
            });

        // 中心 page
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(new_locale) =
                page::render(ui, self.active_page, self.locale, &mut self.theme)
            {
                self.locale = new_locale;
            }
        });
    }
}

/// 尝试在系统层面找一份 CJK 字体灌进 egui。失败就保持默认(显示豆腐块,
/// W7 用 bundled font 解决)。
fn try_install_system_cjk_font(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simhei.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
        ]
    };

    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            // egui 0.31 FontData::from_owned 接 bytes;TTC 取 index 0
            let mut data = egui::FontData::from_owned(bytes);
            data.tweak.scale = 1.0;
            data.index = 0; // 多语言 TTC 默认头一份就是中文

            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("system_cjk".into(), data.into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "system_cjk".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("system_cjk".into());
            ctx.set_fonts(fonts);
            return;
        }
    }
}
