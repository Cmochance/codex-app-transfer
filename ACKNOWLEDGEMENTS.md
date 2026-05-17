# 致谢与上游借鉴索引

本文档是上游借鉴的 **catalog**,目的:**方便定位、索引、更新**。

- **定位**: 看到 codebase 某段代码 → 通过本文档反查上游来源
- **索引**: 新增上游借鉴时按 schema 加 1 个 entry,GitHub 自动生成 TOC
- **更新**: 上游有重要变更时定位本项目对应 file:line 同步

> README.md / README.en.md 致谢段只列一句话概览;详情全部在本文档维护。
> 新增借鉴 = 同 PR 内 README 加一行 + 本文档加 1 个 entry。

## 借鉴形式术语

| 形式 | 含义 |
|---|---|
| 算法 1:1 复刻 | 上游核心逻辑 byte-for-byte 或语义级移植 |
| 数据模式参照 | 上游数据结构 / 静态注册表 / 常量原样镜像 |
| Wire-level 对齐 | HTTP/SSE 协议字节级行为复现 |
| 反向工程产物借鉴 | 上游对闭源 API 的反向工程结论直接复用 |
| 算法借鉴 | 上游 idea 借走,细节自行实现 |
| Prompt 蓝本 | 上游 prompt 文本作为骨架 |
| 思路 / 模式借鉴 | 设计思路启发,无代码复用 |
| 配置迁移参照 | 因历史 fork 需对齐旧字段命名 |
| 产品形态启发 | 概念/UX 启发,无代码层借鉴 |
| 架构基座 | 框架依赖,非严格"借鉴" |

---

## farion1231/cc-switch

- **Link**: https://github.com/farion1231/cc-switch
- **形式**: 产品形态启发
- **借鉴清单**:
  - provider switching 范式(把 ~/.codex 多账号/多 provider 切换抽象成桌面 first-class) → 整个 v1.x→v2.x provider 管理 UX(无代码锚点)

## lonr-6/cc-desktop-switch

- **Link**: https://github.com/lonr-6/cc-desktop-switch
- **形式**: 早期 fork 演化基础 + 配置迁移参照
- **借鉴清单**:
  - v1.x 桌面壳骨架 + README 结构 → 早期产品形态(无代码锚点,v2 重写时基本替换)
  - 历史 `updateUrl` 默认值(指向 lonr-6 fork) → `crates/registry/src/healing.rs` `LEGACY_OWNERS` 常量自愈

## BerriAI/litellm

- **Link**: https://github.com/BerriAI/litellm
- **形式**: 数据模式参照 + 算法 1:1 复刻 + 思路借鉴
- **借鉴清单**:
  - `response.in_progress` SSE 事件生成时机 → `crates/adapters/src/responses/converter.rs:236-254`
  - usage 字段规范化(`_transform_chat_completion_usage_to_responses_usage`) → `crates/adapters/src/responses/converter.rs`(grep `litellm` 多处)
  - Vertex AI TypedDict 1:1 镜像 → `crates/adapters/src/gemini_native/types.rs`(顶部注释明示)
  - tool result 配对修复(防 Anthropic 400) → `crates/adapters/src/responses/request.rs`
- **同步策略**: litellm 主线 issue 关注;类型镜像变更时手动 diff 同步

## tauri-apps/tauri

- **Link**: https://tauri.app · https://github.com/tauri-apps/tauri
- **形式**: 架构基座
- **借鉴清单**:
  - Tauri v2 + 自定义 `cas://localhost/` 协议 → `src-tauri/src/main.rs`(`register_asynchronous_uri_scheme_protocol`)
  - `tauri-plugin-single-instance` / `tauri-plugin-shell` → `src-tauri/Cargo.toml`

## Piebald-AI/claude-code-system-prompts

- **Link**: https://github.com/Piebald-AI/claude-code-system-prompts
- **形式**: Prompt 蓝本(精简移植)
- **借鉴清单**:
  - 9-section autocompact prompt(`agent-prompt-conversation-summarization.md` 反编译公开版) → `crates/adapters/src/responses/compact.rs:42-70`(v2.0.12 从原 86 字符 prompt 升级)

## 7as0nch/mimo2codex

- **Link**: https://github.com/7as0nch/mimo2codex
- **形式**: 算法 1:1 复刻 + 思路借鉴
- **借鉴清单**:
  - `buildResponseSnapshot` SSE 响应快照算法 → `crates/adapters/src/responses/converter.rs`(grep `mimo2codex` 10+ 处)
  - `sequence_number` 单调递增(`state.nextSeq()`) → 同上
  - annotation 解析与映射 → 同上(跨所有 provider 复用)
  - `warnOnce` 全局去重日志策略 → `crates/adapters/src/lib.rs`
- **同步策略**: mimo2codex 主线变动手动 diff 关键 SSE 行为

