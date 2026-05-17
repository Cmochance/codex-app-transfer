---
id: 26
priority: P2
type: refactor
status: active
created: 2026-05-17
related_pr: null
---

# Plugins / MCP namespace 工具只在 chat 转发路径生效 — UI / README 需提示

## 触发上下文

2026-05-17 调研用户报"百炼 Token Plan apikey 连接失败"(已由 PR #188 修正 preset `apiFormat: responses → openai_chat`)时,发现一个并不止于 preset 配错的设计性 gap:**Plugins / MCP 功能跟"协议转换路径"强绑定,Responses 直连(passthrough)provider 启用 Plugins 后工具会被上游静默丢失**,且 UI / README 无任何提示。

用户原话(2026-05-17):
> "Plugins 功能的开启似乎是和协议转发绑定的,responses 直连的提供商是不是不会打开 Plugins 功能?"

这条 followup 不是为了拆 Plugins 解绑,而是把"哪些 provider 能用 Plugins"显式化,避免下一个用户复现踩坑。

## 问题描述

### 现状代码

MCP 工具用 Codex CLI 内部 `type:"namespace"` 包装传给本应用,需要 adapter **展平成普通 chat tool 数组**才能让第三方 chat completions provider 看懂:

- 展平逻辑在 `crates/adapters/src/responses/request.rs:2288-2323`(`convert_responses_tool_to_chat_tool` 的 `"namespace"` 分支)
- 该函数只在 `ResponsesAdapter::prepare_request` 里被调用
- 路由表(`crates/adapters/src/registry.rs:94-117`)按 `apiFormat` × `client_path` 分发:
  - `apiFormat="openai_chat"` + 入站 `/responses` → `ResponsesAdapter`(转换)→ 展平触发 ✓
  - `apiFormat="responses" | "openai_responses"` + 入站 `/responses` → `ResponsesPassthroughAdapter`(字节透传)→ body 原样发上游 ✗
- passthrough 实现:`crates/adapters/src/passthrough.rs:50-68`(`prepare_request` 直接把 body 透传,有测试 `body_is_byte_level_passthrough` at line 138-146 兜底确保不改字段)

### 上游能力差异

- 真正原生实现 OpenAI Responses API 的上游(OpenAI 官方、Azure OpenAI Responses GA endpoint)— 上游本身懂 `type:"namespace"` 包装 → Plugins 直连工作正常
- 国内绝大多数"Responses 兼容"反代实际背后是 Chat Completions 实现 → 直连透传 `namespace` 必然解析失败 → 工具静默 drop,前端看不到任何错误

### 期望

至少 3 处需要提示用户:

1. **Plugins 设置开关旁边**(`frontend/index.html:417` 已有 `autoUnlockCodexPluginsHint` settings-note)— 追加一句"仅在所选 provider 走协议转发(chat completions / anthropic messages 等)时实际生效;Responses 直连 provider 的 MCP 工具会被上游忽略"
2. **provider 表单**:当用户选 `apiFormat="responses"` 时,如果检测到 settings.autoUnlockCodexPlugins=true,在表单底部 inline warning"该 provider 走 Responses 直连,Plugins / MCP 不会通过本代理生效"
3. **README 稳定性矩阵**(`README.md:88-95`,英文同):新增一列"Plugins/MCP via proxy"或在备注里点明 chat-route only

## 已有调研

- 协议路由与 adapter 选择:`crates/adapters/src/registry.rs:25, 37, 89-117`(`responses_passthrough` field + `lookup_for_request` doc)
- passthrough 行为及其测试:`crates/adapters/src/passthrough.rs:50-68, 138-146`
- namespace 展平实现及其注释:`crates/adapters/src/responses/request.rs:2288-2323`(注释里就说"第三方 chat completions provider 不认 namespace type / 必须递归展平内层 functions 为顶级 tool 数组")
- Plugins 设置 UI 当前文案:`frontend/index.html:417`
- Plugins 设置默认值与 daemon 启动:`src-tauri/src/main.rs:63-85`,`src-tauri/src/admin/handlers/settings.rs:61` (`autoWakeCodexPet: true` 同段亦含 plugin 默认)

## 风险 / 不确定性

- **是否应该在 passthrough 路径也做 namespace 展平?** 看似是个一致性 fix,但 OpenAI 官方 Responses API 原生收 namespace,在它的路径上展平反而是错。需要先按 baseUrl host 或 provider 的某个 metadata 区分"原生 Responses"vs"自称兼容实际是 chat",代价高。建议先走"显式提示"这条低成本路径。
- **UI 提示的精确触发点**:`apiFormat in ["responses", "openai_responses"]` 是路由侧的真值,但用户表单里可能填别的字符串(grok / `Openai-Responses` 等大小写变体)— 触发逻辑应该跟 `lookup_for_request` 的 normalize 规则一致,直接复用 registry 那边的 helper,不要在前端自己写一份。
- **README 矩阵列宽**:已经很满,加一列要重排,可考虑放进现有"备注"列。

## 建议方向

下次接手 1 步先做:

1. 在 `frontend/index.html:417` `autoUnlockCodexPluginsHint` 补一句中英文(同步 `frontend/js/i18n.js` 如有),不引入新逻辑 — 最低成本止血。
2. 第 2 步:provider 表单 inline warning。需要决定 trigger 在前端还是后端 API(`/api/providers/*`)→ 建议后端在 `POST /api/providers` validate 里 return 一个 `warnings` 字段,前端通用渲染,这样 tray 切换 / provider 列表也能复用。
3. 第 3 步:README 稳定性矩阵。如果决定加列就连英文 README 一起改。

## 关联资源

- 触发 PR:#188 (bailian-token-plan preset 修复)
- 关联 issue:#187
- 关联 follow-up:#27(打开 Plugins 后 Codex Desktop 二次 splash 诊断)
- 上游参考:`crates/adapters/src/responses/request.rs:2288-2323` 注释里引用了 namespace 工具包装的 Codex CLI 上游约定
