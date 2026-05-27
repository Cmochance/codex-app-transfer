//! `/api/diagnostic/*` — transfer 内部 silent 行为可视化(MOC-32 PR-2b)。
//!
//! 当前只有 `dropped_tools_status` — 返 adapter 累计 drop 计数 snapshot,让
//! user / maintainer 在前端 dashboard 看到 silently dropped 工具类型。Refs:
//! - PR-2 在 [`codex_app_transfer_adapters::warn_once_drop_tool`] 加 counter
//! - PR-2b 把 counter 暴露给前端(本文件)
//!
//! **Why 单独模块**: `proxy.rs` 已经有 proxy stats / logs 等,diagnostic 跟
//! proxy runtime 状态不同维度(diagnostic = 静态 silent drop counter,
//! proxy = runtime traffic);独立模块未来加更多 diagnostic endpoint(eg
//! dropped fields / unsupported provider features / 等)语义干净。

use axum::{response::IntoResponse, Json};
use serde_json::json;

/// `GET /api/diagnostic/dropped-tools` — 返本进程累计 drop 过的 Responses API
/// tool types 统计。
///
/// 返:`{ total: u32, by_type: { "tool_search": 31, ... } }`
/// - `total`: 所有 drop 累计次数(单 type 多次 +1)
/// - `by_type`: per-type 累计次数
///
/// **本进程生命周期累加,重启归零** — 跟 PR-2 `warn_once_drop_tool` counter
/// 语义一致。前端轮询本 endpoint(每 N 秒)可以实时展示。
pub async fn dropped_tools_status() -> impl IntoResponse {
    let snap = codex_app_transfer_adapters::dropped_tool_counters_snapshot();
    let total: u32 = snap.values().sum();
    let by_type: serde_json::Map<String, serde_json::Value> = snap
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::from(v)))
        .collect();
    Json(json!({
        "total": total,
        "by_type": by_type,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::Response;

    async fn body_json(resp: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        serde_json::from_slice(&body).expect("json parse")
    }

    #[tokio::test]
    async fn dropped_tools_status_returns_total_and_by_type_shape() {
        // 先 drop 几个 unique type 验 endpoint 返结构正确(global counter
        // 跨 test 共享,所以用 unique type name 避免 race + 同时验 by_type 含
        // 我们刚 drop 的 type)
        codex_app_transfer_adapters::warn_once_drop_tool("test_diag_endpoint_alpha");
        codex_app_transfer_adapters::warn_once_drop_tool("test_diag_endpoint_alpha");
        codex_app_transfer_adapters::warn_once_drop_tool("test_diag_endpoint_beta");

        let resp = dropped_tools_status().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        // 结构 sanity
        assert!(body.get("total").is_some(), "应有 total 字段");
        assert!(body.get("by_type").is_some(), "应有 by_type 字段");
        assert!(body["total"].as_u64().is_some(), "total 是 number");
        assert!(body["by_type"].is_object(), "by_type 是 object");

        // 我们刚 drop 的 type 应该在 by_type 里
        let by_type = &body["by_type"];
        assert_eq!(
            by_type["test_diag_endpoint_alpha"].as_u64(),
            Some(2),
            "alpha 应累计 2 次"
        );
        assert_eq!(
            by_type["test_diag_endpoint_beta"].as_u64(),
            Some(1),
            "beta 应累计 1 次"
        );
    }
}
