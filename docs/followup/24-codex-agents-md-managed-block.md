---
id: 24
priority: P2
type: research
status: resolved
created: 2026-05-17
related_pr: 206, 229
resolved_pr: 206, 229
resolved_date: 2026-05-20
---

# #24 RFC: Codex 全局 AGENTS.md / 配置文件"受管块"管理(借鉴 AiMaMi)

## 触发上下文

- **来源**: 2026-05-17 chat 中调研 borawong/AiMaMi 项目时识别的高 ROI 借鉴点
- **AiMaMi 上游 file:line**:
  - `borawong/AiMaMi:src-tauri/src/core/custom_instructions.rs:1-130` — 受管块解析 / preview / apply / rollback / clear / history 全套核心
  - `borawong/AiMaMi:src-tauri/src/commands/custom_instructions.rs` — 5 个 Tauri command 暴露给前端
  - `borawong/AiMaMi:src-tauri/src/core/mcp.rs:1-100` — 同模式应用到 `config.toml` 的 MCP server 段(3 类 marker: bottom / top / router-top)
  - `borawong/AiMaMi:src-tauri/src/core/skills.rs:1-80` — 同模式应用到 SKILL.md 备份/恢复
- **本仓库当前状态**: 全局 instructions / MCP / Skills 全靠用户手改 Codex CLI 配置文件,我们没提供任何管理 UI / API。`src-tauri/src/admin/handlers/` 列表里搜不到 instructions / agents.md / mcp / skills 任何 handler

## 问题描述

**现状**:Codex CLI 的几个关键用户配置文件 — `~/.codex/AGENTS.md`(全局 prompt instructions)、`~/.codex/config.toml`(provider + MCP server 配置)、`~/.codex/skills/*/SKILL.md`(skill 包)— 用户必须手动用文本编辑器维护。codex-app-transfer 当前**只**管 `model_provider` / `preset` / 模型映射等切换类配置,**不**管这些 prompt/扩展类文件。

**期望**:让 codex-app-transfer 在这些文件中维护"受管块"(managed block),通过 HTML 注释 marker 把"app 受管区"跟"用户手写区"物理隔离 — app 可以安全地 update / rollback 自己那段,**永远不动**用户手写区;用户也可以随时手改自己那段,app 不会覆盖。

**差距**:整套"marker + history + Protected 模式"的能力当前为零;Q3(MCP/Skills/Memories/Agents 四合一管理页)的 4 个 tab 中,**Agents tab** 跟 **MCP tab** 都直接依赖这套基础设施,Skills tab 跟它共享 history/backup 抽象。所以本 ticket 是 Q3 实施前的**底层基础**。

## 已有调研

### AiMaMi 的"受管块"模型(从公开源码逐行验证)

1. **Marker 设计**(`src-tauri/src/core/custom_instructions.rs:1-130`):

   ```markdown
   <!-- AIMAMI_CUSTOM_INSTRUCTIONS_START -->
   <app 受管内容,可被 apply/rollback>
   <!-- AIMAMI_CUSTOM_INSTRUCTIONS_END -->
   ```

   HTML 注释在 Markdown / TOML 中都不影响渲染/解析(Markdown 透明,TOML 用 `#` 注释 marker 同理)。Parser 用正则定位 START/END,把文件切成 `[before_user, managed, after_user]` 三段。

2. **4 个核心操作**:
   - `preview` — 算出"如果 apply 文件会变成什么样",**只**返回 diff,不写盘
   - `apply` — 真写盘 + 把当前 managed 段存进 history snapshot
   - `rollback` — 从 history 取出第 N 条还原
   - `clear` — 删掉 managed 段(连同 marker),还原成"app 介入前"

3. **History 机制**:`HISTORY_LIMIT=10`,每次 apply 把旧 managed 段连同 timestamp 推到环形缓冲,超 10 条丢最老的。存储在 app 数据目录(非 Codex 目录)避免污染。

4. **Protected 模式**(`CustomInstructionProtectionState`):如果检测到 marker **外**用户手写区在两次 apply 之间被修改,**跳过**本次 apply 并 surface 警告 — 避免"用户刚改 app 又 overwrite"误伤。这是关键的非破坏性设计,跟用户偏好 `不主动破坏性降级` 完全一致。

5. **MCP 变种**(`src-tauri/src/core/mcp.rs:1-100`):同模式应用到 TOML,有 3 类 marker(`# --- AiMaMi Managed Block (bottom/top/router-top) ---`),`enabled=false` 时通过**移动块位置**而非删除来实现软开关(避免反复"删/插"破坏用户邻近注释的位置)。

### 为什么本仓库适合借鉴

- 我们 README.md 已存在"协议层 + provider 切换"这条主线,加 prompt/扩展类管理是**自然延伸**,跟 Codex CLI 用户期待的"桌面伴侣"角色更对齐
- 跟我们 proxy 层零耦合 — 受管块逻辑是纯文件读写 + marker 解析,独立模块,审查面小
- 跟现有 `src-tauri/src/admin/handlers/` 平铺式 HTTP handler 风格一致,可直接加 `admin/handlers/managed_block.rs`(或拆 `agents_md.rs` / `mcp_toml.rs` / `skills.rs` 3 个 handler 共享 `admin/services/managed_block.rs` 通用 trait)

### 不直接 copy AiMaMi 的部分

