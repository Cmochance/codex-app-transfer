//! Admin server —— 把 v1.4 的 `/api/*` 路由 1:1 翻成 axum + 静态 frontend/.
//!
//! 设计:
//! - 不绑端口:通过 Tauri 的自定义 URI scheme(`cas://localhost/`)将 webview
//!   请求路由进 axum router(`tower::ServiceExt::oneshot`),全程同进程,无 TCP
//!   往返
//! - **frontend/ 整目录通过 `include_dir!` 编进二进制**,首次加载零 IO
//! - `/api/*` 数据 shape **严格对齐 v1.4**(frontend/js/api.js 不需要任何改动)
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
        // 总状态
        .route("/api/status", get(handlers::common::status))
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
            "/api/providers/{id}/secret",
            get(handlers::providers::crud::get_secret),
        )
        .route(
            "/api/providers/{id}/draft",
            post(handlers::providers::crud::save_draft),
        )
        .route(
            "/api/providers/{id}/test",
            post(handlers::providers::test::test_provider),
        )
        .route(
            "/api/providers/{id}/usage",
            post(handlers::providers::balance::query_provider_usage),
        )
        .route(
            "/api/providers/{id}/models",
            put(handlers::providers::crud::update_models),
        )
        .route(
            "/api/providers/{id}/models/available",
            get(handlers::providers::models::fetch_provider_models),
        )
        .route(
            "/api/providers/{id}/models/autofill",
            post(handlers::providers::models::autofill_provider_models),
        )
        .route(
            "/api/providers/compatibility",
            get(handlers::providers::test::provider_compatibility),
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
        .route(
            "/api/desktop/status",
            get(handlers::desktop::desktop_status),
        )
        .route(
            "/api/desktop/configure",
            post(handlers::desktop::desktop_configure),
        )
        .route("/api/desktop/clear", post(handlers::desktop::desktop_clear))
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
        // MOC-62:MCP 凭据"可移植保险箱"——文件丢失时用户确认恢复 / 忽略。
        .route(
            "/api/desktop/mcp-credentials/restore",
            post(handlers::desktop::mcp_credentials_restore),
        )
        .route(
            "/api/desktop/mcp-credentials/discard",
            post(handlers::desktop::mcp_credentials_discard),
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
        .route("/api/sessions/clear", post(handlers::proxy::sessions_clear))
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
        // Config
        .route(
            "/api/config/backup",
            post(handlers::settings::create_backup),
        )
        .route("/api/config/backups", get(handlers::settings::list_backups))
        .route("/api/config/export", get(handlers::settings::export_config))
        .route(
            "/api/config/import",
            post(handlers::settings::import_config),
        )
        // Feedback
        .route("/api/feedback", post(handlers::feedback::submit_feedback))
        // Gemini CLI OAuth (login / status / logout)
        .merge(handlers::gemini_oauth::routes())
        // Plugin Unlock (CDP injection for Codex Desktop)
        .merge(handlers::plugin_unlock::routes())
        // Codex Desktop UI Theme (#264, 独立 toggle 不依赖 plugin_unlock)
        .merge(handlers::theme::routes())
        // Antigravity OAuth (login / status / logout / cancel)
        .merge(handlers::antigravity_oauth::routes())
        // Codex AGENTS.md 受管块管理(#24 / #25 Agents tab MVP, 借鉴 borawong/AiMaMi)
        .route(
            "/api/codex/agents-md/status",
            get(handlers::agents_md::status),
        )
        .route(
            "/api/codex/agents-md/preview",
            post(handlers::agents_md::preview),
        )
        .route(
            "/api/codex/agents-md/apply",
            post(handlers::agents_md::apply),
        )
        .route(
            "/api/codex/agents-md/rollback",
            post(handlers::agents_md::rollback),
        )
        .route(
            "/api/codex/agents-md/clear",
            post(handlers::agents_md::clear),
        )
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
        // MCP servers 受管块 (#24 #25 PR-A, config.toml TOML 变种)
        .route(
            "/api/codex/mcp-toml/status",
            get(handlers::mcp_toml::status),
        )
        .route(
            "/api/codex/mcp-toml/preview",
            post(handlers::mcp_toml::preview),
        )
        .route("/api/codex/mcp-toml/apply", post(handlers::mcp_toml::apply))
        .route(
            "/api/codex/mcp-toml/rollback",
            post(handlers::mcp_toml::rollback),
        )
        .route("/api/codex/mcp-toml/clear", post(handlers::mcp_toml::clear))
        .route(
            "/api/codex/mcp-toml/history",
            get(handlers::mcp_toml::history),
        )
        // Memories 受管块 (#25 ~/.codex/memories/MEMORY.md 层次化索引)
        .route(
            "/api/codex/memories-md/status",
            get(handlers::memories_md::status),
        )
        .route(
            "/api/codex/memories-md/preview",
            post(handlers::memories_md::preview),
        )
        .route(
            "/api/codex/memories-md/apply",
            post(handlers::memories_md::apply),
        )
        .route(
            "/api/codex/memories-md/rollback",
            post(handlers::memories_md::rollback),
        )
        .route(
            "/api/codex/memories-md/clear",
            post(handlers::memories_md::clear),
        )
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
        // Skills file-snapshot backup / restore (#24 #25 PR-B)
        .route(
            "/api/codex/skills/list",
            get(handlers::skills::list_handler),
        )
        .route(
            "/api/codex/skills/backup",
            post(handlers::skills::backup_handler),
        )
        .route(
            "/api/codex/skills/backups",
            get(handlers::skills::backups_handler),
        )
        .route(
            "/api/codex/skills/restore",
            post(handlers::skills::restore_handler),
        )
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
            "/api/codex/mcp/marketplace/sources",
            get(handlers::mcp::list_sources),
        )
        .route(
            "/api/codex/mcp/marketplace/sources/add",
            post(handlers::mcp::add_source),
        )
        .route(
            "/api/codex/mcp/marketplace/sources/remove",
            post(handlers::mcp::remove_source),
        )
        .route(
            "/api/codex/mcp/marketplace/sources/toggle",
            post(handlers::mcp::toggle_source),
        )
        .route(
            "/api/codex/mcp/marketplace/index",
            get(handlers::mcp::marketplace_index),
        )
        // 静态文件兜底
        .fallback(static_files::serve_static)
        .with_state(state)
}
