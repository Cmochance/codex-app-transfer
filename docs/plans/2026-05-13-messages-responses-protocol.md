# Messages <=> Responses 协议补充完整方案

> 日期: 2026-05-13
> 目标: 为 Claude 系列模型新增一条一等公民的 Anthropic Messages 协议适配路径,让本地 Codex Responses 请求可以转换为上游 `/v1/messages`,并把上游 Anthropic Messages SSE 转回 OpenAI Responses SSE。
> 当前状态: P14 已按 LiteLLM 默认路径将 `anthropic_messages` 主链路改成 Responses <-> Anthropic Messages 直接转换,并继续补齐顶层 Anthropic 参数、system block cache、tool_use extension fields、server_tool_use usage、provider metadata 与 code execution result 映射。Anyrouter 实测已确认连接、default model 路由与 Anthropic native web search 可用;tool-call / continuation 仍被 Anyrouter 429 `Service Unavailable` 阻断,未能端到端确认。Claude preset 仍未添加。

## 1. 结论

本功能应新增 `anthropic_messages` 协议,而不是继续把 `anthropic` / `claude` / `messages` 当成 `responses` 的历史别名。原因是这些值在当前 UI 文案里已经被展示成 Anthropic Messages,但实际运行时仍走 Responses -> Chat 转换,这会让 Claude 原生 Messages 上游拿到错误 wire shape。

实施路径已遵守根目录架构文档中的 `core + mapper + thin adapters` 规则:

- `core` 继续只放协议无关的生命周期能力,例如路由归一化、会话恢复、Responses SSE 事件拼装。
- `mapper/anthropic_messages.rs` 承担 Responses <=> Anthropic Messages 的协议映射。
- `anthropic_messages/mod.rs` 只做薄编排,像 `responses`、`gemini_native`、`grok_web` 一样调用 mapper trait。
- `registry` 已增加 `anthropic_messages` adapter,并显式处理 `anthropic` / `claude` / `messages` / `claude_messages` 迁移。
- P7 正在同步 `ARCHITECTURE_PROTOCOL_GUIDE.md`、Phase 5 RFC、README/CHANGELOG 与变更清单,避免文档继续停留在 Phase 4 结构。

## 2. 已完成的参考基线

### 2.1 本地 LiteLLM 已更新

`docs/litellm` 是 `.gitignore` 中声明的本地参考目录,不是当前仓库跟踪文件。已从 `https://github.com/BerriAI/litellm` 克隆最新 main 到临时目录,再用 `rsync --delete --exclude .git` 同步回 `docs/litellm`。

当前参考基线:

- LiteLLM version: `1.85.0`
- LiteLLM main HEAD: `431daa1479f0af506696d1dff236d95566abdddc`
- HEAD summary: `431daa1 Merge pull request #27812 from BerriAI/litellm_lazyFeatureRootPath`
- 同步校验: `rsync -ani --delete --exclude .git /private/tmp/codex-litellm-main-20260513/ docs/litellm/` 无输出。

### 2.2 可借鉴代码

本地 LiteLLM 1.85.0 中存在可直接借鉴的 Anthropic 相关实现:

- `docs/litellm/litellm/llms/anthropic/experimental_pass_through/responses_adapters/transformation.py`
  - 明确实现 Anthropic `/v1/messages` 与 OpenAI Responses 的字段互转。
  - 可借鉴内容: `messages` / `system` / `tools` / `tool_choice` / `thinking` / `context_management` / `metadata.user_id` 的映射规则。
- `docs/litellm/litellm/llms/anthropic/experimental_pass_through/responses_adapters/streaming_iterator.py`
  - 实现 Responses SSE -> Anthropic Messages SSE。
  - 本项目需要相反方向,但事件对应关系可反向使用: `message_start`、`content_block_start`、`content_block_delta`、`message_delta`、`message_stop`。
- `docs/litellm/litellm/llms/anthropic/experimental_pass_through/messages/transformation.py`
  - 记录 Anthropic Messages 原生请求的必要约束,例如 `max_tokens` 必填、`anthropic-version`、`context_management`、thinking 参数处理。
- `docs/litellm/litellm/llms/anthropic/chat/transformation.py`
  - 可借鉴 OpenAI Chat -> Anthropic Messages 的成熟边界处理: tool name sanitize、tool schema 修正、`tool_choice` 映射、metadata.user_id 校验、Anthropic beta header 聚合。
