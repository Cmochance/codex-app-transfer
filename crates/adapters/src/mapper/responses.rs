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
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

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
        provider: &Provider,
    ) -> Result<RequestPlan, AdapterError> {
        // [MOC-299] api_format=responses 但上游无 compaction 能力的第三方(grok):Codex 的 autocompact
        // (V1 /responses/compact 或 V2 compaction_trigger)grok 不认(404/422)。像 chat 路径一样本地摘要:
        // strip → 注入 summarize prompt → 走上游普通 /responses(stream:false)→ 响应侧包成单 compaction item。
        if crate::mapper::grok_build::responses_upstream_lacks_compaction(provider) {
            if let Some(kind) = crate::responses::compact::detect_compact(client_path, &body) {
                let stripped = match kind {
                    crate::responses::compact::CompactKind::V1 => body.to_vec(),
                    crate::responses::compact::CompactKind::V2 => {
                        crate::responses::compact::strip_compaction_trigger(&body)?
                    }
                };
                let summ = Bytes::from(crate::responses::compact::build_compact_responses_body(
                    &stripped,
                )?);
                // 套 grok_build 适配(tools/reasoning 归一),让摘要请求也被 grok 接受
                let summ =
                    crate::mapper::grok_build::adapt_grok_build_request_body(&summ, provider)
                        .unwrap_or(summ);
                return Ok(RequestPlan {
                    // 摘要请求走普通 /responses(V1 的 client_path=/responses/compact 必须改路由否则 grok 404)
                    upstream_path: rewrite_local_path_for_upstream("/responses"),
                    body: summ,
                    upstream_headers: HeaderMap::new(),
                    response_session: None,
                    adapter_metadata: None,
                    is_compact: true,
                    compact_v2: kind == crate::responses::compact::CompactKind::V2,
                    original_responses_request: None,
                });
            }
        }

        // [MOC-299] grok-build:/responses 是真 Responses API 但工具集受限(不认 Codex 的
        // custom/namespace/tool_search/image_generation)且不支持 reasoning.effort。进 1:1 透传
        // 前把请求体适配成 grok 接受的形态(工具转换复用 chat 路径决策,见 mapper::grok_build)。
        // 非 grok-build provider 恒不改,透传语义与 MOC-234 完全一致。其余 provider 仍严格 1:1。
        // [MOC-301/304 review P1] grok 响应侧 shim 的 context 必须从**适配前** body 提取 —— 此时
        // tools 还带 custom/tool_search/namespace 类型;`adapt_grok_build_request_body` 之后它们全变
        // function,`grok_shim_request_context` 就认不出了(会得空 context → shim 永不 repack)。
        // 提取后 stash 进 adapter_metadata,response 侧从那读(而非从已适配的 request_plan.body 重推)。
        let grok_shim_ctx = if crate::mapper::grok_build::is_grok_build_provider(provider) {
            Some(crate::mapper::grok_build::grok_shim_request_context(&body))
        } else {
            None
        };
        let body = if crate::mapper::grok_build::is_grok_build_provider(provider) {
            crate::mapper::grok_build::adapt_grok_build_request_body(&body, provider)
                .unwrap_or(body)
        } else {
            body
        };

        // [MOC-234] 只读观测整合(gate=breakdown_enabled,默认关零开销):旁路 parse 一份
        // 副本算 responses 原生 context_breakdown + 喂会话观测镜像。返回的 adapter_metadata
        // 仅携带本轮 input items + prev_id 供 response 侧 tee 记录链头。
        let mut adapter_metadata = build_observe_metadata(client_path, &body);
        if let Some(ctx) = grok_shim_ctx {
            if let Ok(v) = serde_json::to_value(&ctx) {
                match adapter_metadata.as_mut() {
                    Some(Value::Object(m)) => {
                        m.insert("grok_shim_ctx".into(), v);
                    }
                    _ => adapter_metadata = Some(serde_json::json!({ "grok_shim_ctx": v })),
                }
            }
        }

        // 路径 normalize:剥 `/openai` legacy prefix + `/claude/v1/messages` alias +
        // 前导 `/v1`(provider.base_url 已带 `/v1`)+ 保 query。**不能**只剥 `/v1`,
        // 否则 `/openai/v1/responses` 透传成 `…/v1/openai/v1/responses` → 上游 404。
        Ok(RequestPlan {
            upstream_path: rewrite_local_path_for_upstream(client_path),
            // model 已由 forward.rs 在 adapter 前 rewrite/strip。**非 grok** provider:1:1 字节直透,
            // 此处不改写任何字段(compact / web_search / namespace 全部原样)。**grok-build**:上方
            // `adapt_grok_build_request_body` 已改写(工具归一 / reasoning / 去重),此处 `body` 是适配后
            // 的字节(compact 请求则更早在函数开头就走了本地 compaction 分支 return,不到这)。
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
        provider: &Provider,
        request_plan: &RequestPlan,
    ) -> Result<ResponsePlan, AdapterError> {
        // [MOC-299] 本地 compaction:上游是我们发的 stream:false 摘要请求,回来的完整 JSON 在此包成
        // Codex 期待的 compaction 响应(V2 单 compaction item SSE / V1 非流式 JSON)。复用 chat 路径包装。
        if request_plan.is_compact {
            return if request_plan.compact_v2 {
                crate::responses::compact::build_compact_v2_response_plan(
                    upstream_status,
                    upstream_headers,
                    upstream_stream,
                )
            } else {
                crate::responses::compact::build_compact_response_plan(
                    upstream_status,
                    upstream_headers,
                    upstream_stream,
                )
            };
        }

        // [MOC-234] **上游非 2xx → 合规 `response.failed` SSE**,绝不裸传错误体 —— **仅限流式
        // `/responses` create 请求**(body `stream:true`)。实测原生 Responses 上游(及第三方反代)
        // 报错常返 HTTP 4xx/5xx + JSON error body(甚至 `content-type: text/event-stream` 但 body
        // 不是 SSE 帧)。流式下 Codex 客户端等不到 SSE 终止事件 → 卡 Thinking、错误不显示。这里复用
        // 与 chat/grok/gemini 同一套 `core::failure_stream`,把上游错误转成 `response.created` +
        // `response.failed`(HTTP 写 200,Codex 据 response.failed 渲染并按 code fail-fast / retry)。
        //
        // reviewer:**非流式**(`stream:false` 的 JSON)/ **子路径**(`/responses/{id}/cancel` 等)
        // 请求,客户端期望的是 JSON 错误体,包成 SSE 200 反让其误判成功 → 这类**原样回灌**上游错误
        // (status + headers + body 1:1)。**成功流亦 1:1 字节直透。**
        if !upstream_status.is_success() {
            if request_is_streaming(request_plan) {
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
            // 非流式 / 子路径:原样回灌上游错误,不包 SSE(否则破坏 JSON 错误语义)。
            return Ok(ResponsePlan {
                status: upstream_status,
                headers: upstream_headers,
                stream: upstream_stream,
            });
        }

        // 成功路径:套一层**对流式零影响**的纯透传 tee。tee 的 `poll_next` 只把 chunk(Arc clone)
        // 非阻塞入队后原样立即返回,**不改字节、不 await、不解析**;SSE 抽行找 `response.completed`、
        // 把本轮(input+output)记进 always-on 会话观测镜像(供 breakdown 拼全历史 + orphan-400 降级
        // 重建上下文),全在独立 spawned task 异步完成(见 `ObserveTeeStream` doc)。
        // [MOC-301/304] grok:响应侧 tool-call shim —— 把 grok 回的 `function_call`(name=apply_patch /
        // tool_search,因请求侧转了 function)重打包回 Codex 认的 `custom_tool_call` / `tool_search_call`,
        // 并给发现的 namespace 工具补 `namespace`。**仅 grok**;其余 responses passthrough 仍严格 1:1。
        // cwd 供 apply_patch preflight;ctx 携带请求侧「哪些工具真被 lower + name→namespace」元数据
        // (review:只 repack 真被 lower 的、给 MCP 工具补 namespace)。
        if crate::mapper::grok_build::is_grok_build_provider(provider) {
            let parsed = serde_json::from_slice::<Value>(&request_plan.body).ok();
            let cwd = parsed
                .as_ref()
                .and_then(|v| crate::responses::apply_patch_preflight::extract_cwd(Some(v)));
            // [review P1] 从 map_request stash 的适配前元数据读(而非从已适配 body 重推,那会得空 ctx)。
            let ctx = request_plan
                .adapter_metadata
                .as_ref()
                .and_then(|m| m.get("grok_shim_ctx").cloned())
                .and_then(|v| {
                    serde_json::from_value::<crate::mapper::grok_build::GrokShimContext>(v).ok()
                })
                .unwrap_or_default();
            // [review thread2] stream:false 的 grok 成功响应是单 JSON(非 SSE),SSE shim 不改写它 →
            // 走 JSON 改写路径;流式(Codex Desktop 常态)走 SSE shim + ObserveTee。
            if !request_is_streaming(request_plan) {
                return grok_rewrite_json_response(
                    upstream_status,
                    upstream_headers,
                    upstream_stream,
                    cwd,
                    ctx,
                );
            }
            // [review Pit5Z] observe **在 shim 之前**套 —— 观测镜像记的是**原始 grok 上游**
            // (function_call),客户端拿的是 shim 后的 custom_tool_call/tool_search_call。理由:
            // orphan-400 transparent retry(store:false 续轮兜底)从观测镜像重建 body 后**直接回灌
            // 上游、不再过 adapt_grok_build_request_body**,故镜像必须是 grok 认的 function_call;若
            // 记 shim 后的 custom_tool_call/tool_search_call,retry 会把 grok 不认的类型送回去 → 422。
            let observed = Box::pin(ObserveTeeStream::new(
                upstream_stream,
                observe_ctx_from_plan(request_plan),
            )) as ByteStream;
            let stream = Box::pin(crate::responses::grok_tool_shim::GrokShimStream::new(
                observed, cwd, ctx,
            )) as ByteStream;
            return Ok(ResponsePlan {
                status: upstream_status,
                headers: upstream_headers,
                stream,
            });
        }
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

    // `input` 可为 array(item 列表)或 plain string(等价单条 user 文本)。reviewer:string 形态
    // 旧版当无 input items → 镜像只记 output,后续 orphan 重建 / breakdown 丢掉本轮用户 prompt。
    // 统一把 string 规整成一条 user message item 记入镜像(与 array 形态等价)。
    let input_items: Vec<Value> = match parsed.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }]
        })],
        _ => Vec::new(),
    };
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

