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
#[cfg(target_os = "macos")]
use tauri::Manager;

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
        // [MOC-271] **核心修复(bug1)**:`.accessory` 模式下 macOS **不会自动**把 app 的窗口带到前台
        // —— 切到 `.accessory` 后若只做窗口级 `show()/set_focus()`,窗口会掉到其它 app 后面(「开 hideDock
        // 当场掉后面」;别的隐藏-Dock 软件没这问题,是我们少了这一步)。正解:切完**显式做 app 级激活**
        // (`activate_macos_app` = NSRunningApplication.activate(.activateAllWindows))把 app + 窗口一起带到
        // 前台。保持 `.accessory`(Dock 仍隐藏),不切 `.regular`、不置顶。
        // 参考 artlasovsky「Fine-Tuning macOS App Activation Behavior」:accessory app 靠显式 activate +
        // makeKeyAndOrderFront(Tauri `set_focus` 内部即此)即可正常到前。
        let was_visible = app
            .get_webview_window("main")
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        let _ = app.set_activation_policy(policy);
        if was_visible {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
            crate::activate_macos_app();
        }
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