- `docs/litellm/litellm/llms/anthropic/chat/handler.py`
  - 可借鉴 Anthropic SSE chunk 解析逻辑,尤其是 `content_block_delta` 的 `text_delta`、`input_json_delta`、`thinking_delta` 和 `message_delta` usage/stop_reason 处理。

旧 memory 中提到的 `docs/copilot-api-caozhiyuan` 本 checkout 不存在,因此未使用该参考项目。由于 LiteLLM 已经包含精确协议族实现,本方案不再依赖泛化 GitHub 搜索。

### 2.3 官方协议事实

Anthropic 官方文档确认:

- Messages streaming 使用 SSE 事件: `message_start`、`content_block_start`、`content_block_delta`、`content_block_stop`、`message_delta`、`message_stop`,并可能出现 `ping` 与未知事件。
- `content_block_delta` 包含 `text_delta`、`input_json_delta`、`thinking_delta` 等类型。
- 工具调用由 assistant `tool_use` block 表示,工具结果由后续 user `tool_result` block 表示,而且 tool result 必须紧跟对应 tool use,并排在该 user message content 的最前面。

参考:

- https://docs.anthropic.com/en/docs/build-with-claude/streaming
- https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/implement-tool-use

## 3. 当前项目基线

### 3.1 架构约束

根目录 `ARCHITECTURE_PROTOCOL_GUIDE.md` 已明确协议新增流程:

1. 先写最小 RFC 或方案,说明目标、边界、风险、回滚策略。
2. 新协议优先新增 mapper。
3. 实现 `RequestMapper` / `ResponseMapper`。
4. adapter 层仅做薄编排。
5. registry 增加路由入口。
6. 补齐单元测试、契约测试、关键 provider/路径回归。

`docs/protocol-unification-rfc-phase4.md` 已确认 Phase 4 的落地状态: `core` 负责共享生命周期,`mapper` 负责 provider/protocol 差异,`responses` / `gemini_native` / `gemini_cli` adapter 已完成 trait 接线。

当前实际代码还包含 `grok_web` mapper 和 adapter,方案实施时需要把架构文档一起补齐。

### 3.2 P6 前 Anthropic/Messages 行为

P6 前行为不是原生 Anthropic Messages:

- `crates/adapters/src/registry.rs` 把 `anthropic` / `claude` / `messages` 归到 `ResponsesAdapter`。
- `src-tauri/src/admin/handlers/providers/mod.rs` 把这些别名规范化为 `responses`。
- `frontend/js/api.js` 保存 provider 时也把这些值改写成 `responses`。
- `frontend/js/app.js` 与 `frontend/js/i18n.js` 却把它们展示成 Anthropic Messages 或 native passthrough。
- `docs/api-route-status.md` 已记录 `/v1/messages`、`/claude/v1/messages` 目前只是兼容 alias,不是 Rust adapter 的一等入口。

因此这次不是“补一个路由 alias”,而是新增真实协议转换链路。P6 后这些历史别名已在 registry/backend/frontend 三层收敛到 canonical `anthropic_messages`。

## 4. 目标架构

新增模块:

```text
crates/adapters/src/
  anthropic_messages/
    mod.rs              # AnthropicMessagesAdapter 薄编排
    request.rs          # Responses input item -> Anthropic Messages body direct conversion
    response.rs         # Anthropic Messages SSE -> Responses SSE
    types.rs            # 内部状态机与 Anthropic block helper,按需要添加
  mapper/
    anthropic_messages.rs
```

需要修改的现有模块:

```text
crates/adapters/src/lib.rs
crates/adapters/src/mapper/mod.rs
crates/adapters/src/registry.rs
src-tauri/src/admin/handlers/providers/mod.rs
src-tauri/src/admin/handlers/providers/test.rs
src-tauri/src/admin/handlers/providers/models.rs
src-tauri/src/admin/handlers/desktop.rs
frontend/js/api.js
frontend/js/app.js
frontend/js/i18n.js
crates/registry/src/presets_data.json   # P7 真实 Claude 验证通过后才添加 Claude preset
ARCHITECTURE_PROTOCOL_GUIDE.md
docs/protocol-unification-rfc-phase4.md 或新 Phase 5 RFC
```

