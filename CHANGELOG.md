# Changelog

逐版本要点。详细变更见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) 与 `release-notes/v*.md`。

## Unreleased — PR #153 draft

**Anthropic Messages 协议适配**:新增 canonical `apiFormat=anthropic_messages`,将 Codex CLI Responses 请求转换到 Anthropic `/v1/messages`,并把 Anthropic Messages SSE 还原为 Responses SSE。当前 PR 已覆盖 text、thinking、tool_use、tool_result repair、`previous_response_id`、compact response、upstream error、provider test/model list 与 UI 保存显示路径。

Claude preset 暂不开放:需要 P7 真实 Claude text、tool-call、`previous_response_id`、upstream error 验证通过后再加入默认 preset。

## v2.1.14 — 2026-05-23

**Codex 文档管理 4 子页完整重做**:Sidebar → Codex 整页改成 Agents / Memories / Skills / MCP 四 sub-tab,每个 sub-tab raw 模式编辑对应 codex 配置,SHA-256 hash 独立 history 互不交叉。

- **Agents**(PR #244):任意位置 `AGENTS.md` raw 全文 read/write + Tauri 文件系统选择;按 `.git/` 自动分类 project-root / subdir 显示 chip(`borawong/AiMaMi` 设计参考)
- **Memories**(PR #244):固定管理 `~/.codex/memories/MEMORY.md`(主索引)+ `memory_summary.md`(摘要) — 基于 codex `memories/` crate 调研结论:这两个 file 是 AI session 启动时实际注入 prompt 的 user-editable 索引,`raw_memories.md` / `rollout_summaries/` / `phase2_workspace_diff.md` 等是 codex 内部 Phase 1-2 自动管理,不暴露
- **Skills**(PR #245):扫 `~/.codex/skills/<name>/SKILL.md` 全列表 raw 编辑;"打开文件夹"按钮调系统 `open` / `xdg-open` / `explorer` 让用户在 Finder/资源管理器改 SKILL.md 之外的子文件(scripts / examples / templates)。codex 实际无静态 skill 索引文件(skill list runtime 进 prompt,见 codex `memories/read/src/usage.rs`),不引入虚拟"目录索引"条目
- **MCP**(PR #245):`toml_edit::DocumentMut` round-trip 解析 `~/.codex/config.toml`,只动 `[mcp_servers.*]` 节,保留注释 + decor + 其他配置节;前端 left list + right JSON read-only/textarea toggle,底部 2 按钮(新增 / 编辑);保留未建模字段(`tools` per-tool approval / `env_vars` / codex 未来新加字段)防 round-trip 数据丢失;Plugins 子页扫 `~/.codex/plugins/cache/<market>/<plugin>/<ver>/` 列已安装 plugin,enable toggle + uninstall 双确认。Marketplace + Deeplink(`codex-app-transfer://v1/import?...` URL scheme + confirmation modal)后端全栈实现,前端入口 followup #40 待 registry repo 起好再激活

**Devin pre-merge 安全/正确性修复**(本次共 13 项):tarball 60s timeout + Content-Length 预检 + streaming size cap 防 OOM;name/marketplace/version path-safety(`.` `..` 整字符串拒);uninstall 同等校验;restore 路径 atomic tmp+rename;upsert_server 保留未建模字段;tarball wrapper 同名子目录 collision FP 修复;`InstallInput` serde camelCase;modal 位置一致性等。

完整改动:[PR #244](https://github.com/Cmochance/codex-app-transfer/pull/244)(Agents/Memories)+ [PR #245](https://github.com/Cmochance/codex-app-transfer/pull/245)(Skills/MCP)。

## v2.1.13 — 2026-05-22

**`apply_patch` diff UI 在 chat-completions provider 上工作**(close #235):chat-completions provider(DeepSeek / Kimi / MiMo 等)上 Codex App 的 `apply_patch` 工具不渲染 diff UI 问题完整修复。

- wire 层 `custom_tool_call` SSE 桥接 + 多轮 `previous_response_id` 历史回放(PR #236)
- prompt 修复:V4A `@@` 单端语法 / 删除 EMPTY LINE anchor 误导 / 明示 MINIMAL Update form / Add File 全 `+` 前缀 / prefix 无空格 / `*** Begin Patch` literal 第一行 / Move + Update 必须 ≥1 hunk(纯重命名用 Delete + Add File 替代)(PR #236 + PR #240)
- prompt 强 normative:ALWAYS 用 `apply_patch` / NEVER 用 shell `>` redirect 写文件内容,全文 rewrite 同样走 `*** Delete File:` + `*** Add File:`(PR #241,用户实测反馈 184 行 README rewrite 模型走 `cat <<EOF >` 绕过 diff UI 引出)。配 `printf '\n' > <path>` seed 空文件 carve-out
- envelope `output[]` interrupted `apply_patch` status 跟流式 done event 一致(防 partial V4A 误执行,Devin pre-merge review BUG fix)
- guidance system message 仅 first turn 注入,防多轮累积污染上下文(Devin pre-merge review BUG fix)

真机三 provider 端到端验证:Kimi For Coding round 7 = 12/14 success / Xiaomi MiMo (Token Plan) round 8 = 用户反馈基本无问题 / DeepSeek V4 Pro round 9 = 9/9 = 100% success,reasoning 零 self-correction。

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

Python → Rust/Tauri 全栈重写,核心结论 + 量化对比见 [`release-notes/v2.0.0.md`](release-notes/v2.0.0.md)。重写过程 7 阶段 + 30+ 修订日志归档在维护者本地 `docs/`(`docs/` 已 gitignored,见 .gitignore Local-only docs 段)。

逐版本 release notes:[v2.0.0](release-notes/v2.0.0.md) / [v2.0.2](release-notes/v2.0.2.md) / [v2.0.3](release-notes/v2.0.3.md) / [v2.0.4](release-notes/v2.0.4.md) / [v2.0.5](release-notes/v2.0.5.md) / [v2.0.6](release-notes/v2.0.6.md) / [v2.0.7](release-notes/v2.0.7.md) / [v2.0.8](release-notes/v2.0.8.md)(无 v2.0.1 release notes — 跟随 v2.0.0 工程修订发布)。

## v1.0.x(Python,已归档)

Python + cryptography 验签时代,已被 v2.x Rust 主线全面取代,新装请直接用 v2.x。逐版本 release notes:[v1.0.0](release-notes/v1.0.0.md) / [v1.0.1](release-notes/v1.0.1.md) / [v1.0.2](release-notes/v1.0.2.md) / [v1.0.3](release-notes/v1.0.3.md)(v1.0.4 工程版本无独立 release notes,详见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases))。

---

> Followup backlog(跨 session 长期持有的研究 / refactor / 观测 tickets)在 **Linear workspace `Mochance`**(team Mochance,label `Improvement`)。历史 `docs/followup-tracker.md` + `docs/followup/` 详情已归档到维护者本地 `docs/`(gitignored)。
