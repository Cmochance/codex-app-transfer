//! 「隐藏程序坞图标」(macOS)。
//!
//! 设置 `hideDockIcon` 开启时,把 NSApplication 的 activation policy 切成
//! **Accessory** —— app 不再占用程序坞(Dock)位置、不出现在 Cmd-Tab,但**菜单栏
//! tray 图标仍可唤起窗口**(tray 在 [`crate`] main.rs 已建)。关闭回 **Regular**。
//!
//! 走 Tauri `AppHandle::set_activation_policy`(内部 dispatch 主线程),非 macOS 全 no-op。
//! startup(读持久化设置)+ save_settings hot-reload(用户当场 toggle)共用 [`apply_from_settings`]。

use std::sync::OnceLock;

use tauri::AppHandle;

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// setup 时存全局 AppHandle —— 运行时 save_settings hot-reload 切策略时取用
/// (AdminState 建 router 时尚无 AppHandle,跟 mimo_session 同走全局 OnceLock)。
pub fn init(handle: AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

/// 应用 Dock 图标显隐:`hidden=true` → Accessory(无 Dock / 仅菜单栏),`false` → Regular。
pub fn apply(hidden: bool) {
    #[cfg(target_os = "macos")]
    if let Some(app) = APP_HANDLE.get() {
        let policy = if hidden {
            tauri::ActivationPolicy::Accessory
        } else {
            tauri::ActivationPolicy::Regular
        };
        let _ = app.set_activation_policy(policy);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = hidden;
}

/// 从 settings JSON 读 `hideDockIcon` 并应用(startup + save_settings hot-reload 共用)。
pub fn apply_from_settings(settings: &serde_json::Value) {
    apply(
        settings
            .get("hideDockIcon")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    )
}
