//! Codex CLI 配置文件集成(Stage 2.5).
//!
//! 端口自 Python 端 `backend/registry.py` 中第 850-1300 行(`~/.codex/*` 相关)。
//! **不**负责 ChatGPT 桌面客户端的 plist / Windows 注册表注入(那是另一条线,
//! 留 Stage 2.5b)。
//!
//! 入口:
//! - [`apply_provider`]:把当前 active provider 写入 `~/.codex/config.toml` +
//!   `~/.codex/auth.json`(根级别 line-based 同步,保留用户其它字段)
//! - [`restore_codex_state`]:基于快照精确还原我们改过的 key,**不动**用户
//!   在我们运行期间手动加的内容
//! - [`snapshot_codex_state`]:首次 apply 前自动调一次,把原状态打包到
//!   `~/.codex-app-transfer/codex-snapshots/active/<session>/`
//! - [`ensure_file_store_mode`]:向 `~/.codex/config.toml` 写 / 删
//!   `mcp_oauth_credentials_store = "file"`,切换 Codex MCP OAuth 凭据存储模式
//!   (MOC-62 可移植保险箱开关)
//! - [`sync_mcp_credentials`]:让 `~/.codex-app-transfer/mcp-credentials.json` 镜像
//!   跟随 `~/.codex/.credentials.json`(捕获新授权 + 传播登出删除),**绝不写 live**;
//!   live 整文件缺失时只报告可恢复条数,由用户确认式 restore 处理(MOC-62)
//!
//! 路径解析全部走 [`CodexPaths`],测试可注入临时目录。

pub mod apply;
pub mod auth;
pub mod mcp_credentials;
pub mod model_catalog;
pub mod paths;
pub mod residual;
pub mod snapshot;
pub mod toml_sync;

pub use apply::{
    apply_provider, restore_codex_snapshot, restore_codex_state, restore_stale_codex_sessions,
    ApplyConfig, ApplyResult,
};
pub use auth::{read_auth, write_auth};
pub use mcp_credentials::{
    discard_mcp_mirror, ensure_file_store_mode, restore_available_count,
    restore_mcp_credentials_from_mirror, sync_mcp_credentials, SyncReport,
};
pub use model_catalog::{
    catalog_models_for_pool, catalog_models_for_provider,
    catalog_models_for_provider_with_display_names, strip_model_suffix, upsert_catalog_models,
    CatalogModel, PoolProviderMeta,
};
pub use paths::CodexPaths;
pub use residual::{
    detect_signatures_in_text, repair_residual_pollution, scan_residual_pollution,
    signature_fields_to_strip, MatchedSignature, PollutedFile, PollutionSourceKind, RepairReport,
    RepairedFile, ResidualScanReport,
};
pub use snapshot::{
    gc_trash_older_than, get_snapshot_status, has_snapshot, has_stale_active_snapshot,
    list_snapshots, snapshot_codex_state, SnapshotInfo, SnapshotManifest, SnapshotStatus,
    TRASH_RETENTION_DAYS,
};
pub use toml_sync::sync_root_value;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("registry io: {0}")]
    RegistryIo(#[from] codex_app_transfer_registry::IoError),
    #[error("home directory not resolved: set $HOME or pass paths explicitly")]
    NoHome,
    /// 通用领域错误,用于配置文件形态非法等场景 — 区别于
    /// `Json(serde_json::Error)`(解析失败)给上层"我故意拒绝处理"的语义。
    #[error("{0}")]
    Other(String),
}
