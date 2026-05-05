//! Admin server —— 把 v1.4 的 `/api/*` 路由 1:1 翻成 axum + 静态 frontend/.
//!
//! 设计:
//! - 不绑端口:通过 Tauri 的自定义 URI scheme(`cas://localhost/`)将 webview
//!   请求路由进 axum router(`tower::ServiceExt::oneshot`),全程同进程,无 TCP
//!   往返
//! - **frontend/ 整目录通过 `include_dir!` 编进二进制**,首次加载零 IO
//! - `/api/*` 数据 shape **严格对齐 v1.4**(frontend/js/api.js 不需要任何改动)
//!
//! 详见 docs/migration-plan.md Stage 6 修订日志。

pub mod handlers;
pub mod registry_io;
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
        .route("/api/instance-info", get(handlers::instance_info))
        .route(
            "/api/instance-show-window",
            post(handlers::instance_show_window),
        )
        // 总状态
        .route("/api/status", get(handlers::status))
        // Providers
        .route(
            "/api/providers",
            get(handlers::list_providers).post(handlers::add_provider),
        )
        .route(
            "/api/providers/{id}",
            put(handlers::update_provider).delete(handlers::delete_provider),
        )
        .route("/api/providers/reorder", put(handlers::reorder_providers))
        .route(
            "/api/providers/{id}/default",
            put(handlers::set_default_provider),
        )
        .route(
            "/api/providers/{id}/activate",
            post(handlers::activate_provider),
        )
        .route("/api/providers/{id}/secret", get(handlers::get_secret))
        .route("/api/providers/{id}/draft", post(handlers::save_draft))
        .route("/api/providers/{id}/test", post(handlers::test_provider))
        .route(
            "/api/providers/{id}/usage",
            post(handlers::query_provider_usage),
        )
        .route("/api/providers/{id}/models", put(handlers::update_models))
        .route(
            "/api/providers/{id}/models/available",
            get(handlers::fetch_provider_models),
        )
        .route(
            "/api/providers/{id}/models/autofill",
            post(handlers::autofill_provider_models),
        )
        .route(
            "/api/providers/compatibility",
            get(handlers::provider_compatibility),
        )
        .route("/api/providers/test", post(handlers::test_provider_payload))
        .route(
            "/api/providers/models/available",
            post(handlers::fetch_provider_models_payload),
        )
        // Presets
        .route("/api/presets", get(handlers::list_presets))
        // Desktop / Codex CLI
        .route("/api/desktop/status", get(handlers::desktop_status))
        .route("/api/desktop/configure", post(handlers::desktop_configure))
        .route("/api/desktop/clear", post(handlers::desktop_clear))
        .route(
            "/api/desktop/snapshot-status",
            get(handlers::desktop_snapshot_status),
        )
        // Proxy lifecycle
        .route("/api/version", get(handlers::version))
        .route("/api/proxy/start", post(handlers::start_proxy))
        .route("/api/proxy/stop", post(handlers::stop_proxy))
        .route("/api/proxy/status", get(handlers::proxy_status))
        .route("/api/proxy/logs", get(handlers::proxy_logs))
        .route("/api/proxy/logs/clear", post(handlers::proxy_logs_clear))
        .route(
            "/api/proxy/logs/open-dir",
            post(handlers::proxy_logs_open_dir),
        )
        // Settings
        .route(
            "/api/settings",
            get(handlers::get_settings).put(handlers::save_settings),
        )
        // Update
        .route("/api/update/check", get(handlers::update_check))
        .route("/api/update/install", post(handlers::update_install))
        // Config
        .route("/api/config/backup", post(handlers::create_backup))
        .route("/api/config/backups", get(handlers::list_backups))
        .route("/api/config/export", get(handlers::export_config))
        .route("/api/config/import", post(handlers::import_config))
        // Feedback
        .route("/api/feedback", post(handlers::submit_feedback))
        // 静态文件兜底
        .fallback(static_files::serve_static)
        .with_state(state)
}
