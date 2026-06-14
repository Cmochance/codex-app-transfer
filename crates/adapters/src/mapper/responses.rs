//! [MOC-234] `responses ↔ responses` 1:1 直透 mapper。
//!
//! 这是把**原生 OpenAI Responses API 上游**纳入统一 `mapper` 框架的薄映射器:
//! 请求侧与响应侧都是 **1:1 字节直透**(同协议,无转换),但建在
//! `RequestMapper` / `ResponseMapper` trait 上,与 chat / gemini / anthropic 等
//! 转换 mapper 结构对齐 —— 让原生 Responses 流量也跑进 canonical 转发管线,便于
//! 在**一处**统一挂载只读整合(context breakdown / session 观测 / 埋点)。
//!
//! 适用:`apiFormat == "responses" | "openai_responses"` 且入站 `/responses` /
//! `/responses/*` / `/messages` / `/messages/*`(见 `registry::lookup_for_request`)。
//!
//! ## 与 `mapper::chat`(`ResponsesAdapter`)的本质区别
//! `chat` 做 Responses → Chat 协议翻译(状态机重写 SSE envelope);本 mapper 假设
//! 上游**原生实现 Responses API**(OpenAI 官方 / 忠实中转的反代),请求体与响应流
//! 全部原样转发,envelope / `sequence_number` / `previous_response_id` session 均由
//! 上游产生与管理,代理不重写、不重建。
//!
//! ## 硬约束(MOC-234):Codex 自有 / 上游原生能力不接管
//! `compact`(`/responses/compact` 与 v2 `compaction_trigger`)、`web_search`、MCP
//! `namespace` 工具包等都**原样 1:1 直透原生上游**:
//! - `is_compact = false` 恒定 —— 绝不走本项目本地 `compact.rs` 包装;
//! - 不剥 / 不注 `web_search`,不触发 forward 层的 web_search transparent retry;
//! - 不展平 namespace,不改 tool 定义。
//! 接进这些本项目资产会让原生上游的体验降级,故一律不碰。
//!
//! ## Session
//! `response_session = None` —— 透传场景上游自管 `previous_response_id`,代理不写
//! 也不读本项目的 chat 形 `ResponseSessionCache`(形状不同,混写会被 chat 路径读坏)。
//! 后续若要为 Usage / 上下文面板做 by-source 明细,走**独立的只读观测镜像**,
//! 绝不把重建历史回注请求(见 MOC-234 Step 3)。

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use http::{HeaderMap, StatusCode};

use crate::mapper::{RequestMapper, ResponseMapper};
use crate::registry::rewrite_local_path_for_upstream;
use crate::types::{AdapterError, ByteStream, RequestPlan, ResponsePlan};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResponsesPassthroughMapper;

impl RequestMapper for ResponsesPassthroughMapper {
    fn map_request(
        &self,
        client_path: &str,
        body: Bytes,
        _provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        // 路径 normalize:剥 `/openai` legacy prefix + `/claude/v1/messages` alias +
        // 前导 `/v1`(provider.base_url 已带 `/v1`)+ 保 query。**不能**只剥 `/v1`,
        // 否则 `/openai/v1/responses` 透传成 `…/v1/openai/v1/responses` → 上游 404。
        Ok(RequestPlan {
            upstream_path: rewrite_local_path_for_upstream(client_path),
            // 1:1 字节直透:model 已由 forward.rs 在 adapter 前 rewrite/strip,
            // 此处不再改写任何字段(compact / web_search / namespace 全部原样)。
            body,
            upstream_headers: HeaderMap::new(),
            // 上游自管 session,不写本项目 chat 形 cache(见模块 doc)。
            response_session: None,
            adapter_metadata: None,
            // 恒 false:compact 原样直透原生上游,绝不走本地 compact.rs 包装(MOC-234)。
            is_compact: false,
            compact_v2: false,
            // 透传响应已是 Responses 形态,无需 envelope replay,留 None。
            original_responses_request: None,
        })
    }
}

