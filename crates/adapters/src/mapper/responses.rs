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
//! **本项目的 helper prompt 注入对 responses 不存在(= 已剥除)**:apply_patch /
//! web_search 的协助优化 guidance(`responses/request.rs::apply_patch_chat_guidance_message`
//! / `web_tools_guidance_message`)**只在 chat 转换路径注入** —— 那是给缺乏原生
//! lark grammar / 联网工具语义的 chat function-call provider 补的。本 1:1 passthrough
//! 不调 `responses_body_to_chat_body_*`,原生 Responses 上游自带这些能力,故这些注入
//! prompt **结构上不会出现在 responses 请求里**(既不需要、也不应注入)。
//! `request_passthrough_never_injects_helper_guidance` 回归测试锁死此不变量。
//!
//! ## Session
//! `response_session = None` —— 透传场景上游自管 `previous_response_id`,代理不写
//! 也不读本项目的 chat 形 `ResponseSessionCache`(形状不同,混写会被 chat 路径读坏)。
//! 改用**独立的 responses 形会话观测镜像**(`passthrough_observe`,always-on):正常转发
//! 时只读(算 by-source 明细),**仅在上游报 orphan-400 时**沿链重建完整上下文回注重发
//! (`forward.rs` + `tool_call_repair::rebuild_orphan_context_bytes`,store:false 反代续轮兜底,
//! 用户授权的 error-path 降级)。

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use codex_app_transfer_registry::Provider;
use futures_core::Stream;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;

use crate::mapper::{RequestMapper, ResponseMapper};
use crate::registry::{is_responses_compact_subpath, rewrite_local_path_for_upstream};
use crate::responses::context_breakdown::breakdown_enabled;
use crate::responses::{global_passthrough_observe_store, spawn_compute_and_persist_responses};
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
        // [MOC-234] 只读观测整合(gate=breakdown_enabled,默认关零开销):旁路 parse 一份
        // 副本算 responses 原生 context_breakdown + 喂会话观测镜像,**绝不改 body**。返回的
        // adapter_metadata 仅携带本轮 input items + prev_id 供 response 侧 tee 记录链头。
        let adapter_metadata = build_observe_metadata(client_path, &body);

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
            adapter_metadata,
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
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        // [MOC-234] **上游非 2xx → 合规 `response.failed` SSE**,绝不裸传错误体。
        // 实测原生 Responses 上游(及第三方反代)报错常返 HTTP 4xx/5xx + JSON error body
        // (甚至 `content-type: text/event-stream` 但 body 不是 SSE 帧)。流式 `/responses`
        // 请求下,Codex 客户端等不到 SSE 终止事件 → 卡 Thinking、错误不显示。这里复用
        // 与 chat/grok/gemini 同一套 `core::failure_stream`,把上游错误转成
        // `response.created` + `response.failed`(HTTP 写 200,Codex 据 response.failed
        // 渲染错误并按 code fail-fast / retry),与转换路径的报错对接一致。**仅错误路径,
        // 成功流仍 1:1 字节直透。**
        if !upstream_status.is_success() {
            let mut headers = HeaderMap::with_capacity(2);
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(ResponsePlan {
                status: StatusCode::OK,
                headers,
                stream: crate::core::failure_stream::convert_upstream_error_stream(
                    upstream_status,
                    upstream_stream,
                    "resp_passthrough_error".to_owned(),
                    classify_passthrough_error_status(upstream_status.as_u16()),
                    "upstream",
                ),
            });
        }

        // 成功路径:**始终**套一层**纯透传** tee 解析 SSE 的 `response.completed` —— tee 不 await /
        // 不重排 / 不改字节(chunk 原样返回,绝不破流;非 SSE / 失败自然不记录)。tee 内:
        // ① **始终**把上游产生的 tool-call 记进降级缓存(供 orphan-400 修复,不依赖面板);
        // ② 仅当带观测上下文(breakdown 开)时额外把整轮记进观测镜像供拼全历史算明细。
        let stream = Box::pin(ObserveTeeStream::new(
            upstream_stream,
            observe_ctx_from_plan(request_plan),
        )) as ByteStream;
        // 1:1 直透:status / headers 原样回灌。**不强制** content-type —— 与 chat 等转换
        // mapper 不同,透传上游可能返回非 SSE 的合法响应(`stream:false` 的 JSON、
        // `/responses/compact` v1 非流式、`/responses/{id}/cancel` 等),强制
        // `text/event-stream` 会破坏这些响应。上游已按 Responses 协议给正确 content-type,忠实保留。
        Ok(ResponsePlan {
            status: upstream_status,
            headers: upstream_headers,
            stream,
        })
    }
}

