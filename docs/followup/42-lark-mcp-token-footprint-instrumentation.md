---
id: 42
priority: P3
type: research
status: active
created: 2026-05-24
related_pr: null
---

# 飞书 MCP(`@larksuiteoapi/lark-mcp`)真实 token 占用实测调研

## 触发上下文

issue #248 中用户报告 GLM-5.1 + 飞书 MCP 场景下,Codex Desktop 触发 autocompact 但 GLM 处理失败导致上下文丢失。issue 文本描述飞书 MCP 操作"**单次工具调用 + 返回数据动辄数千 token**"(`多维表格读写 / 文档创建/编辑`)是 context 增长的主要触发因素。

主线 PR 修的是 GLM 侧 compact 行为(`compact_thinking_policy.rs` 注册表 + 注入 `thinking.disabled` 等),与飞书 MCP 输入输出**无因果关联**。但飞书 MCP 的真实 token footprint 数据缺失,后续如要做以下任意一项,都需要先有数据:

- per-MCP-tool token budget 或截断策略
- artifact 外置(像现在 tool_call_output 大输出的 artifact bundle 机制)的 MCP 触发阈值调优
- 用户向"哪些 MCP 适合长会话"的可观测性反馈

## 问题描述

**现状**:我们不知道飞书 MCP 各 tool 的真实 input/output token 量级,只有 issue 文本里"数千 token"的定性描述。

**期望**:在用户真实工作流里,采集飞书 MCP 至少 5 类典型操作(`bitable.app_table.list / bitable.app_table_record.batch_get / docx.document.raw_content / im.message.create / drive.file.list`)的 request body 大小 + response body 大小,落到 `docs/research/lark-mcp-token-footprint.md`。

**差距**:
1. 飞书 OpenAPI response schema 在 `larksuiteoapi/lark-mcp` 是动态字段,docs 不直接体现 token 量级
2. Codex Desktop 端 tool_call request/response 经过我们 proxy,但当前没有 per-tool token 计数日志
3. MCP server 自身的 verbose log 默认不开,需要环境变量或代码 patch

## 已有调研

无。issue #248 主对话内已确认"修 GLM 不需要这个数据",但用户后续可能问"我该不该启用某 MCP",那时需要数据支撑。

## 风险 / 不确定性

- 飞书 MCP server 是 Node.js 进程,与我们 Rust proxy 不同进程,常规 inject 不通用 — 需要靠 stdio MCP 协议侧 log 或者直接在 Codex Desktop 侧抓包
- 用户的飞书工作流私有性高,采集需要本人自己跑,不能 agent 远程模拟
- 不同租户、不同表格规模、不同字段类型,token 量级差异极大(空表 < 100 token,多维表格几千行 > 100K token)— 单数据点价值有限,需要 N≥5 样本

## 建议方向

**Step 1**:用户在真实 GLM + 飞书 MCP 工作流跑一会(>10 个 tool call),自己记录每次 tool 调用前后的 Codex Desktop UI 上的 token 计数变化(底栏有显示),挑 5 类 high-impact 操作样本。

**Step 2**:如果需要精确数据,在 `src-tauri/src/proxy_runner.rs` 或 chat body 转换路径加临时 log:

```rust
tracing::info!(
    target: "codex_app_transfer::mcp::token_audit",
    tool_name = %name,
    request_chars = body_str.len(),
    response_chars = response_str.len(),
    "lark-mcp tool call footprint"
);
```

挂 `RUST_LOG=codex_app_transfer::mcp::token_audit=info` 跑一段时间,grep 日志聚合。

**Step 3**:数据落 `docs/research/lark-mcp-token-footprint.md`,如发现某些 tool 平均 > 10K token,考虑:
- 给 MCP tool output 也加 artifact 外置(参考 `compact.rs` 的 `[Tool output stored outside model context]` 机制)
- 或在 README 加 "建议关闭 MCP server X / 关闭 tool Y" 提示

**不要**:在没有数据之前就预设"飞书 MCP 一定需要专门优化"— 可能 GLM 修好后,飞书 MCP 在常规工作流下完全够用,实测才知道。

## 关联资源

- [飞书开放平台 OpenAPI 文档](https://open.feishu.cn/document/home/index)(各 API response schema)
- [`larksuiteoapi/lark-mcp`](https://github.com/larksuite/lark-openapi-mcp)(MCP server 源码)
- issue #248
- 主线修复 PR(`compact_thinking_policy` 注册表)
- `crates/adapters/src/responses/compact.rs:329-376` — 现有的 `compact_message_for_budget` artifact 外置参考实现