新增 canonical protocol:

- canonical: `anthropic_messages`
- aliases: `anthropic`, `claude`, `messages`, `claude_messages`

命名选择:

- 不使用单独的 `messages` 作为 canonical,避免它与本地 `/messages` 路由和 OpenAI Responses 私有 alias 混淆。
- `anthropic_messages` 明确表达上游 wire format,与 `gemini_native`、`grok_web` 的命名风格一致。

## 4.1 P2-P6 落地状态

- P2:新增 Phase 5 RFC、Anthropic request/SSE fixtures、request mapper TDD 入口。
- P3:完成 Responses -> Anthropic Messages request mapper。P13 已修正为直接转换路径,不再经由 OpenAI Chat body。
- P4:完成 Anthropic Messages SSE -> Responses SSE response mapper,写入 `ToolCallCache` 与 `ResponseSessionCache`。
- P5:接入 `AnthropicMessagesAdapter`、`mapper::anthropic_messages`、registry alias、proxy adapter headers。
- P6:接入 admin/provider/UI:
  - `apiFormat` 保存归一到 `anthropic_messages`;
  - provider 测速使用 `/v1/messages` 与 Anthropic ping body;
  - 模型列表推导 `/v1/models`;
  - direct mode 仍只允许 `responses` / `openai_responses`;
  - 前端自定义 provider 保存 canonical `anthropic_messages`,旧 alias 仍显示为 Anthropic Messages。

## 5. 请求映射方案

输入: Codex 本地 OpenAI Responses request。

输出: 上游 Anthropic `/v1/messages` request。

主路径:

1. adapter 接收入站 body,保留完整 `original_responses_request`。
2. normal `/responses` / `/messages` 请求直接解析 Responses body:
   - `input` item 直接转 Anthropic user/assistant content block;
   - `previous_response_id` 只复用 `ResponseSessionCache` 合并历史;
   - `function_call_output` 只复用 `ToolCallCache` 修复被截断的 tool_use 关联;
   - 大 tool output 只复用 artifact normalization;
   - 不调用 `responses_body_to_chat_body_for_provider_with_session`,也不生成 OpenAI Chat body 作为中间态。
3. 由 `anthropic_messages/request.rs` 直接输出 Anthropic Messages:
   - `system` / `developer` / `instructions` -> top-level `system`;
   - user text -> `{ "type": "text", "text": ... }`;
   - user image -> `{ "type": "image", "source": ... }`,支持 URL 与可识别的 data URL/base64;
   - assistant text -> `text` block;
   - Responses `function_call` -> assistant `tool_use` block;
   - Responses `function_call_output` 或 role=`tool` message -> 后续 user `tool_result` block;
   - `tools[].function` -> Anthropic `tools[]` 的 `name` / `description` / `input_schema`;
   - `tool_choice=auto|required|none|function name` -> `auto|any|none|tool`;
   - `parallel_tool_calls` -> Anthropic `disable_parallel_tool_use` 反向布尔值;
   - `max_output_tokens` -> Anthropic `max_tokens`;
   - `stop` / `stop_sequences` -> `stop_sequences`;
   - `reasoning` / `reasoning_effort` -> Anthropic `thinking`;Claude 4.6/4.7 这类 adaptive thinking 模型按 LiteLLM 规则转为 `thinking.type=adaptive + output_config.effort`;
   - `metadata.user` / `user` -> `metadata.user_id`,但需要过滤 email/phone 形态,避免 Anthropic 拒绝;
   - Responses `text.format` -> Anthropic `output_format`,并按 LiteLLM `filter_anthropic_output_schema` 规则过滤 Claude structured output 不支持的 schema 约束字段;
   - Anthropic 原生 top-level 字段 `context_management`、`container`、`output_config`、`output_format`、`speed`、`cache_control`、`inference_geo`、`mcp_servers` 允许穿透到 `/messages`;
   - system / developer / `instructions` 带 Anthropic text block 或 `cache_control` 时保留为 top-level `system` block list,普通纯文本仍输出 string;
   - assistant `tool_use.cache_control` / `tool_use.caller` 在 direct path 中保留;
   - Claude `document` / `container_upload` content block 与富 `tool_result` 内容保留;`container_upload` 出现时自动补 Anthropic `code_execution_20250522` hosted tool。