/// 本轮请求是否为**流式**(body `"stream": true`)。仅流式 `/responses` create 的上游错误才包成
/// `response.failed` SSE;非流式(JSON)/ 子路径(cancel 等)的错误原样回灌(见 `map_response`)。
/// 透传 body 1:1,故 stream 字段忠实反映客户端意图;缺省 / 非 true → 非流式。
fn request_is_streaming(plan: &RequestPlan) -> bool {
    serde_json::from_slice::<Value>(&plan.body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// [MOC-301/304 review thread2] 非流式(`stream:false`)grok 成功响应:上游是单 JSON `{output:[...]}`
/// (非 SSE)。`GrokShimStream` 只 parse `data:` 帧、不改写它 → apply_patch/tool_search 的 function_call
/// 原样回客户端被误路由。这里 collect 完整 JSON → 用 shim 的 `rewrite_json_response` 改写 `output[]`
/// (与流式 envelope 同一套)→ 回改写后的 JSON。Codex Desktop 常态是 `stream:true`,此路径是稳健兜底。
fn grok_rewrite_json_response(
    upstream_status: StatusCode,
    upstream_headers: HeaderMap,
    upstream_stream: ByteStream,
    cwd: Option<String>,
    ctx: crate::mapper::grok_build::GrokShimContext,
) -> Result<ResponsePlan, AdapterError> {
    let stream = Box::pin(futures_util::stream::once(async move {
        use futures_util::StreamExt;
        let mut buf: Vec<u8> = Vec::new();
        let mut upstream = upstream_stream;
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(b) => buf.extend_from_slice(&b),
                Err(e) => return Err(e),
            }
        }
        match serde_json::from_slice::<Value>(&buf) {
            Ok(mut v) => {
                let shim = crate::responses::grok_tool_shim::GrokToolCallShim::new(cwd, ctx);
                shim.rewrite_json_response(&mut v);
                let out = serde_json::to_vec(&v).unwrap_or(buf);
                Ok::<Bytes, std::io::Error>(Bytes::from(out))
            }
            // 非 JSON(成功路径理论上都是 JSON;兜底原样回,不吞)。
            Err(_) => Ok(Bytes::from(buf)),
        }
    })) as ByteStream;
    Ok(ResponsePlan {
        status: upstream_status,
        headers: upstream_headers,
        stream,
    })
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

/// [MOC-234] 只读观测 tee:**对流式输出零影响的纯透传**。`poll_next` 拿到 chunk 后只做两件
/// O(1) 的事 —— 把 chunk(`Bytes` 是 Arc,clone 仅 +1 引用计数、不拷数据)经 unbounded channel
/// 非阻塞 `send` 给观测 task,然后**原样立即返回**。流式热路径上**无解析、无扫描、无锁、无 await**。
///
/// SSE 增量抽行、找 `response.completed`、把本轮(input+output)记进会话观测镜像,全部在一个
/// **独立 spawned task**([`observe_response_stream`])里异步消费 channel 完成 —— 解析再重也只
/// 占用后台 task,绝不挤占 chunk 投递(根治「中间流程卡输出 / 整段文字闪烁」)。
///
/// 无观测上下文(compact / parse 失败)或不在 tokio runtime(单测/非 async)→ 不起 task、
/// `observe_tx = None`,退化为彻底零开销的纯透传。流结束时本 struct 被 drop → `observe_tx`
/// drop → task 的 `rx` 收到 channel 关闭 → 自然收尾。
struct ObserveTeeStream {
    inner: ByteStream,
    /// 把 chunk 旁路给异步观测 task 的发送端。`None` = 不观测 → 纯透传。
    observe_tx: Option<UnboundedSender<Bytes>>,
}

impl ObserveTeeStream {
    fn new(inner: ByteStream, observe: Option<(Option<String>, Vec<Value>)>) -> Self {
        // 仅当 ① 有观测上下文 ② 处于 tokio runtime 内,才起异步观测 task;否则纯透传。
        // 解析/记录全在该 task 里跑,流式热路径只剩 O(1) 入队(见 struct doc)。
        let observe_tx = match (observe, tokio::runtime::Handle::try_current()) {
            (Some(observe), Ok(handle)) => {
                let (tx, rx) = unbounded_channel::<Bytes>();
                handle.spawn(observe_response_stream(rx, observe));
                Some(tx)
            }
            _ => None,
        };
        Self { inner, observe_tx }
    }
}

impl Stream for ObserveTeeStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                // 唯一旁路开销:Arc 计数 clone + 非阻塞 unbounded send,均 O(1)。
                // 不解析、不扫描、不等待 → 对流式输出零影响。send 失败(task 已收尾)忽略。
                if let Some(tx) = &this.observe_tx {
                    let _ = tx.send(chunk.clone());
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
    }
}

