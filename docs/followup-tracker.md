# Followup Tracker（项目级长期 backlog）

> 本文档记录**跨 session 长期持有**的 followup 任务,避免在 session 结束、context compaction、新分支起点等时机丢失。Claude / Agent / 任何贡献者发现"当前 PR 范围内不修但值得跟踪"的问题时,**必须**在此文档落条目,带可回溯的完整背景。

## 维护规则

### 何时新增条目

任何以下情况:

- review agent / human reviewer 找到非 BLOCKER 但有价值的发现(MED / LOW / NIT / deferred)
- 实施过程发现"超出当前 PR scope 但 prod 真问题"
- 跨 adapter / 跨 crate / 跨架构层的重构建议(touch 太多 caller,当前 PR 不适合)
- 上游协议 / 标准 / 客户端行为研究 ticket(需要抓包 / 真机 / 跨项目调研)
- 测试基础设施 / fixture / CI 改进点

### 何时移除条目

- 条目完整实施 + 合并 main → **删除**该条(同 commit 删 followup-tracker.md 对应段,记 PR/commit reference)
- 条目经评估发现是误判 / 不再适用 → 删除并在 commit message 说明原因
- 条目背景变化(上游修了 / 协议变了)→ 更新条目内容,不删

### 条目格式(强制)

每条至少包含:

```markdown
### #N [Priority] [Type] Title (≤80 字符)

- **触发上下文**: PR 链接 + 具体 file:line + agent finding 引用 / human 反馈引用
- **问题描述**: 现状代码做了什么 / 期望应该做什么 / 差距具体在哪
- **已有调研**: 已经看过的代码 / 文档 / 真实数据 / 假设验证结果
- **风险 / 不确定性**: 实施前需要先解决的疑问(尤其跨项目 / 上游行为依赖)
- **建议方向**: 下次接手时第一步该做啥(不要重新调研)
- **创建日期**: YYYY-MM-DD(便于判断信息新鲜度)
```

**关键:写得够详细,半年后回看不需要重新研究**。如果条目读起来"我得重新看一遍代码才能下手",说明背景没写够。

---

## Active

### #23 [P3 / 研究 ticket] grok_web 末尾 url_citation 列表是否冗余

- **触发上下文**: 原 task #23(grok_web inline `[N]` citation 精确位置)在 task #25 流程中被重新评估。原描述见 `crates/registry/src/presets_data.json:261` —"已知限制:inline `[N]` 精确位置 citation 暂未实现(仅追加在结尾)"。
- **问题描述**: 当前 `crates/adapters/src/grok_web/response.rs:1631 accumulate_web_search_url_citations` / `:1658 accumulate_x_search_url_citations` / `:1277 accumulate_generic_search_url_citations` 把 grok 后端的 `webSearchResults` / `xSearchResults` / `connectorSearchResults` 全部 dump 成 `url_citation` annotation 数组,在 message 末尾通过 `response.output_text.annotation.added` 事件 emit。但 grok 模型 final text 同时已经把这些 URL **作为 markdown inline link**(如 `[官网](https://example.com)`)写进正文 — Codex 客户端按 markdown 渲染可点跳。结果:**同一 URL 在正文 + 末尾列表各显示一次**,用户视角是冗余 / 噪音;且对于"grok 后端搜了但模型没引用"的 URL,末尾还会列出用户从没见过的链接,体验差。
- **已有调研**:
  1. `docs/grok/img/docs/R1.js` 真实抓包 final text 拼接后(grep `"messageTag":"final"` + python 反序列化拼 token):**完全没有 `[N]` 编号 marker**,grok 用纯 markdown link 形式
  2. OpenAI Responses `url_citation` spec 字段 `start_index` / `end_index` 设计是"正文中 [N] token 的字符偏移",grok 数据没 `[N]` → 我们当前设 `0` / `0` 跟设真实偏移**用户视角行为无区别**(因为没角标可定位)
  3. Codex CLI 客户端实际渲染 url_citation 列表的代码路径**没抓**(本仓库不含 Codex CLI 源码)— 假设是"列表式末尾显示",但可能错
- **风险 / 不确定性**:
  - **不确定 1**: Codex CLI 是否真的把 url_citation 渲染成末尾列表?如果它已经"看到 markdown link 就**不**再渲染对应 citation"(智能去重),那当前 dump 就不算冗余,只是没生效。需要真机或客户端代码确认
  - **不确定 2**: 是否有用户依赖"末尾 reference page"作 fact-check 入口?如果有,删除会变 regression
  - **不确定 3**: 删除 url_citation dump 后,reasoning 段(thinking 阶段已有的 `connector_search_results_appends_to_reasoning_and_emits_citation` 等 markdown bullet 渲染)是否能替代审计追溯职责?
- **建议方向**:
  1. **优先**: 真机收集 v2.1.6+ 用户反馈,问"末尾的 citation 列表对你有用吗?跟正文链接重复你是否觉得冗余?"
  2. 真机起 Codex CLI + grok provider 观察 url_citation 实际渲染形态(截图)
  3. 决策树:
     - 用户觉得冗余 + Codex CLI 确实是简单列表式 → **删 url_citation dump 三条路径**,保留 reasoning 段 bullet list(`accumulate_*_url_citations` 全删)
     - 用户依赖 / Codex CLI 智能去重 → **保留现状**,把本条删掉(同时改 `presets_data.json:261` 把"暂未实现 [N]"措辞换成"按 markdown link 直接引用,无角标")
- **创建日期**: 2026-05-13(原 task #23 创建 2026-05-12,本条是评估降级后的接续 ticket)
- **关联 PR**: 当前无未合 PR;原触发是 PR #135-#138 grok_web 引入 web search 阶段,后续讨论在 PR #146 task 25 流程中

---

## Resolved

(条目完成后从 Active 移到这里,只留 1-2 行 + PR ref,防止文档膨胀。30 天后可清理。)

<!-- 示例:
### ~~#25 P1 cloud_code Gemini mapper 漏配 session_cache~~
- 已修于 PR #146(2026-05-13 merged)。cloud_code mapper 改用 `responses_body_to_gemini_request_with_session` + `global_response_session_cache()`,跟 gemini_native 主路径对齐。
-->
