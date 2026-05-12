# Commit Hash Rewrite Map — 2026-05-13

2026-05-13 跑了一次 `git filter-repo` rewrite,去除所有 `Co-Authored-By` trailer(AI 工具 Claude / Cursor / Copilot 等被 GitHub Contributors graph 误算贡献者)。本次 rewrite 后 489 个 commit 都换了新 hash;**PR 编号 / URL / description / 评论 / discussion 完全保留**,仅老 commit hash 跳转链接失效。

## 备份

rewrite 前的 origin/main HEAD 被保留在分支 [`main-backup-before-rewrite-2026-05-13`](https://github.com/Cmochance/codex-app-transfer/tree/main-backup-before-rewrite-2026-05-13)(指向 `c62d03d`),作灾难回滚兜底。

## 用法

- 在老 commit message / issue 评论 / 外部 blog 看到老 hash → 在下表第 3 列查到 → 第 4 列拿新 hash → `git show <new-hash>` 看实际内容
- 老 PR 页面 "Commits" 标签里的失效链接 → 表格第 4 列直接拿新 commit URL

## PR ↔ Commit Hash 映射(按 PR 倒序,共 142 项)

| PR | 标题 | 老 commit | 新 commit |
|----|------|-----------|-----------|
| [#149](https://github.com/Cmochance/codex-app-transfer/pull/149) | docs(readme): 中文优先重构 + 拆 CHANGELOG + 加 README.en.md | `c62d03dbb49b` | `fbe55fc7673e` |
| [#148](https://github.com/Cmochance/codex-app-transfer/pull/148) | Fix compact context budgeting | `90c52800fc65` | `188372422eb2` |
| [#147](https://github.com/Cmochance/codex-app-transfer/pull/147) | docs: 新增 followup-tracker.md 项目级长期 backlog | `9b28c8a1bfb7` | `d19444be8fa8` |
| [#146](https://github.com/Cmochance/codex-app-transfer/pull/146) | fix(cloud_code): prod 路径漏配 session_cache 导致多轮历史 silent loss | `aff8ce613b63` | `ebcbde5a1ce9` |
| [#145](https://github.com/Cmochance/codex-app-transfer/pull/145) | fix(core/input): previous_response_id 存在但 session_cache=None 时 surf… | `ec22103d6369` | `47e383d8f97b` |
| [#144](https://github.com/Cmochance/codex-app-transfer/pull/144) | refactor(grok_web): session_cache 改必填参数 + 删 fallback foot-gun | `47b305770a39` | `fc2fe8a29ada` |
| [#143](https://github.com/Cmochance/codex-app-transfer/pull/143) | fix(session): L2 sqlite 失败 surface tracing::warn + stable error_id | `abf8f5564d9e` | `5127b38371c0` |
| [#142](https://github.com/Cmochance/codex-app-transfer/pull/142) | fix(grok_web): assistant.tool_calls flatten 渲染防 orphan Tool Result | `1b0813d506d1` | `e52ed84da86a` |
| [#141](https://github.com/Cmochance/codex-app-transfer/pull/141) | fix(minimax): role=system 转 user + [System]\n prefix (close #139) | `ff905bc33d29` | `723435d57da9` |
| [#140](https://github.com/Cmochance/codex-app-transfer/pull/140) | feat(grok_web): 多轮上下文 + autocompact 接入 core::input + session_cache | `84b67694e0eb` | `b3d814ad7e48` |
| [#138](https://github.com/Cmochance/codex-app-transfer/pull/138) | fix(grok_web): emit_event 注入 sequence_number,修 reasoning/streaming … | `72f454bbd33d` | `2dd2eb2bd67c` |
| [#137](https://github.com/Cmochance/codex-app-transfer/pull/137) | feat(grok_web): strip <grok:render> + cardAttachment → markdown(tas… | `41d4e3d4a26d` | `9933ebbe5ad1` |
| [#136](https://github.com/Cmochance/codex-app-transfer/pull/136) | feat(grok_web): Plan A UI 极简 + dynamic statsig + builtin preset 卡片 | `ae3b83482a4d` | `3c7f0049a815` |
| [#135](https://github.com/Cmochance/codex-app-transfer/pull/135) | feat(grok_web): R1 PR-6 — connector/collection/rag search 帧累积 | `fa562af2c230` | `d3afde6bb678` |
| [#134](https://github.com/Cmochance/codex-app-transfer/pull/134) | feat(grok_web): R1 PR-5 — codeExecutionResult → reasoning markdown … | `852eb66ac3f1` | `1b1acec9e891` |
| [#133](https://github.com/Cmochance/codex-app-transfer/pull/133) | feat(grok_web): R1 PR-4 cleanup — TD2 newtypes + H1 HeaderValue pro… | `aa4e785402fd` | `454077056e95` |
| [#132](https://github.com/Cmochance/codex-app-transfer/pull/132) | feat(grok_web): R1 PR-3 — message item lifecycle + url_citation ann… | `f5e41263e27d` | `2ba355d23221` |
| [#131](https://github.com/Cmochance/codex-app-transfer/pull/131) | feat(grok_web): R1 PR-2 — tool_usage_card / raw_function_result → r… | `4b252597b5c2` | `bf7260341a7b` |
| [#130](https://github.com/Cmochance/codex-app-transfer/pull/130) | feat(grok_web): R1 PR-1 — LRU ParentResponseTracker + thinking → re… | `903f31ae54b9` | `c9b2de31104f` |
| [#129](https://github.com/Cmochance/codex-app-transfer/pull/129) | feat(adapters): grok_web R3 PoC — apiFormat=grok_web + authScheme=g… | `991d7b3002fc` | `f444fc265c0d` |
| [#128](https://github.com/Cmochance/codex-app-transfer/pull/128) | refactor(adapters): 完成 P3 路由规则与 mapper 边界收敛 | `a08e17f06d97` | `dbf173a15008` |
| [#127](https://github.com/Cmochance/codex-app-transfer/pull/127) | refactor(gemini_native): 复用 responses chat-body 骨架收敛 normalized 管道 | `42c0d0bf60bf` | `9c75962bb1d0` |
| [#126](https://github.com/Cmochance/codex-app-transfer/pull/126) | refactor(gemini_native): 复用 responses 输入主管道收敛 tool_call_cache 接线 | `820fc2790940` | `cab1b60e2e0a` |
| [#125](https://github.com/Cmochance/codex-app-transfer/pull/125) | refactor(adapters): 抽取 P2 输入侧 previous_response 恢复公共层 | `3322e3d4ed4b` | `570047ee5532` |
| [#124](https://github.com/Cmochance/codex-app-transfer/pull/124) | refactor(adapters): 抽取 responses 事件公共层并落地 Phase 1 RFC | `639fc1e4c3dc` | `20a4d02b22a3` |
| [#123](https://github.com/Cmochance/codex-app-transfer/pull/123) | feat(gemini_native): 对齐 responses 续话与 compact 流程 | `caf73bd18a74` | `258cdf70e79c` |
| [#122](https://github.com/Cmochance/codex-app-transfer/pull/122) | revert(proxy): 移除 wire-dump 诊断 patch(本地诊断结束) | `049172b8814e` | `55819f057fee` |
| [#121](https://github.com/Cmochance/codex-app-transfer/pull/121) | feat(proxy): 加 success 路径 wire-dump 诊断 patch(env flag 控制) | `fc0f71cea350` | `fd0fbcc118bd` |
| [#120](https://github.com/Cmochance/codex-app-transfer/pull/120) | refactor(gemini_native): 对齐 cliproxy 移除软约束注入,统一 drop 冲突 wire 字段 | `df11cea9d24e` | `57a7d94fbd65` |
| [#119](https://github.com/Cmochance/codex-app-transfer/pull/119) | fix(gemini_native): 软约束追加语言守恒指令防止中文 prompt 回复变英文 | `8856d6d2e0a9` | `915e34a0bd1a` |
| [#118](https://github.com/Cmochance/codex-app-transfer/pull/118) | feat(ui): Provider 表单新增 Gemini soft constraints 配置 | `8a8418b361b8` | `e77097a32993` |
| [#117](https://github.com/Cmochance/codex-app-transfer/pull/117) | fix(gemini_native): 默认改 minimal 软约束并支持 off/strict 配置 | `abb08f5203be` | `5cb2f07f5f32` |
| [#116](https://github.com/Cmochance/codex-app-transfer/pull/116) | refactor: 3 处 home 解析收敛到 registry::paths::resolve_home | `30d56912dc8c` | `fbddca0b752d` |
| [#115](https://github.com/Cmochance/codex-app-transfer/pull/115) | fix(gemini_oauth): TokenStore 加 USERPROFILE 回退,修 Windows 状态加载失败 | `12aba7cdcd18` | `4d85c44060df` |
| [#114](https://github.com/Cmochance/codex-app-transfer/pull/114) | feat(frontend/antigravity): 加 Antigravity OAuth provider UI(paralle… | `d301ad16473d` | `e2c50dc817a9` |
| [#113](https://github.com/Cmochance/codex-app-transfer/pull/113) | feat(antigravity): 加 Google Antigravity OAuth provider 后端(复刻 Gemini… | `a9d96d9f27fa` | `8b64b381b54b` |
| [#112](https://github.com/Cmochance/codex-app-transfer/pull/112) | docs(readme): 致谢段每条压缩到 ≤ 20 字符 | `8241f2cad58e` | `cba465796755` |
| [#111](https://github.com/Cmochance/codex-app-transfer/pull/111) | release: v2.1.5 — Gemini CLI OAuth UI 精修 + email userinfo + 后端硬化收官 | `212851ed6044` | `a871df04655d` |
| [#110](https://github.com/Cmochance/codex-app-transfer/pull/110) | fix(frontend/i18n): HTML data-i18n-pending + inline CSS 消除单帧 fallba… | `71156431755a` | `9a2ddfd4a3fc` |
| [#109](https://github.com/Cmochance/codex-app-transfer/pull/109) | feat(admin/registry_io): cross-process file lock 防多 .app 实例 RMW race | `3399f5932fb7` | `5713e7a4323a` |
| [#108](https://github.com/Cmochance/codex-app-transfer/pull/108) | feat(admin/registry_io): with_config_write reentrant detection (sil… | `9048702423bf` | `84e60256bf85` |
| [#107](https://github.com/Cmochance/codex-app-transfer/pull/107) | chore(admin/registry_io): save() 改 module-private 防新 callsite 误用 ra… | `2de159e29d26` | `34b37afa3612` |
| [#106](https://github.com/Cmochance/codex-app-transfer/pull/106) | refactor(admin): 全栈迁移 11 个 raw load+save callsite → with_config_wri… | `67565b3b446d` | `b9ed9dbf4e65` |
| [#105](https://github.com/Cmochance/codex-app-transfer/pull/105) | fix(gemini-cli-oauth): app exit 等 in-flight OAuth task 真退出 (PR #100… | `67a76b7170a6` | `e69288087964` |
| [#104](https://github.com/Cmochance/codex-app-transfer/pull/104) | fix(gemini-cli-oauth): cancel response 区分 poison-recovery + preempt… | `7fa9c1d4b932` | `c27399984020` |
| [#103](https://github.com/Cmochance/codex-app-transfer/pull/103) | fix(gemini-cli-oauth): cancel 贯穿整 login pipeline (oneshot → watch::… | `4009562552c6` | `57ec0a82099e` |
| [#102](https://github.com/Cmochance/codex-app-transfer/pull/102) | refactor(frontend/i18n): formatI18n → tFmt 统一 + 启动时序消除空窗乱码 (PR #97 … | `8a11f1bb59ee` | `298b2eee77dd` |
| [#101](https://github.com/Cmochance/codex-app-transfer/pull/101) | perf(admin/oauth): shared OAuth HTTP client OnceLock pooled (PR #97… | `a9453d7c32c9` | `20c49d779d08` |
| [#100](https://github.com/Cmochance/codex-app-transfer/pull/100) | feat(gemini-cli-oauth): OAuth login cancellation token + 抢占 + app e… | `2c3a70f94a6e` | `4f2329da8928` |
| [#99](https://github.com/Cmochance/codex-app-transfer/pull/99) | refactor(admin/registry_io): atomic with_config_write + 迁 OAuth 防 R… | `f12812a84e83` | `ea403053a82a` |
| [#98](https://github.com/Cmochance/codex-app-transfer/pull/98) | chore: consolidate AI Git authors under Cmochance via .mailmap | `3abefefc9c25` | `aedb01398d7b` |
| [#97](https://github.com/Cmochance/codex-app-transfer/pull/97) | feat(gemini-cli): OAuth 直连(impersonate gemini-cli)+ Cloud Code Assi… | `f67c68c0d9ee` | `0c9c88f95ff9` |
| [#96](https://github.com/Cmochance/codex-app-transfer/pull/96) | feat(feedback): auto diagnostics bundle and contact email | `915e276212a4` | `47f096cd00b2` |
| [#94](https://github.com/Cmochance/codex-app-transfer/pull/94) | feat: Gemini 专门适配 + Web Search 配置开关 + 测速 fix(hold,等抓包) | `633541987f6d` | `d7d1b9c9779e` |
| [#93](https://github.com/Cmochance/codex-app-transfer/pull/93) | feat: 加 Google AI Studio (Gemini 3.x) builtin preset 卡片 | `4fb326418e49` | `290cf077b6ae` |
| [#92](https://github.com/Cmochance/codex-app-transfer/pull/92) | fix(test): 测速 401/403 改绿色 + 文案明示"连接 OK,鉴权未验证"(保持 v2.1.3) | `fa8ad44ffd9d` | `1bd6ba03a471` |
| [#91](https://github.com/Cmochance/codex-app-transfer/pull/91) | release: v2.1.3 prep — bump src-tauri 版本号 + README rollup 补 v2.1.3 … | `3bfdc48e0f04` | `9b7cd5c127b5` |
| [#90](https://github.com/Cmochance/codex-app-transfer/pull/90) | feat(telemetry): tracing → proxy_telemetry.logs 全局桥接(根治 workspace 5… | `215522bb9526` | `9adedd86c77a` |
| [#89](https://github.com/Cmochance/codex-app-transfer/pull/89) | fix(followup): H1 测速文案分级 + H2 forward 错误处理 + M2/M3 子分类 | `8c48e2808836` | `3929bac12bef` |
| [#88](https://github.com/Cmochance/codex-app-transfer/pull/88) | feat(adapters): ResponsesPassthroughAdapter 字节级透传 OpenAI Responses … | `9e5eee3f4bbd` | `462350eed55d` |
| [#87](https://github.com/Cmochance/codex-app-transfer/pull/87) | chore(frontend): 协议类型 UI 改 readonly display(后端字段保留,留 hook 给后续 respo… | `6b6fc5f5fe9f` | `f6f3c723b274` |
| [#86](https://github.com/Cmochance/codex-app-transfer/pull/86) | docs(readme): 致谢/贡献者紧凑 + 删 v1.0.4 一致表述 + v2.x 段拆分 | `ae6e14308dbe` | `38df6b36e603` |
| [#85](https://github.com/Cmochance/codex-app-transfer/pull/85) | release: v2.1.2 prep — bump 版本号 + README rollup 补 P6 web_search 全栈 | `e76451b311b2` | `d02d1f64ba3f` |
| [#84](https://github.com/Cmochance/codex-app-transfer/pull/84) | docs(web-search): Qwen/GLM 实证阻断,暂停留 follow-up(P6 4 家完成收尾) | `80f7d9fa4984` | `9d2ff21b21ce` |
| [#83](https://github.com/Cmochance/codex-app-transfer/pull/83) | feat: MiniMax builtin preset + 官方图标 + web_search 显式 drop | `f67a35fc45f6` | `3e1c08c4dfef` |
| [#82](https://github.com/Cmochance/codex-app-transfer/pull/82) | feat(adapters): DeepSeek web_search 显式 drop(文档实证 API 不支持) | `8c93a99ca60b` | `3e5dc6ed3d2e` |
| [#81](https://github.com/Cmochance/codex-app-transfer/pull/81) | feat(adapters): Kimi web_search 映射(builtin_function $web_search + 强… | `26bbb6079f19` | `39ab930cbb1d` |
| [#80](https://github.com/Cmochance/codex-app-transfer/pull/80) | feat: web_search MiMo 映射 + 通用 url citation + A/B 双层防错(P6 第一批) | `b56f44dc544f` | `c1a1da975e9a` |
| [#79](https://github.com/Cmochance/codex-app-transfer/pull/79) | release: v2.1.1 prep — bump 版本号 + README rollup 补 P1-P5 | `ae2a6817a603` | `46815c23f3ce` |
| [#78](https://github.com/Cmochance/codex-app-transfer/pull/78) | fix(adapters): MiMo 图片兜底 + namespace MCP 工具调用全栈修复(P1-P5) | `e350c212433d` | `8f0fb3d67ceb` |
| [#77](https://github.com/Cmochance/codex-app-transfer/pull/77) | fix(ci): macos-13 已退役 → 改用 macos-15-intel(根因 v2.1.0 release 卡 24h+) | `55a41c640e0f` | `51da6a91bed8` |
| [#76](https://github.com/Cmochance/codex-app-transfer/pull/76) | docs(readme): 致谢段补 Piebald-AI/claude-code-system-prompts | `742fef1824f8` | `25dfe016861c` |
| [#75](https://github.com/Cmochance/codex-app-transfer/pull/75) | feat(release): 加 macOS Intel x64 build matrix(closes #61) | `f09133c9854b` | `64e0a159d67d` |
| [#74](https://github.com/Cmochance/codex-app-transfer/pull/74) | release: v2.0.12 prep — bump 版本号 + 修 README 重复段 | `55fb758fcc20` | `cf91513cbbd1` |
| [#73](https://github.com/Cmochance/codex-app-transfer/pull/73) | chore(icons): 更新应用图标(全平台) | `72f0fc299b02` | `6e795a219bb1` |
| [#72](https://github.com/Cmochance/codex-app-transfer/pull/72) | chore(release.yml): 关闭 generate_release_notes 避免 What's Changed 重复 | `9783f1a44b39` | `377ed4956b55` |
| [#71](https://github.com/Cmochance/codex-app-transfer/pull/71) | feat(adapters): compact prompt 改写 Claude Code 9-section,根治 summary 断片 | `2555fb82d244` | `329b8ce0e070` |
| [#70](https://github.com/Cmochance/codex-app-transfer/pull/70) | fix(proxy): ws warmup 改用 Close frame,根治 Codex CLI 卡 5 分钟 idle timeout | `a2025123d35d` | `4706d4eae121` |
| [#69](https://github.com/Cmochance/codex-app-transfer/pull/69) | chore: bump version 2.0.10 → 2.0.11(修真实代码版本号) | `36089be2ae57` | `434739ea68d3` |
| [#68](https://github.com/Cmochance/codex-app-transfer/pull/68) | docs: README v2.0.11 | `e5f7f3fee46a` | `3db13eddf2c9` |
| [#67](https://github.com/Cmochance/codex-app-transfer/pull/67) | fix(proxy): ws handler 识别 warmup / 空 input frame,不转 HTTP | `db7069d763b7` | `6e9316710043` |
| [#66](https://github.com/Cmochance/codex-app-transfer/pull/66) | refactor: user-facing 字符串改回英文,对齐 SDK 错误处理 | `98758037e40c` | `788ed104d132` |
| [#65](https://github.com/Cmochance/codex-app-transfer/pull/65) | feat(adapters): ResponseSessionCache sqlite 持久化 (30 天 TTL) + admin … | `4c3e93e419b7` | `7a957a527e09` |
| [#64](https://github.com/Cmochance/codex-app-transfer/pull/64) | fix(adapters+proxy): cache miss + empty input 返回 OpenAI SDK 兼容 400 … | `471bd7d29665` | `9eb141679eff` |
| [#63](https://github.com/Cmochance/codex-app-transfer/pull/63) | docs: README v2.0.10 stability rollups + guide tsSpeedText 更新 | `b5443c856493` | `95bb97107977` |
| [#62](https://github.com/Cmochance/codex-app-transfer/pull/62) | fix: MiMo 404 + apiFormat fallback 全栈统一 openai_chat + healing 按 bas… | `fe06fd2f2952` | `8c32fb4f3223` |
| [#60](https://github.com/Cmochance/codex-app-transfer/pull/60) | fix(proxy+registry): Kimi For Coding Windows 403 真根因 — config 自愈 + … | `a3409c2da92b` | `3b41df2f699e` |
| [#59](https://github.com/Cmochance/codex-app-transfer/pull/59) | release: v2.0.10 — Kimi For Coding Windows 403 修复 + Full access 必要性文档 | `f73257a323cb` | `8f6abf257936` |
| [#58](https://github.com/Cmochance/codex-app-transfer/pull/58) | fix(admin): provider extra_headers 提交时校验,避免运行时静默丢 header | `138dc1dafb2c` | `8c64af177e49` |
| [#57](https://github.com/Cmochance/codex-app-transfer/pull/57) | fix(proxy): 出站剔除 Codex CLI 身份头,修 Kimi For Coding Windows 403 | `9a337fcafb90` | `1b657e5add0b` |
| [#56](https://github.com/Cmochance/codex-app-transfer/pull/56) | chore: 仓库结构整理(docs/ 子目录分类 + .gitignore 加固) | `523a01fb81ad` | `204255feda8c` |
| [#55](https://github.com/Cmochance/codex-app-transfer/pull/55) | refactor(admin): handlers Round 2 拆分(desktop + providers 二级,_legacy… | `7b48d29b6a59` | `da35da94d9d9` |
| [#54](https://github.com/Cmochance/codex-app-transfer/pull/54) | refactor(admin): handlers.rs Round 1 拆分(5229 行 → 6 文件) | `f83d1c208883` | `4b54792a5a6d` |
| [#53](https://github.com/Cmochance/codex-app-transfer/pull/53) | fix(adapters): compaction item 渲染成 user message,避免 compact 后失忆 | `2c685ef43b31` | `b32564d3fa9f` |
| [#52](https://github.com/Cmochance/codex-app-transfer/pull/52) | fix(adapters): 本地实现 /responses/compact 端点(替代 PR #51) | `71b7e6283bae` | `0f2feea1676d` |
| [#50](https://github.com/Cmochance/codex-app-transfer/pull/50) | docs(readme): 加 community contributors 致谢段 | `eb984ef0859d` | `cee8c2d3b5a5` |
| [#49](https://github.com/Cmochance/codex-app-transfer/pull/49) | docs(readme): bump to v2.0.9 + 反映最近 fix | `63352fc19fe5` | `d7b4d0e6db1f` |
| [#47](https://github.com/Cmochance/codex-app-transfer/pull/47) | fix: 修复MiniMax, 清洗mmx不兼容的chat setting, 移除mmx对tool strict的不兼容, 增加<thin… | `e1e892fe61e1` | `5bea47fcb4f0` |
| [#45](https://github.com/Cmochance/codex-app-transfer/pull/45) | fix(codex_integration): catalog context_window 支持任意数值 + 内置 provider… | `9671dca79667` | `dc2a07a18717` |
| [#44](https://github.com/Cmochance/codex-app-transfer/pull/44) | release: v2.0.9 — vision 白名单模型级精确匹配 | `51dc1e74ec9b` | `08b3f70e97d8` |
| [#43](https://github.com/Cmochance/codex-app-transfer/pull/43) | fix(adapter): vision 白名单从 provider 子串改为模型级精确匹配 | `af3ee0fd4647` | `d85734593a14` |
| [#42](https://github.com/Cmochance/codex-app-transfer/pull/42) | fix(proxy+adapter): UA override + usage cached_tokens 默认 — 解决 Kimi … | `ec8b7ab35003` | `bac9a3bee1b1` |
| [#41](https://github.com/Cmochance/codex-app-transfer/pull/41) | release: v2.0.8 — Kimi/DeepSeek thinking UI 显示修复 + extraHeaders {ap… | `aa2dff4b6f70` | `0b7c9e85ed71` |
| [#40](https://github.com/Cmochance/codex-app-transfer/pull/40) | release: v2.0.7 — DeepSeek json_schema 降级 + 空 messages 透传 + Windows UX | `71ab6f032bac` | `b779adbde8ea` |
| [#39](https://github.com/Cmochance/codex-app-transfer/pull/39) | docs(readme): 同步到 v2.0.6 + 中间版本归档说明 | `6ddd96056921` | `1d3b50ee2dee` |
| [#38](https://github.com/Cmochance/codex-app-transfer/pull/38) | release: v2.0.6 — Kimi 阻塞修复 + Windows 终端 flash + 托盘菜单修复 | `889e8c5f3589` | `62c23033c106` |
| [#37](https://github.com/Cmochance/codex-app-transfer/pull/37) | fix(proxy/adapter): TracedStream + empty-msg 守卫 + DeepSeek 视觉剥离 + b… | `bb706ca70a15` | `e851d9ebf60e` |
| [#36](https://github.com/Cmochance/codex-app-transfer/pull/36) | docs(readme): 加 Codex CLI 实接对话截图 | `ff8421c11f8e` | `e5e45e2bebaa` |
| [#35](https://github.com/Cmochance/codex-app-transfer/pull/35) | fix(release): 同步 Cargo.lock 到 2.0.5 | `05ecef90f118` | `a6fe990547a6` |
| [#34](https://github.com/Cmochance/codex-app-transfer/pull/34) | fix(codex-integration): catalog 始终写,非 1M provider 也显示真实模型名 | `998b37768fe1` | `0c4fb07cafd8` |
| [#33](https://github.com/Cmochance/codex-app-transfer/pull/33) | feat(proxy): 4xx/5xx 上游错误时把请求体 + 响应体片段写日志 | `83c44834be69` | `ccb3188c4f31` |
| [#32](https://github.com/Cmochance/codex-app-transfer/pull/32) | release: v2.0.5 — 协议层 / 重启 / 启用按钮链路收敛 | `49dc67f833ea` | `781dd6433001` |
| [#31](https://github.com/Cmochance/codex-app-transfer/pull/31) | fix(restart): macOS 重启 Codex App 加 `-n` + grace 让 launchd reap | `059115f0336b` | `16918df8246c` |
| [#30](https://github.com/Cmochance/codex-app-transfer/pull/30) | perf(frontend): 表单页"启用"按钮即时反馈,后台并发刷新 | `05adc04a1272` | `fd7d111d8451` |
| [#29](https://github.com/Cmochance/codex-app-transfer/pull/29) | fix(frontend): 表单页"一键生成 Codex CLI 配置"按钮直接换成"启用" | `aa6a73b96aa0` | `bb492517fc8e` |
| [#28](https://github.com/Cmochance/codex-app-transfer/pull/28) | feat(adapter): tool call cache + 工具调用历史重建 | `c7da6e40c8e9` | `52f52e8942fc` |
| [#27](https://github.com/Cmochance/codex-app-transfer/pull/27) | feat(adapter): emit response.in_progress right after response.created | `6e6a53b809d1` | `61975167d413` |
| [#26](https://github.com/Cmochance/codex-app-transfer/pull/26) | release: v2.0.4 — 协议转换 / 重启 / 配置链路修复 | `871766a0b930` | `c067d0455441` |
| [#25](https://github.com/Cmochance/codex-app-transfer/pull/25) | Release v2.0.3 fixes | `50bc730423f3` | `9b59af8131c8` |
| [#24](https://github.com/Cmochance/codex-app-transfer/pull/24) | chore: prepare v2.0.2 rehearsal | `c15dce4c379e` | `b5b5afddd066` |
| [#23](https://github.com/Cmochance/codex-app-transfer/pull/23) | docs: align p3 release documentation | `059f99c3505e` | `7266c90bda22` |
| [#22](https://github.com/Cmochance/codex-app-transfer/pull/22) | fix: restore p2 protocol compatibility | `f57a96c6d90d` | `c4de878bc350` |
| [#21](https://github.com/Cmochance/codex-app-transfer/pull/21) | fix: restore p1 core flows | `1a92e2eefad2` | `3915ecdafbd9` |
| [#20](https://github.com/Cmochance/codex-app-transfer/pull/20) | fix: complete p0 feedback flow | `01f98871b673` | `ab5c4ed537dd` |
| [#19](https://github.com/Cmochance/codex-app-transfer/pull/19) | fix: preserve config import secrets | `38a559ef1ea1` | `f5cd0779a28b` |
| [#18](https://github.com/Cmochance/codex-app-transfer/pull/18) | test: verify proxy telemetry logs | `c974f567595c` | `8039882435c2` |
| [#17](https://github.com/Cmochance/codex-app-transfer/pull/17) | docs: mark p0.4 in progress | `0b53aac42dac` | `323ae038a2d7` |
| [#16](https://github.com/Cmochance/codex-app-transfer/pull/16) | fix: verify v1 provider checks | `81b9117db299` | `e6707a64b80a` |
| [#15](https://github.com/Cmochance/codex-app-transfer/pull/15) | Restore v1 provider admin flows | `212eb691bbb6` | `587ddb99b5c6` |
| [#14](https://github.com/Cmochance/codex-app-transfer/pull/14) | fix: restore provider speed test | `354430cb0e68` | `cc98379789c6` |
| [#13](https://github.com/Cmochance/codex-app-transfer/pull/13) | fix(release): macOS 无 Developer ID 时显式 ad-hoc 签 .app 再打 dmg | `0037b38d302b` | `6c29fc5dfa60` |
| [#12](https://github.com/Cmochance/codex-app-transfer/pull/12) | fix: harden Codex 0.128 model catalog routing | `97dc61484bb4` | `aa68fbbf51b3` |
| [#10](https://github.com/Cmochance/codex-app-transfer/pull/10) | chore(release): bump 2.0.0 → 2.0.1 准备 v2.0.1 release | `b1cfce86837e` | `ce4fa3c227e4` |
| [#9](https://github.com/Cmochance/codex-app-transfer/pull/9) | ci: 拆 fast-check + 条件 tauri-check, 终结每 PR 都跑 7min apt | `d808872b30e4` | `e2c913bee413` |
| [#8](https://github.com/Cmochance/codex-app-transfer/pull/8) | docs(readme): 邀请用户提 PR 协助完善 | `ce34475c163c` | `fd580df6014b` |
| [#7](https://github.com/Cmochance/codex-app-transfer/pull/7) | fix: align Responses reasoning schema and history repair | `de30b05fa816` | `0ebb006084f5` |
| [#6](https://github.com/Cmochance/codex-app-transfer/pull/6) | fix(release): 修 PR #3 dispatch test 暴露的 3 个 release pipeline 问题 | `7252ef3b51c7` | `8c8daf7147b7` |
| [#5](https://github.com/Cmochance/codex-app-transfer/pull/5) | chore: Phase 4 — README/migration-plan 收尾 + cleanup 归档 | `078dfe7d68a3` | `7f26ecc89714` |
| [#4](https://github.com/Cmochance/codex-app-transfer/pull/4) | chore: Phase 3 — xtask 重写跨语言契约工具 | `55ece90c2074` | `f97c4b9971db` |
| [#3](https://github.com/Cmochance/codex-app-transfer/pull/3) | chore: Phase 2 — 切换 release pipeline 到 Tauri bundler + GH Actions | `1408b731241d` | `753a84498b38` |
| [#2](https://github.com/Cmochance/codex-app-transfer/pull/2) | chore: Phase 1 清理旧 Python 死码 | `6fdcf99c8f0e` | `5f139d2e669c` |
| [#1](https://github.com/Cmochance/codex-app-transfer/pull/1) | fix: repair DeepSeek thinking tool history / 修复 DeepSeek thinking 工具历史 | `904095d54aa2` | `5ed958fe97e5` |

> 生成于 2026-05-13。142 / 142 条找到新 hash 映射;0 条无 mergeCommit 或未在 commit-map 中(通常是已 rebased PR 或 squash 前的中间状态)。完整 489 commit 映射见 `.git/filter-repo/commit-map`(local only,不入仓库)。