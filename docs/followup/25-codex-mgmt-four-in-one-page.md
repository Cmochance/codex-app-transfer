---
id: 25
priority: P2
type: feature
status: active
created: 2026-05-17
related_pr: null
---

# #25 MCP / Skills / Memories / Agents 四合一管理页(借鉴 AiMaMi)

## 触发上下文

- **来源**: 2026-05-17 chat session Q3 — 用户提出 "我也需要增加 MCP / Skills / Memories / Agents 管理页面,放在同一页,在左上角下拉选项中切换,参考该项目的实现方式"
- **AiMaMi 参考**:
  - 整体布局: `borawong/AiMaMi:src/main-app.tsx:71-187`(`useState<Route>` + lazy import + Promise.allSettled 预热 + 滑动转场)
  - 切换控件: `borawong/AiMaMi:src/components/layout/sidebar.tsx:30-37,178-201`(shadcn `<Sidebar collapsible="icon">` 6 个 nav item)
  - 路由类型: `borawong/AiMaMi:src/types/navigation.ts:1-9`(`type Route = "overview"|"customInstructions"|"mcp"|"skills"|"maintenance"|"settings"`)
  - State 方案: `@tanstack/react-query` v5.62 单一 queryClient,无 zustand/redux
- **本仓库当前状态**: 前端是 `frontend/index.html` + `frontend/js/app.js` 单页 vanilla JS,没有 React / sidebar 路由系统,4 个 tab 内容当前完全缺失
- **依赖 followup**: 本 ticket 实施前先看 [[24-codex-agents-md-managed-block]](24-codex-agents-md-managed-block.md) — 受管块基础设施是本 ticket 中 Agents tab 跟 MCP tab 的底层依赖

## 问题描述

**现状**: codex-app-transfer 前端只覆盖 provider / preset / 模型映射切换 + 设置 + Plugins 解锁等"切换类"功能。Codex CLI 的 4 类用户配置文件(AGENTS.md 全局 prompt instructions / config.toml 中的 MCP server 段 / Codex memory 文件 / `~/.codex/skills/`)**完全没有任何 UI 管理入口**,用户必须用文本编辑器手改。

**期望**: 加一个"Codex 资产管理"页面,4 个 tab 切换:
1. **Agents tab** — 管 `~/.codex/AGENTS.md` 全局 instructions(受管块 + diff preview + apply / rollback / history)
2. **MCP tab** — 管 `~/.codex/config.toml` 的 `[mcp_servers]` 段(受管块软开关)
3. **Memories tab** — 管 Codex memory 文件(spike 待确认 Codex CLI 是否有等价 memory 机制)
4. **Skills tab** — 扫 `~/.codex/skills/*/SKILL.md` 列表 + 备份/恢复

**差距**:
- 前端缺路由 / sidebar / tab 系统(当前 vanilla JS 单页)
- 后端缺管理 API(handlers/ 平铺 HTTP handler 风格已建,但没接 Codex 配置文件管理)
- 4 个 tab 中 3 个依赖 [[24-codex-agents-md-managed-block]] 的受管块基础设施

## 已有调研

### AiMaMi 的整体架构(从公开源码逐行验证)

1. **路由 = 纯 useState**(`src/main-app.tsx:71` `const [route, setRoute] = useState<Route>("overview")`)— 不引 react-router,适合小到中型应用
2. **lazy import + 预热**(`src/main-app.tsx:39-58, 96-105`)— 启动后 deferred 900ms 再 `Promise.allSettled([import("@/components/{mcp|skills|...}/...")])` 预热全部 page chunk,首次切换 tab 零延迟
3. **滑动转场**(`src/main-app.tsx:177-187`)— `routeOrder.filter(mounted).map → <PageStage state={getStage(c)}>` 同时挂载相邻页,加 CSS transform 滑入滑出
4. **Sidebar 而非 dropdown**(`src/components/layout/sidebar.tsx:30-37`)— 6 个 nav item 平铺,collapsible="icon" 折叠成 64px 图标列,展开 176px;每 item 可挂状态 badge
5. **State = tanstack-query 单一 queryClient**(`src/main-app.tsx:60` `const queryClient = createAppQueryClient()`)— 每 page 用 `useQuery({ queryKey, queryFn: api.xxx })` + `useMutation` 直接驱动,**没**用 zustand / redux / jotai

### 我跟用户讨论的决策点

- **用户要求**: "左上角下拉选项中切换" — 但 sidebar 比 dropdown 优势:
  1. 可显示状态 badge(MCP 启用数 / Agents 上次 apply 时间),dropdown 折叠时全看不见
  2. 4 个 tab 都高频跳转,dropdown 多一次点
  3. shadcn `<Sidebar>` 是成熟组件,跟 AiMaMi 同套
- **如果用户坚持 dropdown**: 改 top bar `<Select>`,所有 backend / state 不变,只换布局组件 — 退路存在,不阻塞主架构

