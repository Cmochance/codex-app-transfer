# Messages <=> Responses 任务推进树

> 当前任务: 为 Claude 系列模型新增 `anthropic_messages` 协议适配。
> 方案文档: `docs/plans/2026-05-13-messages-responses-protocol.md`
> 当前状态: P17 已按 LiteLLM 参考补齐 invalid thinking signature 一次性恢复重试与 Anthropic OAuth token 头处理;Anyrouter forced tool-call + `function_call_output` continuation 真实探针仍通过。

## 已确认事实

- `docs/litellm` 是 `.gitignore` 中声明的本地参考目录,不属于当前仓库跟踪文件。
- 本地 LiteLLM 已同步到 BerriAI/litellm main `431daa1479f0af506696d1dff236d95566abdddc`,版本 `1.85.0`。
- 根目录架构要求新增协议走 `core + mapper + thin adapters`,adapter 层不能承载复杂 provider-specific 分支。
- P6 前 `anthropic` / `claude` / `messages` 仍归一到 `responses`;P6 后这些历史别名已归一到 canonical `anthropic_messages`。
- 当前代码实际已有 `grok_web` mapper/adapter,根架构文档后续需要同步补齐。

## 推进树

### P0 参考基线

- [x] 获取远端 LiteLLM main。
- [x] 同步 `docs/litellm` 到 LiteLLM `1.85.0`。
- [x] 校验同步后本地参考目录与临时克隆无差异。
- [x] 定位 Anthropic Messages / Responses 可借鉴实现。

### P1 架构阅读与方案调整

- [x] 读取 `ARCHITECTURE_PROTOCOL_GUIDE.md`。
- [x] 读取 `docs/protocol-unification-rfc-phase4.md`。
- [x] 读取当前 mapper/adapter/registry 实现。
- [x] 将方案调整为 `anthropic_messages` 一等协议,而不是历史 alias 补丁。
- [x] 保存完整方案到 `docs/plans/2026-05-13-messages-responses-protocol.md`。

### P2 RFC 与测试夹具

- [x] 新增 Phase 5 RFC 或后续 RFC 段落。
- [x] 明确 compact path 是否进入 MVP。
- [x] 准备 text / thinking / tool_use / error / unknown event SSE fixtures。
- [x] 先写 request mapper 失败单测。

### P3 Request Mapper

- [x] 新增 `crates/adapters/src/anthropic_messages/request.rs`。
- [x] 复用 Responses input/session pipeline。
- [x] 实现 chat-shape -> Anthropic Messages lowering。
- [x] 实现 tool name sanitize 与 reverse map。
- [x] 实现 Anthropic path/header/max_tokens/thinking/tool_choice 映射。
- [x] 通过请求侧单测。

### P4 Response Mapper

- [x] 新增 `crates/adapters/src/anthropic_messages/response.rs`。
- [x] 实现 Anthropic Messages SSE -> Responses SSE 状态机。
- [x] 写入 `ToolCallCache` 与 `ResponseSessionCache`。
- [x] 覆盖 max_tokens、error、unknown event、stream interrupted。
- [x] 通过响应侧单测。

### P5 Adapter 与 Registry

- [x] 新增 `AnthropicMessagesAdapter`。
- [x] 新增 `mapper::anthropic_messages::AnthropicMessagesMapper`。
- [x] 更新 `lib.rs`、`mapper/mod.rs`、`registry.rs`。
- [x] 更新 mapper contract tests。
- [x] 更新 registry alias tests。
- [x] 接通 adapter 默认 outbound headers 到 proxy 转发路径。

### P6 配置与 UI

- [x] backend normalization 输出 `anthropic_messages`。
- [x] provider test/model-list 分支适配 Anthropic Messages。
- [x] direct-mode bypass 继续只允许 `responses` / `openai_responses`。
- [x] frontend 保存、展示、i18n 文案更新。
- [ ] P7 真实 Claude 验证通过后再添加 Claude preset。

### P7 文档与验收

- [x] 更新 `ARCHITECTURE_PROTOCOL_GUIDE.md` 与 RFC 变更清单。
- [x] 更新 README 或 release notes。
- [x] 运行 `cargo fmt --all`。
- [x] 运行 `cargo test -p codex-app-transfer-adapters`。
- [x] 运行 `cargo test -p codex-app-transfer-registry`。
- [x] 运行 `cargo test -p codex-app-transfer`。
- [x] 前端静态资源验证:当前仓库根目录无 `package.json`,使用 Tauri/Rust 构建链验证嵌入资源。
- [x] 使用 Claude-compatible provider 做 text、tool-call、`previous_response_id` 真实验证。Anyrouter 代理端到端探针已覆盖 forced tool-call + continuation,用户 2026-05-14 手动完成一轮完整编程验证。
- [ ] 使用官方 Anthropic/Claude provider 做 text、tool-call、`previous_response_id`、upstream error 真实验证。Blocked:当前没有官方 Anthropic API key provider 或 Anthropic OAuth 登录授权实现。

### P8 Anyrouter / Anthropic Messages 深水区补齐

- [x] 对齐 LiteLLM Anthropic web search 映射,将 Responses `web_search` 转为 Anthropic hosted `web_search_20250305`。
- [x] 对齐 LiteLLM server tool 解析行为,将 Anthropic `server_tool_use` 的 `web_search` 转回 Responses `web_search_call`,并保留 `web_search_tool_result` URL citations。
- [x] 保留 Anthropic thinking `signature_delta` / `redacted_thinking` 的会话续传信息,避免 previous_response_id 续轮丢签名块。
- [x] Anyrouter preset 启用 Anthropic 原生 web search,并通过 `proxy.force_default_model=true` 强制所有入站模型别名回到 `models.default`。
- [x] 运行 P8 定向测试与必要回归。

### P9 剩余协议丢失面补齐

- [x] request tools:保留 `namespace` 包装 name/description 元数据,并继续保证 response function_call 带 namespace 字段。
- [x] request tools:保留 `custom` 工具 grammar/schema/format 语义,避免只剩一个无上下文的 `input` 字符串。
- [x] request tools:把 `strict`、`cache_control`、`defer_loading`、`allowed_callers`、`input_examples` 等可被 Anthropic tool 接收的扩展字段送入 Anthropic tool。
- [x] request tools:对 Anthropic 原生 hosted tools 和可确定映射的 Responses 专属工具做 passthrough/降级;不可等价的 file/image 类 Responses 工具仍不伪装为 Claude 可执行工具,继续走现有 warn/drop 诊断路径。
- [x] response blocks:非 web 的 Anthropic `*_tool_result` 与未知 response block 至少保留 provider-specific trace,不再完全忽略。
- [x] 运行 P9 定向测试与必要回归。

### P10 LiteLLM Claude 能力复原补齐

