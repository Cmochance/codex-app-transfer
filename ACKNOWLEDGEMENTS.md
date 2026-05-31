# 致谢与上游借鉴索引

本项目在开发中借鉴了多个开源上游的实现。本文档**详细记录每条借鉴在本项目里的形式、范围、落地位置(file:line)、本地差异与同步策略**,作为 catalog,供贡献者 / 未来维护者快速完成以下 3 类任务:

| Use case | 入口 |
|---|---|
| **定位** — 看到 codebase 某段代码不理解,想知道借鉴来源 | grep 文件 → 在本文档"借鉴清单"列里反查上游 |
| **索引** — 新增 / 修改借鉴时落 entry | 按下方 Entry Schema 加 / 改一个 `### 项目名` section |
| **更新** — 上游有重要变更想同步 | 通过"借鉴清单 file:line"定位本项目实现 → 按"同步策略"判断如何 sync |

> README.md / README.en.md 致谢段只列一句话概览;详情全部在本文档维护。
> **新增借鉴 = 同 PR 内 ① README 致谢段加一行概览 + ② 本文档加完整 entry,缺一不可。**

## 如何使用本文档

- **定位**:先看"借鉴形式术语",理解 entry 标记的强度;然后看"借鉴清单"每条 → 本项目 file:line。
- **索引**:每个上游唯一 `### 项目名` section,GitHub 自动生成 TOC,Ctrl+F 项目名即跳。
- **更新**:
  - **小修**(同步常量 / fix bug):按"借鉴清单"定位 file:line,改完同步 entry 的"代码层引用 quote"
  - **上游主线大改 / 失效**:在 **Linear workspace `Mochance`**(team Mochance,label `Improvement`)开 followup issue,长跨度跟踪
  - **License 变更 / 上游归档**:必看 entry 的 License + TOS 字段做合规判断

## Entry Schema

每个上游对应一个 `### 项目名` section,按以下字段顺序写:

| 字段 | 必填 / 可选 | 用途 |
|---|---|---|
| **Link** | 必填 | 上游 GitHub URL |
| **License** | 必填(算法/数据/wire 类),可选(思路/启发类) | 合规硬要求 — 算法 1:1 复刻 / 整体借鉴必须明确 license |
| **借鉴形式** | 必填 | 按下方术语表选 1-3 个 tag |
| **首次借鉴 PR / 时间** | 必填 | PR # 或 "v1.x 起继承"。追溯何时入库,future 重写时知道决策 baseline |
| **借鉴清单** | 必填 | 每条 "<上游做了什么 / 本项目借了什么>" → `本项目 file:line` |
| **本项目差异 / 扩展** | 可选,有重要本地改造时填 | future 维护者关键信息 — 跟上游同步前知道哪些是本地改造不能覆盖 |
| **同步策略** | 可选 | 上游变更怎么 detect + 怎么 sync。无策略可标 "monitor only" |
| **TOS / 法律注意** | 条件性 | 灰色区 / 反向工程 / API ToS 限制时强制填 |
| **关联 PR / followup / issue** | 可选 | 交叉索引(关联本项目内的 PR / issue / followup) |
| **代码层引用 quote** | 可选 | 贴 1-2 段本项目代码注释原文,evidence 无需翻代码 |

## 借鉴形式术语

| Tag | 含义 | License 处置 |
|---|---|---|
| **算法 1:1 复刻 / 整体借鉴** | 上游核心逻辑 byte-for-byte 或语义级移植 | **必须** 保留 license + 作者署名,代码注释带上游 file:line |
| **数据模式参照** | 上游数据结构 / 静态注册表 / 常量原样镜像 | 同上,加版本 / 同步日期 |
| **Wire-level 对齐** | HTTP/SSE 协议字节级行为复现(headers / endpoint / param 顺序) | 同上 |
| **反向工程产物借鉴** | 上游对闭源 / 灰色 API 的反向工程结论被直接复用 | 必须明示 reference;本项目不重复反向工程 |
| **算法借鉴** | 上游某条 helper / 算法 idea 借走,细节自行实现 | 注释提及上游归属 |
| **Prompt 蓝本** | 上游 prompt 文本作为骨架,本项目调整后使用 | 注释标注 prompt 起源 |
| **思路 / 模式借鉴** | 设计思路启发,无代码复用 | 注释提一句即可 |
| **配置迁移参照** | 因历史 fork / 迁移需对齐旧上游字段命名 | 注释标注历史背景 |
| **产品形态启发** | 概念 / UX 启发,无代码层借鉴 | README 致谢即可 |
| **架构基座** | 框架依赖,非严格"借鉴" | README 致谢即可 |

---

## farion1231/cc-switch

- **Link**: https://github.com/farion1231/cc-switch
- **License**: 见上游 LICENSE(未在本项目代码注释中固化版本)
- **借鉴形式**: 产品形态启发
- **首次借鉴 PR / 时间**: 项目早期(v1.x 之前),无具体 PR
- **借鉴清单**:
  - provider switching 范式(把 ~/.codex / ~/.config/codex 多账号 / 多 provider 切换抽象成桌面 first-class 概念) → 整个 v1.x → v2.x provider 管理 UX(无代码 file:line — 概念级启发)
- **同步策略**: monitor only(产品形态参考,无代码 sync 需求)

## lonr-6/cc-desktop-switch