### 跟本项目现有前端的兼容性问题

**关键**: 本项目 `frontend/` 是 **vanilla JS + Bootstrap**(`frontend/index.html` + `frontend/js/app.js` 3000+ 行),**没有 React**。要落地 AiMaMi 的方案,选项:

- **A. 在现有 `frontend/` 内单独起 React iframe / micro-frontend** — 隔离 React 跑 4 合一页,主页继续 vanilla JS。优点:不重写主应用;缺点:两套 build pipeline
- **B. 把整个 `frontend/` 迁到 React/Vite + shadcn-ui** — 跟 AiMaMi 完全对齐。优点:技术栈统一;缺点:重构面太大,跟当前 v2.1.9 稳定路线冲突
- **C. 不引 React,用 vanilla JS + Bootstrap Nav Tabs 复刻 sidebar + tab 模式** — 优点:零技术栈变更;缺点:lazy import + tanstack-query 这些 React 生态收益拿不到

**推荐 A**: 单独 React 子页隔离,4 合一页是独立 sub-app,主菜单加个入口跳转 — 风险低 + 跟 AiMaMi 实现高度对齐 + 不影响主应用稳定性。

## 风险 / 不确定性

1. **Codex CLI 的真实文件 schema 待验证**:
   - AGENTS.md 全局 instructions 路径 / 加载顺序?
   - config.toml 中 MCP server 是 `[mcp_servers.<name>]` table 还是 `[[mcp_servers]]` array?
   - Codex CLI 有 memory 文件吗?路径?(可能根本没有 → Memories tab 砍掉或换其他需求)
   - Skills 是 `~/.codex/skills/<name>/SKILL.md` 这种结构吗?(可能 Codex CLI 不支持 skill,只 Claude Code 支持)
   - **实施前必须 spike** `node_modules/@openai/codex/dist/` 或 https://github.com/openai/codex 源码
2. **前端栈选型**(A/B/C)需用户拍板,本 ticket 推荐 A 但**不强制**
3. **依赖 [[24-codex-agents-md-managed-block]]**:3 个 tab 共享受管块 trait,#24 先于本 ticket 实施;Skills tab 独立可同步进行
4. **跟 codex_plugin_unlocker 不重叠**:本 ticket 改 Codex CLI 配置文件,跟 Codex Desktop 进程的 plugin 解锁完全不同层 — 但**前端布局**可能希望同一页同时呈现 CLI 资产管理 + Desktop Plugins 状态,需统一规划
5. **用户对 dropdown 偏好的尊重**:用户明示"左上角下拉" — 实施时如改 sidebar 需先跟用户解释 trade-off 拿确认,不可静默换控件

## 建议方向

### 第一步(spike,3-5h)

1. 读 Codex CLI 源码确认 AGENTS.md / MCP / memory / skill 真实 schema(file 路径 + 数据结构 + 加载顺序)
2. 决前端栈选型(A/B/C)— 推荐 A,**但**先跟用户对齐再动
3. 决控件 sidebar vs dropdown — 推荐 sidebar,带 trade-off 跟用户说明

### 第二步(实施,stacked PR)

| PR | 范围 | 依赖 |
|---|---|---|
| PR1 | 后端 `managed_block` trait + 单测 | [[24-codex-agents-md-managed-block]] 实施 |
| PR2 | Agents tab(后端 + 前端最小 demo) | PR1 |
| PR3 | MCP tab(`toml_edit` + 受管块) | PR1 + spike MCP schema |
| PR4 | Memories tab(条件:Codex 真有 memory) | spike 结果 |
| PR5 | Skills tab(file-level snapshot,独立机制) | 无,可跟 PR1-4 并行 |
| PR6 | 四合一 sidebar + page 转场动效整合 | PR2-5 |

每 PR 独立可 merge,user 验收增量。

### 第三步(用户面文档)

- README 中英文加 "Codex 资产管理" 段(描述 4 tab 能做什么)
- 同 PR 加 borawong/AiMaMi 致谢段(符合 `feedback_credit_upstream_in_readme` 规则)

## 关联资源

- **AiMaMi 源码引用**:
  - https://github.com/borawong/AiMaMi/blob/main/src/main-app.tsx
  - https://github.com/borawong/AiMaMi/blob/main/src/components/layout/sidebar.tsx
  - https://github.com/borawong/AiMaMi/blob/main/src/types/navigation.ts
- **本仓库相关**:
  - `frontend/index.html` + `frontend/js/app.js` — 当前 vanilla JS 主页
  - `src-tauri/src/admin/handlers/` — handler 风格参考
- **Codex CLI 上游(spike 必读)**:
  - https://github.com/openai/codex
- **关联 followup**:
  - [[24-codex-agents-md-managed-block]] — 必须先实施,本 ticket 3 个 tab 的底层
- **本 ticket 出处**: 2026-05-17 chat session Q3 / Q4 完整分析
