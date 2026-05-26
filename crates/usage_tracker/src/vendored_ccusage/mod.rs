//! # Vendored from ryoppippi/ccusage (MIT, Copyright 2025 ryoppippi)
//!
//! Source repo: https://github.com/ryoppippi/ccusage
//! Upstream commit: 2b9599ca (ccusage v20.0.5,2026-05-26 拉取)
//! Upstream license: see `LICENSE` next to this file.
//!
//! 本目录所有 `.rs` 文件 vendor 自 ccusage Rust monorepo,出于以下原因 vendor 而非
//! Cargo 依赖:ccusage `crate` 仅声明 `[[bin]]` 无 `[lib]` target;本项目作为 Tauri
//! admin REST + HTML 输出,不需要 ccusage CLI/terminal/main/blocks/output 层。
//!
//! 修改原则:零业务逻辑改动 — vendor 后只调整 `crate::xxx` → `super::xxx` /
//! `crate::vendored_ccusage::xxx`,以及 `pub(crate)` → `pub`/`pub(super)`,不动算法。

// Vendored sub-modules — 见各 mod 顶部 `// upstream: ...` 标注源 file path。
//
// **不 vendor**(CLI 耦合或 Phase 1 不需要):
// - `cli.rs` / `commands/` / `main.rs` — CLI 入口
// - `summary.rs` / `progress.rs` / `output.rs` / `blocks.rs` — CLI 输出 / aggregation 上层
// - `cost.rs` / `pricing.rs` — Phase 2 cost 计算时再 vendor
// - `adapter/codex/loader.rs` / `aggregate.rs` — 依赖 `SharedArgs` / `progress`,本项目用
//   父 crate `loader.rs` / `aggregator.rs` 重写薄壳(算法 1:1 跟随 ccusage)

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod codex;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod date_utils;

#[allow(dead_code)]
pub mod error;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod fast;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod home;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod types;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod utils;
