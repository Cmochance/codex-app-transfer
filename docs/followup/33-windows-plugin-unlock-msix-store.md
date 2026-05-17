---
id: 33
priority: P1
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# Plugin Unlock Windows:Microsoft Store (MSIX) 启动限制导致 `--remote-debugging-port` 无法注入,Plugin Unlock 完全不工作

## 触发上下文

2026-05-17 用户报"Windows 上 Plugins 完全无法解锁"。主对话 + agent 调研 (general-purpose) 确认是 known design limitation,代码注释自承(`desktop.rs:155-156`)。

Agent 进一步调研开源生态 6 种 Windows MSIX CDP 注入方案,evidence-based 推荐 **Method 1 (IApplicationActivationManager) + Method 6 (检测非-Store .exe fallback)** 双管齐下。本 followup 跟踪实施。

代码 evidence:
- `src-tauri/src/admin/handlers/desktop.rs:154-160` Windows 分支硬写 `["explorer.exe", "shell:AppsFolder\\<WINDOWS_STORE_APP_ID>"]`,**忽略 extra_args**
- 注释 line 155-156 明示:"Windows Store 应用不支持通过 explorer.exe 传递命令行参数。如需调试端口,需用户手动修改快捷方式或使用其他启动方式"
- `should_attach_debug_port` (`desktop.rs:320-329`) 返 `["--remote-debugging-port=9222", "--remote-allow-origins=*"]` 在 Windows 上**静默丢失**
- daemon `detect_cdp` (`codex_plugin_unlocker.rs:273-289`) 连 `http://127.0.0.1:9222/json/list` → connection refused → 状态永远 `Disconnected`

## 问题描述

### 现状

Codex Desktop on Windows 通过 Microsoft Store / MSIX 分发,Shell 启动(`explorer.exe shell:AppsFolder\<AUMID>`)**协议层面不传命令行参数**。本应用启动 Codex 时 `--remote-debugging-port=9222` 被 OS 剥除,Codex.exe 9222 端口不监听 → CDP 不可达 → Plugin Unlock daemon 永远 Disconnected → Plugins 标签始终锁定。

### 期望

Windows 上 Plugin Unlock 跟 macOS 等效工作,或至少给用户清晰错误提示(而不是静默不工作)。

## 已有调研

Agent 调研 6 种方案 evidence-based:

| Method | 结论 | Evidence |
|---|---|---|
| 1. `IApplicationActivationManager::ActivateApplication` (WinAPI COM) | **App-specific** — TradingView Desktop MSIX 成功,Claude Desktop 失败;需对 Codex Desktop 真机 empirical test | [emremigh/tradingview-mcp-windows-msix-fix](https://github.com/emremigh/tradingview-mcp-windows-msix-fix/blob/main/launch_msix_debug.ps1) 成功案例;[zstnbb/PCE-Core ADR-018](https://github.com/zstnbb/PCE-Core/blob/main/Docs/docs/engineering/adr/ADR-018-msix-store-app-capture-strategy.md) 记录 Claude Desktop 失败 |
| 2. 直接 `.exe` (`WindowsApps/...`) | **ACL 阻断** | PCE-Core ADR-018 §2.1 实测 "WindowsApps 路径 ACL 拒绝执行" |
| 3. Runtime CDP attach | **不支持** | [electron/electron#10445](https://github.com/electron/electron/issues/10445) — debug port 只在进程启动时生效 |
| 4. DLL / Frida injection | **风险高,被开源项目拒绝** | PCE-Core ADR-018 §3.3 拒采用,AV false-positive ≥5% + ToS reverse-engineering 风险 |
| 5. `.lnk` 快捷方式劫持加参数 | **MSIX activation 剥所有 cmdline args** | tmurgent / advancedinstaller / Microsoft 官方 docs 一致 |
| 6. **检测非-Store 直装 .exe**(OpenAI 也提供 direct download) | **可行,最稳** | OpenAI 官方提供 [非 Store 直装版本](https://developers.openai.com/codex/app/windows);wallneradam/claude_autoapprove 同模式 (target `%LOCALAPPDATA%\AnthropicClaude\claude.exe`) |

### Agent 最终推荐

**Method 1 + Method 6 fallback**:
- 优先尝试 IApplicationActivationManager COM 调用注入 AUMID(需 verify Codex Desktop AUMID = `OpenAI.Codex_<hash>!App`)+ args=`--remote-debugging-port=9222`,empirical test 看 CDP 端口能否在 10s 内监听
- 失败 fallback: 检测 `Get-AppxPackage OpenAI.Codex` + `%LOCALAPPDATA%\OpenAICodex\Codex.exe`(或直装路径),如果是直装版,`std::process::Command::new(exe).arg("--remote-debugging-port=9222")` 直起绕过 MSIX
- 都不行: UI 显示 "Windows Plugin Unlock 不可用,Microsoft Store 版限制 — 推荐从 openai.com 下直装版本"

## 风险 / 不确定性

- **IApplicationActivationManager 跟 Codex Desktop 兼容性未知** — 必须真机 empirical test(本项目作者无 Windows 测试机时受阻)
- **AUMID 字符串获取**:Windows Store 包名带 publisher hash,需要 `Get-AppxPackage` 动态查 → 写在 Rust 里需要 PowerShell 调用或 WinAPI
- **法律**:操作 Codex Desktop COM 激活借鉴 TradingView 模式,无 reverse engineering,**合规**;但需确认 OpenAI EULA
- **直装版本是否真存在 / 是否所有 Windows 用户都能装** — OpenAI Codex Desktop direct download 当前 status 需 verify

## 建议方向

下次接手按这个顺序:

1. **PR P0 立刻做**(止血):`desktop.rs:154-160` Windows 分支加 + UI Status 加 "Windows MSIX 限制,Plugin Unlock 暂不可用,详见 docs/" 提示,避免用户以为是 bug 浪费 debug 时间
2. **PR P1 调研**(用户提供 Windows 测试机):写一个 PowerShell + Rust + COM interop 的最小 spike,在用户 Windows 上跑 IApplicationActivationManager 实测能否给 Codex Desktop AUMID 注入 args 让 CDP 9222 监听
3. **PR P1 实施**(spike 成功):写 `windows` crate `IApplicationActivationManager` binding,改 `open_command` Windows 分支 + 增加 AUMID 检测 helper
4. **PR P1 fallback**(spike 失败 / 部分用户走直装版):加 `Get-AppxPackage` + 直装路径检测,直装版用 `std::process::Command::new(exe)`,Store 版退到 P0 提示

## 关联资源

- 触发 PR:#191(macOS P0 闪烁优化,本 followup 是它的"out of scope")
- 关联 issue:#190
- 关联 followup:[#32 macOS setAuthMethod React 重渲调研](32-plugin-unlock-react-context-rerender.md)
- 上游参考:
  - [emremigh/tradingview-mcp-windows-msix-fix](https://github.com/emremigh/tradingview-mcp-windows-msix-fix) — 成功案例
  - [zstnbb/PCE-Core ADR-018](https://github.com/zstnbb/PCE-Core/blob/main/Docs/docs/engineering/adr/ADR-018-msix-store-app-capture-strategy.md) — 失败 / 各方案对比 ADR
  - [wallneradam/claude_autoapprove](https://github.com/wallneradam/claude_autoapprove) `claude_autoapprove.py:161-163` — Method 6 非-Store 直装路径检测同模式
  - [Microsoft Learn: IApplicationActivationManager](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-iapplicationactivationmanager-activateapplication)
  - [openai/codex#21538](https://github.com/openai/codex/issues/21538) — 企业用户请求非 Store installer
