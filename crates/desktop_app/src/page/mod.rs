//! Page 枚举 + 各 page placeholder。W2 起步:每 page 只放一行 t!() 文字 +
//! Settings page 有 Theme/Language 切换器。W3-W5 各 page 完整实装。

use eframe::egui;

use crate::i18n::Locale;
use crate::theme::ThemeName;

#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Dashboard,
    ProvidersAdd,
    Providers,
    Desktop,
    Proxy,
    Settings,
    Guide,
}

impl Default for Page {
    fn default() -> Self {
        Self::Dashboard
    }
}

impl Page {
    pub const ALL: &'static [Self] = &[
        Self::Dashboard,
        Self::Providers,
        Self::ProvidersAdd,
        Self::Desktop,
        Self::Proxy,
        Self::Settings,
        Self::Guide,
    ];

    /// 给 nav 显示的 i18n key
    pub fn nav_key(self) -> &'static str {
        match self {
            Self::Dashboard => "nav.dashboard",
            Self::Providers => "nav.providers",
            Self::ProvidersAdd => "providers.add",
            Self::Desktop => "nav.desktop",
            Self::Proxy => "nav.proxy",
            Self::Settings => "nav.settings",
            Self::Guide => "nav.guide",
        }
    }
}

pub mod dashboard;
pub mod desktop;
pub mod guide;
pub mod providers;
pub mod providers_add;
pub mod proxy;
pub mod settings;

/// W2 占位渲染:简单标题 + i18n 翻译 + "TODO Wn" 提示
pub fn placeholder(ui: &mut egui::Ui, locale: Locale, title_key: &str, todo_label: &str) {
    ui.add_space(8.0);
    ui.heading(crate::i18n::lookup_owned(locale, title_key));
    ui.add_space(4.0);
    ui.label(format!("(W2 placeholder · 完整实装在 {todo_label})"));
}

pub fn render(
    ui: &mut egui::Ui,
    page: Page,
    locale: Locale,
    theme: &mut ThemeName,
) -> Option<Locale> {
    match page {
        Page::Dashboard => dashboard::render(ui, locale),
        Page::Providers => providers::render(ui, locale),
        Page::ProvidersAdd => providers_add::render(ui, locale),
        Page::Desktop => desktop::render(ui, locale),
        Page::Proxy => proxy::render(ui, locale),
        Page::Guide => guide::render(ui, locale),
        Page::Settings => return settings::render(ui, locale, theme),
    }
    None
}
