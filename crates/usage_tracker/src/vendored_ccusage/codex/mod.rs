//! Codex CLI rollout adapter. vendored from `rust/crates/ccusage/src/adapter/codex/`.

// loader.rs / aggregate.rs 不 vendor — ccusage 上游版本依赖 `cli::SharedArgs` /
// `progress` 这些 CLI 层,本项目用 HTTP 入参,改写 wrapper 比 patch vendor 干净。
// 本项目自有的 loader / aggregator 在父 crate `crates/usage_tracker/src/{loader,aggregator}.rs`。

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod parser;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod paths;

#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod types;