## router-for-me/CLIProxyAPI

- **Link**: https://github.com/router-for-me/CLIProxyAPI
- **形式**: Wire-level 对齐 + 数据模式参照
- **借鉴清单**:
  - Gemini CLI / Antigravity OAuth ClientMetadata / UA / version 常量 → `crates/gemini_oauth/src/constants.rs`(多处明示)
  - OAuth callback query 参数顺序(必须跟上游一致让 Google 端识别) → `crates/gemini_oauth/src/antigravity/flow.rs`
  - Code Assist 模型清单(交集对齐 gemini-cli upstream + CLIProxyAPI `internal/registry/models/models.json`) → `src-tauri/src/admin/handlers/providers/models.rs:336-372`
  - Antigravity `:fetchAvailableModels` 调用模式 + 静态种子 fallback → `src-tauri/src/admin/handlers/providers/models.rs:268-333`
- **同步策略**: Google 端协议变动时 CLIProxyAPI 一般先跟,本项目对照其 commit 同步常量

## chenyme/grok2api

- **Link**: https://github.com/chenyme/grok2api
- **形式**: 反向工程产物借鉴 + 算法借鉴 + 数据模式参照
- **借鉴清单**:
  - Grok Web endpoint 表 + SSE schema(闭源 web app 反向工程) → `crates/adapters/src/grok_web/types.rs`
  - dynamic statsig ID 生成算法 → `crates/adapters/src/grok_web/auth.rs`(grep `chenyme` 15+ 处)
  - `sso={t}; sso-rw={t}` cookie 双写 → `crates/adapters/src/grok_web/auth.rs`
  - tool_calls flatten 模式(v2.1.6) → `crates/adapters/src/grok_web/request.rs`
  - 内置工具 emoji 映射(`_TOOL_FMT`) → `crates/adapters/src/grok_web/response.rs`
- **注意**: TOS 灰色区,沿用上游"仅本机个人使用"立场,不提供托管服务

## galaxywk223/codex-plugin-unlocker

- **Link**: https://github.com/galaxywk223/codex-plugin-unlocker
- **License**: MIT(明确标注)
- **形式**: 算法整体借鉴 + 本地差异
- **借鉴清单**:
  - 整套 Plugins 解锁注入脚本算法(`packages/codex_plugin_unlocker/inject/plugin-unlock.js`) → `src-tauri/src/codex_plugin_unlocker.rs:389-541`
  - React Context.Provider 反查 `setAuthMethod`(沿 fiber.return 向上爬) → 同上
  - DOM 级 enable(disabled + `__reactProps.disabled`) → 同上
  - MutationObserver 持续 enforce(防 SPA 重渲冲掉) → 同上
  - CDP `--remote-debugging-port=9222 --remote-allow-origins=*` flag → `src-tauri/src/admin/handlers/desktop.rs:320-329`
- **本项目差异**:
  - 上游早期版本走 useState hook;Codex Desktop 26.513+ 失效。本项目改走 React Context 反查更稳
  - 加 DOM-level strict fallback(setter 找不到也能让按钮可点)
- **同步策略**: Codex Desktop 主线升级若让脚本失效,优先看 galaxywk223 主线是否有修

## QwenLM/qwen-code

- **Link**: https://github.com/QwenLM/qwen-code
- **License**: Apache-2.0
- **形式**: 数据模式参照
- **借鉴清单**:
  - 百炼 Token Plan 套餐处理思路:不调上游 list models API,静态硬编码模型清单(因 gateway 不暴露 `/models`) → `src-tauri/src/admin/handlers/providers/models.rs:380-394`
  - host 检测 helper(`token-plan.cn-beijing.maas.aliyuncs.com`) → `src-tauri/src/admin/handlers/providers/models.rs:71-82`
  - 模型清单 `TOKEN_PLAN_MODELS` 4 条(`qwen3.6-plus / deepseek-v3.2 / glm-5 / MiniMax-M2.5`) → 同上
- **同步策略**: 上游 `packages/cli/src/auth/providers/alibaba/tokenPlan.ts` `TOKEN_PLAN_MODELS` 变化时同步本项目硬编码列表 + 加测试 case

---

## 维护规则

- **新增**: 1 个 PR 内 ① README 致谢段加一行概览 ② 本文档加完整 entry(必含 Link / 形式 / 借鉴清单含 file:line),缺一不可
- **更新**: 上游有重要变化时,定位"借鉴清单"里的 file:line 同步;复杂变更走 docs/followup-tracker.md
- **删除**: 代码被重写不再依赖时,本文档 entry 移到末尾 `## 已不再依赖` 段保留追溯,不直接删 — 历史归属必须可回溯
- **License 合规**: 算法 1:1 复刻 / 整体借鉴必须保留 license + 作者署名;思路借鉴文档致谢即可
