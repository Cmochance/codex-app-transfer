use eframe::egui;

use crate::i18n::Locale;

pub fn render(ui: &mut egui::Ui, locale: Locale) {
    super::placeholder(ui, locale, "dashboard.title", "W3");
}
