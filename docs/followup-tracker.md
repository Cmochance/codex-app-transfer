# Followup Tracker（项目级长期 backlog）

跨 session 长期持有的 followup 任务索引。Claude / Agent / 任何贡献者发现"当前 PR 范围内不修但值得跟踪"的问题时,**必须**在 `docs/followup/` 落详情文件 + 在本文档对应段加索引行。

## 文档结构(多级,索引轻量,详情按需读取)

```
docs/
├── followup-tracker.md           # 本文档 — 顶层索引(短行 + 一句话 hook),长期维护
└── followup/
    ├── 23-grok-web-url-citation-redundancy.md   # 单条 followup 详情(强制详细)
    └── <id>-<slug>.md
```

**核心约束**:
- **索引行短** — 每条 Active / Resolved 1 行,≤150 字符,只放"是什么 + 链接"
- **详情文件详细** — 写到"半年后回看不需要重新调研"的程度,见下方"详情文件强制格式"
- 这样 Claude / 用户读索引时只 pull 几 KB 进 context,需要细节才打开对应详情文件

## 详情文件强制格式

每个 `docs/followup/<id>-<slug>.md` 必须包含(顶部 YAML frontmatter + 正文章节):

```yaml
---
id: 23
priority: P0 | P1 | P2 | P3
type: bug | research | refactor | infra | nit
status: active | resolved | dropped
created: YYYY-MM-DD
related_pr: <PR# 或 null>
---
```

正文章节(顺序固定,缺一不可):

1. **触发上下文** — 原 task / agent finding / 反馈来源 + 具体 file:line 引用
2. **问题描述** — 现状代码做了什么 / 期望应该做什么 / 差距具体在哪
3. **已有调研** — 已经看过的代码 / 文档 / 真实数据 / 假设验证结果(file:line + 引用片段)
4. **风险 / 不确定性** — 实施前需要先解决的疑问(尤其跨项目 / 上游行为依赖)
5. **建议方向** — 下次接手时第一步该做啥(不要重新调研),含决策树
6. **关联资源** — 相关 PR / docs / 上游 repo / 真机数据样本路径

**关键**:写得**够详细**,半年后回看不需要重新研究代码 / 重新抓包 / 重新读 agent finding。如果读起来"得重新看一遍才能下手",说明背景没写够 — 加更多 file:line 引用 / 真实数据片段 / 决策推导链。

## 维护规则

### 何时新增条目

任何以下情况:

- review agent / human reviewer 找到非 BLOCKER 但有价值的发现(MED / LOW / NIT / deferred)
- 实施过程发现"超出当前 PR scope 但 prod 真问题"
- 跨 adapter / 跨 crate / 跨架构层的重构建议(touch 太多 caller,当前 PR 不适合)
- 上游协议 / 标准 / 客户端行为研究 ticket(需要抓包 / 真机 / 跨项目调研)
- 测试基础设施 / fixture / CI 改进点

操作:

1. 在 `docs/followup/` 新建 `<id>-<slug>.md`(id 递增,slug = kebab-case 短描述)
2. 按"详情文件强制格式"写完整背景
3. 在本文档 Active 段加 1 行索引:`- [#N P? Title](followup/<id>-<slug>.md) — 一句话 hook(≤80 字符)`
4. 跟代码 PR 同 commit 落仓库(不靠 task list / commit message / memory)

### 何时移到 Resolved

条目完整实施 + 合并 main 时:

1. 把详情文件 frontmatter `status:` 改成 `resolved`,加 `resolved_pr` 跟 `resolved_date`
2. 本文档 Active 段索引行**移到** Resolved 段,改成 `- ~~#N Title~~ → PR #M (YYYY-MM-DD)` 形式
3. 详情文件**保留**作历史归档(不删,便于回溯)
4. Resolved 段每 30 天 review 一次,真正过期且 PR 已合很久(>90d)可批量归档到 `docs/followup/archive/`

### 何时 drop(误判 / 不再适用)

详情文件 frontmatter `status:` 改成 `dropped` + 加 `dropped_reason` 字段 + 索引行删掉。详情文件保留作历史回溯。

---

## Active