4. 强制 `stream: true`,因为本地 Codex Responses 主链路预期 SSE。
5. 上游路径使用 `/messages`;如果 provider base URL 不含 `/v1`,由 proxy URL 拼接规范决定是否生成 `/v1/messages`。实施前必须补路径测试覆盖以下 base URL:
   - `https://api.anthropic.com/v1`
   - `https://api.anthropic.com`
   - 第三方兼容端点已含 `/anthropic` 或 `/v1` 的场景。
6. 请求 header:
   - 默认补 `anthropic-version: 2023-06-01`;
   - 保留用户配置的 extra headers;
   - 若启用 beta 功能,集中追加 `anthropic-beta`;当前按 LiteLLM 对齐 computer use、MCP client、advanced tool use、file-id documents、code execution、container skills、context management / compact、structured output、effort、web fetch、fast mode 与 advisor tool 生成 beta header。proxy 转发层需要把 adapter 动态 beta 与用户 provider 卡片中已有的 `anthropic-beta` 合并为单个 header,不能让 provider 静态 header 覆盖动态能力开关。

请求侧必须主动校验的规则:

- Anthropic `max_tokens` 必填;若入站没有 `max_output_tokens`,使用项目内可解释默认值,并在方案 RFC 中说明。
- tool result 必须紧跟 tool use;转换层应主动重排可安全重排的 block,无法修复时返回可诊断 400,不能静默丢弃。
- tool name 必须符合 Anthropic `^[a-zA-Z0-9_-]{1,128}$`;需要维护 forward/reverse map,在响应侧还原给 Codex。
- Anthropic server tools 与 Codex client tools 不等价;MVP 不把 server tool 当成本地 function tool。

## 6. 响应映射方案

输入: 上游 Anthropic Messages SSE。

输出: 本地 OpenAI Responses SSE。

状态机:

1. `message_start`
   - emit `response.created`;
   - emit `response.in_progress`;
   - 使用本地 `ResponseSessionPlan.response_id` 作为 Responses id,不要直接用 Anthropic message id 替代,保证 `previous_response_id` 续接稳定。
2. `content_block_start`
   - `text` -> open Responses message output item,emit `response.output_item.added` 与 `response.content_part.added`;
   - `thinking` / `redacted_thinking` -> open Responses reasoning item,emit reasoning summary part;
   - `tool_use` -> open Responses function_call item,记录 `call_id`、工具名、参数 accumulator;
   - `server_tool_use(name=web_search)` -> Responses `web_search_call`;
   - 非 web `server_tool_use` -> Responses `function_call`,并保留 `caller` / `cache_control` 等 Anthropic extension fields;
   - `bash_code_execution_tool_result` / `code_execution_tool_result` -> Responses `code_interpreter_call`;
   - 未知 block -> Responses `reasoning` trace item,避免静默丢失。
3. `content_block_delta`
   - `text_delta.text` -> `response.output_text.delta`;
   - `thinking_delta.thinking` -> `response.reasoning_summary_text.delta`;
   - `input_json_delta.partial_json` -> `response.function_call_arguments.delta`,并追加到参数 accumulator;
   - `signature_delta` -> session-only Anthropic thinking block signature,供续轮回灌;
   - 未知 delta -> Responses `reasoning` trace item。
4. `content_block_stop`
   - text -> emit `response.output_text.done`、`response.content_part.done`、`response.output_item.done`;
   - thinking -> emit reasoning done 与 output item done;
   - tool_use -> emit `response.function_call_arguments.done` 与 `response.output_item.done`;
   - 已关闭的 tool call 写入 `ToolCallCache`,供下一轮 tool_result 修复。
5. `message_delta`
   - `stop_reason=end_turn` -> completed;
   - `stop_reason=tool_use` -> completed,但 completed output 内必须含 function_call;
   - `stop_reason=max_tokens` -> `response.incomplete`,reason=`max_output_tokens`;
   - `stop_reason=stop_sequence` -> completed,可保留 stop_sequence metadata;
   - usage 映射为 `input_tokens`、`output_tokens`,cache tokens 放入 `input_tokens_details`,Anthropic `server_tool_use` 保留到 Responses `usage.server_tool_use`;
   - `container`、`context_management`、`compaction` 等 provider-specific 字段放入 Responses `metadata.anthropic_*`。
