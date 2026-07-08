//! Admin server —— 把 v1.4 的 `/api/*` 路由 1:1 翻成 axum + 静态 frontend/.
//!
//! 设计:
//! - 不绑端口:通过 Tauri 的自定义 URI scheme(`cas://localhost/`)将 webview
//!   请求路由进 axum router(`tower::ServiceExt::oneshot`),全程同进程,无 TCP
//!   往返
//! - **`frontend/dist`(Vue/Vite 构建产物)通过 `include_dir!` 编进二进制**,首次加载零 IO
//! - `/api/*` 数据 shape 沿用 v1.4 契约,由 `frontend/src/api/*.ts`(Vue 重构后)消费
//!
//! 详见 docs/refactor/migration.md Stage 6 修订日志。

pub mod handlers;
pub mod registry_io;
pub mod services;
pub mod signature;
pub mod state;
pub mod static_files;

use axum::{
    routing::{get, post, put},
    Router,
};

pub use state::AdminState;

pub fn build_app_router(state: AdminState) -> Router {
    Router::new()
        // 单实例握手
        .route("/api/instance-info", get(handlers::common::instance_info))
        .route(
            "/api/instance-show-window",
            post(handlers::common::instance_show_window),
        )
        // [MOC-261 二-5] 旧综合状态聚合 /api/status 已删(死代码,见 common.rs 注释)
        // Chrome 就绪检查/按需下载 (MOC-144 headless 抓取后端)
        // [MOC-261 二-6] 旧 /api/chrome/detect 已删(冗余死端点,见 chrome.rs 注释)
        .route("/api/chrome/ready", get(handlers::chrome::ready))
        .route("/api/chrome/ensure", post(handlers::chrome::ensure))
        // 连接器市场(多源,phase2):官方源(私有 storage 仓库)+ 自加源聚合 + 图标代理
        .route(
            "/api/marketplace/connectors",
            get(handlers::marketplace_connectors::connectors),
        )
        .route(
            "/api/marketplace/icon",
            get(handlers::marketplace_connectors::icon),
        )
        .route(
            "/api/marketplace/sources",
            get(handlers::marketplace_connectors::sources),
        )
        .route(
            "/api/marketplace/sources/add",
            post(handlers::marketplace_connectors::add_source),
        )
        .route(
            "/api/marketplace/sources/remove",
            post(handlers::marketplace_connectors::remove_source),
        )
        .route(
            "/api/marketplace/sources/toggle",
            post(handlers::marketplace_connectors::toggle_source),
        )
        // Providers
        .route(
            "/api/providers",
            get(handlers::providers::crud::list_providers)
                .post(handlers::providers::crud::add_provider),
        )
        .route(
            "/api/providers/{id}",
            put(handlers::providers::crud::update_provider)
                .delete(handlers::providers::crud::delete_provider),
        )
        .route(
            "/api/providers/reorder",
            put(handlers::providers::crud::reorder_providers),
        )
        .route(
            "/api/providers/{id}/default",
            put(handlers::providers::crud::set_default_provider),
        )
        .route(
            "/api/providers/{id}/activate",
            post(handlers::providers::crud::activate_provider),
        )
        .route(
            "/api/providers/{id}/mimo-login",
            post(handlers::providers::crud::mimo_login),
        )
        .route(
            "/api/providers/{id}/opencode-login",
            post(handlers::providers::crud::opencode_login),
        )
        .route(
            "/api/providers/{id}/kimi-login",
            post(handlers::providers::crud::kimi_login),
        )
        .route(
            "/api/providers/{id}/secret",
            get(handlers::providers::crud::get_secret),
        )
        .route(
            "/api/providers/{id}/models/available",
            get(handlers::providers::models::fetch_provider_models),
        )
        .route(
            "/api/providers/test",
            post(handlers::providers::test::test_provider_payload),
        )
        .route(
            "/api/providers/models/available",
            post(handlers::providers::models::fetch_provider_models_payload),
        )
        // Presets
        .route(
            "/api/presets",
            get(handlers::providers::presets::list_presets),
        )
        // Desktop / Codex CLI
        .route("/api/desktop/clear", post(handlers::desktop::desktop_clear))
        // [MOC-277] superpowers 强约束插件:设置页开关 effective 默认 + 翻开关即时收敛
        .route(
            "/api/desktop/superpowers/status",
            get(handlers::desktop::superpowers_status),
        )
        .route(
            "/api/desktop/superpowers/reconcile",
            post(handlers::desktop::superpowers_reconcile),
        )
        .route(
            "/api/desktop/snapshots",
            get(handlers::desktop::desktop_snapshots),
        )
        .route(
            "/api/desktop/restore",
            post(handlers::desktop::desktop_restore),
        )
        .route(
            "/api/desktop/restart-codex-app",
            post(handlers::desktop::restart_codex_app),
        )
        .route(
            "/api/desktop/open-snapshot-dir",
            post(handlers::desktop::open_snapshot_dir),
        )
        .route(
            "/api/desktop/snapshot-status",
            get(handlers::desktop::desktop_snapshot_status),
        )
        .route(
            "/api/desktop/scan-residual",
            get(handlers::desktop::desktop_scan_residual),
        )
        .route(
            "/api/desktop/repair-residual",
            post(handlers::desktop::desktop_repair_residual),
        )
        // MOC-62 / 一-4:MCP 凭据"可移植保险箱"——文件丢失时逐条确认恢复 / 移除 / 忽略。
        .route(
            "/api/desktop/mcp-credentials/restore",
            post(handlers::desktop::mcp_credentials_restore),
        )
        .route(
            "/api/desktop/mcp-credentials/remove",
            post(handlers::desktop::mcp_credentials_remove),
        )
        .route(
            "/api/desktop/mcp-credentials/ignore",
            post(handlers::desktop::mcp_credentials_ignore),
        )
        .route(
            "/api/desktop/mcp-credentials/status",
            get(handlers::desktop::mcp_credentials_status),
        )
        // Conversation export (#271)
        .route(
            "/api/conversations/list",
            get(handlers::conversations::list_handler),
        )
        .route(
            "/api/conversations/{id}",
            get(handlers::conversations::detail_handler),
        )
        .route(
            "/api/conversations/export",
            post(handlers::conversations::export_handler),
        )
        .route(
            "/api/conversations/delete",
            post(handlers::conversations::delete_handler),
        )
        // [MOC-261 二-4] 清空会话历史(两者都清):全部 rollout 移回收站 + 清 proxy L2 续轮缓存。
        .route(
            "/api/conversations/clear-all",
            post(handlers::conversations::clear_all_handler),
        )
        // [CAT-255] 导入/恢复其他工具留下的隔离会话(第三方 model_provider 就地双向归一)。
        .route(
            "/api/codex-sessions/detect-foreign",
            get(handlers::codex_sessions::detect_foreign_handler),
        )
        .route(
            "/api/codex-sessions/import",
            post(handlers::codex_sessions::import_handler),
        )
        .route(
            "/api/codex-sessions/restore",
            post(handlers::codex_sessions::restore_handler),
        )
        // Token usage stats (#279, ccusage-vendored)
        .route("/api/usage/summary", get(handlers::usage::usage_summary))
        .route(
            "/api/usage/conversation/cache-series",
            get(handlers::usage::cache_series),
        )
        // Proxy lifecycle
        .route("/api/version", get(handlers::common::version))
        .route("/api/proxy/start", post(handlers::proxy::start_proxy))
        .route("/api/proxy/stop", post(handlers::proxy::stop_proxy))
        .route("/api/proxy/status", get(handlers::proxy::proxy_status))
        // [MOC-169] 诊断流量查看器开关
        .route(
            "/api/trace-viewer/start",
            post(handlers::trace_viewer::start_trace_viewer),
        )
        .route(
            "/api/trace-viewer/stop",
            post(handlers::trace_viewer::stop_trace_viewer),
        )
        .route(
            "/api/trace-viewer/status",
            get(handlers::trace_viewer::trace_viewer_status),
        )
        .route(
            "/api/trace-viewer/open",
            post(handlers::trace_viewer::open_trace_viewer),
        )
        .route(
            "/api/system-proxy/status",
            get(handlers::proxy::system_proxy_status),
        )
        .route(
            "/api/diagnostic/dropped-tools",
            get(handlers::diagnostic::dropped_tools_status),
        )
        .route("/api/proxy/logs", get(handlers::proxy::proxy_logs))
        .route(
            "/api/proxy/logs/clear",
            post(handlers::proxy::proxy_logs_clear),
        )
        .route(
            "/api/proxy/logs/open-dir",
            post(handlers::proxy::proxy_logs_open_dir),
        )
        // [MOC-261 二-4] 旧 /api/sessions/clear(只清 proxy L2 缓存、前端从未接)已并入
        // /api/conversations/clear-all(两者都清),独立端点移除。
        // Settings
        .route(
            "/api/settings",
            get(handlers::settings::get_settings).put(handlers::settings::save_settings),
        )
        // Update
        .route("/api/update/check", get(handlers::update::update_check))
        .route(
            "/api/update/install",
            post(handlers::update::update_install),
        )
        // Config 导出/导入(backup-now / backups-list 已移除:无 restore 端点 = 半截 UX,MOC-261)
        .route("/api/config/export", get(handlers::settings::export_config))
        .route(
            "/api/config/import",
            post(handlers::settings::import_config),
        )
        // Feedback
        .route("/api/feedback", post(handlers::feedback::submit_feedback))
        // 系统浏览器打开外部 URL(点赞/反馈链接等)
        .route("/api/open-url", post(handlers::common::open_url_handler))
        // Gemini CLI OAuth (login / status / logout)
        .merge(handlers::gemini_oauth::routes())
        // [MOC-257] 旧 CDP 注入解锁路由(/api/desktop/plugin-unlock/start|stop|status)已废弃,
        // 不再注册 —— 由下面的三态选择器接管同一命名空间。CDP daemon 代码保留但默认不启。
        // Real ChatGPT account detection for plugin mode (MOC-104)
        .merge(handlers::real_account::routes())
        // Three-state plugin unlock selector: off / synthetic / real (MOC-257)
        .merge(handlers::plugin_unlock_mode::routes())
        // Codex Desktop UI Theme (#264, 独立 toggle 不依赖 plugin_unlock)
        .merge(handlers::theme::routes())
        // Antigravity OAuth (login / status / logout / cancel)
        .merge(handlers::antigravity_oauth::routes())
        // z.ai / bigmodel GLM 账号登录 OAuth (login / status / logout / cancel,MOC-252 Stage 3)
        .merge(handlers::zai_oauth::routes())
        // Trae(字节 TRAE SOLO CN / Work CN)账号登录 OAuth(login / status / logout / cancel,CAT-257)
        .merge(handlers::trae_oauth::routes())
        // WorkBuddy(腾讯 CodeBuddy)账号登录 OAuth(login / status / logout / cancel)
        .merge(handlers::workbuddy_oauth::routes())
        .merge(handlers::qoder_oauth::routes())
        .merge(handlers::grok_build_oauth::routes())
        // Codex AGENTS.md(Agents tab):raw 全文编辑 + history/backup/restore + 路径管理。
        // [MOC-261 二-1] 旧受管块 marker 模式(status/preview/apply/rollback/clear)已删:
        // 被 raw 整文件编辑取代、前端零引用、无内部调用方,按死代码移除。
        .route(
            "/api/codex/agents-md/history",
            get(handlers::agents_md::history),
        )
        .route(
            "/api/codex/agents-md/paths",
            get(handlers::agents_md::list_paths),
        )
        .route(
            "/api/codex/agents-md/paths/add",
            post(handlers::agents_md::add_path),
        )
        .route(
            "/api/codex/agents-md/paths/remove",
            post(handlers::agents_md::remove_path),
        )
        .route(
            "/api/codex/agents-md/raw",
            get(handlers::agents_md::raw_get).post(handlers::agents_md::raw_write),
        )
        .route(
            "/api/codex/agents-md/backup",
            post(handlers::agents_md::backup),
        )
        .route(
            "/api/codex/agents-md/restore-raw",
            post(handlers::agents_md::restore_raw),
        )
        // [MOC-261 二-3] config.toml MCP 段旧「受管块」端点(mcp-toml/status|preview|apply|
        // rollback|clear|history)已删:MCP UI 走 mcp_servers CRUD + config/raw,整模块前端零引用、
        // 无内部调用方。共享 managed_block 服务保留(history 读取仍被 agents-md 用;写路径深剥离见 MOC-273)。
        // Memories MEMORY.md(Memories tab):raw 全文编辑 + history/backup/restore + 路径管理。
        // [MOC-261 二-2] 旧受管块 marker 模式(status/preview/apply/rollback/clear + 未路由 history)
        // 已删:被 raw 整文件编辑取代、前端零引用、无内部调用方,按死代码移除。
        .route(
            "/api/codex/memories-md/history",
            get(handlers::memories_md::history_raw),
        )
        .route(
            "/api/codex/memories-md/paths",
            get(handlers::memories_md::list_paths),
        )
        .route(
            "/api/codex/memories-md/paths/add",
            post(handlers::memories_md::add_path),
        )
        .route(
            "/api/codex/memories-md/paths/remove",
            post(handlers::memories_md::remove_path),
        )
        .route(
            "/api/codex/memories-md/raw",
            get(handlers::memories_md::raw_get).post(handlers::memories_md::raw_write),
        )
        .route(
            "/api/codex/memories-md/backup",
            post(handlers::memories_md::backup),
        )
        .route(
            "/api/codex/memories-md/restore-raw",
            post(handlers::memories_md::restore_raw),
        )
        // [MOC-261 一-9] Skills 目录级 tar.gz 快照(/skills/{list,backup,backups,restore})已删:
        // 前端未接 + 单文件 SKILL.md 备份(下面 skills-md)已覆盖常用需求,按死代码移除。
        // Skills SKILL.md raw 编辑 + 打开文件夹(新)
        .route(
            "/api/codex/skills-md/paths",
            get(handlers::skills_md::list_paths),
        )
        .route(
            "/api/codex/skills-md/raw",
            get(handlers::skills_md::raw_get).post(handlers::skills_md::raw_write),
        )
        .route(
            "/api/codex/skills-md/backup",
            post(handlers::skills_md::backup),
        )
        .route(
            "/api/codex/skills-md/restore-raw",
            post(handlers::skills_md::restore_raw),
        )
        .route(
            "/api/codex/skills-md/history",
            get(handlers::skills_md::history),
        )
        .route(
            "/api/codex/skills-md/reveal",
            post(handlers::skills_md::reveal),
        )
        // MCP — 结构化重做(Servers form + Plugins + Marketplace)
        .route(
            "/api/codex/mcp/servers",
            get(handlers::mcp::list_servers).post(handlers::mcp::upsert_server),
        )
        .route(
            "/api/codex/mcp/servers/delete",
            post(handlers::mcp::delete_server),
        )
        .route(
            "/api/codex/mcp/servers/backup",
            post(handlers::mcp::backup_servers),
        )
        .route(
            "/api/codex/mcp/servers/restore",
            post(handlers::mcp::restore_servers),
        )
        .route(
            "/api/codex/mcp/servers/history",
            get(handlers::mcp::history_servers),
        )
        .route(
            "/api/codex/mcp/config/raw",
            get(handlers::mcp::raw_get_config).post(handlers::mcp::raw_write_config),
        )
        .route("/api/codex/mcp/plugins", get(handlers::mcp::list_plugins))
        .route(
            "/api/codex/mcp/plugins/toggle",
            post(handlers::mcp::toggle_plugin),
        )
        .route(
            "/api/codex/mcp/plugins/uninstall",
            post(handlers::mcp::uninstall_plugin),
        )
        .route(
            "/api/codex/mcp/plugins/install",
            post(handlers::mcp::install_plugin),
        )
        .route(
            "/api/codex/mcp/plugins/icon",
            get(handlers::mcp::plugin_icon),
        )
        .route(
            "/api/codex/mcp/plugins/skill",
            get(handlers::mcp::plugin_skill),
        )
        // 静态文件兜底
        .fallback(static_files::serve_static)
        .with_state(state)
}
