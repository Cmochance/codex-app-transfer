# Messages <=> Responses 任务推进树

> 当前任务: 为 Claude 系列模型新增 `anthropic_messages` 协议适配。
> 方案文档: `docs/plans/2026-05-13-messages-responses-protocol.md`
> 当前状态: P5 Adapter 与 Registry 已完成;`anthropic_messages` 已可通过 provider `apiFormat` 路由,但尚未接入配置/UI/preset。

## 已确认事实

- `docs/litellm` 是 `.gitignore` 中声明的本地参考目录,不属于当前仓库跟踪文件。
- 本地 LiteLLM 已同步到 BerriAI/litellm main `431daa1479f0af506696d1dff236d95566abdddc`,版本 `1.85.0`。
- 根目录架构要求新增协议走 `core + mapper + thin adapters`,adapter 层不能承载复杂 provider-specific 分支。
- 当前 `anthropic` / `claude` / `messages` 仍归一到 `responses`,并不是真正的 Anthropic Messages 协议转换。
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

- [ ] backend normalization 输出 `anthropic_messages`。
- [ ] provider test/model-list 分支适配 Anthropic Messages。
- [ ] direct-mode bypass 继续只允许 `responses` / `openai_responses`。
- [ ] frontend 保存、展示、i18n 文案更新。
- [ ] 验证通过后再添加 Claude preset。

### P7 文档与验收

- [ ] 更新 `ARCHITECTURE_PROTOCOL_GUIDE.md` 与 RFC 变更清单。
- [ ] 更新 README 或 release notes。
- [ ] 运行 `cargo fmt --all`。
- [ ] 运行 `cargo test -p codex-app-transfer-adapters`。
- [ ] 运行 `cargo test -p codex-app-transfer-registry`。
- [ ] 运行 `cargo test -p codex-app-transfer`。
- [ ] 运行 `npm run build`。
- [ ] 使用本地 secret 做 Claude text、tool-call、previous_response_id、upstream error 真实验证。

## 当前下一步

进入 P6 配置与 UI:让 backend/provider normalization 能保存并使用 `anthropic_messages`,并补齐 provider test/model-list 分支。仍然不要添加 Claude preset,直到 P7 真实 Claude 验证完成。

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