/// [MOC-234] 异步观测 task(在流式热路径**之外**运行):消费 chunk channel,增量抽行找
/// `response.completed`,拿 `response_id` + output 后把本轮(input + output)记进会话观测镜像
/// (链头 = 本轮 `response_id`)。镜像 always-on,支撑 ① breakdown 拼全历史算明细;② orphan-400
/// 降级沿链重建完整上下文。记录一次即返回(后续 chunk 随 channel 关闭被丢弃)。
///
/// `scan_pos` 只扫新增字节找 `\n`(线性),防巨型单行 `response.completed`(长会话可数 MB)跨
/// 大量小 chunk 到达时退化 O(n²);超 `MAX_OBSERVE_PENDING_LINE` 视为病态、放弃观测(降级)。
async fn observe_response_stream(
    mut rx: UnboundedReceiver<Bytes>,
    observe: (Option<String>, Vec<Value>),
) {
    let (prev_id, input_items) = observe;
    let mut line_buf: Vec<u8> = Vec::new();
    let mut scan_pos = 0usize;
    // 沿 chunk 流找到 `response.completed` 就 break 出 (id, output);channel 关闭/超长行 → None。
    // 两条 None 路径都是**观测降级**(转发不受影响),debug 落一条线索便于现场排查「某轮为何没
    // 进观测镜像 / orphan 自愈或 breakdown 失灵」(否则完全静默)。
    let completed = loop {
        let Some(chunk) = rx.recv().await else {
            // channel 关闭(上游 EOF / 客户端断开)仍未见 response.completed:截断 / 中止 /
            // 非 SSE 响应 → 本轮不记录。
            tracing::debug!(
                "responses observe: stream ended before response.completed; turn not recorded"
            );
            break None;
        };
        line_buf.extend_from_slice(&chunk);
        let mut hit = None;
        loop {
            match line_buf[scan_pos..].iter().position(|&b| b == b'\n') {
                Some(rel) => {
                    let nl = scan_pos + rel;
                    let line: Vec<u8> = line_buf.drain(..=nl).collect();
                    scan_pos = 0; // 头部已 drain,剩余从头续扫
                    if let Some(found) = parse_completed_event(&line) {
                        hit = Some(found);
                        break;
                    }
                }
                None => {
                    scan_pos = line_buf.len(); // 全扫过暂无 `\n`,下个 chunk 接着扫
                    break;
                }
            }
        }
        if hit.is_some() {
            break hit;
        }
        if line_buf.len() > MAX_OBSERVE_PENDING_LINE {
            tracing::debug!(
                pending_bytes = line_buf.len(),
                cap = MAX_OBSERVE_PENDING_LINE,
                "responses observe: pending SSE line exceeded cap; abandoning observation for this turn"
            );
            break None;
        }
    };
    if let Some((id, output)) = completed {
        let mut items = input_items;
        items.extend(output);
        global_passthrough_observe_store().record_turn(&id, prev_id, items);
    }
}