- [x] request top-level:对齐 LiteLLM 保留 `context_management`、`container`、`output_config`、`output_format`、`speed`、`cache_control` 等 Anthropic 原生字段。
- [x] request structured output:将 Responses `text.format` / Chat `response_format` 映射为 Anthropic `output_format`,并按 LiteLLM 过滤 Claude output schema 不支持的约束字段。
- [x] request beta headers:按实际请求中的 computer、MCP、tool_search、programmatic tool calling、input_examples、file_id、code_execution、container skills、context_management、structured output、effort、fast mode、advisor tool 自动追加 `anthropic-beta`,并在 proxy 转发层与 provider 已配置的 `anthropic-beta` 合并为单个 header。
- [x] request default headers:对齐 LiteLLM / Anthropic Messages 契约补齐 `accept: application/json`。
- [x] request content blocks:保留 Claude `document` / `container_upload` block、tool_result 富内容与 `is_error` / `cache_control`,并在 container upload 出现时自动补 code execution hosted tool。
- [x] 运行 P10 定向测试与必要回归。

### P17 LiteLLM 错误恢复与 Anthropic OAuth 头补齐

- [x] 对齐 LiteLLM `invalid thinking signature` 恢复策略:Anthropic Messages 上游 400 命中签名错误时,移除请求中的 `thinking` / `redacted_thinking` 内容块和顶层 `thinking`,并只透明重试一次。
- [x] 同步清理 `ResponseSessionPlan.messages` 中的 stale thinking block,避免重试成功后又把无效签名写回本地 `previous_response_id` 会话缓存。
- [x] 对齐 LiteLLM Anthropic OAuth token 处理:`sk-ant-oat*` key 走 `Authorization: Bearer`,合并 `oauth-2025-04-20` beta,并补 `anthropic-dangerous-direct-browser-access:true`。
- [x] 验证普通 Anyrouter Bearer key 不会被误识别为 Anthropic OAuth token,不会额外注入 OAuth headers。
- [x] 将 Vertex/Azure/Bedrock `cache_control.scope` provider-specific 清理项加入 `docs/followup-tracker.md`,当前不实现。
- [x] 运行 P17 定向测试、代理回归与 Anyrouter 真实连接探针。

## 当前下一步

P17 已完成 LiteLLM 错误恢复/OAuth 头补齐和 Anyrouter 回归。下一步可继续扩大真实 provider 覆盖:structured output、MCP/custom tool metadata、code execution、upstream error 与长上下文续轮;Vertex/Azure/Bedrock 的 `cache_control.scope` 差异已降级到 follow-up。

## 执行记录

### 2026-05-13 P2

- 新增 `docs/protocol-unification-rfc-phase5-anthropic-messages.md`,把 `anthropic_messages` 定为 Claude 系列的一等 canonical protocol。
- 确认 `/responses/compact` 进入 MVP。原因:compact 是 Codex 本地生命周期端点,若普通 Claude turn 可用但 compact 失败,长会话仍不可用。
- 明确 compact 实现策略:复用现有 compact prompt 与 history budget 逻辑,将 chat-shaped compact request 再降到 Anthropic Messages,上游使用非流式 `/messages`,响应包装为 Codex compact output。
- 新增 `crates/adapters/tests/fixtures/anthropic_messages/` 夹具,覆盖 text、thinking、tool_use、error、unknown event SSE。
- 新增 request mapper JSON fixture,覆盖纯文本请求和 tool_use/tool_result pairing。
- 新增 `crates/adapters/tests/anthropic_messages_request.rs`:默认测试校验 fixture 可解析;两个 `#[ignore]` 测试作为 P3 的 request mapper TDD 入口。

### 2026-05-13 P3

- 新增 `crates/adapters/src/anthropic_messages/mod.rs` 与 `request.rs`,只落请求侧转换能力,尚未接入 adapter/registry。
- 请求侧复用 `responses_body_to_chat_body_for_provider_with_session`,因此保留现有 `previous_response_id`、tool-call repair、compact prompt 和 history budget 行为。
- 实现 chat-shape -> Anthropic Messages lowering:
  - `system` / `developer` 汇总为 top-level `system`;
  - user/assistant text 转 `text` block;
  - assistant `tool_calls` 转 `tool_use` block;
  - `tool` message 转 user `tool_result` block;
  - image URL/data URL 转 Anthropic image block;
  - assistant `reasoning_content` 转 thinking block。
- 实现 tool name sanitize:
  - 非 `^[a-zA-Z0-9_-]{1,128}$` 字符替换为 `_`;
  - 合法前导 `_` 保持不变;
  - 碰撞时追加数字后缀;
  - 返回 forward/reverse map,供 P4 response mapper 还原工具名。
- 实现 Anthropic 请求侧参数:
  - upstream path 根据 base URL 是否已含 `/v1` 选择 `/messages` 或 `/v1/messages`;
  - default headers 暴露 `anthropic-version: 2023-06-01` 与 `content-type: application/json`,P5 接 proxy 时再合并进出站请求;
  - `max_tokens` 必填,缺省使用 `4096`;
  - compact 请求使用 `stream:false`,普通请求使用 `stream:true`;
  - `tool_choice` 与 `parallel_tool_calls` 映射为 Anthropic `tool_choice.disable_parallel_tool_use`;
  - `reasoning_effort` 映射为 Anthropic `thinking`;
  - email/phone 形态 user id 不写入 `metadata.user_id`。
- 孤立 tool result 现在在请求 mapper 返回可诊断 `BadRequest`,避免把不合法 tool_result 静默发给 Anthropic。

### 2026-05-13 P4

- 新增 `crates/adapters/src/anthropic_messages/response.rs`,实现 Anthropic Messages SSE -> Responses SSE 状态机。
- 响应侧生命周期覆盖:
  - `message_start` 输出 `response.created` 与 `response.in_progress`;
  - `text` block 输出 message item、content part 与 `output_text` delta/done;
  - `thinking` / `redacted_thinking` block 输出 reasoning summary lifecycle;
  - `tool_use` block 输出 function_call item 与 arguments delta/done;
  - `message_stop` 根据 stop reason 输出 `response.completed` 或 `response.incomplete`;
  - `error` event 输出结构化 `response.failed`;
  - `ping` 与未知 event 忽略。
- 响应侧缓存覆盖:
  - tool_use block 关闭时写入 `ToolCallCache`,供下一轮 `tool_result` repair;
  - stream wrapper 结束时把 assistant message 写入 `ResponseSessionCache`,供 `previous_response_id` 恢复。
- 响应侧保留 P3 的 tool name reverse map,上游 sanitized tool name 会在 Responses function_call 与 ToolCallCache 中还原为原始工具名。
- 将 compact response 的 summary 包装逻辑从 `responses::compact` 提成 `compact_response_body_from_summary_text`,让 Anthropic compact 路径复用同一个 `COMPACT_SUMMARY_PREFIX` 与 `<summary>` 抽取规则。
- 新增 `crates/adapters/tests/anthropic_messages_response.rs`,覆盖 text、thinking、tool_use、sanitized tool name reverse、error、unknown event、max_tokens、stream interrupted、session cache 与 Anthropic compact response。

### 2026-05-13 P5

- 新增 `mapper::anthropic_messages::AnthropicMessagesMapper`,实现 `RequestMapper` / `ResponseMapper`,把 P3 request mapper 与 P4 response mapper接入统一 mapper trait。
- 新增薄层 `AnthropicMessagesAdapter`,只负责调用 mapper 层,不承载复杂 provider-specific 分支。
- 更新 `AdapterRegistry`:
  - canonical `anthropic_messages` 接入新 adapter;
  - 历史别名 `anthropic` / `claude` / `messages` / `claude_messages` 现在路由到 `anthropic_messages`;
  - `responses` / `openai_responses` 仍保持 OpenAI Responses 语义与 passthrough 例外。