- **Link**: https://github.com/lonr-6/cc-desktop-switch
- **License**: 见上游 LICENSE
- **借鉴形式**: 早期 fork 演化基础 + 配置迁移参照
- **首次借鉴 PR / 时间**: v1.x 起继承(本项目即由此 fork 演化)
- **借鉴清单**:
  - v1.x 桌面壳骨架 + README 结构 → v1.x 时代产品基础(v2 重写时基本替换,无残留 file:line)
  - 历史 `updateUrl` 字段默认值(指向 `lonr-6/codex-app-transfer`) → `crates/registry/src/healing.rs` `LEGACY_OWNERS` 常量。老 config.json 残留旧 updateUrl 时自愈到当前 owner
- **本项目差异 / 扩展**:
  - 整个 v2 架构(Tauri v2 + Rust crates + cas:// in-process router)已完全重写,不再共享上游代码
  - 仅保留 healing 逻辑兼容老用户的 config.json 迁移
- **同步策略**: monitor only — 仅 healing 字段如发现更多 legacy owner 字段时补 LEGACY_OWNERS
- **代码层引用**(`crates/registry/src/healing.rs` 节选):
  > 背景:本项目由 lonr-6/cc-desktop-switch 及后续 fork 演化而来,早期默认 updateUrl 指向 `lonr-6/codex-app-transfer`。用户老 config.json 里残留该值时,迁移到当前 owner 防 update 失效。

## BerriAI/litellm

- **Link**: https://github.com/BerriAI/litellm
- **License**: MIT(本项目代码注释引用为参考,未 fork 代码)
- **借鉴形式**: 数据模式参照 + 算法 1:1 复刻 + 思路借鉴
- **首次借鉴 PR / 时间**: 多 PR 持续借鉴(协议转换是核心场景,长期参考)
- **借鉴清单**:
  - `response.in_progress` SSE 事件生成时机(严格客户端 — litellm 自身 / Anthropic 工具链 — 期望的事件序列) → `crates/adapters/src/responses/converter.rs:236-254`
  - usage 字段规范化(litellm `_transform_chat_completion_usage_to_responses_usage` 字段映射,chat→responses reasoning_tokens / cached_tokens / total_tokens 等) → `crates/adapters/src/responses/converter.rs`(grep `litellm` 多处)
  - Vertex AI TypedDict 1:1 镜像(`litellm/types/llms/vertex_ai.py`) → `crates/adapters/src/gemini_native/types.rs`(顶部注释明示 "1:1 镜像")
  - tool result 配对修复(防 Anthropic 400 invalid request) → `crates/adapters/src/responses/request.rs`
  - per-provider `get_supported_openai_params` 白名单(各家 `llms/<provider>/chat/transformation.py`)作为 reasoning_effort 入表证据交叉验证:
    - `llms/deepseek/chat/transformation.py:41-63` — DeepSeek 折叠 all non-none 到 thinking.type=enabled(本项目信官方 docs 而非 LiteLLM 保守实现,见 issue #254)
    - `llms/moonshot/chat/transformation.py:91-146` `get_supported_openai_params` — Kimi 不收 reasoning_effort
    - `llms/zai/chat/transformation.py:36-58` `get_supported_openai_params` — GLM 只承认 `thinking` 字段
    - `llms/minimax/chat/transformation.py:87-102` `get_supported_openai_params` — MiniMax 只承认 `thinking` + `reasoning_split`
    - `llms/dashscope/chat/transformation.py`(全文 82 行,无 `get_supported_openai_params`)— 百炼 Qwen 走父类透传,effort 字段可能被 silent ignored
    → `crates/registry/src/reasoning_effort_policy.rs`(各 match arm 注释引用上游 file:line)
- **本项目差异 / 扩展**:
  - 按 Rust 类型系统重写,不引入 PyO3 / pyo3-runtime
  - 转换逻辑保留 litellm 行为语义,但实现路径完全独立(Rust async/await 而非 Python)
  - usage 字段映射跟随 litellm 主线;本项目额外加了 reasoning_tokens 非 0 校验防上游 0 漏统计
  - DeepSeek reasoning_effort 映射**主动偏离** litellm 保守实现(litellm 折叠成 thinking.type=enabled,不区分档位);本项目按官方 docs xhigh→max 真实映射(issue #254 用户报告 litellm 行为让 max 档不可达)
- **同步策略**:
  - litellm 主线 issue 关注协议层变更(尤其 OpenAI/Anthropic protocol updates)
  - 类型镜像变更时手动 diff `litellm/types/llms/vertex_ai.py` 同步 `gemini_native/types.rs`
  - 各 provider `get_supported_openai_params` 变更时同步 `reasoning_effort_policy.rs` 对应 match arm
- **代码层引用**(节选):
  > //! 1:1 镜像 LiteLLM `litellm/types/llms/vertex_ai.py` 的 TypedDict 定义
  > `response.in_progress`,严格客户端(litellm 自身、Anthropic 工具链)
  > 与 litellm 的 `_transform_chat_completion_usage_to_responses_usage` (docs/litellm/.../litellm_completion_transformation/transformation.py)
  > LiteLLM `llms/deepseek/chat/transformation.py:41-63` 实际把所有非 none 折叠成 `thinking.type=enabled`,**不区分档位** — 比官方 docs 保守。本项目信官方 docs。
  > Kimi (Moonshot) — `llms/moonshot/chat/transformation.py:91-146` `get_supported_openai_params` 不收 reasoning_effort。
  > 智谱 GLM (Z.AI) — `llms/zai/chat/transformation.py:36-58` `get_supported_openai_params` 只承认 `thinking` 字段。
  > MiniMax M2.x — `llms/minimax/chat/transformation.py:87-102` `get_supported_openai_params` 只承认 `thinking` + `reasoning_split`。

## tauri-apps/tauri

- **Link**: https://tauri.app · https://github.com/tauri-apps/tauri
- **License**: Apache-2.0 / MIT(双 license)
- **借鉴形式**: 架构基座(框架依赖,非严格"借鉴")
- **首次借鉴 PR / 时间**: v2.x 重写时引入(替换 v1.x 的 Electron 壳)
- **借鉴清单**:
  - Tauri v2 桌面应用框架 → 整个 `src-tauri/` 树
  - 自定义 URI scheme `cas://localhost/` → in-process axum router 模式(借助 `register_asynchronous_uri_scheme_protocol`) → `src-tauri/src/main.rs`(`.register_asynchronous_uri_scheme_protocol("cas", ...)`)
  - `tauri-plugin-single-instance` 单实例 → `src-tauri/src/main.rs` setup chain
  - `tauri-plugin-shell` 跨平台进程管理 → 同上
- **本项目差异 / 扩展**:
  - cas:// 协议把 Tauri webview 跟内嵌 axum router 拼成"in-process HTTP",前端 fetch 不出 webview 沙箱,免 CORS / 免本机端口监听
  - frontend 直接 `include_dir!` 进二进制,运行时零文件依赖
- **同步策略**: Tauri 主线版本升级走 `cargo update -p tauri` + 跑 CI(workspace check + Tauri check)
- **关联 PR / followup**: 整个 v2 架构迁移历史在 git log;v1→v2 切换是分水岭 commit

## openai/codex

- **Link**: https://github.com/openai/codex
- **License**: Apache-2.0
- **借鉴形式**: Prompt 蓝本(精简移植)+ 协议反查(数据模式参照)
- **首次借鉴 PR / 时间**: v2.0.x 起协议结构反查;fix/219 起 prompt 结构借鉴
- **借鉴清单**:
  - `COMPACT_SUMMARIZATION_PROMPT` 基础骨架 → `crates/adapters/src/responses/compact.rs:82-92`
    (源文件:`codex-rs/core/templates/compact/prompt.md`,~460 chars)
  - `COMPACT_SUMMARY_PREFIX` 常量文本(明文,历史字段名 `encrypted_content` 是包袱)
    → `crates/adapters/src/responses/compact.rs:97`
    (源文件:`codex-rs/core/templates/compact/summary_prefix.md`)
  - `CompactionInput` 请求结构 → `compact.rs` 反序列化逻辑
    (源文件:`codex-rs/codex-api/src/common.rs`)
  - `CompactHistoryResponse { output: Vec<ResponseItem> }` + `ResponseItem::Compaction { encrypted_content }` 响应结构
    → `compact.rs` 序列化路径
    (源文件:`codex-rs/codex-api/src/endpoint/compact.rs` + `codex-rs/protocol/src/models.rs:882`)
- **本项目差异 / 扩展**:
  - prompt 补两条 Claude Code 关键 bullet("All user messages verbatim" + "Next Step verbatim quote"),
    借鉴自 Piebald-AI/claude-code-system-prompts 反编译公开版本第 6 / 9 段(见下方同名 entry)
  - 本地加 quality-check gate(`validate_compact_summary_quality`) + input budget pruning,
    upstream codex-rs 无对应逻辑
- **同步策略**: `codex-rs/core/templates/compact/` 路径改动时手动 diff `prompt.md` / `summary_prefix.md`

## Piebald-AI/claude-code-system-prompts

- **Link**: https://github.com/Piebald-AI/claude-code-system-prompts
- **License**: 反编译公开版本,作者发布(见上游 LICENSE)
- **借鉴形式**: Prompt 蓝本(精简移植)
- **首次借鉴 PR / 时间**: v2.0.12(从原 Codex CLI 86 字符 prompt 升级为 9-section 长 prompt);fix/219 简化为 2 条补充 bullet
- **借鉴清单**:
  - **v2.0.12(历史,已替换)**:9-section 结构化 autocompact prompt 整体骨架
    (`agent-prompt-conversation-summarization.md` 反编译公开版本)
  - **fix/219(当前)**:"All user messages verbatim" + "Next Step verbatim quote" 两条 bullet 措辞
    → `crates/adapters/src/responses/compact.rs:89-90`
    (对应原文第 6 / 9 段)
- **本项目差异 / 扩展**:
  - fix/219 起,主 prompt 骨架改从 `openai/codex` 借(见上方 entry);Piebald-AI 仅贡献 2 条锚定 bullet
  - 去掉了 `<analysis>` + `<summary>` 二段输出 schema、9-section 强结构、few-shot example
- **同步策略**: 上游 prompt 变化频率低;若 Claude Code 主线 prompt 大改可手动 diff 第 6 / 9 段措辞
- **代码层引用**(`crates/adapters/src/responses/compact.rs` 节选):
  > **v2.0.12 prompt rewrite**:从原 Codex CLI 的 86 字符 prompt 改为 Claude Code 风格的 9-section 结构化 prompt(精简移植自 Piebald-AI/claude-code-system-prompts 反编译公开版本 `agent-prompt-conversation-summarization.md`)。
  >
  > **fix/219 prompt simplification**:回退到 `openai/codex` 短 prompt 骨架(~460 chars),补充 Piebald-AI 第 6/9 段两条锚定 bullet,合计 ~800 chars。真机 DeepSeek v4-pro:比 9-section 长版本快 ~40% / 省 ~48% token。

## 7as0nch/mimo2codex

- **Link**: https://github.com/7as0nch/mimo2codex
- **License**: 见上游 LICENSE
- **借鉴形式**: 算法 1:1 复刻(跨多 provider 复用)+ 思路借鉴
- **首次借鉴 PR / 时间**: MiMo 集成早期(具体 PR 见 git log `mimo2codex`)
- **借鉴清单**:
  - `buildResponseSnapshot` SSE 响应快照算法(`streamToSse.ts:75-105`) → `crates/adapters/src/responses/converter.rs`(grep `mimo2codex` 10+ 处)
  - `sequence_number` 单调递增(`state.nextSeq()`,`streamToSse.ts:71-72`) → 同上
  - annotation 解析与映射(`streamToSse.ts:156-163, 338-352`) → 同上(跨所有 provider 复用,不仅 MiMo)
  - `warnOnce` 全局去重日志策略(`reqToChat.ts:158-172`) → `crates/adapters/src/lib.rs`
- **本项目差异 / 扩展**:
  - 算法跨所有 provider 复用(MiMo / DeepSeek / Kimi / GLM / Bailian / MiniMax 等都走同一套 SSE 状态机)
  - 按 Rust 类型系统重写,event ordering / state machine 用 enum + state transition
- **同步策略**:
  - mimo2codex 主线变动手动 diff 关键 SSE 行为(`streamToSse.ts` 改动是优先关注点)
  - 新加 SSE event type 时同步本项目 enum
- **代码层引用**(`converter.rs` 多处节选):
  > 借鉴 mimo2codex `streamToSse.ts:75-105` `buildResponseSnapshot`。
  > 借鉴 mimo2codex `streamToSse.ts:71-72` `sequence_number: state.nextSeq()`。
  > 借鉴 mimo2codex `streamToSse.ts:156-163, 338-352` 1:1 复刻,跨所有 provider。
  > 7as0nch/mimo2codex `reqToChat.ts:158-172` 的 `warnOnce` 思路:全局去重。

## router-for-me/CLIProxyAPI

- **Link**: https://github.com/router-for-me/CLIProxyAPI
- **License**: 见上游 LICENSE
- **借鉴形式**: Wire-level 对齐 + 数据模式参照
- **首次借鉴 PR / 时间**: Gemini OAuth 集成期 + 后续 Antigravity 集成期(具体 PR 见 git log `CLIProxyAPI`)
- **借鉴清单**:
  - Gemini CLI / Antigravity OAuth ClientMetadata / UA / version 常量(`header_utils.go::DetectUserAgent`,`auth/antigravity/constants.go`,`antigravity_version.go`) → `crates/gemini_oauth/src/constants.rs`(10+ 处明示锚点)
  - OAuth callback query 参数顺序(必须与上游一致让 Google 端识别,`auth/antigravity/auth.go:60-68`) → `crates/gemini_oauth/src/antigravity/flow.rs`
  - Code Assist 模型清单(交集对齐 gemini-cli upstream + CLIProxyAPI `internal/registry/models/models.json` provider=gemini-cli) → `src-tauri/src/admin/handlers/providers/models.rs:336-372`
  - Antigravity `:fetchAvailableModels` 调用模式 + 静态种子 fallback(`cmd/fetch_antigravity_models/main.go`) → `src-tauri/src/admin/handlers/providers/models.rs:268-333`
- **本项目差异 / 扩展**:
  - CLIProxyAPI 有 background goroutine 拉最新版本号;本项目用 hardcode fallback 每次重新授权时刷新(简化运维)
  - Rust async/await 代替 Go goroutine
  - 静态 antigravity 模型种子内嵌成 `antigravity_static_models()`(crate 内)而非 model.json
- **同步策略**:
  - CLIProxyAPI 主线 commit 关注(尤其 Google 端协议更新)
  - Antigravity model 列表跟随 CLIProxyAPI `models.json` 同步
  - 任何 ClientMetadata 字段变更必须立刻同步,否则 Google 端可能拒绝
- **代码层引用**(节选):
  > //! 借鉴 [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI)
  > CLIProxyAPI `header_utils.go::DetectUserAgent` 一致(format ...)。
  > CLIProxyAPI 有 background goroutine 拉最新版本号,Rust 当前用 hardcode fallback,每次重新授权时刷新。
  > query 参数顺序对齐 CLIProxyAPI `auth/antigravity/auth.go:60-68`。

## chenyme/grok2api

- **Link**: https://github.com/chenyme/grok2api
- **License**: 见上游 LICENSE
- **借鉴形式**: 反向工程产物借鉴 + 算法借鉴 + 数据模式参照
- **首次借鉴 PR / 时间**: Grok Web 集成 PR(R1 Plan A,2026-05-12 加 grok-web preset)
- **借鉴清单**:
  - Grok Web endpoint 表 + SSE schema(闭源 web app 反向工程,`xai_chat.py`) → `crates/adapters/src/grok_web/types.rs`
  - dynamic statsig ID 生成算法(`app/dataplane/proxy/adapters/headers.py::_statsig_id`) → `crates/adapters/src/grok_web/auth.rs`(15+ 处明示 `chenyme` 锚点)
  - `sso={t}; sso-rw={t}` cookie 双写行为(用户只提供 sso 时自动双写) → `crates/adapters/src/grok_web/auth.rs`
  - tool_calls flatten 模式(v2.1.6 加,处理 Grok Web 多 tool 嵌套) → `crates/adapters/src/grok_web/request.rs`
  - 内置工具 emoji 图标映射(`xai_chat.py::_TOOL_FMT`) → `crates/adapters/src/grok_web/response.rs`
- **本项目差异 / 扩展**:
  - Rust 实现,statsig 算法去 Python 依赖
  - tool_calls flatten 在 v2.1.6 加(chenyme 早期版本无)
- **同步策略**:
  - Grok Web 上游协议变动时(grok.com 改 endpoint / SSE format / statsig 算法),关注 chenyme/grok2api 主线 commit
  - statsig 算法尤其关键 — 上游一改本项目立刻挂
- **TOS / 法律注意**: ⚠️ 反代 grok.com Web 端,grok TOS 灰色区。沿用 chenyme 立场:**仅限本机个人使用本机 SuperGrok 账号,不应作为对外服务发布**。Provider 表单 UI 同步显示该警告(`frontend/js/i18n.js` `grokWeb.tosWarning`)。
- **代码层引用**(节选):
  > 内置工具名 → emoji 图标(对照 chenyme `xai_chat.py::_TOOL_FMT`)。
  > chenyme/grok2api 反向工程产出借鉴(endpoint table + SSE schema)
  > **算法**(参考 chenyme/grok2api `app/dataplane/proxy/adapters/headers.py::_statsig_id`):...
  > chenyme `sso={t}; sso-rw={t}` 行为复刻:用户只提供 sso 时自动双写。

## galaxywk223/codex-plugin-unlocker

- **Link**: https://github.com/galaxywk223/codex-plugin-unlocker
- **License**: **MIT**(明确,本项目代码注释固化)
- **借鉴形式**: 算法整体借鉴 + 本地差异
- **首次借鉴 PR / 时间**: 2026-05-11(本项目 plugin unlock 集成 PR,见 git log `codex-plugin-unlocker`)
- **借鉴清单**:
  - 整套 Plugins 解锁注入脚本算法(上游 `packages/codex_plugin_unlocker/inject/plugin-unlock.js`) → `src-tauri/src/codex_plugin_unlocker.rs:389-541`(~150 行 JS in Rust raw string)
  - React Context.Provider 反查 `setAuthMethod`(沿 fiber.return 向上爬,找带 setAuthMethod + authMethod 字段的 Provider value) → 同上(line 392-433)
  - DOM 级 enable(清 disabled 属性 + `__reactProps.disabled`) → 同上(line 461-477)
  - MutationObserver 持续 enforce(防 SPA 路由 / sidebar 重渲冲掉) → 同上(line 532-537)
  - CDP `--remote-debugging-port=9222 --remote-allow-origins=*` flag(Chrome 111+ / Electron 同代起的硬性要求,见上游 `launcher.py:55-58`) → `src-tauri/src/admin/handlers/desktop.rs:320-329`
  - HTTP API 路由 → `src-tauri/src/admin/handlers/plugin_unlock.rs`
  - 启动时 auto unlock daemon(可由 `autoUnlockCodexPlugins` 关) → `src-tauri/src/main.rs:63-85`
- **本项目差异 / 扩展**:
  - **关键改造**:上游早期版本走 useState hook 链找 setter;Codex Desktop 26.513+ 之后 React state 结构改了导致 hook-scan 失效。本项目改走 React Context.Provider 反查,更稳定
  - 加 DOM-level strict fallback:即使 setter 找不到也能让按钮可点
  - inject script 内 short-circuit `if (auth.authMethod === 'chatgpt') return true;`(避免重复注入,但首次必然触发一次 — 历史 followup #27 已 resolved by PR #191,详情归档在本地 `docs/`)
- **同步策略**:
  - Codex Desktop 主线升级若让脚本失效,**优先**看 galaxywk223 主线是否有修
  - 若上游也跟进新版 Codex Desktop,本项目同步 inject script(注意保留本地 React Context 改造跟 DOM fallback)
- **关联 PR / followup**: PR #191 主修;长期"setAuthMethod 触发 AuthContext 重 mount"消除调研 → Linear [MOC-5](https://linear.app/mochance/issue/MOC-5)
- **代码层引用**(`codex_plugin_unlocker.rs:389-401` 节选):
  > 算法借鉴 galaxywk223/codex-plugin-unlocker (MIT, 2026-05-11)
  > https://github.com/galaxywk223/codex-plugin-unlocker/blob/main/codex_plugin_unlocker/inject/plugin-unlock.js
  > 关键差异 vs. 早期版本(找 useState hook 链上的 setAuthMethod setter):
  > - 新策略走 React Context — 从 plugin 入口 DOM 节点拿 fiber,沿 `fiber.return` 向上爬,检查每层 `memoizedProps.value` / `pendingProps.value`,找带 `setAuthMethod` + `authMethod` 字段的对象(即 `AuthContext.Provider` value)

## QwenLM/qwen-code

- **Link**: https://github.com/QwenLM/qwen-code
- **License**: **Apache-2.0**(明确,本项目代码注释固化)
- **借鉴形式**: 数据模式参照
- **首次借鉴 PR / 时间**: **PR #188**(2026-05-17,百炼 Token Plan 套餐适配 + 获取模型 fix)
- **借鉴清单**:
  - 阿里官方 Qwen CLI 对百炼 Token Plan 套餐的处理思路:**不调上游 list models API,直接静态硬编码模型清单**(因 Token Plan gateway 不暴露 `/models` endpoint,所有 unknown path 返 401) → `src-tauri/src/admin/handlers/providers/models.rs:380-394`
  - host 检测 helper(`token-plan.cn-beijing.maas.aliyuncs.com`) → `src-tauri/src/admin/handlers/providers/models.rs:71-82`
  - 模型清单 `TOKEN_PLAN_MODELS` 4 条(`qwen3.6-plus / deepseek-v3.2 / glm-5 / MiniMax-M2.5`,跟上游 `packages/cli/src/auth/providers/alibaba/tokenPlan.ts` 对齐) → 同上
- **本项目差异 / 扩展**:
  - 普通百炼(`dashscope.aliyuncs.com`)不命中此 short-circuit — 它的 `/compatible-mode/v1/models` 用户实测可用,继续走通用 HTTP probe
  - 跟现有 `gemini_cli_oauth` 的 short-circuit 同模式(两者上游都不暴露 list models)
- **同步策略**:
  - 上游 `tokenPlan.ts` `TOKEN_PLAN_MODELS` 数组变化(新增 / 退役模型)时,同步本项目硬编码列表 + 加单元测试 case
  - 关注阿里云 Token Plan 套餐文档新增 endpoint(若未来真支持 `/models`,可移除 short-circuit)
- **关联 PR / followup**: PR #188(主修),issue #187(根 issue)
- **代码层引用**(`models.rs:380-394` 节选):
  > **百炼 Token Plan 套餐** (`token-plan.cn-beijing.maas.aliyuncs.com`) 不暴露 `compatible-mode/v1/models` endpoint(网关在所有 unknown path 都返 401,routing 在 auth 之后)。阿里官方 Qwen CLI 自身就走静态硬编码 — 见 QwenLM/qwen-code `packages/cli/src/auth/providers/alibaba/tokenPlan.ts` 里 `TOKEN_PLAN_MODELS` 数组(Apache-2.0)。这里跟 Qwen CLI 的 canonical 4 条对齐。

## BigPizzaV3/CodexPlusPlus

- **Link**: https://github.com/BigPizzaV3/CodexPlusPlus
- **License**: **MIT**(明确,本项目代码注释固化)
- **借鉴形式**: 算法 1:1 复刻(Rust 翻译版)+ Wire-level 对齐(COM API 调用)
- **首次借鉴 PR / 时间**: PR #191(2026-05-17,Windows Plugin Unlock MSIX 实施)
- **借鉴清单**:
  - **`IApplicationActivationManager::ActivateApplication` Win32 COM 调用**(`launcher.py:347-395`,CLSID `45BA127D-...` + IID `2e941141-...` + vtable[3])→ `src-tauri/src/windows_msix.rs:50-96` `activate_packaged_app`(Rust 用 windows-rs 官方 binding 而非手搓 ctypes COM)
  - **AUMID 自动解析**(`launcher.py:298-304` + `app_paths.py:30-49`):`Get-AppxPackage` PowerShell 反推 `OpenAI.Codex_<publisher>!App` → `src-tauri/src/windows_msix.rs:111-137` `resolve_codex_aumid`
  - **cmdline 序列化**(`launcher.py:411` 用 `subprocess.list2cmdline`):MSIX activation 的 `arguments` 参数是单一 PWSTR 不是 argv 数组 → `src-tauri/src/windows_msix.rs:157-199` `list2cmdline` + `escape_cmdline`(Windows `CommandLineToArgvW` quoting 规则)
  - **Codex Desktop 启动入口 Windows 分支**:`open_codex_app` 走 COM activation 替代 `explorer.exe shell:AppsFolder\...`(后者剥 args)→ `src-tauri/src/admin/handlers/desktop.rs:335-405`
  - **端口冲突探测**(`launcher.py:267-281` `SO_EXCLUSIVEADDRUSE` socket 占位探测思路):9222 被占时 fallback OS 分配的随机空闲端口,daemon 通过 `CDP_PORT` atomic 读最新值 → `src-tauri/src/admin/handlers/desktop.rs::detect_free_cdp_port` + `src-tauri/src/codex_plugin_unlocker.rs::CDP_PORT` / `current_cdp_url`(issue #226 Task 1,PR 同期落地)
- **本项目差异 / 扩展**:
  - Rust 实现用 `windows` crate 0.59+ 官方 binding(`Win32_UI_Shell` feature)而非 Python ctypes 手搓 vtable
  - ActivateApplication 失败时 fallback 到老 `explorer.exe` 路径让 Codex 至少能启动(args 丢失,Plugin Unlock 在 fallback 路径下不工作,但 Codex 本身可用)
  - PowerShell CIM 进程清理已在 PR #201 实施(对照 `launcher.py:434-451`)
  - 端口探测 Rust 用 `std::net::TcpListener::bind` 而非 Python `SO_EXCLUSIVEADDRUSE` socket option — Tokio + std 在跨平台 bind 行为已足够区分占用,无需 Windows 专属 socket option(跨平台一致更易测)
  - 非-Store .exe fallback(Method 6)留作 issue #226 Task 2 后续 PR
- **同步策略**:
  - 上游 launcher.py COM 调用约定变动(Microsoft 更新 ActivateApplication 接口的可能性极低)→ 跟踪 windows-rs major version 升级
  - AUMID 解析逻辑跟随 Codex Desktop AppxManifest 命名约定;若 OpenAI 改 package family name 命名(如改 `OpenAI.CodexCLI`),`resolve_codex_aumid` 的 PowerShell `-Name 'OpenAI.Codex'` filter 需更新
- **关联 PR / followup**: PR #191(主修),历史 followup #33 已 resolved by PR #227(端口冲突探测)+ Task 2 非-Store .exe fallback dropped(issue #226 closed)
- **代码层引用**(`windows_msix.rs:18-25` 节选):
  > 实现路径 1:1 借鉴 `BigPizzaV3/CodexPlusPlus`(MIT,2699 stars)的 Python 实现 `codex_session_delete/launcher.py:283-451`(2026-05-17 同步)。同道项目实证可工作。本 Rust 实现用 `windows` crate 官方 binding 而非手搓 ctypes COM,稳定性更好。

## borawong/AiMaMi

- **Link**: https://github.com/borawong/AiMaMi
- **License**: **MIT**(明确,本项目 `managed_block.rs` 头注释固化)
- **借鉴形式**: 算法借鉴(受管块设计:marker 切分 + 六操作 + Protected mode;细节本地重写)
- **首次借鉴 PR / 时间**: PR #206(2026-05-20,Codex 资产管理 managed block + Agents tab MVP);Protected mode 后续 PR #229
- **借鉴清单**:
  - **受管块算法**(上游 `src-tauri/src/core/custom_instructions.rs:1-130`):注释 marker 把"app 受管区"与"用户手写区"物理隔离 + parse / preview / apply / rollback / clear / history 六操作 + Protected mode → 本项目 `src-tauri/src/admin/services/managed_block.rs`
- **本项目差异 / 扩展**:
  - marker 前缀改成 `cas:`(如 `cas:managed:agents:v1:start`)做项目隔离,避免与上游 / 其他工具 marker 冲突
  - 受管对象从单一 custom instructions 扩展到 `~/.codex/AGENTS.md` + `config.toml` MCP 段 + `skills/*/SKILL.md` 三类
- **关联 PR / followup / issue**: PR #206(主修)/ PR #229(Protected mode);issue #24 #25
- **代码层引用**(`managed_block.rs:1` 节选):
  > Codex 配置文件"受管块"管理 — 借鉴 borawong/AiMaMi(MIT):`src-tauri/src/core/custom_instructions.rs:1-130`。

## ryoppippi/ccusage

- **Link**: https://github.com/ryoppippi/ccusage
- **License**: **MIT**(Copyright 2025 ryoppippi)
- **借鉴形式**: 整体 vendor — Rust 源码直接复制到本仓 + namespace 重定向 + 删 CLI/terminal/output/blocks 等表现层
- **首次借鉴 PR / 时间**: PR #279(2026-05-26,Token 用量统计功能)
- **上游版本**: ccusage v20.0.5,upstream commit `2b9599ca`(2026-05-26 拉取)
- **借鉴清单**(全部位于本项目 `crates/usage_tracker/src/vendored_ccusage/`):
  - `codex/parser.rs` ← `rust/crates/ccusage/src/adapter/codex/parser.rs`(rollout JSONL line-by-line `memchr::memmem::Finder` fast-path 解析)
  - `codex/types.rs` ← `rust/crates/ccusage/src/adapter/codex/types.rs`(`CodexSessionLogEntry` / `CodexLogEntry` / `CodexPayload` / `CodexInfo` / `CodexModelMetadata` / `CodexResultFields` / `CodexTimestamp`)
  - `codex/paths.rs` ← `rust/crates/ccusage/src/adapter/codex/paths.rs`(`CODEX_HOME` env + 默认 `~/.codex/sessions/` 路径发现)
  - `types.rs` ← `rust/crates/ccusage/src/types.rs`(`CodexRawUsage` / `CodexTokenUsageEvent` / `TokenUsageRaw` / `TokenCounts` / `ModelBreakdown` / `CodexGroup` 等)
  - `fast.rs` ← `rust/crates/ccusage/src/fast.rs`(`FxHashMap` / `FxHashSet` / `ByteLines` 等基础 utility)
  - `home.rs` ← `rust/crates/ccusage/src/home.rs`(`home_dir`)
  - `date_utils.rs` ← `rust/crates/ccusage/src/date_utils.rs`(`TimestampMs` / `parse_ts_timestamp` / `format_date_tz` / `parse_tz` / `format_rfc3339_millis` 等)
  - `utils.rs` ← `rust/crates/ccusage/src/utils.rs`(`json_value_u64` / `total_usage_tokens` / `apply_total_token_fallback`)
  - `error.rs` ← `rust/crates/ccusage/src/main.rs:64-95`(`CliError` / `Result` / `cli_error` helper,ccusage 上游放 main.rs 顶层,本项目拎独立模块)
  - MIT LICENSE 副本 + 顶层 attribution 见 `crates/usage_tracker/src/vendored_ccusage/{LICENSE,mod.rs}`
- **本项目差异 / 扩展**:
  - **不 vendor**:`adapter/codex/{loader,aggregate}.rs`(CLI 耦合 `SharedArgs` / `progress`)、`summary.rs` / `blocks.rs` / `output.rs` / `cli.rs` / `main.rs` / `commands/` 等 CLI 表现层 → 本项目自写薄壳 `crates/usage_tracker/src/lib.rs` 替代,算法对照 ccusage `loader.rs` / `aggregate.rs` 1:1(参 lib.rs 顶部文档)
  - **Phase 1 不 vendor pricing / cost**:`pricing.rs` / `cost.rs` / `fast-multiplier-overrides.json` / LiteLLM JSON 嵌入留 Phase 2 加 token cost 显示时再 vendor
  - **不并行加载**:ccusage `loader.rs` 用 `thread::scope` 并行,本项目桌面端单 user 数据量小(~250 文件 1.2GB 实测串行 ~1-2s)走串行,降并发复杂度
  - **输出层全替**:ccusage 走 CLI stdout / terminal table,本项目走 Axum admin `/api/usage/summary` JSON + Tauri vanilla JS 前端 `<table>` 渲染
  - **可见性**:vendor 后所有 `pub(crate)` / `pub(super)` 统一改 `pub`,保 vendor 子模块对父 crate 可见
- **同步策略**:
  - 上游 codex adapter 主线修改(新 token 字段 / parser bug fix)→ 跟随 ccusage 版本号手动 sync,改动小直接 cherry-pick 对应文件
  - 上游 LiteLLM pricing snapshot 升级 → Phase 2 vendor pricing 后再考虑
  - 不自动跟随;固定 vendor commit hash 避免 break(参 `vendored_ccusage/mod.rs` 顶部 `Upstream commit:` 注释)
- **关联 PR / followup**: PR #279(主修),issue #279 + Linear MOC-15
- **代码层引用**(`crates/usage_tracker/src/lib.rs:5-12` 节选):
  > 借鉴自 ryoppippi/ccusage (MIT)。解析 + 数据类型 + paths 见 vendored_ccusage 模块,直接 vendor 自 ccusage `rust/crates/ccusage/src/adapter/codex/{parser,types,paths}.rs` 与同 crate `types.rs` / `fast.rs` / `home.rs` / `date_utils.rs` / `utils.rs`。本文件 loader + aggregator 算法 1:1 对照 ccusage `rust/crates/ccusage/src/adapter/codex/{loader.rs,aggregate.rs}`,但移除 CLI 层(`SharedArgs` / `progress::track_usage_load`)+ 不做并行。

## Cmochance/Codex_Account_Switch

- **Link**: https://github.com/Cmochance/Codex_Account_Switch
- **License**: 同作者自有项目(Cmochance);借鉴仍按上游惯例记录归属。
- **借鉴形式**: 算法借鉴(`codex login` 子进程调起 / 等待-取消模式)+ 数据模式参照(ChatGPT OAuth client_id / refresh endpoint / auth.json chatgpt 结构)+ 思路借鉴(`~/.codex` 之外维护持久镜像、Windows 进程辅助)
- **首次借鉴 PR / 时间**: 早期 Windows 进程辅助起继承(PR #191 期);MOC-104 真实账号 plugin 模式集中借鉴(PR #338,2026-05-31)
- **借鉴清单**:
  - **ChatGPT OAuth 常量 + refresh 请求格式**(上游 `src-tauri/shared/runtime/chatgpt_api.rs:96-108,~400-450`):issuer `https://auth.openai.com`、public client_id `app_EMoamEEZ73f0CkXaXp7hrann`、`POST /oauth/token` `grant_type=refresh_token` 表单格式 → `src-tauri/src/codex_real_account.rs`(`OPENAI_ISSUER` / `OPENAI_OAUTH_CLIENT_ID` / `refresh_if_needed`)
  - **chatgpt 模式 auth.json 结构**(`{auth_mode:"chatgpt", tokens:{access_token,refresh_token,id_token,account_id}, last_refresh}`)→ `codex_real_account.rs::parse_chatgpt_auth` / `apply_refresh_response`
  - **`codex login` 子进程调起 + 等待/取消模式**(上游 `mac/runtime/process.rs::run_codex_login` + `shared/runtime/login_cancel.rs::wait_for_login_or_cancel`)→ `codex_real_account.rs::{start_login,cancel_login,login_status}`(本项目用后台线程 reap + pid kill,不 1:1 复刻)
  - **`~/.codex` 之外维护持久镜像 + 失效恢复**思路(上游 account_backup 多账号目录 + switch overlay)→ `codex_real_account.rs::{imported_mirror_path,import_auth,reconcile_on_startup}`(本项目单账号镜像,不引多账号 switch 体系)
  - **Windows `hide_console_window`(`CREATE_NO_WINDOW` flag)**(上游 `src-tauri/win/runtime/process.rs::hide_console_window`)→ `src-tauri/src/admin/services/desktop/process.rs:175`
  - **OpenAI 官方 Windows Store 包 ID 对齐** → `src-tauri/src/admin/services/desktop/process.rs:16`
  - **纯配置写入模式(代理不参与转发)思路** → `src-tauri/src/admin/services/desktop/snapshot.rs:82`
  - **Windows 进程辅助参照** → `src-tauri/src/windows_msix.rs:235`
- **本项目差异 / 扩展**:
  - 不自建 OpenAI OAuth,登录复用官方 `codex login`(轻、稳、不怕 OpenAI 改 OAuth);上游也是调官方 login。
  - 单账号持久镜像 + 「活动文件失效才自动恢复」,不引入上游的多账号 switch / overlay / plan 查询体系。
  - token 刷新整个 exchange 在 `tokio::Mutex` 内串行,防 single-use refresh_token 并发双 POST。
- **同步策略**: client_id / refresh endpoint 跟随官方 codex 变化(上游同源);monitor only。
- **代码层引用**(`codex_real_account.rs` 节选):
  > ChatGPT desktop / Codex CLI 公开 OAuth client id(借鉴 Codex_Account_Switch chatgpt_api.rs:100,该处注明已对照官方 codex 与 codex-switcher 验证)。

---

## 维护规则

- **新增借鉴**:1 个 PR 内 ① README 致谢段加一行概览 ② 本文档加完整 entry(必填字段全),缺一不可
  - **概览长度硬约束**:README 致谢每行 " — " 之后的描述只写极简标签 —— `README.md` ≤ 20 字、`README.en.md` ≤ 40 字(均按 Unicode 码点计,中英文 / 标点 / 反引号 / 空格各算 1)。完整借鉴形式 / license / file:line / 路径一律放本文档,**不**塞进 README 概览。由 `scripts/check_acknowledgements.py` 在 CI(`docs-acknowledgements` job)强制,超标即 fail —— 写规则没用过(历史上新增条目反复超标),这条改成机器门禁
- **修改已有借鉴**:本项目代码 file:line 变了 → 同步 entry 的"借鉴清单";本项目差异扩展了 → 加到"本项目差异 / 扩展"
- **删除借鉴**(代码被重写不再依赖):entry 移到末尾 `## 已不再依赖的历史借鉴` section 保留追溯,**不直接删** — 历史归属信息必须可回溯
- **License 合规**:任何"算法 1:1 复刻 / 整体借鉴 / 数据模式参照 / Wire-level 对齐" 必须保留 license + 作者署名(代码注释 + 本文档双重记录)
- **上游主线大改 / 失效跟踪**:走 **Linear workspace `Mochance`**(team Mochance)开 followup issue(本文档只放当前状态 catalog,长跨度跟踪走 Linear)
