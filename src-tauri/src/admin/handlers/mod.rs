//! `/api/*` 路由 handler 模块.
//!
//! 拆分自原单文件 `handlers.rs`(5229 行),按域分子模块:
//! - `common`:跨域 helper + status/version + instance
//! - `proxy`:`/api/proxy/*`
//! - `feedback`:`/api/feedback`
//! - `settings`:`/api/settings` + `/api/config/*`
//! - `update`:`/api/update/*`
//! - `_legacy`:Round 2 待迁移的剩余函数(desktop / providers)

pub mod common;
pub mod feedback;
pub mod proxy;
pub mod settings;
pub mod update;

mod _legacy;
pub use _legacy::*;