- 更新 `lib.rs` 和 `mapper/mod.rs`,公开 adapter 并纳入 mapper contract tests。
- 步骤级调整:新增 `RequestPlan.upstream_headers` 与 `adapter_metadata`。原因:
  - P3 已生成 Anthropic 必需默认头,但旧 `RequestPlan` 没有字段传给 proxy,真实请求会丢 `anthropic-version`;
  - P4 response mapper 需要 P3 的 tool name reverse map,否则 registry 接入后 sanitized tool name 无法可靠还原。
- proxy 出站请求现在会合并 adapter 默认协议头,并保持 `provider.extraHeaders` 覆盖 adapter defaults;新增回归测试确认客户端同名 header 不会重复上线。

### 2026-05-13 P6

- 更新 provider `apiFormat` 归一化:
  - `responses` / `openai_responses` 仍归一为 `responses`;
  - `anthropic_messages` / `anthropic` / `claude` / `messages` / `claude_messages` 归一为 `anthropic_messages`;
  - 保留 `gemini_native`、`gemini_cli_oauth`、`antigravity_oauth`、`grok_web` 等既有 canonical 协议值,避免保存 custom provider 时被误写回 `openai_chat`。
- provider 测速新增 Anthropic Messages 分支:
  - baseUrl 已含 `/v1` 时使用 `/messages`;
  - baseUrl 未含版本路径时补 `/v1/messages`;
  - 默认加 `anthropic-version: 2023-06-01`,同时保留 `extraHeaders` 覆盖默认头的能力;
  - ping body 使用 Anthropic Messages 形态 `messages + max_tokens`。
- provider 模型列表新增 Anthropic Messages 分支,从 Messages endpoint 推导 peer `/v1/models`,并复用同一默认版本头。
- direct mode bypass 保持只匹配 `responses` / `openai_responses`;`anthropic_messages` 与历史 Claude alias 继续走 local proxy 做本地协议转换。
- 前端自定义 provider 协议下拉改为保存 `anthropic_messages`;旧值 `anthropic` / `claude` / `messages` 仍能显示为 Anthropic Messages。
- 更新中英文 i18n,将 Anthropic Messages 文案从“原生透传”改为“Responses ↔ Anthropic Messages 本地转换”。
- 未添加 Claude preset。原因:P7 还需要真实 Claude text、tool-call、previous_response_id、upstream error 验证。
- 发现 P7 旧验收项 `npm run build` 与当前仓库结构不匹配:根目录没有 `package.json`,前端是静态资源/Tauri 嵌入链路,后续应以 Rust/Tauri 构建验证替代。

## 验证记录

- 已通过: `cargo fmt --all`
- 已通过: `cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`
  - 结果:2 passed,2 ignored。
  - 既有 warning: `gemini_oauth` 未使用 import、`grok_web` dead_code,均为当前分支新增 P2 前已存在的非阻塞 warning。
- 已确认预期失败: `cargo test -p codex-app-transfer-adapters --test anthropic_messages_request -- --ignored`
  - 结果:2 failed。
  - 失败原因:两个 ignored 测试均命中 `P3 must call the real Anthropic Messages request mapper here` 占位 panic,说明 P3 接入真实 request mapper 后有明确 TDD 入口。
- 已通过: `cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`
  - P3 后结果:12 passed,0 ignored。
  - 覆盖 text fixture、tool_use/tool_result fixture、tool name sanitize/reverse map、tool_choice/parallel mapping、reasoning/metadata、compact 非流式 request、upstream path/default headers、orphan tool result BadRequest。
- 已通过: `cargo test -p codex-app-transfer-adapters`
  - 结果:483 unit tests passed;12 `anthropic_messages_request` integration tests passed;3 `responses_streaming` integration tests passed。
  - 既有 warning 仍为 `gemini_oauth` 未使用 import 与 `grok_web` dead_code,非本次 P3 新增。
- 已通过: `cargo fmt --all --check`
- 已通过: `cargo test -p codex-app-transfer-adapters --test anthropic_messages_response`
  - P4 后结果:10 passed,0 ignored。
  - 覆盖 Anthropic text/thinking/tool_use/error/unknown event/max_tokens/interrupted/session cache/compact response。
- 已通过: `cargo test -p codex-app-transfer-adapters`
  - P4 后结果:483 unit tests passed;12 `anthropic_messages_request` integration tests passed;10 `anthropic_messages_response` integration tests passed;3 `responses_streaming` integration tests passed。
  - 既有 warning 仍为 `gemini_oauth` 未使用 import 与 `grok_web` dead_code,非本次 P4 新增。
- 已通过: `cargo fmt --all --check`
- 已通过: `cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response`
  - P5 后结果:12 request tests passed;10 response tests passed。
- 已通过: `cargo test -p codex-app-transfer-adapters`
  - P5 后结果:484 unit tests passed;12 `anthropic_messages_request` integration tests passed;10 `anthropic_messages_response` integration tests passed;3 `responses_streaming` integration tests passed。
  - 既有 warning 仍为 `gemini_oauth` 未使用 import 与 `grok_web` dead_code,非本次 P5 新增。
- 已通过: `cargo test -p codex-app-transfer-proxy --test auth_and_routing anthropic_messages_forward_injects_adapter_protocol_headers`
  - 说明:沙箱内第一次因本地端口绑定权限失败;提升权限后通过。
- 已通过: `cargo test -p codex-app-transfer-proxy --test auth_and_routing`
  - P5 后结果:15 passed。
- 已通过: `cargo check --workspace`
  - 既有 warning 仍为 `gemini_oauth` 未使用 import、`grok_web` dead_code、`src-tauri` unused doc/dead_code,非本次 P5 新增。
- 已通过: `cargo fmt --all`
- 已通过: `cargo test -p codex-app-transfer normalize_provider_api_format`
  - P6 后结果:2 passed。
- 已通过: `cargo test -p codex-app-transfer provider_test_url_anthropic_messages_uses_messages_endpoint`
  - P6 后结果:1 passed。
- 已通过: `cargo test -p codex-app-transfer model_endpoint_candidates_anthropic_messages_use_models_endpoint`
  - P6 后结果:1 passed。
- 已通过: `cargo test -p codex-app-transfer provider_connection_posts_anthropic_messages_ping_with_version_header`
  - P6 后结果:1 passed。沙箱内首次因 127.0.0.1 端口绑定权限失败;提升权限后通过。
- 已通过: `cargo test -p codex-app-transfer fetch_provider_models_reads_anthropic_messages_models_with_version_header`
  - P6 后结果:1 passed。使用本地 mock `/v1/models` 验证 `anthropic-version` header。
- 已通过: `cargo test -p codex-app-transfer admin::handlers::providers`
  - P6 后结果:20 passed。
- 已通过: `cargo test -p codex-app-transfer anthropic_aliases_never_bypass_proxy`
  - P6 后结果:1 passed。
- 已通过: `cargo fmt --all --check`
- 已确认不可执行: `npm run build`
  - 原因:当前仓库根目录没有 `package.json`;后续 P7 应使用 Tauri/Rust 构建链验证前端静态资源嵌入。