- [#32 P2 Plugin Unlock macOS:setAuthMethod 触发 React 整树重渲(物理消除可行性调研)](followup/32-plugin-unlock-react-context-rerender.md) — PR #191 已 P0 缓解,长期消除需 hook Codex Desktop preload 跨版本不稳

---

## Resolved

(完成条目移这里,1 行索引 + PR ref;详情文件保留作历史归档,30 天后批量进 archive/)

- ~~#23 P3 grok_web 末尾 url_citation 列表是否冗余~~ → PR #233 (2026-05-20),`accumulate_*_url_citations` 路径剥离,完全依托 Reasoning 段 markdown link 渲染,消除重复
- ~~#24 P2 RFC: Codex AGENTS.md / config.toml 受管块管理~~ → PR #206 #229 (2026-05-20),`MarkdownManagedBlock` / `TomlManagedBlock` 双 impl + Protected Mode(`outer_signature` SHA-256 防外部并改)+ 10 条 history ring buffer + 17 单测;Agents/MCP/Skills/Memories 四类资产已上 prod
- ~~#25 P2 MCP / Skills / Memories / Agents 四合一管理页~~ → PR #206 #229 (2026-05-20),sidebar + lazy load + 转场动效 4 个 tab(Agents/MCP/Memories marker / Skills file-snapshot)+ 完整 i18n zh/en + Devin Review BUG_0001 hardcoded Chinese 全修
- ~~#27 打开 Plugins 后 Codex Desktop 二次 splash 根因诊断~~ → PR #191 (2026-05-17),实际是"一次刷新解锁"的早期描述,setAuthMethod 触发 React 重渲不可消除转 [#32](followup/32-plugin-unlock-react-context-rerender.md)
- ~~#28 账号还原:desktop_clear 无 has_snapshot guard~~ → PR #194 (2026-05-17),修法 B noop guard 堵核心 P0;pre-clear 备份 enhancement 微小不开新 followup
- ~~#29 账号还原:cleanup_all=true 物理删光所有 snapshot~~ → PR #194 (2026-05-17),软删除 → trash/ + 30 天 GC 堵核心 P0;dry-run preview + UI 二次确认 enhancement 微小不开新 followup
- ~~#30 账号还原:snapshot 单点存储无冗余 / 无导出入口~~ → PR #201 (2026-05-17),跨平台 external_backup_dir 自动镜像(macOS / Windows / Linux)堵核心 P1;UI 导出/导入按钮 enhancement 真有需求再开新 followup
- ~~#31 账号还原:跨版本 MANAGED_KEYS 升级误删用户 key~~ → **dropped 2026-05-17**,false alarm:整文件 cp 已保留任何 root key,managed list 只影响 restore 操作不影响存储
- ~~#38 P2 macOS Codex seatbelt 静默忽略 config.toml network_access~~ → PR #215 (2026-05-20),改用 Codex 官方 "Full access" 配对(danger-full-access + never)绕过,不再依赖 [sandbox_workspace_write].network_access 字段;上游 issue 仍 Open 但本项目已不受影响
- ~~#34 客户端 latest.json + installer RSA 验签~~ → PR #197 (2026-05-17),公钥 build-time embed + verify_signed_bytes 接 fetch_latest_json + download_asset_impl,8 单测覆盖
- ~~#37 update.rs download_asset_impl: in-memory bytes 防 TOCTOU + 重 add bad-sha256 mismatch 单测~~ → PR #199 (2026-05-17),完全 skip partial 文件消除 verify→rename race + verify_installer_sha256 抽函数 5-case 单测 + 500MB hard cap + 4xx/5xx 错误分类不附 URL
- ~~#26 Plugins / MCP 跟"协议转发"绑定 UI/README 显式提示~~ → PR #205 (2026-05-18),i18n autoUnlockCodexPluginsHint 加协议路径生效说明 + README 兼容矩阵 ⚠️ 备注;provider 表单 inline warning enhancement 留 followup
- ~~#35 macOS update translocation / quarantine 前置检查~~ → PR #205 (2026-05-18),macos_translocation_precheck (update_install 入口早期 reject) + macos_strip_quarantine (launch 前 xattr -d) 借鉴 AiMaMi update.rs:47-113
- ~~#36 Windows update 走 NSIS /D=install_dir 保持安装目录~~ → PR #205 (2026-05-18),install_command_parts Windows 分支追 /D=<current_exe parent> + current_exe_parent_dir helper 借鉴 AiMaMi update.rs:7-23
- ~~#33 P1 Plugin Unlock Windows MSIX 启动限制~~ → PR #191 / #194 / #201 (v2.1.11) + PR #227 (2026-05-20),P1 主体 + P2 Task 1 端口冲突探测全实施;P2 Task 2 非-Store .exe fallback **dropped**(只针对官方 Store 用户 scope,能直装 .exe 的用户能自己跑 Codex.exe --remote-debugging-port 启动,不需要本工具适配;issue #226 closed)

<!-- 示例:
- ~~#25 cloud_code Gemini mapper 漏配 session_cache~~ → PR #146 (2026-05-13)
-->
