//! Codex Desktop Plugins 解锁 daemon 单例(CDP 注入,MOC-100)。
//!
//! [MOC-257] 原 HTTP API(`/api/desktop/plugin-unlock/{status,start,stop,reinject}`)已废弃 —— 插件
//! 解锁改由三态选择器([`super::plugin_unlock_mode`],`/api/desktop/plugin-unlock/*`)接管同一命名空间。
//! 本文件仅保留 daemon 单例 [`get_service`],供 `main.rs` 退出时停 daemon + `settings` 的
//! `autoUnlockCodexPlugins` 设置变更时 start/stop(CDP 强制档,无 UI 入口、默认不启,见三态废弃说明)。

use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::codex_plugin_unlocker::PluginUnlockService;

static UNLOCK_SERVICE: OnceCell<Arc<PluginUnlockService>> = OnceCell::const_new();

/// 拿 OnceCell 内的解锁服务单例。`main.rs` 退出 hook 跟 `settings` 设置变更共享同一实例,
/// 避免各跑一份 daemon。
pub async fn get_service() -> Arc<PluginUnlockService> {
    UNLOCK_SERVICE
        .get_or_init(|| async { Arc::new(PluginUnlockService::with_defaults()) })
        .await
        .clone()
}