- 已只读检查真实本地配置 `~/.codex-app-transfer/config.json`
  - 仅统计 `providers[].apiFormat`,未输出任何 secret。
  - 当前存在 `antigravity_oauth`、`gemini_native`、`grok_web`、`openai_chat`、`responses`;P6 normalizer 会保留这些 canonical 值。

### 2026-05-13 P7 文档更新

- 更新 `ARCHITECTURE_PROTOCOL_GUIDE.md`,将当前状态推进到 Phase 5 Anthropic Messages PR,补齐 `grok_web` 与 `anthropic_messages` mapper/adapter 目录,并新增 canonical protocol 清单与 provider UI 验证门槛。
- 更新 `docs/protocol-unification-rfc-phase5-anthropic-messages.md`,把 RFC 状态从 P3 draft 推进到 P6 complete / P7 validation,补齐 P4-P6 落地状态、rollback 策略和 P7 acceptance gates。
- 更新 `docs/plans/2026-05-13-messages-responses-protocol.md`,记录 P2-P6 已落地事实,并明确 Claude preset 仍需等待 P7 真实 Claude 验证。
- 更新 `README.md` / `README.en.md` / `docs/CHANGELOG.md`,加入 Anthropic Messages 支持说明、provider 兼容矩阵行和未发布变更记录。

### 2026-05-13 P7 验收

- 已通过: `cargo fmt --all`。
- 已通过: `cargo test -p codex-app-transfer-adapters`
  - 结果:484 unit tests passed;12 `anthropic_messages_request` integration tests passed;10 `anthropic_messages_response` integration tests passed;3 `responses_streaming` integration tests passed。
  - 既有 warning 仍为 `gemini_oauth` 未使用 import 与 `grok_web` dead_code,非本次 P7 新增。
- 已通过: `cargo test -p codex-app-transfer-registry`
  - 结果:45 unit tests passed;7 `golden_compat` integration tests passed。
- 已通过: `cargo test -p codex-app-transfer`
  - 结果:78 unit tests passed。
  - 覆盖 `anthropic_aliases_never_bypass_proxy`、Anthropic Messages provider test URL/header/body、模型列表 `/v1/models` 推导与 `normalize_provider_api_format` canonical 保留。
- 已通过: `cargo check -p codex-app-transfer --features custom-protocol`
  - 说明:验证 `src-tauri/build.rs` 监听 `../frontend`、`tauri.conf.json` `frontendDist=../frontend` 与 `include_dir!("$CARGO_MANIFEST_DIR/../frontend")` 这条 Tauri/Rust 静态资源嵌入链路可编译。
- 已完成:Claude-compatible Anyrouter text、tool-call 与 `previous_response_id` continuation 真实验证。
  - 2026-05-13/14 Anyrouter 代理端到端探针返回 `function_call(name=echo_probe, arguments={"value":"ok"})`,第二轮 `previous_response_id` + `function_call_output` 返回 `output_text=done` 且 `response.completed.status=completed`。
  - 2026-05-14 用户手动完成一轮完整编程验证,确认真实 coding workflow 可用。
- 仍阻塞:官方 Anthropic/Claude provider text、tool-call、`previous_response_id`、upstream error 验证。
  - 只读探测未输出 secret 值。
  - 当前 shell 未检测到 `ANTHROPIC_API_KEY` / `CLAUDE_API_KEY`。
  - `~/.codex-app-transfer/config.json` 中没有 Anthropic/Claude/`anthropic_messages` provider,也没有 baseUrl 包含 Anthropic/Claude 的 provider。
  - 当前项目已有 Anthropic API key / 手动 `sk-ant-oat*` token header 兼容处理,但没有类似 Gemini CLI / Antigravity 的 Anthropic 账号 OAuth 登录授权流程。
  - 因此本轮仍不添加官方 Claude preset。

### 2026-05-13 Anyrouter 专属 preset 与真实探测

- 新增 Anyrouter 内置 provider 卡片:
  - `baseUrl=https://anyrouter.top`
  - `apiFormat=anthropic_messages`
  - 默认模型 `claude-opus-4-7`
  - `extraHeaders.anthropic-beta=claude-code-20250219,context-1m-2025-08-07,...`
  - `requestOptions.anthropic_messages.claude_code_compat=true`
  - `requestOptions.anthropic_messages.thinking={type:"adaptive"}`
- 新增代理定向行为:
  - 默认仍剥离 `[1m]` 内部模型后缀,保持 DeepSeek / 百炼等既有上下文窗口标记语义;Anyrouter 新实测不再 opt-in 保留 `[1m]`。
  - Anthropic Messages mapper 支持 `requestOptions.anthropic_messages.thinking` 注入;普通 provider 注入 fixed-budget thinking 时仍会把过小的 `max_tokens` 自动提升到 `budget_tokens + 1024`。
  - 当 provider 设置 `requestOptions.anthropic_messages.claude_code_compat=true` 时,自动补 Claude Code system 首块、JSON 字符串形态 `metadata.user_id`、动态 `X-Claude-Code-Session-Id`、`x-app: cli`、Claude CLI User-Agent 与完整 beta header。
- 真实 Anyrouter 探测结果:
  - 未加 `[1m]`:上游 400 `1m 上下文已经全量可用，请启用 1m 上下文后重试`。
  - 仅加 `anthropic-beta`:上游 500 `new_api_panic`。
  - 加 `[1m]` 但不加 beta/thinking:仍为上游 400。
  - 加 `[1m]` + Claude Code 指纹 + `thinking.type=adaptive`:上游返回 429 `Service Unavailable`。
  - 使用 `claude-opus-4-7` + Claude Code 指纹 + `thinking.type=adaptive`:非流式 200,流式 200。
  - 使用 `claude-opus-4-7` + Claude Code 指纹 + `thinking.type=enabled`:上游 520。
  - 使用 `claude-opus-4-7` + adaptive thinking 但不加 Claude Code 指纹:上游 503。
  - 使用 Claude Code system 首块后追加本地 system 指令:上游 200,说明可以保留 Codex 的系统指令。
  - 结论:Anyrouter 的可用路径是 Claude Code 兼容形态 + unsuffixed model + adaptive thinking;仍不能把通用 Claude preset 标记为完整验证通过。
