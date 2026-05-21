# Changelog

逐版本要点。详细变更见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) 与 `docs/release-notes/v*.md`。

## Unreleased — PR #153 draft + post-#236 apply_patch chat-path prompt quality

**Anthropic Messages 协议适配**:新增 canonical `apiFormat=anthropic_messages`,将 Codex CLI Responses 请求转换到 Anthropic `/v1/messages`,并把 Anthropic Messages SSE 还原为 Responses SSE。当前 PR 已覆盖 text、thinking、tool_use、tool_result repair、`previous_response_id`、compact response、upstream error、provider test/model list 与 UI 保存显示路径。

Claude preset 暂不开放:需要 P7 真实 Claude text、tool-call、`previous_response_id`、upstream error 验证通过后再加入默认 preset。

**apply_patch chat-path prompt-quality 增强**(post-PR #236 follow-up,issue #235):PR #236 修复了 wire 层(`custom_tool_call` SSE 桥接 + 多轮历史回放 + 首版 system 注入 + description 增强),但真机 capture(28-turn / 26MB / DeepSeek + Kimi)显示 7 个 apply_patch tool_call 中 6 个因模型生成的 V4A patch 内容质量被 Codex Desktop 端验证器拒绝(直接吐 Python 代码 / `-` 行不 byte-exact / 选 `exec_command` 而非 `apply_patch`)。本次改动针对模型生成质量,wire 层不再变化。改动点:

- `APPLY_PATCH_CHAT_PATH_SYSTEM_GUIDANCE`(重写 system message 内容):结构 = Tool selection 引导(对抗 exec_command 偏好)+ 上游 V4A 完整教学(verbatim, openai/codex @ `0b4f86095c8005d8f74e9c62b971d72c1670aa88`, Apache-2.0,经 `include_str!` 嵌入)+ 5 条 chat-path 实测 gotcha(byte-exact / Empty-line anchor 仅当 blank 存在 / Add+Update 同 patch / 空文件 / 纯 `+` 行不替换)。
- `APPLY_PATCH_TOOL_DESCRIPTION_FOR_CHAT`(扩展 custom tool description):加入 Tool selection 顶层引导、BYTE-EXACT 匹配规则、3 个 positive example(modify line / Add File / 多 hunk Update)、anti-pattern reminder("NEVER pass raw source code")。
- `APPLY_PATCH_INPUT_DESCRIPTION_FOR_CHAT`(参数级 description mirror):部分 provider 在长历史中降权 tool-level description,参数级补 byte-exact + anti-pattern 紧凑版保持可见性。
- 新增 `crates/adapters/src/responses/apply_patch_v4a_reference.md`:verbatim 镜像上游 L277-L351,头部加 adapter note 显式 override "shell command" 字眼为 "function-call tool"。NOTICE 文件 + ACKNOWLEDGEMENTS.md + README 中英致谢段同步加 attribution。

成功率改善幅度待本 PR push 后真机 regression 测试出 round2 数据,届时在 PR description / 后续 release notes 补具体数字。

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