- AiMaMi 的 Tauri command 包装(`Result<CoreEnvelope<T>, String>` 统一 envelope)跟我们 axum HTTP handler 风格不一样,**只**借鉴算法,不引入新框架抽象
- Marker 字符串改成项目自己的标识(例如 `<!-- codex-app-transfer:managed:agents:start -->`),避免跟 AiMaMi 用户的 marker 冲突(同一台机器可能同时装两个工具)

## 风险 / 不确定性

1. **Codex CLI 是否真读 AGENTS.md 全局 instructions?** 需 verify 路径跟 schema:可能是 `~/.codex/AGENTS.md`,也可能是 `~/.codex/config.toml` 里有个 `instructions = "..."` 字段。**实施前**必须 grep Codex CLI 源码(用户已装的 codex npm 包 `node_modules/@openai/codex/dist/`)或文档,**禁用**"大概率/可能"假设。
2. **TOML 的注释 marker 跟 toml-rs serde 库的关系**:如果用 `toml-edit` crate(保留注释),可以放心改;如果用 `toml::from_str` + 重新 `to_string` 会丢注释 → marker 失效。需先选库再设计。
3. **跟 #23 followup 的关联性**:`#23 grok_web url_citation` 跟本 ticket **无**直接关联,但都属于"用户面 prompt/呈现层"主题,Q3 实施时可一并 review 该主题下其他 followup。
4. **跟 `codex_plugin_unlocker.rs` 的关系**:Q2 反馈"Plugins 默认关闭 + 启用后重启失效"问题与本 ticket **无**直接关联(plugin_unlocker 走 CDP 注入 Codex Desktop 进程,本 ticket 改的是 Codex CLI 配置文件),但**同一个 Agents tab 页面**可能要同时呈现两者状态(Codex CLI AGENTS.md 受管块 + Codex Desktop Plugins 解锁状态),前端布局需统一规划。
5. **MCP block 跟我们 Codex Desktop Plugins 解锁(`src-tauri/src/codex_plugin_unlocker.rs:347-394`)是两个完全不同层**:前者改 CLI config.toml(MCP server 注册表),后者改 Desktop 渲染进程 React state(解锁 Plugins UI 入口)。不要混淆。

## 建议方向

**第一步(本 ticket 实施前必做的 spike)**:

1. 读 `node_modules/@openai/codex/dist/` 或 https://github.com/openai/codex 源码,搞清:
   - AGENTS.md / 全局 instructions 真实路径 + 加载顺序
   - MCP server 在 config.toml 中的 schema(`[mcp_servers.<name>]` table 还是 `[[mcp_servers]]` array)
   - Skills(如果 Codex CLI 也有)的目录结构
2. 选 TOML 库:`toml_edit` (推荐,保留注释/格式) vs `toml` (会丢注释)
3. 决定 marker 命名规范统一前缀,例:`<!-- cas:managed:agents:v1:start -->`(带 v1 便于未来 marker 协议升级)

**第二步(实施 MVP,scoped 到 Agents tab)**:

1. 加 `src-tauri/src/admin/services/managed_block.rs` — 通用 `ManagedBlock` trait,定义 `parse/apply/preview/rollback/clear/history` 抽象接口
2. 加 `src-tauri/src/admin/handlers/agents_md.rs` — `/api/codex/agents-md/{status,preview,apply,rollback,clear,history}` 6 个端点
3. 前端 Agents tab 调这 6 个端点,渲染 diff preview(用 `react-diff-viewer-continued` 或 `diff` lib)+ history 列表 + apply/rollback 按钮
4. **集成测试**用真机 `~/.codex/AGENTS.md` 跑(参考 `feedback_real_config_validation` rule,绝不只用合成 fixture)

**第三步(Q3 后续 tab)**:

- MCP tab 复用 `ManagedBlock` trait 接 `config.toml` 的 `[mcp_servers]` 段
- Memories tab 接 Codex CLI 是否有 memory 文件(spike 阶段查清),如有走同模式
- Skills tab 接 `~/.codex/skills/` 目录扫描 + 备份/恢复(不是 marker 模式,是 file-level snapshot)

## 关联资源

- **AiMaMi 源码**:
  - https://github.com/borawong/AiMaMi/blob/main/src-tauri/src/core/custom_instructions.rs
  - https://github.com/borawong/AiMaMi/blob/main/src-tauri/src/core/mcp.rs
  - https://github.com/borawong/AiMaMi/blob/main/src-tauri/src/core/skills.rs
  - https://github.com/borawong/AiMaMi/blob/main/src-tauri/src/commands/custom_instructions.rs
- **本仓库相关**:
  - `src-tauri/src/admin/handlers/` — 平铺 handler 风格参考
  - `src-tauri/src/admin/state.rs` — `AdminState` 注入模式
  - `src-tauri/src/codex_plugin_unlocker.rs` — Q2 关联(完全不同层,避免混淆)
- **Codex CLI 上游**(spike 必读):
  - https://github.com/openai/codex(npm 包源码,需 verify AGENTS.md / config.toml schema)
- **TOML 库选型**: `toml_edit` https://docs.rs/toml_edit/(保留注释)
- **README 致谢**:实施 PR 同 commit 加 README 中英文致谢段,引用 AiMaMi 项目(符合 `feedback_credit_upstream_in_readme` rule)
- **2026-05-17 chat session**:Q1/Q2/Q3/Q4 完整分析在该 session,本 ticket 是 Q4 的落地产物