- 本地打包验证:
  - 已通过:`cargo test -p codex-app-transfer-adapters provider_request_options`。
  - 已通过:`cargo test -p codex-app-transfer-adapters prepared_request_headers_match_claude_code_metadata_session`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`。
  - 已通过:`cargo test -p codex-app-transfer-registry presets`。
  - 已通过:`make mac-app`。
  - 产物:`dist/mac/Codex App Transfer.app`。
  - 说明:`dist/` 已被 git ignore,不会进入提交。

### 2026-05-13 P8 Anyrouter / Anthropic Messages 深水区补齐

- LiteLLM 参考:
  - `llms/anthropic/chat/transformation.py::map_web_search_tool` 使用 `type=web_search_20250305,name=web_search`,并把 OpenAI `search_context_size` 映射到 Anthropic `max_uses`。
  - `llms/anthropic/chat/handler.py` 将 `server_tool_use` 纳入工具块解析,但明确不把 `web_search_tool_result` 的 `input_json_delta` 当作本地工具调用。
  - `llms/anthropic/chat/transformation.py` 非流式解析保留 `web_search_tool_result`、citations、thinking 与 `redacted_thinking`。
- 本项目补齐:
  - Responses `web_search` 在 `anthropic_messages` / Anyrouter provider 下转 Anthropic hosted web search,不再 drop 后退回本地 MCP web search。
  - Anthropic response mapper 现在把 `server_tool_use(name=web_search)` 输出为 Responses `web_search_call`,把 `web_search_tool_result` 里的 URL 结果累积为 `response.output_text.annotation.added`。
  - Anthropic thinking `signature_delta` 保存到 session-only context,续轮请求重新发回原生 `thinking`/`redacted_thinking` block。
  - Anyrouter preset 加 `requestOptions.web_search_enabled=true` 与 `requestOptions.proxy.force_default_model=true`,避免 Claude 自动触发的 `gpt-5.4` / `gpt-5.4-mini` 等入站模型名绕过 default。
- 日志复核:
  - `~/.codex-app-transfer/logs/proxy-2026-05-13.log` 显示 16:26:41 出现 `gpt-5.4` 与 `gpt-5.4-mini` 入站模型名,随后都被映射为 `claude-opus-4-7`。
  - 同一日志 16:27:17 至 16:28:47 显示 `gpt-5.4` 入站后最终请求体仍为 `model:"claude-opus-4-7"`,上游返回 429 `Service Unavailable`。
  - 结论:用户指出的“Claude/Codex 可能自动触发其他模型调用”属实。本轮 `force_default_model` 不是只覆盖已知 OpenAI slot,还覆盖 provider default 路由下的未知模型名与 slash-route 显式模型名。
- P8 验证:
  - 已通过:`cargo fmt --all`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_response`。
  - 已通过:`cargo test -p codex-app-transfer-adapters`。
  - 已通过:`cargo run -p xtask -- gen-fixtures`。
  - 已通过:`cargo test -p codex-app-transfer-registry`。
  - 已通过:`cargo test -p codex-app-transfer-proxy resolver::tests::force_default_model`。
  - 已通过:`cargo test -p codex-app-transfer-proxy resolver::tests::slash_route_preserves_explicit_openai_slot_without_force_default`。
  - 已通过:`cargo test -p codex-app-transfer-proxy`。沙箱内首次因本地端口绑定权限失败;提升权限后完整通过。

### 2026-05-13 Anyrouter 实测补充

- 本机 `~/.codex-app-transfer/config.json` 中的 Anyrouter provider 已命中 builtin preset,但保存内容仍停留在旧 preset 状态:
  - 有 `apiFormat=anthropic_messages`、`claude_code_compat=true`、`thinking.type=adaptive`。
  - 缺 `requestOptions.proxy.force_default_model=true`、`requestOptions.web_search_enabled=true` 与 `modelCapabilities.claude-opus-4-7.context_window=1000000`。
- 已修复 registry healing 覆盖范围:
  - Anyrouter builtin provider 现在会从 preset 恢复 `requestOptions` 与 `modelCapabilities`,避免旧 Anyrouter 卡片升级后仍缺 native web search、强制 default model 和 1M 能力声明。
  - 仍保留用户字段:`apiKey`、`baseUrl`、`models`、`name`、`id`、`sortIndex`。
  - 非 Anyrouter builtin provider 仍保持原有 `requestOptions` / `modelCapabilities` 用户可调行为。
- 真实 Anyrouter 代理探测:
  - 修复前,text 探针曾返回 200 `pong`;tool 探针返回 Anyrouter 429;web 探针因本地配置缺 `web_search_enabled` 没有走 hosted web search。
  - 修复后,日志确认 `gpt-5.4-mini` / `gpt-5.4` 入站仍统一出站为 `model:"claude-opus-4-7"`。
  - 修复后,Responses `web_search` 成功走 Anthropic hosted web search,探针确认 `webSearch=true` 并返回查询结果。
  - 修复后,text 与 forced tool-call 探针均收到 Anyrouter 429 `Service Unavailable`;其中 tool-call 在上游返回 429 前没有生成 `function_call`,因此 continuation 无法验证。
- 当前结论:
  - Anyrouter 连接、默认模型路由和 Anthropic native web search 可用。
  - 上游 429 在本轮日志中不是模型名绕错导致;请求体出站 model 已是 `claude-opus-4-7`。
  - tool-call / continuation 仍未获得端到端通过结果,需要等 Anyrouter 上游不再对该请求返回 429 后复测。

### 2026-05-13 P9 剩余协议丢失面补齐

- LiteLLM 参考优先级:
  - `docs/litellm/litellm/llms/anthropic/chat/transformation.py::_map_tool_helper` 明确将 OpenAI function/custom tool 转为 Anthropic tool,并规范 `input_schema.type=object`。
  - 同一函数支持 Anthropic hosted tools、`computer_*` tools、OpenAI `mcp` tool -> Anthropic `mcp_servers` 的 `type=url` 映射。
  - 同一函数对 `cache_control`、`defer_loading`、`allowed_callers`、`input_examples` 做工具扩展字段透传。
  - `docs/litellm/litellm/responses/litellm_completion_transformation/transformation.py::transform_responses_api_tools_to_chat_completion_tools` 保留 Responses function tool 的 `strict` 与扩展字段。
  - `docs/litellm/litellm/llms/anthropic/chat/transformation.py` 非流式响应解析会收集非 web 的 `*_tool_result` 到 provider-specific tool results,不静默忽略。
- 本项目补齐:
  - Responses -> Chat 层保留 function/custom tool 的 `cache_control`、`defer_loading`、`allowed_callers`、`input_examples`;`strict:true` 在 Anthropic Messages 层写入 `input_schema.strict`。
  - `namespace` 展平时把 namespace name/description 注入内层 function description,避免 MCP server 语义丢失;response side 继续用 original request 回灌 `namespace`。
  - `custom` tool 保留 `format` 的 grammar/schema 语义到 description 与 `input` 参数说明,不再只剩一个无上下文的字符串参数。
  - Anthropic hosted tools 支持 `web_search_*`、`bash*`、`text_editor*`、`code_execution*`、`web_fetch*`、`memory*`、`tool_search_tool*` 与 `computer_*` passthrough。
  - Responses `computer_use_preview` 映射为 Anthropic `computer_20250124`;Responses `mcp` 映射到 Anthropic request 顶层 `mcp_servers`。
  - 非 web 的 Anthropic `*_tool_result` 与未知 content block 现在保留为 Responses `reasoning` trace item,不进入 session thinking 续传,避免污染下一轮工具回合。
  - 不可等价的 Responses `file_search`、`image_generation` 等 file/image 类工具未伪装成 Claude 可执行工具,仍走现有 warn/drop 诊断路径。
- P9 验证:
  - 已通过:`cargo fmt --all`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request namespace_and_custom_tool_metadata_survives_anthropic_lowering`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request anthropic_native_tools_and_mcp_server_tools_are_preserved`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_response unsupported_anthropic_tool_result_is_preserved_as_trace_item`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_response`。
  - 已通过:`cargo test -p codex-app-transfer-adapters`。

### 2026-05-13 P10 LiteLLM Claude 能力复原补齐