6. `message_stop`
   - emit `response.completed` 或 `response.incomplete`;
   - 将本轮 assistant message 合并进 `ResponseSessionCache`。
7. `error`
   - 如果 stream 已开始,emit `response.failed`;
   - 如果尚未开始,由 adapter 返回结构化 upstream error。
8. `ping` 与未知事件
   - `ping` 忽略;
   - 未知事件 trace-level 记录并继续,符合 Anthropic forward-compatibility 要求。

非流式响应:

- MVP 可以统一强制上游 `stream: true`,因此不单独实现 non-streaming response。
- 如果后续有真实调用方要求 non-streaming,再加 `messages_response_to_responses_sse` helper,复用同一 block finalization 逻辑。

## 7. 配置、UI 与直连规则

### 7.1 normalization

后端与前端保存逻辑必须停止把 Anthropic aliases 改成 `responses`:

- `normalize_provider_api_format("anthropic" | "claude" | "messages") -> "anthropic_messages"`
- `frontend/js/api.js` 保存 provider 时保留 `anthropic_messages`
- `frontend/js/app.js` 展示 canonical `anthropic_messages`
- `frontend/js/i18n.js` 文案改为“Responses <=> Anthropic Messages local conversion”,不能再写 native passthrough。

### 7.2 registry

`AdapterRegistry` 增加:

- field: `anthropic_messages: Arc<dyn Adapter>`
- `lookup("anthropic_messages" | "anthropic" | "claude" | "messages" | "claude_messages") -> anthropic_messages`
- `lookup_for_request("anthropic_messages", "/v1/responses" | "/responses" | "/v1/messages" | "/claude/v1/messages") -> anthropic_messages`

`responses` / `openai_responses` 直连透传规则不改变。`anthropic_messages` 不允许绕过本地 proxy,因为 Codex 仍然说 Responses,上游才是 Messages。

### 7.3 provider test / model list

provider 连接测试需要新分支:

- test URL: `/v1/messages` 或 base URL 已含 `/v1` 时 `/messages`;
- test body: Anthropic Messages body,必须含 `model`、`max_tokens`、`messages`;
- headers: `anthropic-version: 2023-06-01`,以及当前 auth scheme/extra headers。

model list:

- Anthropic 官方没有与 OpenAI `/models` 完全等价的所有场景保障。先沿用当前候选列表的 best-effort,但 `anthropic_messages` 不应复用 `/v1/responses` 派生路径。
- 如果官方或兼容服务没有 model list,UI 应允许用户手填模型,不要把 model-list 失败当作 provider 不可用。

## 8. MVP 边界

MVP 必须支持:

- local Responses -> upstream Anthropic Messages;
- upstream Anthropic Messages SSE -> local Responses SSE;
- text;
- function tools;
- assistant tool_use;
- user tool_result;
- thinking text;
- usage / stop_reason;
- `previous_response_id` session continuation;
- tool call cache round trip;
- provider auth/header/path rules。

MVP 可以暂不支持:

- Anthropic native `/v1/messages` 客户端透传;
- Anthropic server tools 转 Codex function call;
- fine-grained structured output parity;
- non-streaming upstream response;
- code execution / web search / MCP server tools 的 Anthropic 原生 server tool 语义。

不支持项必须可诊断,不能静默丢字段。

## 9. 测试计划

### 9.1 mapper contract

- `mapper/mod.rs::contract_tests` 纳入 `AnthropicMessagesMapper`。
- request contract:
  - upstream path 以 `/` 开头;
  - body 非空;
  - normal path `response_session.is_some()`;
  - `original_responses_request.is_some()`;
  - compact path 行为有独立断言。
- response contract:
  - 成功 path 设置 `content-type: text/event-stream`;
  - 非 2xx path 变成 Responses failure SSE,而不是把 Anthropic error JSON 原样吐给 Codex。

### 9.2 请求映射单测

