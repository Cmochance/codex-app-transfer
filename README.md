# Codex App Transfer

[![GitHub stars](https://img.shields.io/github/stars/Cmochance/codex-app-transfer?style=social)](https://github.com/Cmochance/codex-app-transfer/stargazers)
[![License](https://img.shields.io/github/license/Cmochance/codex-app-transfer)](LICENSE.txt)
[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-24C8DB?logo=tauri)](https://v2.tauri.app/)
[![Downloads](https://img.shields.io/github/downloads/Cmochance/codex-app-transfer/total?label=downloads)](https://github.com/Cmochance/codex-app-transfer/releases)

Codex App Transfer 是一个面向 **OpenAI Codex CLI** 的轻量配置和转发工具。它在本机起一个网关，把 Codex CLI 发出的 Responses API（HTTP 流式 / 非流式请求，含 `/responses` HTTP 回退）翻译成 Chat Completions 格式，再转发到你选择的供应商，比如 Kimi Code、Kimi 月之暗面、DeepSeek V4、Xiaomi MiMo、智谱 GLM、阿里云百炼等。

和 `farion1231/cc-switch` 这类偏 Claude Code / CLI 的 Anthropic 工具不同，本项目专注 OpenAI Codex CLI 的接入：用桌面界面管理供应商、模型映射、转发端口和日志，让 Codex CLI 可以无缝使用第三方 OpenAI 兼容的推理服务。

Windows 安装版和便携版默认会打开独立桌面窗口；浏览器地址只作为调试和备用入口。点击窗口关闭按钮时，应用会缩小到系统托盘继续运行；需要完全退出时，请右键托盘图标选择"退出"。

启动转发后，Codex CLI 通过本机 `127.0.0.1:18080` 与本工具通信。本工具在转换协议、改写模型名、保留 `previous_response_id` 历史的同时，把上游真实错误体写到日志面板，方便排查兼容问题。

## 项目状态

- 当前版本:**v2.1.4**(Python → Rust/Tauri 全栈重写后的当前主线)
- 已验证供应商:Kimi Code(`kimi-for-coding` UA 网关)、Kimi 月之暗面(Moonshot Platform)、DeepSeek V4(官方 baseUrl,含「Max 思维」思考模式)、Xiaomi MiMo (Token Plan / Pay for Token)、MiniMax M2.x(OpenAI-compatible chat,自动剥不兼容字段 + `reasoning_split` + `<think>` 兜底)、**Google AI Studio / Gemini API**(v2.1.4 新增 `gemini_native` 适配器,直转 `:streamGenerateContent` 无中间 chat)
- 实验兼容:智谱 GLM / 阿里云百炼 / 其它 OpenAI Chat 兼容反代
- 平台:v2.0.0 首发只发 macOS arm64;v2.0.1 起发布链路生成 macOS arm64 / Windows x64 / Linux x86_64 资产;**v2.1.0 起新增 macOS Intel x64**(见 [issue #61](https://github.com/Cmochance/codex-app-transfer/issues/61))。v2.0.0 ~ v2.0.5 已归档为中间版本(仅源代码,不提供二进制,详见各 Release 页),生产请用 v2.1.4。早期 Python 版本 v1.0.x 已不再作为推荐稳定版,新用户直接用 v2.x
- 数据兼容:`~/.codex-app-transfer/config.json` 与 v1.x 互通,升级 / 回退不丢配置

> 如果使用过程中出现问题,欢迎提交 PR 协助作者完善,会及时处理,非常感谢。

### v2.x 主线改动(累计到 v2.1.4)

按主题分组。

**Gemini CLI OAuth 直连(impersonate gemini-cli,无需 API key)**(下版主线)
- 新增 `apiFormat=gemini_cli_oauth` + `authScheme=google_oauth_cloud_code`。用户**通过浏览器登录 Google 账号一次**,本地持久化 access/refresh token(`~/.codex-app-transfer/gemini-oauth.json`,Unix 0600 + atomic write),后续请求自动 refresh 复用,**不需要手动配 API key**。免费 tier per-account 配额绑 GCP `cloudaicompanionProject`(自动 provision)
- 新独立 crate `crates/gemini_oauth/` 实现完整 OAuth 流程(impersonate `google-gemini/gemini-cli` web flow):loopback callback server (动态 port) + 32-byte hex CSRF state + `oauth2.googleapis.com/token` code-grant exchange + 60s buffer auto refresh + `cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`+`onboardUser` LRO project bootstrap
- 新 `crates/adapters/src/gemini_cli/` adapter:**复用 `gemini_native` 90% 内部转换**(JSON Schema sanitize / Web Search 软约束 / 多轮 function call cache / failure SSE / etc.),只加 outer envelope 层(`{model, project, user_prompt_id: uuid_v4, request: <inner>}`)+ SSE event 外包 `{response: {...}}` unwrap。proxy/forward.rs 加 `GoogleOauthCloudCode` authScheme 分支:dispatch 时 await `ensure_valid_access_token` 拿 Bearer 注入,token 过期前 60s 自动 refresh + 持久化
- ⚠️ **TOS 灰色 + 风险免责**:`cloudcode-pa.googleapis.com/v1internal` 是 Google **内部未文档化**端点(CLIProxyAPI #1584/#1674 多次因 gemini-cli 升级而 break),Google 可能随时改协议;Code Assist TOS 禁止"非官方客户端 automated/programmatic 使用",impersonation 严格说违反,**用户自担风险**;免费 tier per-account rate limit 严格,heavy use 会 429。本功能**不保证长期可用**,降级路径:切回 v2.1.4 起的 `gemini_native`(API key 模式)
- Wire-level 实现参考 [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI)(MIT, Go, 31.8k⭐)— 致谢段已加注明
- 76 单测覆盖 OAuth(token 持久化 + flow + bootstrap + service refresh)+ adapter(outer envelope + SSE unwrap + chunk-buffered 边界 + keepalive 透传)+ proxy 路由(GoogleOauthCloudCode authScheme dispatch)

**Gemini Native 直转适配器**(v2.1.4,本版主线)
- 新增 `apiFormat=gemini_native` + `authScheme=google_api_key`(`x-goog-api-key` header)。Codex.app `/responses` 入站直接转 Google `:streamGenerateContent?alt=sse` 出站,**完全不走 chat 中间形态**(`crates/adapters/src/gemini_native/`)。Gemini 3 走 `/v1alpha`,Gemini 2.x 走 `/v1beta`,baseUrl 不带版本前缀适配器自动选
- **Web Search**:Gemini 3+ `googleSearch` builtin tool + functionDeclarations 共存(`toolConfig.includeServerSideToolInvocations`);Gemini 2.x 上游不允许共存 → 用 systemInstruction 软约束告知模型 `google_search` 暂不可用(LiteLLM `apply_response_schema_transformation` 风格,不 destructive drop 字段)
- **JSON Schema 兼容化**:`type:["string","null"]` 数组形态 → `string` + `nullable:true`;`anyOf` 含 null → 折叠 nullable;`$ref` / `$defs` 内联展开;`additionalProperties` / `strict` 剥离;空 enum 字符串 → null;object 缺 `properties` 补空对象 — 适配 Gemini 严格 schema 校验,**全部转换非破坏性**
- **多轮 function calling round-trip**:接全局 `ToolCallCache`(`crates/adapters/src/responses/tool_call_cache.rs`)在 `function_call_output` 缺失前置 `function_call` 时按 `call_id` 查 cache **synthesize 假 assistant turn**,根治 Gemini "function response without prior call" 400;`tool_choice` 按值 normalize(`auto/none` drop / `required/specific` BadRequest 不静默吞);`contents` 必须 user role 开头(Gemini strict user/model alternation),首项是 model 时插占位 user turn
- **错误流 → Responses SSE failure**(2026-05-10 修):上游 4xx/5xx 不再透传 raw Gemini JSON 让 Codex.app 卡 Thinking,改成构造合规 SSE failure 流(`response.created` → `response.failed`),含结构化 error code:`auth_error` / `permission_denied` / `quota_exceeded` / `rate_limited` / `timeout` / `service_unavailable` / `content_filter` / `bad_request` / `server_error` / `upstream_transport_error` / `upstream_error`;ByteStream 中途 transport err / 64KB 之外 body / 非 UTF-8 / 2000 字符过长 message 全标 marker 后缀,不静默吞
- 单测 60+ 覆盖 wire round-trip / schema sanitize / tool_choice modes / multi-turn / failure stream 完整 lifecycle / 错误分类 / array-form error body / oversized body / non-UTF-8 / transport err 等

**自定义第三方 + Responses 协议 direct 透传**(v2.1.3)
- 用户在「自定义第三方」preset 显式选 `apiFormat=responses` + 填齐 baseUrl + apiKey → **Codex.app 直连上游不经代理**(借鉴 codex-account-switch 纯配置写入):`~/.codex/config.toml::openai_base_url` 写用户 baseUrl + `auth.json` 写用户 apiKey + 本地 18080 端口不启;适用 OpenAI 官方 / 任何原生实现 OpenAI Responses API 的反代
- **healing 教训仍生效**(2026-05-08 MiMo Token Plan 404):8 条 builtin preset 的 apiFormat 启动时强制覆盖回 `openai_chat`,bypass 只可能命中显式自定义 + 用户主动选透传的第三方 — informed choice
- 配套 `ResponsesPassthroughAdapter`(`crates/adapters/src/passthrough.rs`):字节级透传 OpenAI Responses API,SSE envelope / `sequence_number` / `chatcmpl→resp_` ID 全由上游产生代理不重写;`/responses/compact` 私有扩展即便 apiFormat=responses 也强制留本地 ResponsesAdapter 包装(避免上游 404)
- 「**自定义第三方**」preset 卡片(`bi-puzzle` icon)无限重复添加 + 协议类型 UI 改 readonly + Responses 协议解锁 default 模型映射可空 + baseUrl input 加 `autocapitalize="off"` + form-submit direct 模式必填 baseUrl/apiKey 校验
- ⚠️ **使用注意 — 长对话切第三方 Responses provider 可能报 `thinking_signature_invalid`**:Codex CLI 在长对话超出上下文阈值时会自动 compact 历史,塞一个 `type=compaction` item(Codex CLI 私有协议字段,**OpenAI Responses 标准协议没有**,`encrypted_content` 字段名是历史包袱实际填明文 summary)。第三方 Responses 上游(OpenAI 官方 / Anthropic 系网关 / 自建 wrapper 等)不识别这个 item,会试图把 `encrypted_content` 当 thinking signature 去 verify → 报 `thinking_signature_invalid`。**workaround**:切到第三方 Responses provider 时**新建对话**避免历史累积进 compact item;或者使用走代理转换的 builtin provider(`apiFormat=openai_chat`),代理本地 ResponsesAdapter 已正确把 compaction 展开成 user message 注入下游

**测速文案分级 + 上游错误诊断闭环**(v2.1.3)
- 测速 401/403 仍 `reachable=true`(连接 OK)+ 新 `authStatus="auth_required_or_invalid"` + **绿色显示** + 文案 `connection OK; API key not configured or auth not verified`,解耦"连接性"和"鉴权"语义(避免黄色警告让用户误以为 baseUrl 错);前端 `isProviderTestResultBad()` helper 不依据 authStatus 标黄,真鉴权失败由 Codex CLI 实际请求路径暴露
- `forward.rs` 两处 `resp.bytes().await.unwrap_or_default()` 改 `match` + telemetry.logs.add WARN,避免上游 body read 失败时静默吞错丢失 root cause
- `provider_test_error_label` 拆 7 类(Timeout / ConnectError / RedirectError / DecodeError / RequestError / BodyError / OtherError),用户 toast self-debug
- `desktop.rs::one_million_catalog_ready` catalog 读取/解析失败改用 `proxy_telemetry().logs.add` 写日志面板可见

**全局 tracing → proxy_telemetry.logs 桥接**(v2.1.3 silent failure 根治)
- workspace 5 处历史 `tracing::warn!`(healing apiFormat 强制覆盖 / `warn_once_drop_tool` / `disable_web_search_for` 等)在 Tauri 桌面 binary 之前**完全静默**(没注册 tracing_subscriber)
- 新加 `TelemetryLogsLayer`(`src-tauri/src/telemetry_bridge.rs`)实现 tracing_subscriber Layer trait,`main()` 第一行 init 全局 subscriber 把所有 `tracing::*` event 转发到 `proxy_telemetry().logs` 同通道(`~/.codex-app-transfer/logs/proxy-*.log` + 设置面板 logs viewer 双可见)
- `LevelFilter::INFO` 兜底防未来 dep 引入 TRACE 噪音 + sensitive field redact(`api_key` / `authorization` / `bearer` / `token` / `secret` / `password` 含子串 → `[REDACTED]`)+ `try_init` 失败时 telemetry ERROR 兜底 + 成功 emit INFO `tracing-bridge active` 正向确认
- 未来任何 crate 加 `tracing::*` 调用自动生效,不需要每处手动决策"用 tracing 还是 telemetry.logs"

**MCP 工具调用 + namespace**(v2.1.1)
- `type:"namespace"` 包递归 `flat_map` 展平为顶级 function;chat→responses SSE envelope 回灌 16+ Responses API 字段(`tools` / `tool_choice` / `reasoning` / `text` / `metadata` / `previous_response_id` / `temperature` 等)+ `created_at` + 每 event 单调递增 `sequence_number`
- **function_call SSE output 加 `namespace` 字段**:strings 实证 Codex.app desktop binary 含 `dynamic tool namespace must not be empty for` 校验,缺字段则 namespace 工具调用全返 `unsupported call: <name>`
- unknown tool type `warn_once_drop_tool` 防多轮重发刷屏
- 借鉴 mimo2codex `streamToSse.ts` + Codex.app 二进制反求工程。修复后 Notion / Figma / Zeabur 等 MCP server 完整工具集模型可见可调

**chat 端原生 web_search 工具支持**(v2.1.2,逐家文档实证)
- **Xiaomi MiMo**:私有 `type:"web_search"` + 4 字段透传(`user_location` / `max_keyword` / `force_search` / `limit`)+ A 配置开关 `web_search_enabled` 默认关闭 + B 4xx 上游拒绝时 transparent retry 自动降级 + 进程内 disable cache 防重复失败
- **Kimi (Moonshot)**:内置 `$web_search` builtin_function + 自动注入 `thinking:{type:"disabled"}` 顶级字段(Kimi 文档强制)
- **DeepSeek / MiniMax**:文档实证 chat 端不支持 → 显式 drop + 专属 warn key 帮用户调试
- **跨 provider URL citation 通用入站**:任何 chat 上游 URL 引用通过 `response.output_text.annotation.added` event 透传 Codex.app UI 显示来源链接(P5 之前完全丢失)
- **MiniMax builtin preset 卡片**:粉红渐变音波 logo 真品牌 favicon,直接点选不用手动配 base_url

**配置自愈 / 协议路由 / 客户端身份**
- **Codex CLI 客户端身份头零泄漏**:出站剔除 `originator` / `x-codex-*` / `x-openai-*` / `chatgpt-account-id` / `session_id` / `thread_id` / `user-agent`,reqwest 默认 UA 改中性 `Codex-App-Transfer/<v>`,根治 Kimi For Coding Windows 403 access_terminated_error
- **配置自愈**:load 时按 `baseUrl normalize` 命中 builtin preset(不依赖 `isBuiltin` 字段或随机 hex id),命中即强制覆盖 `apiFormat / authScheme / extraHeaders` + `isBuiltin=true` 写回磁盘,适配真机 v1.x 升级遗留老配置
- **MiMo Token Plan 404 修复**:`apiFormat == "responses"` 不再误读为"上游原生 Responses 透传",所有 provider 走 local_proxy,Responses↔Chat 协议转换在本地完成
- **`apiFormat` fallback 全栈统一为 `openai_chat`**:后端 add+update+import + 前端 mapProvider/providerBody/getPresets/选预设/编辑共 8 处对齐 schema serde default

**Responses 会话 / ws 链路**
- **会话历史持久化**:`ResponseSessionCache` 双层(L1 内存 1000×60min LRU + L2 sqlite 30 天 TTL,落盘 `~/.codex-app-transfer/sessions.db`),Tauri 重启 / TTL 续期不丢历史
- **ws warmup 不打上游 + 立即 Close frame**:`server.rs` ws handler 识别 `generate:false` / 空 input 帧后直接发 ws **Close frame**,Codex CLI 立即转 HTTP `stream_responses_api`(旧版用 `{"type":"error"}` 文本帧实测卡 4 分 48 秒 ws idle timeout 才 fallback)
- **cache miss 错误体对齐 OpenAI SDK**:`previous_response_id` 失效返 HTTP 400 + `code:"previous_response_not_found"` 字面对齐,SDK / Codex CLI 错误处理路径直接复用

**Auto-compact summary 大幅增强**(v2.1.0)
- `/responses/compact` 本地 SUMMARIZATION_PROMPT 从 Codex 86 字符版改为 Claude Code 风格 9-section 强 schema(`<analysis>` + `<summary>` 二段输出 + `Primary Request` / `Files and Code Sections` / `All User Messages 逐字列` / `Errors and Fixes` / `Current Work` / `Next Step verbatim quote` 等固定 section + few-shot example)
- 注入位置从 system 改为最后一条 user message,提升第三方 provider 服从度
- 响应解析只抽 `<summary>` 段进续轮 history,过滤 `<analysis>` chain-of-thought 防污染
- **根治旧 prompt"compact 后只记最近 1-2 个动作"丢任务目标 / 文件路径 / 历次 user 主诉的体感断片**

**多模态 / vision**
- **MiMo 纯图请求兜底**:MiMo 文档要求 content 含 `image_url` 时至少 1 个 text part(否则 400 `Param Incorrect: text is not set`),Codex CLI 粘图未输入文字场景统一兜底插入空格 text part
- DeepSeek 视觉输入剥离避免 400
- vision 白名单按模型级精确匹配(替换 provider 子串匹配)

**Reasoning / thinking 链路**
- Kimi/DeepSeek thinking 在 Codex CLI TUI 显示头修复 + `extraHeaders` 支持 `{apiKey}` 模板(解决 Kimi 403)
- reasoning_content 历史回放 + 全局 `TOOL_CALLS_CACHE` 在历史压缩后重建缺失的 `assistant.tool_calls`
- `response.in_progress` 事件按协议补齐
- chat usage → responses usage 字段规范化(含 `cached_tokens` 默认补零,避免 Kimi/MiMo 重连卡顿)

**MiniMax 兼容**(由 [@lukegood](https://github.com/lukegood) 贡献)
- 请求侧白名单清洗 `reasoning_effort` / `response_format` / `parallel_tool_calls` 等不支持字段;`tool` 剥 `strict`;连续 `system` 合并
- 响应侧 `reasoning_details` 拆 reasoning + `<think>` 兜底拆分仅对 MiniMax 启用

**UI / 桌面**
- Windows 重启 Codex 不再 flash 终端窗口 + 托盘菜单交互修复
- 启用按钮即时反馈,不再等转发回包
- **user-facing 字符串改回英文**:API 响应 message / proxy telemetry log / tray menu / SDK 错误体全部英文,对齐 OpenAI/Anthropic SDK 错误处理风格(注释 / test 仍中文)

### 更新日志

逐版本变更详见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) 或 `docs/release-notes/v*.md`。当前管理 API 与代理入口状态见 [`docs/api-route-status.md`](docs/api-route-status.md)。

## 能做什么

- 管理多套供应商，按 OpenAI 模型名（gpt-5.5 / gpt-5.4 / gpt-5.4-mini / gpt-5.3-codex / gpt-5.2）映射到供应商真实模型 ID。
- 把 Codex CLI 的 Responses API 流式 / 非流式请求转换为 Chat Completions 格式后转发，多轮工具对话上下文 + 思维内容流式展开均已对齐 OpenAI Responses API 协议。
- 兼容 Codex CLI 0.126+ 的 `responses_http` 路径和 `/responses` 路由别名；当前 Rust 主线对外承诺的是 HTTP/SSE 转发入口。
- 自动把上游 `chatcmpl-...` 应答 ID 重写成 Codex CLI 校验通过的 `resp_...`，并保留 deployment affinity 编码；session_cache 查询前自动 decode 回原始 ID。
- thinking 开启的上游（Kimi / DeepSeek 等）三层防御：单空格占位 `reasoning_content`、`reasoning_summary_part` 标准协议事件、全局 `TOOL_CALLS_CACHE` 在历史压缩后重建缺失的 assistant.tool_calls。
- 自动归一化 `reasoning_effort`（`xhigh` / `max` → `high`，`auto` / `none` 直接丢弃），适配只接受 `minimal/low/medium/high` 的供应商。
- Codex CLI 原配置守护：apply 前自动快照 `~/.codex/{config.toml,auth.json}`，退出 / 下次启动按 key 智能合并还原；切到不需转发的 provider 自动停转发服务。
- 实时日志面板：每 2 秒自动刷新；提供"查看日志"按钮直接打开 `~/.codex-app-transfer/logs/`；"清除日志"按钮会把当前日志备份到 `logs/backup/` 后开启新日志，不直接删除文件。
- 反馈弹窗支持可选联系邮箱；默认自动附带诊断材料（环境信息、脱敏配置、最近错误快照及完整请求/响应），减少手工补材料往返。
- 中文 / 英文界面，浅色 / 深色 / 绿色 / 橙色 / 灰色 / 白色多种主题（仅深色会改变背景色）。
- Windows / macOS / Linux 系统托盘 + 跨平台单实例锁定(双击启动会自动唤起已有窗口)。

## 界面预览

| 仪表盘 | 供应商 |
|---|---|
| ![Board](docs/img/Board.png) | ![Providers](docs/img/Providers.png) |
| **设置** | **日志** |
| ![Settings](docs/img/Settings.png) | ![Logs](docs/img/Logs.png) |

### Codex CLI 实际接入

启用任意供应商后，Codex CLI 模型选择器会显示「<provider> / <real-model>」形式的真实模型名（v2.0.5 起所有供应商均如此），对话过程中工具循环 / `previous_response_id` 历史回放 / thinking 模式 reasoning_content 注入全部由本地代理透明处理：

![Codex CLI 实际对话](docs/img/codex-cli-real-chat.png)

## 下载

最新已发布版本在 GitHub Release：

```text
https://github.com/Cmochance/codex-app-transfer/releases/latest
```

推荐普通用户下载（v2.0.x 起 Tauri bundler 直出，不再有 PyInstaller 时代的 `.tar.gz` / `Portable.zip` / `.pkg` / 无后缀 Linux 单文件）：

- `Codex-App-Transfer-v<版本>-Windows-x64-Setup.exe`：Windows NSIS 安装版（推荐）
- `Codex-App-Transfer-v<版本>-Windows-x64.msi`：Windows MSI（企业 MDM / GPO 部署）
- `Codex-App-Transfer-v<版本>-macOS-arm64.dmg`：macOS Apple Silicon 拖拽安装
- `Codex-App-Transfer-v<版本>-macOS-x64.dmg`：macOS Intel x64 拖拽安装(v2.1.0+ 提供,见 [issue #61](https://github.com/Cmochance/codex-app-transfer/issues/61))
- `Codex-App-Transfer-v<版本>-Linux-x86_64.deb`：Debian/Ubuntu 系，自动拉运行时依赖
- `Codex-App-Transfer-v<版本>-Linux-x86_64.AppImage`：通用 Linux x86_64，免安装，`chmod +x` 直接跑

每个二进制都附带 `.sha256` 和 `.sig`（基于 `release/Codex-App-Transfer-release-public.pem` 的 RSA-3072 PKCS#1 v1.5 + SHA-256 签名）。`release/latest.json` 是版本元数据，供应用内"检查更新"读取。

Windows 版目前还没有 Authenticode 代码签名证书，系统可能提示未知发布者；可用 `.sha256` / `.sig` 校验下载完整性。

如果这个工具对你有帮助，欢迎 Star 一下不迷路。遇到问题、想新增供应商支持，或想反馈兼容性 issue，可直接发到 [Issues](https://github.com/Cmochance/codex-app-transfer/issues)。

## 基本用法

1. 启动 Codex App Transfer，弹出桌面窗口。
2. 在仪表盘点右上角加号 → 选择预设或自定义供应商，填入 API Base URL、API Key、模型映射。
3. 在"转发"页面点"启动转发"，本机 `18080` 端口开始监听。
4. 在 Codex CLI 配置文件（`~/.codex/config.toml`）里把 `base_url` 指向 `http://127.0.0.1:18080`，把 API Key 设为本工具显示的 Gateway API Key。
5. 重新打开 Codex CLI，模型选项就会自动列出当前供应商的模型映射。
6. **务必把 Codex CLI 切到 Full access(`/approvals` → "Full access")**。第三方 OpenAI-compatible provider(Kimi / DeepSeek / MiMo / MiniMax 等)在 Codex CLI 默认 `auto` 审批模式下,发起任何工具调用都会被卡在审批弹窗,长会话会因为单次工具调用超时 / 用户没及时点同意而失败。Full access 模式直接放行工具调用,这是接入第三方 provider 的**事实必要前提**。

如果桌面窗口无法打开，可以手动访问备用地址：

```text
http://127.0.0.1:18081
```

## English Quick Start

Codex App Transfer is a lightweight desktop app that turns OpenAI Codex CLI into a multi-provider client. It runs a local gateway, translating Codex CLI's Responses API requests (HTTP streaming / non-streaming requests, including the `/responses` HTTP fallback) into Chat Completions format and forwarding them to providers such as Kimi Code, Kimi Moonshot, DeepSeek V4, Xiaomi MiMo, Zhipu GLM, and Alibaba Cloud Bailian.

Unlike `farion1231/cc-switch` and similar Anthropic-oriented Claude Code tools, this project focuses on OpenAI Codex CLI: manage providers, model mapping, forwarding ports, and logs from a desktop UI so Codex CLI can talk to any third-party OpenAI-compatible inference endpoint.

The Windows installer / portable build opens a standalone desktop window by default; the local browser URL is only a debug fallback. Closing the window minimizes the app to the system tray; right-click the tray icon and choose "Exit" to fully quit.

### Project status

- Current version: **v2.1.4** (current mainline after the full Python → Rust/Tauri rewrite)
- Validated upstream: Kimi Code (`kimi-for-coding` UA gateway), Kimi Moonshot (Platform API), DeepSeek V4 (official baseUrl, with "Max thinking" mode), Xiaomi MiMo (Token Plan / Pay for Token), MiniMax M2.x (OpenAI-compatible chat — incompatible fields auto-stripped, `reasoning_split` enabled, `<think>` tag fallback), **Google AI Studio / Gemini API** (v2.1.4 adds `gemini_native` adapter, direct `:streamGenerateContent` with no chat intermediate)
- Experimental compatibility: Zhipu GLM / Alibaba Cloud Bailian / other OpenAI Chat-compatible reverse proxies
- Platforms: v2.0.0 launched on macOS arm64 only; v2.0.1+ release builds produce macOS arm64 / Windows x64 / Linux x86_64 assets; **v2.1.0+ also ships macOS Intel x64** (see [issue #61](https://github.com/Cmochance/codex-app-transfer/issues/61)). v2.0.0 ~ v2.0.5 are archived as intermediate releases (source-only, no binaries — see each Release page); use v2.1.4 in production. Earlier Python v1.0.x line is no longer recommended for new installs — go straight to v2.x.
- Data compatibility: `~/.codex-app-transfer/config.json` carries over from v1.x without conversion — upgrade or roll back without losing config
### v2.x mainline rollups (cumulative through v2.1.4)

Grouped by theme.

**Gemini CLI OAuth direct (impersonate gemini-cli, no API key required)** (next release main theme)
- New `apiFormat=gemini_cli_oauth` + `authScheme=google_oauth_cloud_code`. Users log in via browser **once** to Google; access/refresh tokens are persisted locally (`~/.codex-app-transfer/gemini-oauth.json`, Unix 0600 + atomic write) and **auto-refreshed** by subsequent requests. **No manual API key needed**. Free-tier per-account quota is bound to a GCP `cloudaicompanionProject` (auto-provisioned).
- New standalone `crates/gemini_oauth/` crate implementing the full OAuth flow (impersonating `google-gemini/gemini-cli`'s web flow): loopback callback server (dynamic port) + 32-byte hex CSRF state + `oauth2.googleapis.com/token` code grant + 60s-buffer auto refresh + `cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`+`onboardUser` LRO project bootstrap
- New `crates/adapters/src/gemini_cli/` adapter: **reuses 90% of `gemini_native`'s internal transforms** (JSON Schema sanitize / Web Search soft constraint / multi-turn function-call cache / failure SSE / etc.), only adding the outer envelope layer (`{model, project, user_prompt_id: uuid_v4, request: <inner>}`) and SSE event `{response: {...}}` outer unwrap. `proxy/forward.rs` adds a `GoogleOauthCloudCode` authScheme branch: dispatch awaits `ensure_valid_access_token` to inject Bearer; tokens auto-refresh 60s before expiry and persist.
- ⚠️ **TOS gray area + disclaimer**: `cloudcode-pa.googleapis.com/v1internal` is Google's **internal, undocumented** endpoint (CLIProxyAPI #1584/#1674 broke multiple times after gemini-cli upgrades); Google may change the protocol at any time. Code Assist TOS forbids "automated/programmatic use outside the official client" — impersonation technically violates it; **users assume the risk**. Free-tier per-account rate limits are strict and heavy use hits 429. This feature is **not guaranteed long-term**; fallback path: switch to `gemini_native` (API key mode, available since v2.1.4).
- Wire-level implementation references [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI) (MIT, Go, 31.8k⭐) — credit added in the Acknowledgements section.
- 76 unit tests cover OAuth (token persist + flow + bootstrap + service refresh) + adapter (outer envelope + SSE unwrap + chunk-buffered boundaries + keepalive passthrough) + proxy routing (GoogleOauthCloudCode authScheme dispatch).

**Gemini Native direct adapter** (v2.1.4, this release's main theme)
- New `apiFormat=gemini_native` + `authScheme=google_api_key` (`x-goog-api-key` header). Codex.app `/responses` inbound transforms directly to Google `:streamGenerateContent?alt=sse` outbound, **never via chat intermediate** (`crates/adapters/src/gemini_native/`). Gemini 3 routes to `/v1alpha`, Gemini 2.x to `/v1beta`; baseUrl without version prefix is auto-resolved by the adapter
- **Web Search**: Gemini 3+ allows `googleSearch` builtin tool + `functionDeclarations` to coexist (`toolConfig.includeServerSideToolInvocations`); Gemini 2.x does not — adapter falls back to a `systemInstruction` soft constraint informing the model that `google_search` is currently unavailable (LiteLLM `apply_response_schema_transformation` style, **never destructive-drops fields silently**)
- **JSON Schema compatibility**: `type:["string","null"]` array form → `string` + `nullable:true`; `anyOf` containing null → folded into nullable; `$ref` / `$defs` inlined; `additionalProperties` / `strict` stripped; empty enum strings → null; objects missing `properties` get an empty object — adapts to Gemini's strict schema validation, **all transformations non-destructive**
- **Multi-turn function calling round-trip**: hooks into the global `ToolCallCache` (`crates/adapters/src/responses/tool_call_cache.rs`); when `function_call_output` is missing the prior `function_call`, looks up by `call_id` and **synthesizes the missing assistant turn**, fixing Gemini's "function response without prior call" 400. `tool_choice` is normalized by mode (`auto/none` drop, `required/specific` BadRequest — never silently swallowed). `contents` must start with `user` role (Gemini's strict user/model alternation); a placeholder user turn is inserted if the first item is a `model` turn
- **Error stream → Responses SSE failure** (2026-05-10 fix): upstream 4xx/5xx no longer passes raw Gemini JSON through (which left Codex.app stuck on "Thinking"). Instead synthesizes a compliant SSE failure stream (`response.created` → `response.failed`) with structured error codes: `auth_error` / `permission_denied` / `quota_exceeded` / `rate_limited` / `timeout` / `service_unavailable` / `content_filter` / `bad_request` / `server_error` / `upstream_transport_error` / `upstream_error`. Mid-read transport errors / bodies > 64KB / non-UTF-8 / messages > 2000 chars are all marked with marker suffixes — never silently swallowed
- 60+ unit tests cover wire round-trip / schema sanitize / tool_choice modes / multi-turn / failure stream complete lifecycle / error classification / array-form error body / oversized body / non-UTF-8 / transport err etc.

**Custom Third-Party + Responses protocol direct passthrough** (v2.1.3)
- Users explicitly pick `apiFormat=responses` on the new "Custom Third-Party" preset card and provide both baseUrl + apiKey → **Codex.app connects directly to upstream, bypassing the proxy** (codex-account-switch style pure-config switch): `~/.codex/config.toml::openai_base_url` ← user baseUrl, `auth.json` ← user apiKey, local 18080 not started. Suitable for OpenAI official / any reverse proxy implementing OpenAI Responses API natively
- **v1.x MiMo Token Plan 404 lesson preserved**: healing on startup force-overrides the 8 builtin presets' `apiFormat` back to `openai_chat`; bypass only triggers on **explicit custom + user-chosen** third-party providers — informed choice
- New `ResponsesPassthroughAdapter` (`crates/adapters/src/passthrough.rs`): byte-level passthrough with SSE envelope / `sequence_number` / `chatcmpl→resp_` IDs all produced by upstream (proxy doesn't rewrite). `/responses/compact` (this repo's private extension) always stays on local ResponsesAdapter to avoid upstream 404
- "Custom Third-Party" preset card (`bi-puzzle` icon) can be added unlimited times + protocol type UI now read-only display + Responses unlocks default model mapping as optional + baseUrl input adds `autocapitalize="off"` + form-submit time direct-mode requires both baseUrl + apiKey
- ⚠️ **Usage caveat — long conversations on third-party Responses providers may hit `thinking_signature_invalid`**: Codex CLI auto-compacts history when it exceeds the context threshold and injects a `type=compaction` item (Codex CLI private-protocol field, **not part of the OpenAI Responses standard**; the field name `encrypted_content` is a historical naming artifact — its value is actually plaintext summary). Third-party Responses upstreams (OpenAI official / Anthropic-style gateways / self-hosted wrappers) don't recognize this item and try to verify `encrypted_content` as a thinking signature → returns `thinking_signature_invalid`. **Workaround**: when switching to a third-party Responses provider, **start a new conversation** to avoid history accumulating into a compaction item; or use a builtin proxy-routed provider (`apiFormat=openai_chat`) — the local ResponsesAdapter correctly expands `compaction` items into user messages before forwarding

**Speed test message tiers + upstream error diagnostic loop** (v2.1.3)
- Speed test 401/403 stays `reachable=true` (connection OK) + new `authStatus="auth_required_or_invalid"` field + **green** display + message `connection OK; API key not configured or auth not verified`, decoupling "connectivity" from "auth" semantics. Frontend `isProviderTestResultBad()` helper does not flag bad on authStatus alone — real auth failures surface via Codex CLI's actual request path
- `forward.rs` two `resp.bytes().await.unwrap_or_default()` paths replaced by `match` + `telemetry.logs.add` WARN to surface body-read failures
- `provider_test_error_label` split into 7 categories (Timeout / ConnectError / RedirectError / DecodeError / RequestError / BodyError / OtherError) for easier user self-debug
- `desktop.rs::one_million_catalog_ready` catalog read/parse failures now write `proxy_telemetry().logs.add` (visible in logs viewer)

**Global tracing → proxy_telemetry.logs bridge** (v2.1.3 silent-failure root cure)
- workspace had 5 historical `tracing::warn!` call sites (healing apiFormat force-override / `warn_once_drop_tool` / `disable_web_search_for` etc.) that were **completely silent in the Tauri desktop binary** (no `tracing_subscriber` registered, events dropped by default)
- New `TelemetryLogsLayer` (`src-tauri/src/telemetry_bridge.rs`) implementing `tracing_subscriber::Layer`, registered at `main()` first line; bridges all `tracing::*` events into `proxy_telemetry().logs` same channel as forward.rs (visible in `~/.codex-app-transfer/logs/proxy-*.log` + Settings logs viewer)
- `LevelFilter::INFO` cap to prevent future dep TRACE-level noise + sensitive-field redaction (`api_key` / `authorization` / `bearer` / `token` / `secret` / `password` substring → `[REDACTED]`) + `try_init` failure → ERROR to telemetry; success → INFO `tracing-bridge active` positive confirmation

**MCP tool dispatch + namespace** (v2.1.1)
- `type:"namespace"` packs flattened via recursive `flat_map` into top-level function tools sent upstream; chat→responses SSE envelope replays 16+ Responses API fields (`tools` / `tool_choice` / `reasoning` / `text` / `metadata` / `previous_response_id` / `temperature` etc.) + `created_at` + monotonically increasing `sequence_number` per event
- **Adds `namespace` field to function_call SSE output**: `strings` against the Codex.app desktop binary surfaces `dynamic tool namespace must not be empty for` validation; missing this field causes every namespace-bound tool call to fail with `unsupported call: <name>`
- `warn_once_drop_tool` prevents per-turn re-warn spam for unsupported tool types
- Borrows from mimo2codex `streamToSse.ts` + Codex.app binary reverse engineering. After the five-layer fix, full Notion / Figma / Zeabur etc. MCP server toolsets become model-visible and dispatchable

**Native chat-endpoint `web_search` tool support** (v2.1.2, per-provider documentation-verified)
- **Xiaomi MiMo**: private `type:"web_search"` + 4 field passthrough (`user_location` / `max_keyword` / `force_search` / `limit`) + A-layer `web_search_enabled` config toggle defaulting off + B-layer transparent retry on upstream 4xx + in-process disable cache
- **Kimi (Moonshot)**: builtin_function `$web_search` + auto-injected `thinking:{type:"disabled"}` top-level field (mandated by Kimi docs)
- **DeepSeek / MiniMax**: documentation-proven not supported on chat endpoint → explicit drop with dedicated warn key
- **Cross-provider URL citation inbound**: any chat-completions upstream returning URL citations flows through `response.output_text.annotation.added` events for Codex.app UI source link display (completely lost before this fix)
- **MiniMax builtin preset card**: official pink-gradient sound-wave M favicon, pickable directly like other builtin providers (no manual base_url)

**Configuration self-healing / protocol routing / client identity**
- **Zero leakage of Codex CLI client identity headers**: strips `originator` / `x-codex-*` / `x-openai-*` / `chatgpt-account-id` / `session_id` / `thread_id` / `user-agent` on outbound; reqwest default UA changed to neutral `Codex-App-Transfer/<v>` — fixes Kimi For Coding Windows 403 access_terminated_error
- **Configuration self-healing**: on load, providers whose normalized `baseUrl` matches any builtin preset are force-overwritten with `apiFormat / authScheme / extraHeaders` and `isBuiltin=true` (independent of the `isBuiltin` field or random-hex ids); admin path persists changes to disk
- **MiMo Token Plan 404 fix**: `apiFormat == "responses"` is no longer misread as "upstream-native Responses passthrough"; every provider goes through `local_proxy` and Responses↔Chat conversion happens locally
- **`apiFormat` fallback unified to `openai_chat` stack-wide**: 8 sites aligned with schema serde default

**Responses session / ws path**
- **Persistent session cache**: two-tier `ResponseSessionCache` (L1 in-memory 1000×60min LRU + L2 SQLite 30-day TTL at `~/.codex-app-transfer/sessions.db`); survives Tauri restarts and TTL renewals
- **ws warmup no longer touches upstream + closes ws immediately**: `server.rs` ws handler detects `generate:false` / empty-input frames and sends a ws **Close frame**, so Codex CLI immediately falls back to HTTP `stream_responses_api` (older builds sent `{"type":"error",...}` text frames and waited the full ws idle timeout — feedback fb-8f5b51fb / fb-0c121681 measured 4m48s; Close frame brings fallback down to seconds)
- **Cache miss error body aligned with OpenAI SDK**: `previous_response_id` not found returns HTTP 400 + `code:"previous_response_not_found"` matching OpenAI server-side behavior literally

**Auto-compact summary quality** (v2.1.0)
- Local `/responses/compact` SUMMARIZATION_PROMPT replaced from the 86-char Codex version to a Claude-Code-style 9-section strict schema (`<analysis>` + `<summary>` two-pass output, required sections `Primary Request` / `Files and Code Sections` / `All User Messages listed verbatim` / `Errors and Fixes` / `Current Work` / `Next Step verbatim quote`, plus a few-shot example)
- Prompt injected as the last user message instead of system, improving third-party provider compliance
- Response parser only extracts the `<summary>` section into resumed history, filtering out `<analysis>` chain-of-thought
- **Fixes the previous "compact only remembers the last 1-2 actions, loses the task goal / file paths / earlier user requests" experience**

**Multimodal / vision**
- **MiMo image-only fallback**: MiMo's vision API requires at least one `text` part when `image_url` parts exist (otherwise 400 `Param Incorrect: text is not set`); the Codex CLI paste-image-without-typing case is covered by appending a single-space text part
- DeepSeek vision (image) inputs stripped to avoid upstream 400
- Vision allowlist switched from provider substring to per-model exact match

**Reasoning / thinking**
- Kimi/DeepSeek thinking header rendering fix in Codex CLI TUI + `extraHeaders` `{apiKey}` template (resolves Kimi 403)
- `reasoning_content` history replay + global `TOOL_CALLS_CACHE` rebuilds missing `assistant.tool_calls` after history compaction
- Emits the protocol-correct `response.in_progress` event
- Normalizes chat-style `usage` into Responses-API `usage` (with `cached_tokens` defaulted to 0 to fix Kimi/MiMo reconnect stalls)

**MiniMax compatibility** (contributed by [@lukegood](https://github.com/lukegood))
- Request-side allowlist clean-up of `reasoning_effort` / `response_format` / `parallel_tool_calls` etc.; `tool` strips `strict`; consecutive `system` merged
- Response-side `reasoning_details` split into reasoning + `<think>` fallback split scoped to MiniMax only

**UI / desktop**
- Windows Codex restart no longer flashes a terminal window + tray menu interactions fixed
- Enable-button gives instant feedback instead of waiting for the forwarder roundtrip
- **User-facing strings switched back to English**: API response messages / proxy telemetry log / tray menu / SDK error bodies all in English to align with OpenAI/Anthropic SDK error styles (comments / tests still in Chinese)

### Changelog

Per-version changes are tracked at [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) (and locally under `docs/release-notes/v*.md`). The current management API and proxy surface are indexed in [`docs/api-route-status.md`](docs/api-route-status.md).

### Getting started

1. Download the latest installer or portable package from [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases/latest).
2. Open Codex App Transfer — a desktop window appears.
3. On the dashboard click the top-right `+` and pick a preset (or define a custom provider). Fill in the API base URL, API key, and model mappings.
4. Open the **Proxy** page and click `Start forwarding` — the app listens on `127.0.0.1:18080`.
5. In Codex CLI's config (`~/.codex/config.toml`), point `base_url` at `http://127.0.0.1:18080` and set the API key to the gateway API key shown in the app.
6. Restart Codex CLI; the model picker now lists the model mappings for the active provider.

> Note (v2): the management UI is now in-process via Tauri's custom `cas://` URI scheme. The v1.x debug fallback at `http://127.0.0.1:18081` no longer exists; use the desktop window (or restart the app to recreate it).

### What it does

- Manages multiple providers and maps OpenAI model names (`gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.2`) to each provider's real model ID.
- Translates Codex CLI's Responses API requests (streaming and non-streaming) to Chat Completions format before forwarding.
- Compatible with the Codex CLI 0.126+ `responses_http` path and the `/responses` route alias (no `/v1/` prefix); the current Rust mainline exposes an HTTP/SSE forwarding surface.
- Re-encodes upstream `chatcmpl-*` IDs into Codex-friendly `resp_<base64>` while preserving deployment affinity, so `previous_response_id` keeps working.
- For thinking-enabled upstreams (Kimi / DeepSeek), automatically attaches `reasoning_content` to historical assistant tool-call messages to avoid `400 thinking is enabled but reasoning_content is missing`.
- Normalizes `reasoning_effort` (`xhigh` / `max` → `high`; `auto` / `none` dropped) so providers that only accept `minimal/low/medium/high` won't reject the request.
- Live log panel auto-refreshing every 2 seconds, with an `Open log folder` button that jumps to `~/.codex-app-transfer/logs/`. The `Clear logs` button archives the active log to `logs/backup/` with a timestamp suffix instead of deleting it.
- Chinese / English UI with light, dark, green, orange, gray, and white themes (only the dark theme changes background colors).
- System tray on Windows / macOS / Linux + cross-platform single-instance lock (a second launch auto-focuses the existing window).

### Security notes

- Provider API keys are stored only in `~/.codex-app-transfer/config.json` — do not upload it together with `logs/` to a public repo.
- The forwarding service binds to `127.0.0.1` only and never hijacks the system proxy. The v2 management UI is served through Tauri's in-process `cas://` scheme instead of a loopback HTTP admin port; `X-CAS-Request` is still sent by the frontend for compatibility, but it is not the current security boundary.
- Backup / export JSON files contain plaintext API keys — keep them on trusted devices only.
- Logs append to `~/.codex-app-transfer/logs/proxy-YYYY-MM-DD.log`. Clearing logs archives them to `logs/backup/` with a timestamp suffix (no deletion).
- **Conversation history persistence (v2.0.11+)**: the proxy persists Responses-API session messages (mapped to `previous_response_id`) to `~/.codex-app-transfer/sessions.db` (SQLite, 30-day TTL) so Codex CLI long sessions survive app restarts without triggering `previous_response_not_found`. The DB contains complete chat history (system / user / assistant / tool messages); to wipe it, either call the admin endpoint `POST /api/sessions/clear` or delete the file directly.
- Windows builds are not Authenticode-signed yet; verify downloads with the published `.sha256` / `.sig` and the verification snippet below.
- This project is not an official OpenAI project.

## 默认端口

- **本机转发服务**:`18080`,Codex CLI 通过它访问上游供应商
- **管理界面(v2)**:Tauri 自定义 `cas://` URI scheme + 同进程 axum,**不再绑定 18081 端口**。v1.4 的 18081 调试入口已移除,管理界面只能通过应用窗口访问

可在 设置 → 端口 修改转发端口,修改后需要重启转发。

## 本地开发(v2 / Rust)

前置:Rust 1.80+(`rustup`)、Tauri CLI(`cargo install tauri-cli --version "^2"`)、macOS 上需要 Xcode CLT。

```bash
git clone https://github.com/Cmochance/codex-app-transfer.git
cd codex-app-transfer

# 启动桌面窗口(开发模式,代码改动自动重编译)
cargo tauri dev

# 单独跑后端测试(不开窗口)
cargo test --workspace
```

`frontend/` 是 v1.4 同款 Bootstrap + 原生 JS,改任何 HTML/CSS/JS 文件后**刷新窗口**即可生效(Tauri 的 cas:// 协议会重新读取 `include_dir!` 嵌入的资源,但需要重新构建二进制——dev 模式下会自动)。

### Fixture 反向 diff(契约测试)

```bash
cargo run -p xtask --release -- gen-fixtures
git diff --exit-code -- tests/replay/fixtures/registry/
```

`tests/replay/fixtures/` 下的 JSON fixture 是字节级契约 (Phase 3 起由 `xtask gen-fixtures` 维护权威源, 之前是 Python `gen_registry_fixtures.py`)。Rust crate 的集成测试 (`crates/registry/tests/golden_compat.rs` + `crates/proxy/tests/streaming_passthrough.rs` 等) 会读这些 fixture 做 round-trip 验证;CI 强制 `git diff --exit-code` 闭环。

## 历史版本(v1.x / Python)

v1.0.4 及更早版本基于 Python 3.11+ + FastAPI + pywebview + PyInstaller。Phase 1-3 清理后,主线不再保留 `backend/` / `main.py` / `scripts/*.py` / `requirements.txt` / `pyproject.toml`;如需运行历史 Python 版本,请切到对应 tag 或 GitHub Release 的 source archive:

```bash
git checkout v1.0.4
pip install -r requirements.txt
python main.py
```

v2.0.0 起主线只维护 Rust 实现。历史 Python 代码只通过 v1.x tag / release source archive 保留。

## 打包(v2)

只需一条命令(macOS):

```bash
make mac-app
```

内部跑 `cargo tauri build --bundles app`,产出落到 `dist/mac/Codex App Transfer.app`。**单二进制 27MB**,不需要 Python 解释器、不需要外部 frontend/ 目录(全部 `include_dir!` 嵌入二进制)、不需要 Docker/Wine。

正式发布走 GitHub Actions 的 `release.yml` 工作流,由 Tauri bundler 生成三平台资产并由 `xtask release-bundle` 生成 `.sha256` / `.sig` / `latest.json`。本地 `make mac-app` 仅用于 macOS 自测;Apple Developer ID notarize 仍是后续工作。

### Windows / Linux

v2.0.0 首发只 ship macOS arm64;v2.0.1 起发布工作流会生成 Windows NSIS / MSI、Linux `.deb` / AppImage 和 macOS `.dmg`。Tauri 2 原生支持跨平台编译,代码层面没有平台特异(除 macOS Apple-specific 的 NSApp.hide / NSRunningApplication.activate 等已 `#[cfg(target_os = "macos")]` 隔离),`cargo tauri build` 在对应平台直接出 native 产物。

### 历史(v1.x / Python)打包

v1.0.4 之前用 PyInstaller + Docker + Wine + NSIS 三平台打包。相关旧脚本已从主线清理;如需复现 v1.x 打包流程,请切到对应 v1.x tag 或 release source archive。主线后续正式发布链路会在 v2.0.x 中切到 Tauri bundler。

### 签名校验

任意有 Python + cryptography 的环境里都能验签 (Python 3.7+, `pip install cryptography`)。本仓库已不再依赖 Python, 但发布产物的验签协议是 RSA-3072 PKCS#1 v1.5 + SHA-256 RFC 标准, Python `cryptography` 是最方便的验证工具之一 (OpenSSL CLI / Rust `rsa` crate 也可):

```bash
python -c "
from pathlib import Path
import base64
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding
pub = serialization.load_pem_public_key(Path('release/Codex-App-Transfer-release-public.pem').read_bytes())
asset = 'release/Codex-App-Transfer-v2.0.6-macOS-arm64.dmg'  # 替换成你下载的产物
sig = base64.b64decode(Path(asset+'.sig').read_text())
pub.verify(sig, Path(asset).read_bytes(), padding.PKCS1v15(), hashes.SHA256())
print('OK')
"
```

## Troubleshooting

### Codex CLI 提示 `404 Not Found url: http://127.0.0.1:18080/responses`

老版本只有 `/v1/responses`，Codex CLI 0.126 起会回退到 `/responses`（不带 `/v1/`）。本工具已加路由别名，更新到 v1.0.1+ 即可。

### Codex CLI 提示 `stream disconnected before completion`

通常是 `response.id` / `response.model` 没有按 Codex CLI 期望填回。本工具会把上游 `chatcmpl-...` 重写成 `resp_<base64>` 并保留请求模型名，请确认转发日志中确实看到了 `resp_...` 而不是 `chatcmpl-...`。

### 上游 400：`thinking is enabled but reasoning_content is missing in assistant tool call message`

Kimi / DeepSeek 在开启 thinking 后强制要求历史中带 tool_call 的 assistant 消息提供 `reasoning_content`。v1.0.1 已自动补默认空字符串，并把 Responses 输入里的 reasoning items 映射到对应 assistant 消息。如果仍出现，请抓一份转发日志反馈。

### 上游 400：`'reasoning_effort' does not support 'xhigh'`

Codex 用户配置里若把 `model_reasoning_effort` 设成 `xhigh` / `max`，本工具会自动降级到 `high`。`auto` / `none` 等 Chat 端不接受的值会被丢弃。

### 端口冲突

v2 默认只监听 `18080`(转发);管理界面已改走 Tauri 同进程 `cas://` scheme,不再占用 18081 端口。如果 18080 被占:

```powershell
netstat -ano | findstr :18080
```

发现占用后，可以关闭占用进程，或在 设置 → 端口 改成空闲端口后重启转发。

### Windows 提示未知发布者

当前 Windows 构建还没有 Authenticode 代码签名证书，所以 Windows 可能提示未知发布者。Release 页面提供 `.sha256` 和 `.sig`，可用于校验安装包没有被替换。

### 日志去哪了

- 应用界面：转发页面下方实时面板，每 2 秒自动刷新。
- 磁盘文件：`~/.codex-app-transfer/logs/proxy-YYYY-MM-DD.log`，可点"查看日志"按钮直接打开。
- 清除日志：把当前日志移到 `logs/backup/` 并加时间戳后缀，不直接删除。

## 技术栈(v2)

- **后端 / 转发**:Rust 1.80+ · axum 0.8 · reqwest 0.12 (rustls-tls) · tokio
- **协议适配**:`crates/adapters` —— Responses ↔ Chat 互转(请求 body + 流式响应状态机,支持 reasoning_content / tool_calls)
- **前端**:HTML + CSS + 原生 JavaScript + Bootstrap 5.3.3(本地化,无 CDN 依赖)
- **桌面壳**:Tauri 2 + tray-icon 0.23,通过自定义 `cas://` URI scheme 把 frontend/ 与 axum 同进程串起来,无 TCP loopback
- **存储**:`~/.codex-app-transfer/config.json`(配置,与 v1.x 互通)、`~/.codex/{config.toml,auth.json}`(Codex CLI 集成)、`~/.codex-app-transfer/codex-snapshot/`(apply 前的备份快照)
- **打包**:`cargo tauri build` 单命令 → `dist/mac/Codex App Transfer.app`(27MB,unsigned 自测;v2.0.x 起接 Apple Developer ID + notarize)

## 重写过程

v2.0.0 是从 v1.0.4 (Python) 一次性重写而来,完整过程(7 阶段 + 30+ 修订日志)记录在 [`docs/refactor/migration.md`](docs/refactor/migration.md),核心结论 + 量化对比 + 关键 bug 修复见 [`docs/release-notes/v2.0.0.md`](docs/release-notes/v2.0.0.md)。

## 安全说明

- API Key 仅保存在本机 `~/.codex-app-transfer/config.json`，不要把它和 `logs/` 一起上传公开仓库。
- 转发服务只监听 `127.0.0.1`，不接管系统代理；v2 管理界面通过 Tauri 同进程 `cas://` scheme 提供，不再暴露 loopback HTTP 管理端口。前端仍会发送 `X-CAS-Request` 兼容头，但当前安全边界是 Tauri 自定义协议，而不是这个请求头。
- 备份 / 导出配置的 JSON 文件包含 API Key 明文，仅在可信设备上保存。
- 代码签名公钥位于 `release/Codex-App-Transfer-release-public.pem`，可用上文"签名校验"里的 Python 片段验证下载完整性。

## 致谢

本项目站在前人的肩膀上:

- **[CC-Switch](https://github.com/farion1231/cc-switch)** — 轻量桌面 + 一键切换 provider 形态启发。
- **[CC Desktop Switch](https://github.com/lonr-6/cc-desktop-switch)** — v1.x 桌面壳 / 托盘 / 打包发布链路骨架沿用。
- **[litellm](https://github.com/BerriAI/litellm)** — Responses ↔ Chat 双向协议转换思路。
- **[Tauri](https://tauri.app/)** — v2 桌面壳 + cas:// 同进程 axum 架构基座。
- **[Piebald-AI/claude-code-system-prompts](https://github.com/Piebald-AI/claude-code-system-prompts)** — autocompact 9-section summary prompt 蓝本。
- **[7as0nch/mimo2codex](https://github.com/7as0nch/mimo2codex)** — MiMo image / namespace 展平 / web_search / annotation 协议借鉴。
- **[router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)**(MIT, Go, 31.8k⭐)— Gemini CLI OAuth 流程 + Cloud Code Assist 内部 API wire 形态参考(`internal/auth/gemini/` + `internal/runtime/executor/gemini_cli_executor.go` + `internal/cmd/login.go`),让我们能在不依赖 API key 的前提下接入 Google Gemini 免费 tier 配额。

### 社区贡献者

感谢以下贡献者通过 Pull Request 直接改进过本项目(按首次提交时间倒序;完整列表见 [Contributors 图表](https://github.com/Cmochance/codex-app-transfer/graphs/contributors)):

- [@lukegood](https://github.com/lukegood) — MiniMax M2.x 接入兼容性([#47](https://github.com/Cmochance/codex-app-transfer/pull/47))。
- [@cw881014](https://github.com/cw881014) — 早期协议层修复 3 PR([#1](https://github.com/Cmochance/codex-app-transfer/pull/1) / [#7](https://github.com/Cmochance/codex-app-transfer/pull/7) / [#12](https://github.com/Cmochance/codex-app-transfer/pull/12))。

如果你提交过 PR 但希望改名 / 补链接 / 移除,直接开 issue 跟我说一声即可。

本项目专注 OpenAI Codex CLI 接入,不是 OpenAI 的官方项目,也不复用其商标、Logo 或发布身份。

## 许可证

MIT License。完整文本见 [LICENSE.txt](LICENSE.txt)。