/// 上游(原生 Responses 反代)HTTP status → 内部语义 kind(再经 [`crate::codex_retry_code`]
/// 映射成 Codex 的 retry-control code)。与 `mapper::chat::classify_chat_error_status` 同口径:
/// 400/401/403 等永久错误 → `invalid_prompt`(surface + fail-fast),timeout / rate_limited /
/// 5xx / 404 等瞬时或不确定态保留原 code → Codex Retryable(「Retryable 比误杀安全」)。
/// 透传场景 400 多为请求格式 / 上游会话状态错误(如 `previous_response_id` + `store:false`
/// 下 function_call 续轮),重试同一请求必复现,故归永久错误 fail-fast。
fn classify_passthrough_error_status(status_u16: u16) -> &'static str {
    match status_u16 {
        400 => "bad_request",
        401 => "auth_error",
        403 => "permission_denied",
        408 | 504 => "timeout",
        429 => "rate_limited",
        500..=599 => "server_error",
        _ => "upstream_error",
    }
}

/// [MOC-234] request 侧旁路观测(非 compact 子路径时):parse 一份 body 副本,拿本轮 input
/// items + prev_id,经 metadata 透传给 response 侧 tee → tee 拿到上游 `response_id` 后把本轮
/// (input+output)记进**会话观测镜像**。**绝不改转发 body。**
///
/// 观测镜像的**写入是 always-on**(不依赖面板)—— 它同时支撑 orphan-400 降级重建上下文
/// (需要历史始终被记下)。**仅 breakdown 面板开时**才额外起后台 o200k by-source 计算 + 落盘
/// (那是热路径上较重的一步,保持 gated)。
fn build_observe_metadata(client_path: &str, body: &Bytes) -> Option<Value> {
    // compact 是生命周期端点、非对话轮,跳过观测(也无需降级重建)。
    if is_responses_compact_subpath(client_path) {
        return None;
    }
    let parsed: Value = serde_json::from_slice(body).ok()?;

    let input_items: Vec<Value> = parsed
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prev_id = parsed
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // [仅面板开] 起后台 responses 原生 breakdown(o200k tokenize + 原子落盘,搬离热路径)。
    // conv_id = prompt_cache_key。全历史 = 沿 prev_id 链回溯的镜像 + 本轮 input。
    if breakdown_enabled() {
        if let Some(conv_id) = parsed
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let mut assembled = match prev_id.as_deref() {
                Some(prev) => global_passthrough_observe_store().assemble_chain(prev),
                None => Vec::new(),
            };
            assembled.extend(input_items.iter().cloned());
            let instructions = parsed
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tools = parsed
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            spawn_compute_and_persist_responses(instructions, assembled, tools, conv_id.to_owned());
        }
    }

    // **始终**返回观测上下文 → response tee always-on 把本轮记进镜像(供 orphan 降级重建)。
    // typed → Value(见 ObserveCtx doc),response 侧 from_value 还原。
    let ctx = ObserveCtx {
        prev_id,
        input_items,
    };
    Some(serde_json::json!({ "passthrough_observe": serde_json::to_value(&ctx).ok()? }))
}

/// request→response 侧内部通道载荷(经 `RequestPlan.adapter_metadata` 的
/// `passthrough_observe` 字段透传)。用 typed struct 而非手搓 JSON:producer
/// (`build_observe_metadata`)与 consumer(`observe_ctx_from_plan`)共用同一定义,
/// 字段漂移即编译错(对齐 `mapper::anthropic_messages` 的 `AnthropicToolNameMaps` 模式)。
#[derive(serde::Serialize, serde::Deserialize)]
struct ObserveCtx {
    /// 本轮请求的 `previous_response_id`(无则 None)。
    prev_id: Option<String>,
    /// 本轮 input items(供 response 侧拿到 response_id 后连同 output 一起记进镜像)。
    input_items: Vec<Value>,
}

