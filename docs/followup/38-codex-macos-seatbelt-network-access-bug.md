---
id: 38
priority: P2
type: bug
status: active
created: 2026-05-19
related_pr: 213
upstream_issue: https://github.com/openai/codex/issues/10390
---

# macOS Codex CLI seatbelt 静默忽略 config.toml 的 network_access(等 OpenAI 修上游)

## 触发上下文

PR #213(#212 实施)真机 verify 发现:codex-app-transfer 已正确把
`sandbox_mode = "workspace-write"` + `[sandbox_workspace_write] network_access = true`
写入 `~/.codex/config.toml`(`tomllib` parse 通过验证),但 Codex CLI 在 macOS
跑 `curl` 仍被 sandbox 拦截。

调研定位到 **OpenAI 官方 Issue [openai/codex#10390](https://github.com/openai/codex/issues/10390)**(状态 **Open**,无修复进度):

> "setting `network_access = true` under `[sandbox_workspace_write]` in
>  `~/.codex/config.toml` has **no effect**"
> "甚至 `sandbox_mode = "danger-full-access"` 在 config.toml 也不够"

macOS 的 seatbelt sandbox **静默忽略** config.toml 里所有 sandbox 相关设置,
只读 CLI flag。这是 Codex CLI 在 macOS 上的实现 bug,不是我们项目的问题。

## 现状(PR #213 land 后)

- **Linux / Windows**:本项目 toggle / config 注入 work normally
- **macOS**:config 写入正确,但被 seatbelt 忽略;UI 文案 + README FAQ 已
  显式警告 macOS 限制 + 引用 #10390 + 给 CLI workaround
- 用户唯一可用 workaround:
  ```bash
  codex --sandbox danger-full-access "..."
  # 或永久 alias
  alias codex='CODEX_SANDBOX_NETWORK_DISABLED=0 codex --sandbox danger-full-access'
  ```

## 待办(等上游修)

1. **监控 openai/codex#10390 进展** —— 当 OpenAI 修 seatbelt 让 config.toml
   的 network_access 真生效后,本 followup 转 resolved,UI/README 警告下架
2. **不主动包装 Codex 启动器** —— 让 codex-app-transfer 帮用户改 alias 或
   wrap CLI 入参是 invasive 路径(改用户 shell rc、跨平台兼容、Codex Desktop
   Electron 启动路径不一致),代价远高于"等上游修"
3. **不主动写 `sandbox_mode = "danger-full-access"`** —— 即便上游 issue 提到
   这是 workaround 路径之一,issue 原文也说"在 config.toml 设它**不够**,
   仍要 CLI flag",且对用户安全门槛降低更大。等真修复

## 验证恢复时机

OpenAI 修了 #10390 后:
1. 真机 macOS 跑 `codex` (无 CLI flag) 测 `curl` 能否 work
2. 若 yes → 本 followup → resolved,docs/followup-tracker.md 索引行移到 Resolved 段
3. UI hint + README FAQ 里 macOS 警告段下架(commit + PR)

## 不影响

- 其他平台 Linux / Windows toggle 完全 work,不在本 followup 范围
- 关闭 toggle 后 web_search 仍可用(由模型自带能力决定)
