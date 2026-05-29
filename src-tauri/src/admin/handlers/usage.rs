//! `/api/usage/*` — 对话 token 用量统计 (#279).
//!
//! - `GET /api/usage/summary?tz=Asia/Shanghai` → 全量 [`UsageReport`](codex_app_transfer_usage_tracker::UsageReport)
//!   含 daily / by-model / by-conversation 三视图 + 顶部 KPI 总和。前端一次拉,本地切 view 不再请求。
//!
//! 数据源:复用 `crates/usage_tracker` 扫 `~/.codex/sessions/` rollout JSONL,解析层
//! 全 vendor 自 ryoppippi/ccusage(MIT)。详见 `crates/usage_tracker/src/lib.rs`
//! 顶部文档 + `vendored_ccusage/mod.rs` attribution。

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_usage_tracker as usage;
use serde::Deserialize;

use crate::admin::handlers::common::err;

#[derive(Debug, Deserialize, Default)]
pub struct UsageSummaryQuery {
    /// 时区(jiff `JiffTimeZone` 兼容,如 `Asia/Shanghai`)。
    /// None / 解析失败 → 走系统时区(对照 ccusage `aggregate.rs:97`)。
    pub tz: Option<String>,
}

pub async fn usage_summary(Query(query): Query<UsageSummaryQuery>) -> impl IntoResponse {
    let tz_owned = query.tz;
    // load_usage_report 内部扫 ~/.codex/sessions/ 全部 rollout 串行解析,
    // ~250 文件 1.2GB 在 release build 内实测 ~1-2s。
    // 用 spawn_blocking 避免阻塞 axum runtime;clone tz 到 String 让 closure 'static。
    match tokio::task::spawn_blocking(move || usage::load_usage_report(tz_owned.as_deref())).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(e)) => {
            // tracing 用 Debug 保留错误链(silent-failure-hunter PR #279 修),
            // 客户端面 Display message 仍精简
            tracing::error!(error = ?e, "usage_summary: load_usage_report failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load usage report failed: {e}"),
            )
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "usage_summary: spawn_blocking join failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("usage task join failed: {e}"),
            )
            .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CacheSeriesQuery {
    /// 对话 session_id(= usage report `by_conversation` 行的 `group`)。
    pub session: String,
}

/// `GET /api/usage/conversation/cache-series?session=<id>` — 该对话逐轮缓存命中
/// 分桶(≤10 根柱),供 Usage tab 点击命中率数字弹窗画直方图(#304)。点击才调用。
pub async fn cache_series(Query(query): Query<CacheSeriesQuery>) -> impl IntoResponse {
    let session = query.session;
    match tokio::task::spawn_blocking(move || usage::cache_series_for_conversation(&session)).await
    {
        Ok(Ok(buckets)) => Json(buckets).into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = ?e, "cache_series: load failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("load cache series failed: {e}"),
            )
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "cache_series: spawn_blocking join failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cache series task join failed: {e}"),
            )
            .into_response()
        }
    }
}
