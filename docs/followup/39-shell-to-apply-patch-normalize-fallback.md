---
id: 39
priority: P3
type: research
status: active
created: 2026-05-22
related_pr: 239
---

# shell→apply_patch normalize 兜底(等 prompt-only 路径失效再激活 PR #239 复活)

## 触发上下文

issue #235 收尾时(2026-05-21),`apply_patch` 在 chat-completions provider 上不渲染 diff UI 的问题通过 **prompt-only** 方案修复:

- **PR #236**(merged 2026-05-21):wire 层 `custom_tool_call` SSE 桥接 + 多 commit 演进的 V4A prompt(单端 `@@` / minimal Update form / Add File `+` 前缀 / prefix 无空格)
- **PR #240**(merged 2026-05-22):Move 空 hunk + `*** Begin Patch` first-line 显式约束

真机三 provider round 7-9 数据证明 **prompt-only 方案足够**:
- Kimi For Coding round 7:14 个 apply_patch 调用 / 12 success / 85.7%(2 fail 已在 PR #240 修复)
- Xiaomi MiMo (Token Plan) round 8:用户反馈基本无问题
- DeepSeek V4 Pro round 9:9 个 apply_patch 调用 / 9 success / **100%** / reasoning 零 self-correction

模型在新 prompt 下**主动**调 `apply_patch` 工具,几乎不再走 `cat <<EOF > file` / `printf > file` / `echo > file` 等 shell file-write 路径。

并行尝试的 **PR #239 server-side normalize 兜底方案已 CLOSED**(2026-05-22),原因:
- round 3 真机数据显示 retry 风暴(72.7% fail rate)
- detector 边界模糊(`>>` / `sed -i` / `tee` / 变量替换 / 多命令组合等需要 reject 透传)
- prompt-only 方案更优雅,直接消除 shell file-write 来源

## 问题描述

### 现状(已 ship)

模型读 PR #236 + #240 的 prompt guidance → 主动调 `apply_patch` 工具 → adapter wire 桥接 → Codex Desktop 渲染 diff card(行号 / `+N -M` 增减 / 文件清单 / 颜色高亮 / 工具调用抬头 / 可点击文件名 6 个元素全部正常)。

### 假设激活条件(P3 触发信号)

如未来出现以下任一情况,**重新评估**是否复活 PR #239 server-side normalize:

1. 新接入 provider / 模型完全无视 prompt guidance,绕过 `apply_patch` 工具直接 shell `cat > file`(真机数据 ≥3 次用户反馈)
2. Codex CLI / Codex App 升级后 `apply_patch` wire 协议改动,prompt 修复在新协议下失效
3. 真机日志统计显示 `apply_patch` 调用率显著下降而 shell file-write 上升(需要观测 hook,目前无)

### 期望(若激活)

复活 PR #239 的 `crates/adapters/src/responses/shell_to_apply_patch.rs` detector(commit history 仍在 GitHub `pull/239/commits`,branch 已删但 SHA 可访问) + `converter.rs::close_tool_call` 接 detector + 重写 SSE 路径,**作为 prompt-only 失效场景的兜底**,而非主路径。

## 已有调研

- **PR #239 实施代码**(closed):`crates/adapters/src/responses/shell_to_apply_patch.rs` 含 21 单测覆盖 `cat <<EOF >` / `printf >` / `echo >` 三种 happy + `>>` / `sed -i` / `tee` / 多命令 / 变量替换等 reject case
- **Antigravity Go agent 借鉴**(`~/.local/bin/agy`):`FILE_CHANGE_TYPE_EDIT` / `file_diff` / `trajectory_file_diffs` token 印证 server-side 统一 diff 事件模型
- **round 3 真机数据**(`~/.codex-app-transfer/logs/apply-patch-debug/round3/`):PR #239 实测 retry 风暴根因 — converter 未及时关闭 stream + detector 边界含糊

## 风险 / 不确定性

- **retry 风暴重现**:PR #239 当时已修复 `args_acc` sync / bare `$VAR` reject / interrupted incomplete 三 BUG(pre-push 3-agent review 捕获),但实际复活时还需重 review converter 跟 normalize 路径的交互
- **Edit / append 仍不支持**:PR #239 MVP 只覆盖 Add File / overwrite,Edit / `>>` / `sed -i` in-place 需要 old file content(adapter 无法直接读),需要 IDE hook 或上下文缓存
- **底层假设变了**:如果 Codex CLI 协议升级,detector 的输入输出形态可能不再适用

## 建议方向(若激活)

1. 重新评估 prompt-only 失效是真是假(可能是新增 prompt 微修就能修,不必动 adapter 层)
2. 若必须 server-side 兜底,**从 PR #239 closed branch cherry-pick** 而非重新实现:
   - GitHub 仍保留 commits:`gh pr view 239 --json commits --jq '.commits[].oid'`
   - `git fetch origin pull/239/head:revive-239` 拉取 closed PR 的 commits
3. 真机验证(round N+)对照 round 3 看 retry 风暴是否仍存在
4. scope 收紧到**只兜底无法用 prompt 修复的场景**,主路径仍是 prompt

## 关联资源

- **主 PR 链**:PR #236 (merged) + PR #240 (merged) — issue #235 ship 路径
- **PR #239 (closed)**:`https://github.com/Cmochance/codex-app-transfer/pull/239` — server-side normalize 实施代码 + close 时的评估评论
- **真机数据归档**:`~/.codex-app-transfer/logs/apply-patch-debug/round{1..9}/` 246MB
- **上游借鉴**:[`Antigravity Go agent`](file:///Users/alysechen/.local/bin/agy) strings dump
- **关联 followup**:无(独立兜底方案,跟 #32 Plugin Unlock React 重渲无关)
