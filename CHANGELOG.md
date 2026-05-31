# Changelog

逐版本要点。详细变更见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) 与 `release-notes/v*.md`。

## Unreleased

- **Plugins 注入强制关闭**(hotfix MOC-98):Codex Plugins 注入(plugin unlock daemon)临时硬禁用,无视用户配置 `autoUnlockCodexPlugins`,对所有用户(含此前显式开启的)强制 OFF。后端:daemon 永不自动启动、手动 `start` / `reinject` API 拒绝、重启 Codex 后不再触发 reinject、不再为 plugin_unlock 申请 CDP 调试端口(theme 注入仍独立可用);前端:设置页开关锁死 OFF + 禁用、「重启 Codex」按钮禁用、hint 标注已临时禁用。各改动点带 `强制关闭(hotfix MOC-98)` 注释便于后续恢复。

## v2.1.18 — 2026-05-31

**主题引擎模块化(5→11)+ Gemini 系一致性修复 + Windows 启动提速**:自 v2.1.17 起合入 14 个 PR。

- **Codex 主题引擎模块化**(MOC-97 #331):换肤引擎重写为「每主题调色板 + 共享结构」,内置主题 5 → 11(新增 `frost` / `nocturne` / `duet` / `rose` / `sonata` / `studio`),每套按背景图独立调出暗玻璃 + 强调色(不再统一红调)。注入改为覆盖 Codex 当前版本的设计令牌(`--color-token-*` + 运行时内联在 `<html>` 的 `--color-*` 层),修好设置页白卡、侧栏 resize 手柄亮带、折叠侧栏浮层透明、顶部内容阴影常驻、侧栏/主区接缝、composer 容器等各视图,改用轻量 6px 玻璃模糊,`carton` 浮动看板娘保留;配合 Win/Linux 重启 Codex 后自动重新应用主题(MOC-73 #315)
- **Gemini / antigravity 系一致性**:传输指纹层对齐官方客户端(MOC-59 #310)、模型列表补 `displayName` + recommended 排序 + 过滤两款 claude(MOC-69 #316)、`apply_patch` 等 freeform 工具修复(请求补 input + 响应 `custom_tool_call`,MOC-75 #314)、上游非 2xx 对齐 Codex 重试白名单,永久错误 surface 不卡死(MOC-79 #325)
- **compact 跨协议支持**:compact 注入 disable-thinking 时同步删 `reasoning_effort`(MOC-87 #327)、compact 支持 Gemini 系(antigravity / Google AI Studio)(MOC-92 #328)
- **`apply_patch` 健壮性**:chat-path 截断检测 + V4A 后验校验(MOC-57 #322)
- **Windows 原生进程操作**:Codex 启动提速(原生进程枚举替 `tasklist` + AUMID 缓存,MOC-94 #329)、退出改用原生 `PostMessage(WM_CLOSE)` 替 PowerShell-WMI(MOC-95 #330)
- **设置 / 配置**:设置页加开关隐藏灰色(TOS-gray)provider preset、默认隐藏(MOC-91 #326),反馈批处理(语言持久化 MOC-70 + baseUrl endpoint 去重 MOC-72 + FAQ,#313),直连模式只写上游配置、不注入 transfer 私货(#318)

完整改动:[v2.1.17...v2.1.18](https://github.com/Cmochance/codex-app-transfer/compare/v2.1.17...v2.1.18)。

## v2.1.17 — 2026-05-29

**tool_search 工具链打通 + MCP 授权可移植保险箱 + Usage 命中率 + Code Graph + 稳定性修复**:自 v2.1.16 起合入 16 个 PR。

- **`tool_search` 工具链全链路**(#289 / #290 / #291 / #293 / MOC-48 #296):Codex 0.130+ 把 server-side MCP 工具 defer 到 `tool_search`、不再直接进 `tools[]`,代理此前会 silently drop;现 chat 路径打通(`tool_search_output` 发现工具 → 注入 chat `tools[]` → 按 `namespace` 路由回上游),新增 dropped-tools 计数器 dashboard + observability,README 兼容矩阵补说明
- **MCP 授权可移植保险箱**(MOC-62 #307,默认开):Codex MCP OAuth 凭据改存可移植文件 `~/.codex/.credentials.json`(0o600)+ 在 `~/.codex` 之外维护镜像;整个凭据文件被切账号 / 误删 / 换机清掉时,下次启动弹确认从备份恢复,单个 server 的主动登出不复活;不解决 OAuth 自然过期,token 明文落盘(0o600)
- **Usage 缓存命中率**(#305):按对话视图显示缓存命中率 + 逐轮命中率直方图;proxy 本地记录 `session → 真实上游模型`,模型列显示真实上游模型而非 `gpt-5.x` 占位
- **Code Graph 自动生成 + Pages 部署**(MOC-52 #298 / #300 / #297):`cargo metadata` 生成交互式 crate 依赖图,GitHub Actions 直接部署 Pages(无 gh-pages 分支、不提交 main)
- **稳定性修复**:`apply_patch` chat 长文档信封修复(#303)、启动防白屏 try-catch(#257)、`model_catalog.json` 自动同步(#266 / #287)、桌面宠物开关真正关闭(MOC-34 #286)、残留扫描启动竞态 + 致谢长度 CI 门禁 + 活跃度图单点态(MOC-54 #306)、fetch 失败修复(#285)

完整改动:[v2.1.16...v2.1.17](https://github.com/Cmochance/codex-app-transfer/compare/v2.1.16...v2.1.17)。

## v2.1.16 — 2026-05-26

**Token 用量统计 + 启用按钮重启解耦**:新增 Usage tab 展示对话 token 用量,并把启用按钮跟重启 Codex Desktop 解耦。

- **Usage tab**(MOC-15 / PR #280):sidebar 第 4 个入口,4 张顶部 KPI 卡 + 三视图(按日 / 按模型 / 按对话),ccusage 同款表格形态;用量解析层 vendor 自 [ryoppippi/ccusage](https://github.com/ryoppippi/ccusage)(MIT)
- **解耦启用与重启 Codex**(MOC-20 / PR #282):Apply 现在只写配置 + toast,不再强制弹重启 modal,避免误点重启杀 Codex 进程丢对话上下文 / 草稿 / 思考

完整改动:[v2.1.15...v2.1.16](https://github.com/Cmochance/codex-app-transfer/compare/v2.1.15...v2.1.16)。

## v2.1.15 — 2026-05-26

**Codex Desktop UX 集成 + 通用 provider 修复综合更新**:本版主要把 transfer 跟 Codex Desktop 的集成面继续做深(主题 / context 圆环 / system prompts i18n / plugin-unlock 强化),同时收掉一批 per-provider reasoning / autocompact 真机暴露的 bug。

- **Codex Desktop 主题页**(PR #265 / issue #264):Sidebar 加 Theme 页,内置 5 套主题(`carton` 带浮动看板娘 + `changli` / `azurlane` / `nailin` / `zani` 单背景),通过 CDP `Page.addScriptToEvaluateOnNewDocument` 一次注入持久(无 daemon)。资源 `include_bytes!` 嵌进 binary(~5MB)。跟 Plugin Unlock 完全独立 toggle(默认关)。支持 user 上传自定义主题(1:1 crop)+ 隐藏 / 删除 + 缩略图实拍(GaussianBlur 防隐私)
- **system prompts 跟随 transfer 语言**(PR #263 / issue #262):注入 Codex 的 system prompts 改读 transfer UI 语言设置,中文 UI 下 Codex 不再固定英文回复
- **Codex Desktop context 圆环**(PR #261 / issue #258):transfer 管理 context 使用率 atom,展示进度环 + 阈值告警 settings
- **CAT_SKIP_MODEL_PROVIDER_WRITE env**(PR #260):配 verify 环境跳过 `model_provider` 字段写入,验证 Codex 自己持久化时不被 transfer 反复覆盖
- **plugin-unlock 注入失败原因分流 + 15s 重试 + 心跳回收**(PR #255 / issue #253):macOS 改用 `--remote-debugging-port=0` + 异步 poll `DevToolsActivePort`(借 codex-theme launcher 同款模式),消除原 `try_bind` 预检与 Chromium bind 之间的 race window
- **Per-provider `reasoning_effort` 策略**(PR #256 / issue #254):新建 `crates/registry/src/reasoning_effort_policy.rs` 注册表,DeepSeek 真实 xhigh→max 到达;Kimi/GLM/MiMo/MiniMax/Qwen 不传该字段(LiteLLM 白名单实证不承认);自定义 provider 保守 fallback。provider 识别改用 `id` / `name` / `base_url` substring(跟 `provider_looks_like` 同范式),修 healing UUID 让 precise id 匹配永远不命中导致整修复失效的真机 bug;补阿里云百炼 `maas.aliyuncs` / `百炼` needle
- **GLM-5.1 autocompact**(PR #250 / issue #248):新建 model 级 `compact_thinking_policy` 注册表
- **docs/ 整目录 gitignored + followup 迁 Linear**(PR #252):内部计划文档不入仓,跨 session followup 改 Linear (MOC-N) 跟踪

完整改动:[v2.1.14...v2.1.15](https://github.com/Cmochance/codex-app-transfer/compare/v2.1.14...v2.1.15)。

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
