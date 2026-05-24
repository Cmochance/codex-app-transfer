---
id: 44
priority: P2
type: refactor
status: active
created: 2026-05-24
related_pr: null
---

# compact 路径剥离历史 reasoning_content(仿 Claude Code 的 stale thinking 主动管理)

## 触发上下文

issue #248 GLM-5.1 autocompact 修复讨论中,讨论"compact 路径还能怎么进一步省 budget"时识别出来。本 PR(`feat/glm-compact-thinking-disabled`,引入 `compact_thinking_policy.rs` 注册表)只解决**当前 compact 调用关 thinking**(节省 output budget),不在范围内的是**历史回放剥离 reasoning_content**(节省 input budget)。两者独立且互补,但工程改动面、回归面、上游 wire 兼容性都不一样,拆分单独 PR。

## 问题描述

**现状**:`crates/adapters/src/responses/request.rs` 的 `extract_input_items` 路径在转换 Codex Desktop `Responses` history → chat `messages` 时:

```rust
// crates/adapters/src/responses/request.rs(主对话路径,non-compact)
if obj.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
    pending_reasoning = Some(extract_reasoning_text(obj));
    continue;
}
// pending_reasoning 被塞到下一条 assistant message 的 reasoning_content 字段
if let Some(reasoning) = pending_reasoning.take() {
    msg_obj.insert("reasoning_content".into(), Value::String(repaired));
}
```

即历史 `reasoning` items **全部塞进** chat history 里 assistant message 的 `reasoning_content` 字段,**会占 input budget**。这是普通 chat 路径的合理实现 —— Kimi / DeepSeek 等 thinking-enabled 上游强制要求 assistant tool_call message 必带 `reasoning_content`,否则 400 "thinking is enabled but reasoning_content is missing"(`compact.rs:200` 注释明确)。

**期望**:**compact 路径下** 历史 reasoning_content 应被剥离 —— 因为:

1. **compact 任务语义**:把历史浓缩成 summary。summary 只要"结论"不要"过程",历史 reasoning 是过程,**语义价值近零**
2. **input budget 紧张**:`COMPACT_CHAT_MESSAGES_MAX_BYTES = 120 * 1024`(`compact.rs:105`)对 message 总字节有上限,历史 reasoning 占据空间挤压真实 user/assistant content 可见窗
3. **本 PR 的 disable thinking 是 output 侧优化**(`thinking.disabled` / `enable_thinking=false` 让当前调用不产生新 reasoning),input 侧的历史 reasoning 仍未治

**差距**:compact path 当前完全沿用主对话路径的 reasoning 处理,没特化。

## 已有调研

