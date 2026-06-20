//! Codex Desktop Plugins 解锁 daemon 单例(CDP 注入,MOC-100)。
//!
//! [MOC-257] 原 HTTP API(`/api/desktop/plugin-unlock/{status,start,stop,reinject}`)已废弃 —— 插件
//! 解锁改由三态选择器([`super::plugin_unlock_mode`],`/api/desktop/plugin-unlock/*`)接管同一命名空间。
//!
//! 本文件仅保留 daemon 单例 [`get_service`]。**保留是为了兼容老配置 + 满足现有生命周期引用,不是当作
//! 长期功能维护**:CDP 注入由 legacy 设置 `autoUnlockCodexPlugins`(MOC-100 强制档)驱动,而 `main.rs`
//! (退出时 stop / 重启 Codex 后 reinject)与 `settings`(该设置变更时 start/stop)仍引用此单例,故不能
//! 直接删。该 legacy 档**正在退役**:前端无入口、默认配置不 seed、对已迁移用户(有 `pluginUnlockMode`)
//! 主动清除残留键 —— 新机制(三态的 synthetic 走 proxy 逐条伪造 / real 走真账号 relay)完全不走 CDP。

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