- text-only Responses input -> Anthropic user text。
- `instructions` -> top-level `system`。
- `max_output_tokens` -> `max_tokens`。
- `tools[].function` -> Anthropic tools。
- named `tool_choice` -> `{ "type": "tool", "name": ... }`。
- `parallel_tool_calls=false` -> `disable_parallel_tool_use=true`。
- assistant function_call history -> assistant `tool_use`。
- function_call_output -> user `tool_result`,且排在 content 最前。
- `previous_response_id` 命中历史后,Messages body 包含上一轮上下文。
- invalid orphan tool_result 返回诊断错误或由 `ToolCallCache` 修复。
- invalid tool name 被 sanitize,并生成 reverse map。
- image URL/data URL 有明确转换结果;无法转换时返回诊断错误或可观测降级。

### 9.3 响应 SSE 单测

- text stream: `message_start` -> text deltas -> `message_stop`,输出 Responses lifecycle。
- thinking stream: `thinking_delta` 输出 reasoning summary events。
- tool stream: `tool_use` + `input_json_delta` 输出 function_call events,并写入 `ToolCallCache`。
- `stop_reason=max_tokens` 输出 `response.incomplete`。
- `ping` 被忽略。
- unknown event 不 panic。
- upstream `error` 输出 `response.failed`。
- stream 中断不能伪装成 `response.completed`。

### 9.4 registry/config/UI 回归

- `lookup("anthropic_messages")` 返回新 adapter。
- `lookup("anthropic" | "claude" | "messages")` 返回新 adapter。
- `lookup_for_request("anthropic_messages", "/v1/responses")` 返回新 adapter。
- `responses` / `openai_responses` passthrough 行为不变。
- backend normalization 把 Anthropic aliases 规范化为 `anthropic_messages`。
- frontend save payload 保留 `anthropic_messages`。
- direct-mode bypass 只允许 `responses` / `openai_responses`,不允许 `anthropic_messages`。
- legacy config healing/import 测试覆盖旧值迁移。

### 9.5 验证命令

实现 PR 的最低门槛:

```bash
cargo fmt --all
cargo test -p codex-app-transfer-adapters
cargo test -p codex-app-transfer-registry
cargo test -p codex-app-transfer
npm run build
```

如果某个命令因本地依赖或平台限制无法运行,需要在 PR 和最终说明中记录失败原因与替代验证。

## 10. 分阶段任务树

### P0 参考基线

- [x] 更新 `docs/litellm` 到 BerriAI/litellm main `431daa1`,版本 `1.85.0`。
- [x] 确认 `docs/litellm` 是 ignored local reference,不会污染 Git status。
- [x] 重新定位 LiteLLM Anthropic Messages / Responses 参考代码。

### P1 架构对齐

- [x] 读取 `ARCHITECTURE_PROTOCOL_GUIDE.md`。
- [x] 读取 `docs/protocol-unification-rfc-phase4.md`。
- [x] 读取当前 mapper/adapter/registry 实现。
- [x] 确认新协议应走 `mapper + thin adapter + registry + contract tests`。

### P2 RFC 与 fixture 准备

- [ ] 新增 Phase 5 RFC 或在 Phase 4 后续章节记录 `anthropic_messages`。
- [ ] 确定 compact path 是否进入 MVP;若进入,需定义 compact -> Messages 的策略。
- [ ] 准备 Anthropic SSE inline fixtures:text、thinking、tool_use、error、unknown event。
- [ ] 先写失败的 request mapper 单测。

### P3 请求 mapper

- [ ] 新增 `crates/adapters/src/anthropic_messages/request.rs`。
- [x] 直接解析 Responses `input` / `tools` / `text.format` / `reasoning` / `context_management`,不经 OpenAI Chat body。
- [x] 仅复用 `ResponseSessionCache`、`ToolCallCache`、artifact output 压缩等协议无关能力。
- [ ] 实现 tool name sanitize/reverse map。
- [ ] 实现 path/header/max_tokens/thinking/tool_choice 映射。
- [ ] 请求侧单测全部通过。

### P4 响应 mapper

- [ ] 新增 `crates/adapters/src/anthropic_messages/response.rs`。
- [ ] 实现 Anthropic SSE parser 与 Responses SSE emitter。
- [ ] 复用 `core::events::emit_sse_event` 与 envelope 构造规则。
- [ ] 写入 `ToolCallCache` 与 `ResponseSessionCache`。
- [ ] 错误、中断、unknown event 单测全部通过。

### P5 adapter 与 registry 接线