**Anthropic 官方文档**([extended-thinking](https://platform.claude.com/docs/en/build-with-claude/extended-thinking) 原话):

> "When a non-tool-result user block is included: on Opus 4.5+ and Sonnet 4.6+, previous thinking blocks are kept; on **earlier Opus/Sonnet models and all Haiku models, all previous thinking blocks are ignored and stripped from context**"

> "The API automatically filters the provided thinking blocks. Uses the relevant thinking blocks necessary to preserve the model's reasoning. **Only bills for the input tokens for the blocks shown to Claude.**"

即 Anthropic 把"历史 thinking 是否真进上下文"当作模型实现细节自动 filter,**只对 tool use 中间状态强制保留**(推理流程要接续)。其它场景**可以 strip**。

**Claude Code 行为**(社区分析 + Anthropic Engineering 博客):

> "Old thinking blocks are the largest tokens-per-message contributor. When the user has been idle for over an hour (cache expired anyway), **only the last thinking turn is kept**."

Claude Code **主动管理 thinking 数量**,idle 后只保留最后一个 thinking turn。compact 触发时,Tier 3 LLM summarization 的 input 不显式含 stale thinking。

**项目内已有的兜底**(`crates/adapters/src/responses/request.rs:1264-` `ensure_thinking_tool_call_reasoning`):

thinking enabled 上游下,会给历史 assistant tool_call message 补占位 `" "`(单空格)字符串。这意味着**即使我们剥离真 reasoning**,只要 placeholder 仍在,Kimi / DeepSeek 等强制要求 reasoning_content 字段存在的上游也不会 400 —— **协议兼容性已有保险**。

## 风险 / 不确定性

1. **`pending_reasoning` 暂存逻辑**`request.rs` 是主对话路径共用,直接改会影响**所有** non-compact chat。需要做的是:
   - 选 A:在 compact path 入口**预处理 input items**,filter 掉 `type=reasoning`,再走 `responses_body_to_chat_body_for_provider` —— 不动主路径,只在 compact synthetic body 上处理
   - 选 B:`responses_body_to_chat_body_for_provider` 加 `strip_history_reasoning: bool` 参数,compact 路径传 true —— 改动面更大但更明确
   - **倾向选 A**,改动局部、不动主路径行为

2. **MiniMax M2.x 的 reasoning_split + interleaved thinking**:M2 的官方文档强调 multi-turn 中"必须保留 thinking chain 连续性"。compact 是中间一次性调用、不是 multi-turn 流水的一部分,剥离应安全,但需要真机验证 M2 不 400。

3. **历史 assistant message 的 `reasoning_content` 字段**:`extract_input_items` 直接处理 `reasoning` items;但 history 中的 `message` items 如果**本身就带** reasoning_content(罕见,Codex Desktop 应该用 `reasoning` 独立 item),需要也 strip 一下。读 fixture 确认。

4. **回归面**:compact 不爆但 summary 质量是否下降?Anthropic 官方观点是历史 reasoning 不重要,但要靠真机 A/B 验证(对比剥离前后的 summary 内容质量)。

5. **跟 `ensure_thinking_tool_call_reasoning` 协作**:剥离真 reasoning + 占位空格补 —— 实际等于把所有历史 assistant tool_call 的 reasoning_content 统一变成 `" "`,理论上无差别(占位本来就是为了通过协议校验)。但要确认上游不会因为"所有 turn 都是 placeholder"而行为异常。

## 建议方向

**Step 1**:fixture 调研 —— 看 `tests/replay/fixtures/` 里有没有带 reasoning items 的真机 Codex Desktop request bundle。如果有,拿来做 before/after 对照测试。

**Step 2**:`compact.rs::build_compact_chat_request` 在 raw_input 处理前加 reasoning items filter:

```rust
let stripped_input = raw_input.map(|v| match v {
    Value::Array(arr) => Value::Array(
        arr.into_iter()
            .filter(|item| {
                item.get("type").and_then(|t| t.as_str()) != Some("reasoning")
            })
            .collect(),
    ),
    other => other,
});
```

Step 3:确认 chat body 输出里 reasoning_content 都是占位 `" "` 或不存在(没真 reasoning 文本),通过现有 `ensure_thinking_tool_call_reasoning` 兜底。

Step 4:加单测:`build_compact_chat_request_strips_history_reasoning_items` —— input 含 N 个 reasoning items + N 个 assistant tool_call + N 个 message,转换后 chat body 不含真 reasoning 文本(只占位)。

Step 5:真机 A/B —— 跑 5 次长会话触发 compact:
- baseline:剥离前
- treatment:剥离后
- 对比 chat body input 字节数(预期降 20-40%) + summary quality(用 #224 `validate_compact_summary_quality` 跑)+ summary 内容主观对比

Step 6:README release notes 提"compact 路径仿 Claude Code,主动剥离历史 reasoning 节省 input budget"。

## 关联资源

- 本 PR(`feat/glm-compact-thinking-disabled`):output 侧 disable thinking 的对位优化
- `crates/adapters/src/responses/request.rs:` `extract_input_items` + `extract_reasoning_text` + `ensure_thinking_tool_call_reasoning`
- `crates/adapters/src/responses/compact.rs:136` `build_compact_chat_request` 入口
- `crates/adapters/src/responses/compact.rs:218` `enforce_compact_chat_message_budget`(后置兜底,跟本优化串联)
- Anthropic 文档:https://platform.claude.com/docs/en/build-with-claude/extended-thinking
- Anthropic compaction 文档:https://platform.claude.com/docs/en/build-with-claude/compaction
- 社区分析:https://barazany.dev/blog/claude-codes-compaction-engine
- issue #248
- PR #224(prompt 简化的对位 PR,本 followup 是其延伸)