/// 解析一行(可含末尾 `\r\n`):是 `data: {...response.completed...}` event 则返回
/// `(response_id, output items)`,否则 `None`。子串预筛跳过对每个 delta 事件的 JSON parse
/// (此处虽在异步 task、即便全 parse 也不影响流式,仍省掉无谓开销)。
fn parse_completed_event(line: &[u8]) -> Option<(String, Vec<Value>)> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let data = std::str::from_utf8(line)
        .ok()?
        .trim_start()
        .strip_prefix("data:")?
        .trim();
    if data.is_empty() || data == "[DONE]" || !data.contains("response.completed") {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("response.completed") {
        return None;
    }
    let resp = v.get("response")?;
    let id = resp.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let output = resp
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some((id.to_owned(), output))
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
        drop(tee); // 关闭 channel,让观测 task 收尾(它在 completed 事件即记录)。

        // 记录已搬进独立异步 task(对流式零影响),yield 让其推进后再断言:观测镜像应记下
        // 本轮(input 1 + output 1 = 2 items),链头 = response_id。
        let store = global_passthrough_observe_store();
        let mut hist = store.assemble_chain(rid);
        for _ in 0..1000 {
            if !hist.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
            hist = store.assemble_chain(rid);
        }
        assert_eq!(hist.len(), 2, "本轮 input+output 应异步记进观测镜像");
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
        // 流式 create 请求(`stream:true`)—— Codex 主路径,错误必须包成 response.failed SSE。
        let plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(br#"{"stream":true}"#),
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

    #[tokio::test]
    async fn response_upstream_400_non_streaming_preserved_raw_not_sse() {
        // [reviewer] 非流式(`stream:false`)/ 子路径(cancel 等)请求的上游错误**原样回灌**
        // (status + JSON body),不包成 SSE 200 —— 否则期望 JSON 错误的 SDK/API 客户端会误判成功。
        use futures_util::StreamExt;
        let err_body =
            br#"{"error":{"message":"bad cancel request","type":"invalid_request_error"}}"#;
        let upstream: ByteStream = Box::pin(futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(Bytes::from_static(err_body))
        }));
        let mut up_headers = HeaderMap::new();
        up_headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        let plan = ResponsesPassthroughMapper
            .map_request(
                "/v1/responses",
                Bytes::from_static(br#"{"stream":false}"#),
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
        // 原样:status 仍 400、content-type 仍 application/json、body 是原 JSON 错误(非 SSE)。
        assert_eq!(resp.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let mut body = Vec::new();
        let mut s = resp.stream;
        while let Some(c) = s.next().await {
            body.extend(c.unwrap());
        }
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("response.failed"),
            "非流式错误不应被包成 SSE: {text}"
        );
        assert!(
            text.contains("bad cancel request"),
            "应原样保留上游 JSON 错误体: {text}"
        );
    }
}
