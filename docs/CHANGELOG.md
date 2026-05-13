# Changelog

逐版本要点。详细变更见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) 与 `docs/release-notes/v*.md`。

## Unreleased — PR #153 draft

**Anthropic Messages 协议适配**:新增 canonical `apiFormat=anthropic_messages`,将 Codex CLI Responses 请求转换到 Anthropic `/v1/messages`,并把 Anthropic Messages SSE 还原为 Responses SSE。当前 PR 已覆盖 text、thinking、tool_use、tool_result repair、`previous_response_id`、compact response、upstream error、provider test/model list 与 UI 保存显示路径。

**Anthropic Messages 协议保真补齐**:对齐 LiteLLM Anthropic tool 映射规则,补齐 namespace 元数据、custom grammar/schema、`strict`、`cache_control`、`defer_loading`、`allowed_callers`、`input_examples`、Anthropic hosted tools、Responses `computer_use_preview` 与 MCP `mcp_servers` 映射。非 web 的 Anthropic `*_tool_result` 与未知 content block 现在以 Responses reasoning trace 保留,不再静默丢弃。

**Claude 原生能力补齐**:继续以 LiteLLM Anthropic 实现为基准,补齐 `context_management`、`container`、`output_config`、`output_format`、`speed`、`cache_control` 等 Anthropic top-level 字段;Responses `text.format` 现在会映射到 Claude structured output `output_format`,并过滤 Claude output schema 不支持的约束字段;Claude 4.6/4.7 系列会用 `thinking.type=adaptive + output_config.effort` 表达 reasoning effort;`document`、`container_upload`、富 `tool_result` 内容、`is_error` 与 `cache_control` 会继续保留。请求会按实际使用的 computer、MCP、tool search、programmatic tool calling、input examples、file-id documents、code execution、container skills、context management、structured output、effort、fast mode 与 advisor tool 自动追加 Anthropic beta header,proxy 转发层会把这些动态 beta 与 provider 已配置的 `anthropic-beta` 合并为单个 header。

**Anyrouter 专属 preset**:新增 Anyrouter provider 卡片,默认 `baseUrl=https://anyrouter.top`、`apiFormat=anthropic_messages`、`claude-opus-4-7`,并定向启用 Claude Code 兼容形态:完整 `anthropic-beta`、`X-Claude-Code-Session-Id`、`x-app: cli`、Claude Code system 指纹、JSON 字符串形态 `metadata.user_id` 与 `thinking.type=adaptive`。P8 起 Anyrouter 强制所有入站模型别名回到 `models.default`,并将 Codex Responses `web_search` 转为 Anthropic 原生 hosted web search,避免 Claude 自动触发的子模型名或本地 web search fallback 造成偏航。2026-05-13 实测同一 key 下旧方案 `claude-opus-4-7[1m]` 返回 429/503,新方案非流式与流式均返回 200。

通用 Claude preset 暂不开放:需要 P7 真实 Claude text、tool-call、`previous_response_id`、upstream error 验证通过后再加入默认 preset。

## v2.1.6 — 2026-05-12

**关键修复**:MiniMax `role=system` 整请求 400(close #139)/ grok_web 多轮历史完整化(`assistant.tool_calls` flatten + `session_cache` 类型层面禁止 foot-gun)/ cloud_code(Gemini OAuth)多轮历史 silent loss prod bug。

**可观测性**:14+ 稳定 `error_id` token 暴露 sqlite + cache 失败路径,operator 可 grep / 聚合(`SESSIONS_DB_{INIT,SAVE,LOAD,...}_FAILED` / `CORE_INPUT_PREV_ID_{WITHOUT_CACHE,CACHE_MISS}` 等)。

完整 6 主线 + provider 矩阵:[Release v2.1.6](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.6)。

## v2.1.5 — 2026-05-11

Gemini CLI OAuth UI 精修 + 后端硬化收官(三层锁 race-free + i18n 启动闪烁修复 + OAuth 用户邮箱回填 + Provider 卡片图标 / 文案对齐 Gemini 品牌)。[Release v2.1.5](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.5)。

## v2.1.4 — 2026-05-10

**Gemini Native 直转适配器**:Codex.app `/responses` 直接转 Google `:streamGenerateContent?alt=sse`,无 chat 中间形态。新 `apiFormat=gemini_native` + `authScheme=google_api_key`。Web Search / JSON Schema 兼容化 / 多轮 function_call round-trip / 错误流 SSE failure 全部对齐 Codex.app 预期。[Release v2.1.4](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.4)。

## v2.1.3 — 2026-05-09

自定义第三方 + Responses 协议 direct 透传(适合 OpenAI 官方 / 原生 Responses 反代)/ 测速文案分级 / 全局 `tracing → proxy_telemetry.logs` 桥接根治 silent failure / Reasoning prefix provider applicability 收敛。[Release v2.1.3](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.3)。

## v2.1.2 — 2026-05-09

chat 端原生 web_search 工具支持(MiMo / Kimi / DeepSeek / MiniMax 各家文档实证 + 跨 provider URL citation 通用入站)/ MiniMax builtin preset 卡片。[Release v2.1.2](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.2)。

## v2.1.1 — 2026-05-09

MCP 工具调用 + namespace(`type:"namespace"` 包递归展平 + function_call SSE `namespace` 字段补齐根治 Codex.app `unsupported call`)/ Auto-compact summary 9-section 强 schema 大幅增强。[Release v2.1.1](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.1)。

## v2.1.0 — 2026-05-09

新增 macOS Intel x64 二进制(close #61)/ 会话历史持久化(L1 内存 LRU + L2 sqlite 30 天 TTL,Tauri 重启不丢历史)/ ws warmup 不打上游 + 立即 Close frame 防 Codex CLI 4 分 48 秒 idle timeout / 多模态 / vision 兼容(MiMo 纯图兜底 + DeepSeek 视觉剥离 + 白名单按模型级精确匹配)。[Release v2.1.0](https://github.com/Cmochance/codex-app-transfer/releases/tag/v2.1.0)。

## v2.0.x

Python → Rust/Tauri 全栈重写,重写过程 7 阶段 + 30+ 修订日志见 [`docs/refactor/migration.md`](refactor/migration.md),核心结论 + 量化对比见 [`docs/release-notes/v2.0.0.md`](release-notes/v2.0.0.md)。

逐版本 release notes:[v2.0.0](release-notes/v2.0.0.md) / [v2.0.2](release-notes/v2.0.2.md) / [v2.0.3](release-notes/v2.0.3.md) / [v2.0.4](release-notes/v2.0.4.md) / [v2.0.5](release-notes/v2.0.5.md) / [v2.0.6](release-notes/v2.0.6.md) / [v2.0.7](release-notes/v2.0.7.md) / [v2.0.8](release-notes/v2.0.8.md)(无 v2.0.1 release notes — 跟随 v2.0.0 工程修订发布)。

## v1.0.x(Python,已归档)

Python + cryptography 验签时代,已被 v2.x Rust 主线全面取代,新装请直接用 v2.x。逐版本 release notes:[v1.0.0](release-notes/v1.0.0.md) / [v1.0.1](release-notes/v1.0.1.md) / [v1.0.2](release-notes/v1.0.2.md) / [v1.0.3](release-notes/v1.0.3.md)(v1.0.4 工程版本无独立 release notes,详见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases))。

---

> Followup backlog(跨 session 长期持有的研究 / refactor / 观测 tickets)见 [`docs/followup-tracker.md`](followup-tracker.md) + `docs/followup/<id>-<slug>.md` 详情。
