---
id: 39
priority: P2
type: refactor
status: active
created: 2026-05-21
---

# shell→apply_patch normalize:扩展 Edit / append / sed -i 模式 + output normalize

## Background

本 PR(post-#236 follow-up,issue #235)的 MVP scope 只覆盖 **Add File / 完整 overwrite** 三种 shell file-write 模式:

- `cat <<'EOF' > /path/file` (here-doc, single/double/unquoted terminator)
- `printf '<content>' > /path/file` (escape \\n / \\t / \\\\ / \\' / \\" 展开)
- `echo '<content>' > /path/file` (字面 literal,无 escape 展开)

跑通后让 Codex Desktop 渲染 4+2 diff UI 元素(行号 / `+N -M` / 文件清单 / 颜色 / 抬头 / 可点击)。

**严格 reject**(透传原 exec_command 不动)的 shell 模式 + 触发条件构成本 followup:

| 模式 | 出现场景 | 当前行为 | 期望行为 |
|------|----------|----------|----------|
| `>>` append | 追加 log line / 配置补行 | 透传 shell,UI 无 diff card | Edit hunk 加 `+` 行 in V4A |
| `sed -i 's/x/y/' file` | 替换字符串 | 透传 shell,UI 无 diff card | Edit hunk(需 read old content)|
| `sed -i 'N,Md' file` | 删除行 | 透传 | Edit hunk(删除行)|
| `tee -a file` | append via pipe | 透传 | Edit append |
| `cat <<EOF >> file` | here-doc append | 透传 | Edit append |

## 难点

Edit / append 模式需要 **old file content** 才能生成 V4A `-` 行(byte-exact)。adapter 不能跑 shell `cat` 拿真实文件(scope 违规 + 副作用),只能从以下来源拿:

1. **模型在更早的 turn 用 exec_command `cat file` 已读过** → adapter 缓存 (file_path → last_cat_output)。多 turn 状态,跨 stream 持久化复杂。
2. **Codex Desktop 提供 read_file MCP / hook** → 上游 IDE 端读,adapter 不需要持有状态。但需要 Codex Desktop 暴露这个能力(目前未确认)。
3. **放弃 Edit,仅做 Add File + complete overwrite**(当前 MVP)。

## Output normalize(独立子项)

MVP 后,模型下一 turn 看到的 tool_call_output 是 Codex Desktop apply_patch handler 返回的 `Success. Updated the following files: /tmp/x.py`。但模型期待的是 shell 输出 `Exit code: 0\n<stdout>`。

模型可能困惑("我跑了 shell,为什么返回 apply_patch success?"),触发额外 verify turn(模型再发一个 `cat file` 确认)。
worst case 多 1 turn 但不破坏 UI 渲染。

**期望优化**:adapter 把 apply_patch handler output 反向映射成 shell-style 输出格式,让模型无感:
```
Exit code: 0
Wall time: 0 seconds

(file /tmp/x.py written: N lines)
```

## Acceptance criteria

- Edit 模式支持(至少 `sed -i 's/x/y/' file` 单行替换):需要 adapter 自带文件缓存或 Codex Desktop hook
- append 模式支持(`>>` / `tee -a` / `cat <<EOF >>`)
- output normalize:apply_patch handler output → shell-style 输出,模型 verify turn 减少

## 触发再开 PR 的信号

- 真机数据(round N+)显示用户大量使用 Edit / append 场景导致 UI 体验不一致
- 上游 Codex Desktop 暴露 file read MCP / hook(消除 adapter 持有文件状态的复杂性)
- 用户报告"明明改了文件但 UI 没渲染"事件 ≥3 次

## Refs

- 主 PR: issue #235 / PR #(本 PR)
- baseline 数据: `~/.codex-app-transfer/logs/apply-patch-debug/round{1,2,3}/`