- LiteLLM 参考优先级:
  - `docs/litellm/litellm/llms/anthropic/common_utils.py::get_anthropic_headers` / `update_headers_with_optional_anthropic_beta` 作为 beta header 自动检测基线。
  - `docs/litellm/litellm/llms/anthropic/chat/transformation.py::map_response_format_to_anthropic_output_format` 与 `filter_anthropic_output_schema` 作为 Claude structured output 映射基线。
  - `docs/litellm/litellm/llms/anthropic/experimental_pass_through/messages/transformation.py::_translate_reasoning_effort_to_anthropic` / `_translate_legacy_thinking_for_adaptive_model` 作为 Claude 4.6/4.7 adaptive thinking 映射基线。
  - `docs/litellm/litellm/types/llms/anthropic.py::AnthropicMessagesRequestOptionalParams` 作为 Anthropic top-level 字段保留清单。
- 本项目补齐:
  - Responses -> Chat 在 Anthropic provider 下保留 `context_management`、`container`、`output_config`、`output_format`、`speed`、`cache_control`,并保留 Claude `xhigh` / `max` reasoning effort 名称。
  - Anthropic Messages request 将 OpenAI/Responses `context_management` compaction list 转为 Anthropic `edits[{type:compact_20260112,trigger}]`。
  - Responses `text.format` / Chat `response_format` 转 Anthropic `output_format`,并过滤 `minLength`、`maxLength`、`minimum`、`maximum`、`minItems`、`maxItems` 等 Claude output schema 不支持的约束,约束信息并入 description。
  - Claude 4.6/4.7 模型的 legacy `thinking.type=enabled` / `reasoning_effort` 转为 `thinking.type=adaptive + output_config.effort`,对齐 LiteLLM adaptive thinking 行为。
  - Claude `document`、`container_upload`、tool_result 富内容、`is_error`、`cache_control` 现在保留;出现 `container_upload` 时自动补 `code_execution_20250522` hosted tool。
  - request headers 会按 computer use、MCP client、advanced tool use、file-id documents、code execution、container skills、context management / compact、structured output、effort、web fetch、fast mode、advisor tool 自动追加 Anthropic beta header;proxy 转发层会把 provider 配置里的静态 `anthropic-beta` 与 adapter 动态 beta 合并为单个 header,避免用户卡片覆盖 LiteLLM 对齐后的能力开关。
  - 默认 Anthropic Messages header 现在包含 `anthropic-version`、`accept: application/json` 与 `content-type: application/json`。
- P10 验证:
  - 已通过:`cargo fmt --all`。
  - 已通过:`cargo fmt --all --check`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`。当前 21 tests passed,包含 file-id document 与 non-adaptive `output_config.effort` beta 覆盖。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_response`。
  - 已通过:`cargo test -p codex-app-transfer-adapters`。
  - 已通过:`cargo test -p codex-app-transfer`。沙箱内第一次因本地端口监听权限失败;提升权限后 78 tests passed。
  - 已通过:`cargo test -p codex-app-transfer-proxy --test auth_and_routing`。沙箱内第一次因本地端口监听权限失败;提升权限后 16 tests passed。
  - 已通过:`cargo test -p codex-app-transfer-proxy`。提升权限后 65 unit tests、16 auth/routing tests、1 cache miss e2e、4 streaming passthrough tests 全部通过。

### 2026-05-13 P11 Messages 协议 drop 面系统排查

- 排查基线:
  - 已按架构文档要求从本地实现与主仓库 `docs/litellm/` 参考目录对照,未做外部搜索。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response`。当前 21 request tests、13 response tests 全部通过。
- 已确认不是当前 drop 的主链路:
  - Responses `web_search` 在 Anthropic Messages / Anyrouter 下已转 Anthropic hosted web search;Anthropic `server_tool_use(name=web_search)` 与 `web_search_tool_result` / `web_fetch_tool_result` 已转为 Responses web search call 与 URL annotations。
  - Anthropic `thinking` / `redacted_thinking` 与 `signature_delta` 已保留到 session cache 的原生 Anthropic content block,续轮会直接回灌原生 thinking block。
  - 未知 Anthropic `content_block_start` 与未知 `content_block_delta` 已保留为 Responses `reasoning` trace item,不是静默丢弃。
- P13/P14 已处理的原丢失项:
  - Responses `input_file.file_id` 已直接转 Anthropic `document.source={type:file,file_id}`。
  - Responses reasoning item 已可累积并转为 Anthropic `thinking` block;孤儿 reasoning 会作为 assistant thinking message 续传。
  - Anthropic response side 的 `message_start/message_delta.container`、`context_management`、`compaction` 已进入 Responses `metadata.anthropic_*` provider-specific 字段。
  - Anthropic `usage.server_tool_use` 已进入 Responses `usage.server_tool_use`。
  - 非 web `bash_code_execution_tool_result` / `code_execution_tool_result` 已映射为 Responses `code_interpreter_call`,并保留 container id、call id、code 与 logs。
  - 顶层 Anthropic `stop_sequences`、`inference_geo`、直接传入的 `mcp_servers` 现在会直传;assistant `tool_use.cache_control` / `caller` 也会保留。
- 仍有明确丢失或语义降级面:
  - `input_audio` / `input_video` 在 Anthropic Messages 路径没有原生等价映射;audio 会降级为 JSON/text,video URL 会按 image_url 或文本处理。
  - 未识别 chat role 在 Anthropic Messages request lowering 中会跳过;`system` / `developer` 已收敛到 top-level system,但未来新 role 会静默丢。
  - 工具定义中缺 `type` / 缺 name、空 namespace、缺 display 尺寸的 `computer_use_preview` 会被丢弃或 warn/drop;Responses-only `file_search`、`image_generation`、`local_shell` 等仍按无 Anthropic 等价能力处理。
- 建议后续优先级:
  1. P11.5:继续对照 LiteLLM native Messages passthrough,确认 advisor / tool_search 历史块是否还有 provider-specific 字段需要保留。
  2. P11.6:若真实 Claude 上游返回更多非 web server tool result 类型,补充到 structured result 或 provider trace。
  3. P11.7:评估 `input_audio` / `input_video` 在 Claude 最新 Messages API 中是否已有原生 block,避免长期文本降级。

### 2026-05-13 P12 LiteLLM 路径复核与直接转换决策

- LiteLLM 路径结论:
  - Native Anthropic `/v1/messages` provider 走原生 Messages passthrough,`messages/transformation.py` 明确写着 Anthropic Messages request/response 不需要转换,只做 max_tokens、thinking、context_management、advisor history 等 Anthropic 自身规范化。
  - OpenAI provider 的 Anthropic `/v1/messages` 入口默认走 Responses API 直连路径:`messages/handler.py` 中 `_RESPONSES_API_PROVIDERS={"openai"}`,未开启 `use_chat_completions_url_for_anthropic_messages` 时调用 `LiteLLMMessagesToResponsesAPIHandler`,后者直接 `translate_request` 后调用 `litellm.responses()` / `litellm.aresponses()`。
  - LiteLLM 的 `responses_adapters/transformation.py` 是直接 `Anthropic /v1/messages <-> OpenAI Responses API`,没有先转 Chat;streaming wrapper 也直接把 Responses 事件映射为 Anthropic SSE。
  - Chat/completions fallback 只用于非 Responses provider 或显式 opt-out,不是 OpenAI Responses 与 Anthropic Messages 互转的默认路径。
- 本项目决策:
  - 后续 `anthropic_messages` 不再以 Responses -> Chat-shaped messages -> Anthropic Messages 作为主实现路径。
  - 新增一条完整直接转换路径,由 Responses input/output item 直接映射到 Anthropic Messages messages/system/tools/mcp_servers/thinking/context_management,并复用共享的 session/cache/compact/tool-pairing core 能力,而不是复用旧 Chat provider lowering。
  - Chat 中间态后续仅作为 OpenAI-compatible Chat provider 的实现细节或临时 fallback,不得作为 Claude / Anthropic Messages 协议保真的目标形态。

### 2026-05-13 P13 Anthropic Messages 请求主链路改为直转

- 修复原因:
  - 用户明确指出 LiteLLM 默认是 Anthropic Messages <-> Responses 直转,此前本项目请求侧实现却复用了 `responses_body_to_chat_body_for_provider_with_session` 再降到 Anthropic Messages,违背了协议层完整切分的架构目标。
- 本项目修正:
  - `crates/adapters/src/anthropic_messages/request.rs` 的非 compact 主入口已改为直接解析 Responses `input` item,生成 Anthropic `system`、`messages`、`tools`、`mcp_servers`、`tool_choice`、`thinking`、`context_management`、`output_format`、`metadata`。
  - `previous_response_id` 只复用 `merge_messages_with_previous_response` 与 `ResponseSessionCache` 这类协议无关会话能力;孤立 tool result 只复用 `ToolCallCache` 与 artifact output 压缩能力,不再经过 OpenAI Chat body。
  - `/responses/compact` 已先构造 synthetic Responses compact body,再走 Anthropic Messages 直接转换;`anthropic_messages` 路径不再调用 `build_compact_chat_request`。
  - Responses `input_file.file_id` 现在直接转 Anthropic `document.source={type:file,file_id}`;Responses reasoning item 可直接转 Anthropic `thinking` block,不再依赖 Chat `reasoning_content` 中间字段。
- 验证:
  - 已通过:`cargo fmt --all`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response`。
  - 已通过:`cargo test -p codex-app-transfer-adapters`。
  - 已确认:`rg -n "responses_body_to_chat_body_for_provider|responses_body_to_chat_body_for_provider_with_session|compact_chat|chat_body_to_anthropic" crates/adapters/src/anthropic_messages crates/adapters/src/mapper/anthropic_messages.rs` 无输出;旧 `chat_body_to_anthropic_messages_request` helper 已移除。

