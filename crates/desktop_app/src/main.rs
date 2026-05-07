//! Codex App Transfer 桌面 v3 — eframe + egui 原生主程序。
//!
//! W2 起步阶段:窗口 1024×700,7 页 placeholder,主题/语言切换。
//! W3+ 逐页填充功能。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod page;
mod theme;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 700.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Codex App Transfer"),
        ..Default::default()
    };
    eframe::run_native(
        "Codex App Transfer",
        opts,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
