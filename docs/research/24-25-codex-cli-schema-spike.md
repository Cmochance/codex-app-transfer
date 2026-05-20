# Codex CLI 配置 schema spike(#24 + #25 实施前必做)

**Date**: 2026-05-18
**Trigger**: 起 PR 推进 [#24](../followup/24-codex-agents-md-managed-block.md) + [#25](../followup/25-codex-mgmt-four-in-one-page.md) 前的必做调研(#24 详情 "建议方向" 第一步)

## 1. Codex CLI 真实文件结构

通过 `ls ~/.codex/` + `strings /Applications/Codex.app/Contents/Resources/codex` + 实文件 grep 验证。

### 1.1 全局 instructions(`~/.codex/AGENTS.md`)

- **路径**: `~/.codex/AGENTS.md`,4.2k bytes,用户已手写
- **Codex CLI 加载逻辑**(strings binary 印证):

  > AGENTS.md spec: The scope of an AGENTS.md file is the entire directory tree rooted at the folder that contains it. For every file you touch in the final patch, you must obey instructions in any AGENTS.md file whose scope includes that file. More-deeply-nested AGENTS.md files take precedence in the case of conflicting instructions. Direct system/developer/user instructions (as part of a prompt) take precedence over AGENTS.md instructions.

  → `~/.codex/AGENTS.md` 是 home repo 全局 instructions,所有 Codex CLI session 都加载。
- **当前用户内容形态**: 用户手写,无任何 marker。我们 marker 插入后 Codex CLI 会**整文件读取**,marker 是 markdown 注释不影响 Codex 解析。

### 1.2 MCP server 配置(`~/.codex/config.toml`)

- **Schema**(verified line 125-160):
  ```toml
  [mcp_servers.<name>]
  url = "https://..."                    # required for HTTP
  type = "stdio"                          # optional, stdio command 形式
  bearer_token_env_var = "<ENV_VAR>"     # optional auth
  ```
- **当前用户内容**: 12+ servers(vercel / cloudflare-* / v0 / context7 / supabase / notion / neon / figma / ...)
- **关键约束**: TOML 注释敏感 — 必须用 `toml_edit` crate 保留注释跟原文件 layout,不能用 `toml::from_str` + 重 `to_string`(会丢注释 marker 失效)

### 1.3 Skills(`~/.codex/skills/<name>/SKILL.md`)

- **Schema 真实存在**: `ls ~/.codex/skills/` 含 20+ skills(agents-docs-sync / algorithmic-art / brand-guidelines / chronicle / cloudflare-deploy / codex-primary-runtime / doc-coauthoring / docx / figma / figma-implement-design / find-skills / frontend-design / gh-fix-ci / internal-comms / mcp-builder / netlify-deploy / pdf / pptx / render-deploy / ...)
- **每 skill 目录结构**(binary strings 印证 `init_skill.py`):
  ```
  ~/.codex/skills/<name>/
    SKILL.md
    scripts/        (optional)
    references/     (optional)
    examples/       (optional)
  ```
- **不适合 marker 模式** — Skills 是独立目录,管理走 file-level snapshot(backup/restore)而非受管块

### 1.4 Memories(`~/.codex/memories/`)

- **结构**: 复杂 ingestion pipeline,**不是简单单文件**:
  ```
  ~/.codex/memories/
    MEMORY.md              209k bytes(主存储)
    memory_summary.md      55k bytes
    _new_blocks.md         routing layer
    rollout_summaries/*.md ingestion queue
    raw_memories.md
    _update_memsum.py / fix_memory.py(用户自维护 scripts)
    .git/                  (用户用 git 跟踪 memory 变化)
  ```
- Codex CLI binary strings 印证:`rollout_summaries → raw_memories.md → MEMORY.md` 三层 ingestion
- **#25 MVP scope 决策**: **砍掉 Memories tab** — 用户系统过于个人化(`.git/` + 自维护 scripts 说明 user 自己有 workflow),app 介入风险高且没明显 user demand

## 2. 选型决策

### 2.1 TOML 库:`toml_edit`(确认)

- `toml` crate `from_str + to_string` 会丢注释跟原 layout → marker 必失效
- `toml_edit` 保留注释 / 空行 / 字段顺序 → marker 模式可行
- 加 dep: `toml_edit = "0.22"`(目前最新稳定)

### 2.2 前端栈:**C (vanilla JS + Bootstrap)**

#25 详情列了 A (React iframe) / B (全栈迁 React) / C (vanilla 复刻 sidebar+tab) 三个选项,detail 推 A。

**Critical 评估** + 本仓库现状变化:

- 本仓库 PR #195(2026-05-17 merged)刚完成 CSS 组件库重构(`frontend/css/` 拆 24 文件 + gallery.html 预览页),vanilla JS + Bootstrap 栈刚优化定型
- React iframe 引两套 build pipeline,跟刚完成的 CSS 组件库重复造轮子
- 用户 PR #195 总结中明确"5165 行 shadcn 组件库借鉴价值低 — 真要做 React 子页直接 npx shadcn 起新栈,而不是 fork AiMaMi"

**选 C**:跟现有 frontend/css 组件库栈一致,新 Codex 资产管理页面用现有 Bootstrap component(`.panel` / `.button-row` / `.segmented` / `.modal` 等)+ 加几个新组件文件(`components/diff-viewer.css` / `components/history-list.css`)。

### 2.3 控件:dropdown(尊重用户偏好)

- 用户 2026-05-17 明示"左上角下拉选项中切换"
- #25 详情推 sidebar(状态 badge + 高频跳转更顺手)— 但**用户偏好优先**
- 实施 dropdown:Bootstrap `<select>` 或 custom segmented control(可后续按需切 sidebar 不阻塞 backend)

### 2.4 Marker 规范

按 #24 详情建议:

```markdown
<!-- cas:managed:agents:v1:start -->
<app 受管内容>
<!-- cas:managed:agents:v1:end -->
```

- `cas:` 项目 prefix(避免跟 AiMaMi / 其他工具 marker 冲突)
- `managed:` 模式标识
- `agents:` block 类型(后续 `mcp:` / `skills:` 同 prefix)
- `v1:` 版本(为将来 marker schema 升级留余地)
- `start` / `end` 边界

TOML 变种(mcp_servers 段):

```toml
# cas:managed:mcp:v1:start
[mcp_servers.<app-managed-name>]
url = "..."
# cas:managed:mcp:v1:end
```

## 3. 实施 scope(本 PR 第一刀)

按 #25 详情"stacked PR P1-P6"plan,本 PR = **P1 (managed_block trait) + P2 (Agents tab end-to-end demo)**:

### Backend

- `src-tauri/src/admin/services/managed_block.rs` — 通用 trait + parse / preview / apply / rollback / clear / history(generic over file type)
- `src-tauri/src/admin/handlers/agents_md.rs` — 6 个 HTTP endpoints
- `src-tauri/Cargo.toml` — 加 `toml_edit = "0.22"`(MCP tab 后续 PR 用,本 PR 暂未触发)
- 单测覆盖:用真 `~/.codex/AGENTS.md` 跑 parse → apply → rollback 端到端

### Frontend

- `frontend/index.html` — 加 "Codex 资产管理" 入口(从主导航或 settings 子页)
- 新 page section + Agents tab UI(vanilla JS + Bootstrap):
  - dropdown 切 tab(只 Agents tab,后续 PR 加 MCP / Skills)
  - 显示当前受管块状态(已注入 / 未注入 + 上次 apply 时间)
  - "Preview" 按钮 → diff 弹窗
  - "Apply" 按钮 → 调 backend + 刷新状态
  - "History" 折叠 → 列 10 条 + "Rollback to this" 按钮
  - "Clear" 按钮 → 二次确认

### Docs

- `README.md` / `README.en.md` 加 "Codex 资产管理" 段(描述能做什么)+ 致谢 `borawong/AiMaMi`
- followup `#24` / `#25` detail 加 spike findings + 实施进度
- followup-tracker.md 不动(本 PR 不 close #24/#25 — 后续 stacked PR 完成 MCP/Skills 后才 close 这 2 条)

### 不在本 PR scope(留 stacked PR 后续)

- MCP tab(toml_edit + mcp_servers 受管块)
- Skills tab(file-level snapshot)
- Memories tab(MVP 砍掉)
- sidebar / lazy load / 转场动效

## 4. 关联资源

- AiMaMi 源码(借鉴):
  - https://github.com/borawong/AiMaMi/blob/main/src-tauri/src/core/custom_instructions.rs
  - https://github.com/borawong/AiMaMi/blob/main/src-tauri/src/core/mcp.rs
- 本仓库锚点:
  - `src-tauri/src/admin/handlers/` — 平铺 handler 风格
  - `src-tauri/src/admin/state.rs` — AdminState 注入
  - `frontend/css/components/` — 组件库(PR #195 落地)
- followup detail:
  - [`docs/followup/24-codex-agents-md-managed-block.md`](../followup/24-codex-agents-md-managed-block.md)
  - [`docs/followup/25-codex-mgmt-four-in-one-page.md`](../followup/25-codex-mgmt-four-in-one-page.md)