impl ResponseMapper for ResponsesPassthroughMapper {
    fn map_response(
        &self,
        upstream_status: StatusCode,
        upstream_headers: HeaderMap,
        upstream_stream: ByteStream,
        _provider: &Provider,
        _request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        // 1:1 直透:status / headers / 流原样回灌。**不强制** content-type ——
        // 与 chat 等转换 mapper 不同,透传上游可能返回非 SSE 的合法响应(`stream:false`
        // 的 JSON、`/responses/compact` v1 非流式、`/responses/{id}/cancel` 等),
        // 强制 `text/event-stream` 会破坏这些响应。上游已按 Responses 协议给正确
        // content-type,忠实保留。
        Ok(ResponsePlan {
            status: upstream_status,
            headers: upstream_headers,
            stream: upstream_stream,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use http::header::{CONTENT_TYPE, TRANSFER_ENCODING};
    use indexmap::IndexMap;

    fn dummy_provider() -> Provider {
        Provider {
            id: "dummy".into(),
            name: "dummy".into(),
            base_url: "https://api.openai.com/v1".into(),
            auth_scheme: "bearer".into(),
            api_format: "responses".into(),
            api_key: "k".into(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        }
    }

    #[test]
    fn request_is_byte_level_1to1() {
        let body = Bytes::from_static(
            br#"{"model":"gpt-5.5","input":[],"tools":[{"type":"web_search"}],"stream":true}"#,
        );
        let plan = ResponsesPassthroughMapper
            .map_request("/v1/responses", body.clone(), &dummy_provider())
            .unwrap();
        assert_eq!(plan.body, body, "body 必须字节级 1:1,不改写任何字段");
        assert_eq!(plan.upstream_path, "/responses");
    }

    #[test]
    fn request_keeps_compact_native_never_local_wrapping() {
        // MOC-234 约束:compact 端点 1:1 直透原生上游,is_compact 恒 false。
        for path in [
            "/responses/compact",
            "/v1/responses/compact",
            "/openai/v1/responses/compact",
        ] {
            let plan = ResponsesPassthroughMapper
                .map_request(path, Bytes::from_static(b"{}"), &dummy_provider())
                .unwrap();
            assert!(
                !plan.is_compact,
                "{path}: compact 必须 1:1 直透,绝不走本地 compact 包装"
            );
            assert!(!plan.compact_v2);
        }
    }

    #[test]
    fn request_normalizes_legacy_prefixes_and_keeps_query() {
        assert_eq!(
            ResponsesPassthroughMapper
                .map_request(
                    "/openai/v1/responses?stream=true&foo=bar",
                    Bytes::from_static(b"{}"),
                    &dummy_provider()
                )
                .unwrap()
                .upstream_path,
            "/responses?stream=true&foo=bar"
        );
        assert_eq!(
            ResponsesPassthroughMapper
                .map_request(
                    "/claude/v1/messages",
                    Bytes::from_static(b"{}"),
                    &dummy_provider()
                )
                .unwrap()
                .upstream_path,
            "/messages"
        );
    }

    #[test]
    fn request_no_session_no_metadata() {
        let plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(b"{}"),
                &dummy_provider(),
            )
            .unwrap();
        assert!(plan.response_session.is_none());
        assert!(plan.adapter_metadata.is_none());
        assert!(plan.original_responses_request.is_none());
    }

    #[tokio::test]
    async fn response_preserves_status_and_content_type_1to1() {
        // 1:1:不强制 text/event-stream,保留上游 content-type(此处用非 SSE 的
        // application/json 验证强制逻辑没被引入)。
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        let plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(b"{}"),
                &dummy_provider(),
            )
            .unwrap();
        let resp = ResponsesPassthroughMapper
            .map_response(
                StatusCode::OK,
                headers,
                Box::pin(stream::empty()),
                &dummy_provider(),
                &plan,
            )
            .unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(
            resp.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "透传必须 1:1 保留上游 content-type,不强制 event-stream"
        );
    }
}