### 2026-05-13 P14 LiteLLM 直转遗漏补齐

- LiteLLM 参考:
  - `docs/litellm/litellm/llms/anthropic/experimental_pass_through/messages/transformation.py` 的 native Messages 参数集。
  - `docs/litellm/litellm/llms/anthropic/experimental_pass_through/responses_adapters/transformation.py` 的 Anthropic Messages <-> Responses 直接 adapter。
  - `docs/litellm/litellm/types/llms/anthropic.py` 中 `cache_control`、`caller`、`mcp_servers`、document、tool result 与 usage/server tool 类型定义。
- 本项目修正:
  - request side 直传 `stop_sequences`、`inference_geo` 与顶层 `mcp_servers`,并与 Responses `tools[type=mcp]` 生成的 Anthropic `mcp_servers` 合并。
  - `instructions` / system / developer content 若带 Anthropic text block 或 `cache_control`,会保留为 top-level `system` block list,不再统一字符串化。
  - assistant `tool_use` 与 Responses `function_call` 续转 Anthropic `tool_use` 时保留 `cache_control` 与 `caller`。
  - response side 将 `usage.server_tool_use` 保留在 Responses usage,并把 `container`、`context_management`、`compaction` 放入 `metadata.anthropic_*`。
  - 未知 `content_block_delta` 现在会输出 provider trace;`bash_code_execution_tool_result` / `code_execution_tool_result` 会输出 `code_interpreter_call`。
  - session cache 内 assistant 历史改为 Anthropic 原生 `content` block list,不再保存 Chat-shaped `tool_calls` / `anthropic_thinking_blocks`。
- 验证:
  - 已通过:`rustfmt --edition 2021 crates/adapters/src/anthropic_messages/request.rs crates/adapters/src/anthropic_messages/response.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response`。当前 22 request tests、14 response tests 全部通过。
  - 已确认:`rg -n "responses_body_to_chat|chat_body_to_anthropic|build_compact_chat|compact_chat|chat_body" crates/adapters/src/anthropic_messages crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs crates/adapters/src/mapper/anthropic_messages.rs` 无输出。

### 2026-05-13 P15 Messages 直转保真再排查

- LiteLLM 对照:
  - `docs/litellm/litellm/llms/anthropic/experimental_pass_through/messages/transformation.py` 对 native Messages passthrough 只做 Anthropic 自身规范化,包括 filtering billing header、thinking/context_management 处理,不降级到 Chat。
  - `docs/litellm/litellm/types/llms/anthropic.py` 中 text、tool_use、document、tool_result 等 content block 均允许携带 Anthropic 扩展字段,其中 `cache_control`、`caller`、document metadata 需要保留。
  - `docs/litellm/litellm/llms/anthropic/chat/transformation.py::extract_response_content` 会识别 `server_tool_use`、`*_tool_result`、web search results 与 code execution result,不是静默丢弃。
- 本轮确认的缺口:
  - request side 的 Responses content array 中文本、图片、文件、thinking block 仍有 `cache_control` / document metadata 丢失面。
  - assistant history 中直接传入的 Anthropic native `server_tool_use`、`web_search_tool_result`、`tool_search_tool_result`、`advisor_tool_result`、`compaction` 和其他 `*_tool_result` 会被字符串化。
  - response side 虽能输出 Responses event,但 `previous_response_id` session cache 会从 Responses output items 反推 assistant message,导致 server tool use/result、code execution result 等 Anthropic 原生历史在续轮中丢失或降级。
  - Anthropic native `tool_choice.disable_parallel_tool_use`、message `name` 与 LiteLLM billing-header filtering 也有对齐缺口。
- 本项目修正:
  - request side 对 `text/input_text/output_text`、`input_image`、`input_file`、`thinking/redacted_thinking` 保留 `cache_control`;`input_file` 同时保留 `title`、`context`、`citations`。
  - assistant message content 对已知 Anthropic server tool/result block 走原样保留,不再文本化;未知 `*_tool_result` 也按原生 block 保留。
  - request side 保留 message `name`,保留 Anthropic native `tool_choice.disable_parallel_tool_use`,并过滤 `x-anthropic-billing-header:` system text。
  - 按 LiteLLM native Messages 规则处理 advisor history:未启用 `advisor_20260301` tool 时移除 `server_tool_use(name=advisor)` 与 `advisor_tool_result`,避免 Anthropic 上游 400;启用 advisor tool 时原样保留。
  - response side 新增 `closed_session_blocks`,session cache 优先保存 Anthropic 原生 assistant content block list;`server_tool_use`、web search result、tool_search result、code execution result、text、thinking 均可续轮原样回灌。
  - 不修改 chat、gemini_native 或其他协议转换区块;本轮代码变更只落在 `crates/adapters/src/anthropic_messages/*` 与对应测试。
