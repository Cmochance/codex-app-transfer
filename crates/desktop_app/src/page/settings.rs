//! Settings page —— W2 起步阶段:已实装 Theme + Language 切换器。
//! W3 完整化(端口 / 4 开关 / 兼容性 / 备份 / 反馈 / About / 检查更新)。

use eframe::egui;

use crate::i18n::{lookup_owned, Locale};
use crate::theme::ThemeName;

/// 返回 `Some(new_locale)` 表示 locale 在本帧被切换,`None` 表示无变化。
pub fn render(ui: &mut egui::Ui, locale: Locale, theme: &mut ThemeName) -> Option<Locale> {
    let mut new_locale: Option<Locale> = None;

    ui.add_space(8.0);
    ui.heading(lookup_owned(locale, "nav.settings"));
    ui.add_space(12.0);

    ui.label(format!("(W2 placeholder · 完整实装在 W3)"));
    ui.add_space(16.0);

    // 主题切换
    ui.horizontal(|ui| {
        ui.strong(lookup_owned(locale, "settings.theme"));
        ui.add_space(12.0);
        for &t in ThemeName::ALL {
            let selected = *theme == t;
            if ui.selectable_label(selected, t.label()).clicked() {
                *theme = t;
            }
        }
    });

    ui.add_space(8.0);

    // 语言切换
    ui.horizontal(|ui| {
        ui.strong(lookup_owned(locale, "settings.language"));
        ui.add_space(12.0);
        if ui
            .selectable_label(locale == Locale::Zh, Locale::Zh.label())
            .clicked()
        {
            new_locale = Some(Locale::Zh);
        }
        if ui
            .selectable_label(locale == Locale::En, Locale::En.label())
            .clicked()
        {
            new_locale = Some(Locale::En);
        }
    });

    ui.add_space(20.0);
    ui.separator();
    ui.add_space(8.0);
    ui.weak(format!(
        "i18n keys 总计:{} / 当前 locale:{} / 主题:{}",
        crate::i18n::KEY_COUNT,
        locale.code(),
        theme.label()
    ));

    new_locale
}