/// 从 `RequestPlan.adapter_metadata` 取出 response 侧记录所需的观测上下文。
/// 无观测上下文(面板关 / compact / parse 失败)→ None,response 侧不套 tee。
fn observe_ctx_from_plan(plan: &RequestPlan) -> Option<(Option<String>, Vec<Value>)> {
    let obs = plan.adapter_metadata.as_ref()?.get("passthrough_observe")?;
    let ctx: ObserveCtx = serde_json::from_value(obs.clone()).ok()?;
    Some((ctx.prev_id, ctx.input_items))
}

/// 单条 SSE `data:` 行(`response.completed` event)在被解析前允许在行缓冲里累积的上限。
/// `response.completed` 携带完整 output,长会话可能数 MB;超此上限放弃解析该流(观测降级、
/// 不影响转发),防病态超长行 OOM。
const MAX_OBSERVE_PENDING_LINE: usize = 8 * 1024 * 1024;

/// [MOC-234] 只读观测 tee:**纯透传** + 旁路增量解析 SSE 找 `response.completed`,拿 response_id
/// + output items 后把本轮(input+output)记进会话观测镜像。chunk 原样返回(不 await / 不重排 /
/// 不改字节),解析失败 / 非 SSE / 超长行 → 静默不记录(降级,绝不影响转发)。仅记录一次。
struct ObserveTeeStream {
    inner: ByteStream,
    line_buf: Vec<u8>,
    /// 已扫过、确认其中无 `\n` 的前缀长度。下个 chunk 只从这里起扫新增字节,避免
    /// 每 chunk 全量重扫 —— 否则巨型单行 `response.completed`(数 MB)跨大量小 chunk
    /// 到达时会退化成 O(n²)(code review 指出)。
    scan_pos: usize,
    /// 观测上下文(本轮 prev_id + input items),仅 `breakdown_enabled` 时 `Some`。
    /// `Some` 才把整轮记进观测镜像供 breakdown 拼全历史;function_call 缓存(降级修复用)
    /// **无论 observe 是否 Some 都记**(始终运行,见 `process_line`)。
    observe: Option<(Option<String>, Vec<Value>)>,
    recorded: bool,
    gave_up: bool,
}

impl ObserveTeeStream {
    fn new(inner: ByteStream, observe: Option<(Option<String>, Vec<Value>)>) -> Self {
        Self {
            inner,
            line_buf: Vec::new(),
            scan_pos: 0,
            observe,
            recorded: false,
            gave_up: false,
        }
    }

    /// 把一个 chunk 喂进行缓冲,抽出完整行逐行处理(找 `response.completed`)。
    /// 用 `scan_pos` 只扫新增字节找 `\n`(线性);找到则 drain 该行、游标归零续扫剩余。
    fn feed(&mut self, chunk: &[u8]) {
        if self.recorded || self.gave_up {
            return;
        }
        self.line_buf.extend_from_slice(chunk);
        loop {
            match self.line_buf[self.scan_pos..]
                .iter()
                .position(|&b| b == b'\n')
            {
                Some(rel) => {
                    let nl = self.scan_pos + rel;
                    let line: Vec<u8> = self.line_buf.drain(..=nl).collect();
                    self.process_line(&line[..line.len().saturating_sub(1)]);
                    self.scan_pos = 0; // 头部已 drain,剩余从头继续扫
                    if self.recorded {
                        return;
                    }
                }
                None => {
                    self.scan_pos = self.line_buf.len(); // 全扫过、暂无 `\n`,下个 chunk 接着扫
                    break;
                }
            }
        }
        // 无换行的超长 pending 行(病态)→ 放弃,防无界增长。
        if self.line_buf.len() > MAX_OBSERVE_PENDING_LINE {
            self.gave_up = true;
            self.line_buf = Vec::new();
            self.scan_pos = 0;
        }
    }