- P15 验证:
  - 已通过:`rustfmt --edition 2021 crates/adapters/src/anthropic_messages/request.rs crates/adapters/src/anthropic_messages/response.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response`。当前 25 request tests、15 response tests 全部通过。
  - 已通过:`git diff --check -- crates/adapters/src/anthropic_messages/request.rs crates/adapters/src/anthropic_messages/response.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs`。
  - 已确认:`rg -n "responses_body_to_chat|chat_body_to_anthropic|build_compact_chat|compact_chat|chat_body" crates/adapters/src/anthropic_messages crates/adapters/src/mapper/anthropic_messages.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs` 无输出。

### 2026-05-13 P16 Anyrouter forced tool-call 失败原因

- 现象:
  - 代理日志中 failed forced tool-call 的最终上游请求已经是 `model:"claude-opus-4-7"`,工具定义和 `tool_choice:{"type":"tool","name":"echo_probe"}` 都已到达 Anthropic Messages 请求体。
  - 同一个请求还带有 `thinking:{"type":"adaptive"}`;在代理流式路径上游返回 Anyrouter 429 `Service Unavailable`,没有产生 Responses `function_call`,因此 continuation 不能验证。
- LiteLLM / Anthropic 对照:
  - LiteLLM 的 Anthropic tool_choice mapper 会把 OpenAI named function tool choice 映射为 Anthropic `{"type":"tool","name":...}`。
  - LiteLLM native Messages passthrough 同时支持 `tool_choice` 与 `thinking`,但在 extended thinking 与工具历史冲突时会移除 `thinking` 来保留工具回合连续性。
  - Anthropic 官方 extended thinking 文档明确说明强制工具选择 `tool_choice.type=any/tool` 不兼容 extended thinking;只允许 `auto` 或 `none`。
- Anyrouter A/B 验证:
  - `tool_choice.type=tool` + `thinking.type=adaptive`:直连 Anyrouter 返回 520 空响应;代理流式探针表现为 429 `Service Unavailable`。
  - `tool_choice.type=tool` 且不带 `thinking`:直连 Anyrouter 返回 200,并产生 `tool_use(name=echo_probe,input.value=ok)`。
  - 有工具且带 `thinking.type=adaptive`,但不强制 tool_choice 或显式 `auto`:直连 Anyrouter 返回 200。
  - 文本请求 + `thinking.type=adaptive`:直连 Anyrouter 返回 200。
- 修复:
  - `crates/adapters/src/anthropic_messages/request.rs` 在 provider request options 注入完成后检查最终 Anthropic wire body。
  - 若 `tool_choice.type` 为 `any` 或 `tool`,则移除 `thinking`,并移除 `output_config.effort`;若 `output_config` 为空则整体移除。
  - 保留原始强制 `tool_choice`,不降级为 `auto`,因为用户或上游显式要求 forced tool-call 时,工具调用语义优先于 thinking。
- P16 验证:
  - 已通过:`cargo fmt --all --check`。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request`。当前 25 request tests 全部通过。
  - 已通过:`cargo test -p codex-app-transfer-adapters forced_tool_choice_drops_incompatible_thinking_but_preserves_tool_choice`。
  - 已通过 Anyrouter 代理端到端探针:`cargo run --manifest-path .tmp/anyrouter-e2e/Cargo.toml`。
  - 探针使用当前 worktree 的 `build_router` / `StaticResolver`,读取本机 Anyrouter provider 配置并应用 builtin healing。
  - 第一轮请求:`model=gpt-5.4-mini`、forced `tool_choice`、`reasoning.effort=high`;代理最终路由到 Anyrouter default 后返回 `function_call(name=echo_probe, arguments={"value":"ok"})`。
  - 第二轮请求:`previous_response_id` + `function_call_output`;代理续轮返回 `output_text=done`,并收到 `response.completed.status=completed`。

### 2026-05-14 P17 LiteLLM 错误恢复与 Anthropic OAuth 头补齐

- LiteLLM 参考:
  - `litellm/llms/base_llm/anthropic_messages/transformation.py` 在 Anthropic `/v1/messages` 400 命中 invalid thinking signature 时允许一次 HTTP retry。
  - `litellm/llms/anthropic/common_utils.py` 使用 `invalid` + `signature` + `thinking` + `block` 四个关键字识别签名错误,并移除 `messages[].content[]` 中的 `thinking` / `redacted_thinking` block 和顶层 `thinking`。
  - 同文件以 `sk-ant-oat` 识别 Anthropic OAuth token,把认证切到 `Authorization: Bearer`,合并 `oauth-2025-04-20` beta,并补 `anthropic-dangerous-direct-browser-access:true`。
  - LiteLLM 的 Vertex/Azure/Bedrock Claude 分支会移除 `cache_control.scope`,但这是 provider-specific 兼容项,当前用户已明确降级为 follow-up。
- 本项目修正:
  - `crates/adapters/src/anthropic_messages/request.rs` 新增 invalid thinking signature 检测和请求体清理函数。
  - 清理函数会同步处理 `RequestPlan.response_session.messages`,避免重试成功后把旧无效签名继续写入本地 session cache。
  - `crates/proxy/src/forward.rs` 在上游 400 body 被消费后新增 Anthropic invalid thinking signature 分支,只重发一次,不改变 web_search 透明降级既有逻辑。
  - `crates/proxy/src/forward.rs` 新增 Anthropic OAuth token 头处理;普通 Anyrouter Bearer key 不匹配 `sk-ant-oat`,因此不会注入 OAuth beta 或 dangerous browser header。
  - `docs/followup-tracker.md` 新增 #24,记录 Vertex/Azure/Bedrock `cache_control.scope` 后续 provider-specific 清理事项。
- P17 验证:
  - 已通过:`cargo fmt --all --check`。
  - 已通过:`cargo test -p codex-app-transfer-adapters invalid_signature_retry_strips_thinking_blocks_from_body_and_session`。
  - 已通过:`cargo test -p codex-app-transfer-adapters invalid_thinking_signature_error_detector_matches_litellm_keywords`。
  - 已通过:`cargo test -p codex-app-transfer-adapters`。当前 491 unit tests、25 request integration tests、15 response integration tests、3 responses streaming tests 全部通过。
  - 已通过:`cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response`。当前 25 request tests、15 response tests 全部通过。
  - 已通过:`cargo test -p codex-app-transfer-proxy --test auth_and_routing`。当前 19 proxy routing/auth tests 全部通过;本地端口绑定需要提升权限。
  - 已通过 Anyrouter 真实连接回归:`cargo run --manifest-path .tmp/anyrouter-e2e/Cargo.toml`。第一轮返回 `function_call(name=echo_probe, arguments={"value":"ok"})`;第二轮 `previous_response_id` + `function_call_output` 返回 `output_text=done`,且 `response.completed.status=completed`。

### 2026-05-13 手动测试 app 打包

- 已运行:`make mac-app`。
- 产物:`dist/mac/Codex App Transfer.app`。
- 校验:
  - bundle 存在,主可执行文件存在:`dist/mac/Codex App Transfer.app/Contents/MacOS/codex-app-transfer`。
  - app 大小约 `34M`。
  - `codesign -dv` 显示 `Signature=adhoc`,架构为 `arm64`;这是本地自测未签名 app 的预期形态。
