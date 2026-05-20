//! Admin server services — 跨 handler 复用的纯逻辑 / file IO 抽象.
//!
//! 与 `handlers/` 区别:
//! - `handlers/`:HTTP 入口,parse query/body,调 services,返回 axum response
//! - `services/`:无 HTTP 概念,纯函数 + Result,可独立单测

pub mod managed_block;
pub mod skills_backup;