    /// 处理一行(已去 `\n`):仅认 `data: {...response.completed...}`,拿 id + output 记录本轮。
    fn process_line(&mut self, line: &[u8]) {
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        let Ok(s) = std::str::from_utf8(line) else {
            return; // 跨 chunk 切断多字节字符的残行,下一行再来(罕见,降级)
        };
        let Some(data) = s.trim_start().strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if v.get("type").and_then(Value::as_str) != Some("response.completed") {
            return;
        }
        let resp = v.get("response");
        let id = resp
            .and_then(|r| r.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if id.is_empty() {
            return;
        }
        let output = resp
            .and_then(|r| r.get("output"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // **始终**(不依赖 breakdown 面板)把本轮(input + output)记进会话观测镜像,链头 =
        // 本轮 response_id。镜像同时支撑:① breakdown 拼全历史算明细(面板开时);② orphan-400
        // 降级时沿链重建完整上下文。observe 是 always-on(`build_observe_metadata` 始终返回 ctx),
        // 故续轮 input 总被记下;output 里的 function_call 也随之入链,供降级拼回。
        if let Some((prev_id, input_items)) = self.observe.take() {
            let mut items = input_items;
            items.extend(output);
            global_passthrough_observe_store().record_turn(id, prev_id, items);
        }
        self.recorded = true;
    }
}

impl Stream for ObserveTeeStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
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
    fn request_passthrough_never_injects_helper_guidance() {
        // [MOC-234] 用户约束:web_search / apply_patch 的本项目 helper prompt 注入
        // (chat 转换路径专属)绝不能出现在 responses 请求。1:1 passthrough 不调 chat
        // 转换,故首轮(无 previous_response_id)+ 注册 apply_patch + web_search 工具时,
        // 出站 body 必须与入站字节完全相同(零 guidance 注入)。此测试锁死该不变量:
        // 未来若误把 responses 路由进 chat 转换 / 加共享注入点,这里会立刻失败。
        let inbound = br#"{"model":"gpt-5.5","input":[{"type":"message","role":"user","content":"patch this file and search the web"}],"tools":[{"type":"custom","name":"apply_patch"},{"type":"web_search"}],"stream":true}"#;
        let body = Bytes::from_static(inbound);
        let plan = ResponsesPassthroughMapper
            .map_request("/v1/responses", body.clone(), &dummy_provider())
            .unwrap();
        assert_eq!(
            plan.body, body,
            "passthrough 首轮注册 apply_patch/web_search 也必须字节级 1:1,绝不注入 helper guidance"
        );
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
    fn request_no_session_no_envelope_replay() {
        let plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(b"{}"),
                &dummy_provider(),
            )
            .unwrap();
        // 透传不写 chat 形 session、无 envelope replay。
        assert!(plan.response_session.is_none());
        assert!(plan.original_responses_request.is_none());
        // adapter_metadata 现在恒带观测上下文(always-on,供 breakdown + orphan 降级);
        // 它是 adapter↔proxy 内部通道,不进 user-facing 协议、不改转发 body。
        assert!(plan.adapter_metadata.is_some());
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

    #[tokio::test]
    async fn observe_tee_records_turn_and_passes_bytes_through() {
        use crate::responses::global_passthrough_observe_store;
        use futures_util::StreamExt;
        use serde_json::json;

        // 唯一 response_id 避免与并发测试在全局 store 串(store 按 id 隔离)。
        let rid = "obs_test_tee_r1";
        let input_item =
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]});
        // SSE:故意把 response.completed 事件**跨 chunk 切断**,验证行缓冲重组。
        let completed = format!(
            "data: {}\n\n",
            json!({
                "type":"response.completed",
                "response":{
                    "id": rid,
                    "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]
                }
            })
        );
        let (a, b) = completed.split_at(completed.len() / 2);
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            )),
            Ok(Bytes::from(a.to_owned())),
            Ok(Bytes::from(b.to_owned())),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ];
        let expected: Vec<u8> = chunks
            .iter()
            .flat_map(|c| c.as_ref().unwrap().to_vec())
            .collect();

        let inner: ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let mut tee = ObserveTeeStream::new(inner, Some((Some("prev_x".into()), vec![input_item])));

        // 透传字节必须与上游完全一致(tee 不改流)。
        let mut got: Vec<u8> = Vec::new();
        while let Some(chunk) = tee.next().await {
            got.extend(chunk.unwrap());
        }
        assert_eq!(got, expected, "tee 必须 1:1 透传上游字节");

        // 观测镜像应记下本轮(input 1 + output 1 = 2 items),链头 = response_id。
        let hist = global_passthrough_observe_store().assemble_chain(rid);
        assert_eq!(hist.len(), 2, "本轮 input+output 应记进观测镜像");
    }

    #[test]
    fn observe_ctx_from_plan_reads_metadata() {
        use serde_json::json;
        let mut plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(b"{}"),
                &dummy_provider(),
            )
            .unwrap();
        plan.adapter_metadata = Some(json!({
            "passthrough_observe": {
                "prev_id": "resp_prev",
                "input_items": [{"type":"message","role":"user","content":"x"}]
            }
        }));
        let (prev, items) = observe_ctx_from_plan(&plan).expect("应解析出观测上下文");
        assert_eq!(prev.as_deref(), Some("resp_prev"));
        assert_eq!(items.len(), 1);

        // 无 metadata(如 compact 路径)→ observe_ctx None,response 侧不套 tee。
        let mut bare = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(b"{}"),
                &dummy_provider(),
            )
            .unwrap();
        bare.adapter_metadata = None;
        assert!(observe_ctx_from_plan(&bare).is_none());
    }

    #[test]
    fn build_observe_metadata_always_on_except_compact() {
        // 观测镜像写入 always-on(不依赖 breakdown 面板)→ 普通 /responses 恒返回 ctx,
        // 供 orphan 降级重建历史。compact 子路径跳过(生命周期端点、非对话轮)。
        assert!(
            build_observe_metadata("/v1/responses", &Bytes::from_static(b"{}")).is_some(),
            "普通 responses 应恒返回观测上下文(always-on)"
        );
        assert!(
            build_observe_metadata("/v1/responses/compact", &Bytes::from_static(b"{}")).is_none(),
            "compact 跳过观测"
        );
    }

    #[tokio::test]
    async fn response_upstream_400_becomes_response_failed_sse_not_raw_passthrough() {
        // [MOC-234] 真机复现:上游(jp.yemoren / new-api)对工具续轮返 HTTP 400 + 裸 JSON
        // error 体(甚至标 content-type=event-stream 但非 SSE 帧)→ 若 1:1 直透,Codex 流式
        // 客户端等不到 SSE 终止事件 → 卡 Thinking、错误不显示。本路径必须转成 response.failed SSE。
        use futures_util::StreamExt;
        let err_body = br#"{"error":{"message":"No tool call found for function call output with call_id call_X.","type":"invalid_request_error","param":"input"}}"#;
        let upstream: ByteStream = Box::pin(futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(Bytes::from_static(err_body))
        }));
        let mut up_headers = HeaderMap::new();
        up_headers.insert(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        let plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(b"{}"),
                &dummy_provider(),
            )
            .unwrap();
        let resp = ResponsesPassthroughMapper
            .map_response(
                StatusCode::BAD_REQUEST,
                up_headers,
                upstream,
                &dummy_provider(),
                &plan,
            )
            .unwrap();
        // HTTP 写 200 + SSE,Codex 才会读流拿到 response.failed(裸 400 会卡 Thinking)。
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(
            resp.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        let mut body = Vec::new();
        let mut s = resp.stream;
        while let Some(c) = s.next().await {
            body.extend(c.unwrap());
        }
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("event: response.failed"),
            "上游错误必须转成 response.failed SSE: {text}"
        );
        assert!(
            text.contains("invalid_prompt"),
            "400 → invalid_prompt(fail-fast,不卡 retry): {text}"
        );
        assert!(
            text.contains("No tool call found"),
            "应带上上游错误 message 供用户/模型看到: {text}"
        );
    }
}