- [ ] 新增 `AnthropicMessagesAdapter`。
- [ ] 新增 `mapper::anthropic_messages::AnthropicMessagesMapper`。
- [ ] `lib.rs` / `mapper/mod.rs` / `registry.rs` 接线。
- [ ] registry alias 与 `lookup_for_request` 回归测试通过。
- [ ] mapper contract tests 纳入 Anthropic Messages。

### P6 配置、UI、Preset

- [ ] backend normalization 改为 `anthropic_messages`。
- [ ] provider test URL/body/header 增加 Anthropic Messages 分支。
- [ ] direct-mode bypass 测试锁定 `anthropic_messages` 不直连。
- [ ] frontend save/display/i18n 更新。
- [ ] 转换链路验证后再增加 Claude preset。

### P7 文档与验收

- [ ] 更新 `ARCHITECTURE_PROTOCOL_GUIDE.md` 当前 module tree,补入 `grok_web` 与 `anthropic_messages`。
- [ ] 更新 Phase RFC 的变更清单、测试结果、风险与回滚策略。
- [ ] 更新 README 或 release notes,说明 Claude Messages 适配能力和限制。
- [ ] 运行最低验证命令。
- [ ] 用本地 secret 做 Claude text、tool-call、previous_response_id、upstream error 真实验证,不在日志或回复中暴露 secret。

## 11. 回滚策略

按阶段回滚:

1. 若 mapper 单测失败且未接线,删除 `anthropic_messages` 新模块即可。
2. 若接线后 registry 行为异常,优先回滚 `registry.rs` 与 normalization,保留未启用 mapper 代码继续修。
3. 若真实 provider 验证发现 Claude edge case,保留 canonical `anthropic_messages`,但暂时不迁移 `anthropic` / `claude` / `messages` aliases,只让显式新值启用。
4. 若影响 `responses` / `openai_responses` direct-mode,必须优先回滚 direct-mode 相关修改,因为该路径已有稳定用户。

## 12. 当前建议

先按 P2-P5 完成 adapters 层闭环,不要一开始就加 preset。等 adapter 单测、registry 回归、provider test 分支都稳定后,再做 P6 的 UI 和 preset。这样可以把协议风险限制在 Rust adapter 层,避免半成品配置入口让用户误选。

## 13. 当前 P15 补充结论

2026-05-13 继续按 LiteLLM native Messages passthrough 与 Anthropic 类型定义对照后,确认 `anthropic_messages` 入口已经不再经过 Chat 降级。新发现并补齐的剩余 drop 面集中在直转内部字段保真:

- request side 保留 Responses content array 上的 Anthropic `cache_control`,覆盖 text、image、document、thinking/redacted thinking;document 同时保留 `title`、`context`、`citations`。
- assistant history 直接携带的 `server_tool_use`、`web_search_tool_result`、`web_fetch_tool_result`、`tool_search_tool_result`、`advisor_tool_result`、`compaction` 与其他 `*_tool_result` 现在按 Anthropic 原生 block 保留,不再字符串化。
- response side 的 `previous_response_id` session cache 改为优先保存 Anthropic 原生 assistant content block list,避免下一轮回灌时把 server tool use/result、code execution result 或 thinking 降级为 Responses/Chat 形态。
- 同步保留 message `name`、Anthropic native `tool_choice.disable_parallel_tool_use`,并按 LiteLLM 行为过滤 `x-anthropic-billing-header:` system block。
- 同步 LiteLLM native Messages 的 advisor history 规则:未启用 `advisor_20260301` tool 时移除历史里的 advisor server tool/result block,启用 advisor tool 时原样保留。

本轮未修改 chat、gemini_native 或其他协议转换区块。验证通过:

```bash
rustfmt --edition 2021 crates/adapters/src/anthropic_messages/request.rs crates/adapters/src/anthropic_messages/response.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs
cargo test -p codex-app-transfer-adapters --test anthropic_messages_request --test anthropic_messages_response
git diff --check -- crates/adapters/src/anthropic_messages/request.rs crates/adapters/src/anthropic_messages/response.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs
rg -n "responses_body_to_chat|chat_body_to_anthropic|build_compact_chat|compact_chat|chat_body" crates/adapters/src/anthropic_messages crates/adapters/src/mapper/anthropic_messages.rs crates/adapters/tests/anthropic_messages_request.rs crates/adapters/tests/anthropic_messages_response.rs
```
