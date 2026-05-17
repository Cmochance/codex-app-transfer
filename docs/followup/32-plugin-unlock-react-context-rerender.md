---
id: 32
priority: P2
type: research
status: active
created: 2026-05-17
related_pr: 191
---

# Plugin Unlock macOS:`setAuthMethod` 触发 React AuthContext 整树重渲(物理消除可行性调研)

## 触发上下文

2026-05-17 用户报 macOS Plugin Unlock 首次打开 Codex Desktop 时 "Plugins 锁定 → ~1s 内界面可见'刷新一下' → 解锁"。PR #191 (issue #190) 通过 P0-A(daemon 5s 启动延迟 → 1s)+ P0-B(unlocked 后 disconnect MutationObserver)缓解,但**核心闪烁原因 — `setAuthMethod('chatgpt')` 触发 React 顶层 AuthContext 重渲 — 物理无法消除**。本 followup 跟踪长期方案。

代码 evidence:
- `src-tauri/src/codex_plugin_unlocker.rs:434-441` `spoofChatGPTAuthMethod`:`auth.setAuthMethod('chatgpt')` 是顶层 React Context.Provider value mutation
- `src-tauri/src/codex_plugin_unlocker.rs:422-433` `authContextValueFrom` 沿 fiber.return 向上爬,确认借鉴 galaxywk223 上游策略找的是 React Context 而非具体子组件 state

## 问题描述

### 现状

inject 脚本调 `setAuthMethod('chatgpt')` 必然让 AuthContext.Provider value 变化 → React 把整棵子树标 dirty → 下一帧 commit 时重渲所有 consumers(理论上 Codex Desktop 大部分组件都依赖 AuthContext,包括顶部 nav / sidebar / 主区) → 用户视觉上看到一次"刷新"。

### 期望

理想:用户**完全看不到锁定状态**(Plugins 从第一次渲染就是解锁的)。

但要做到这点必须在 React **第一次 commit 前**就把 AuthContext value 设成 chatgpt — 当前 CDP 注入的时机最早只能在 `Page.frameStoppedLoading` 或 `Runtime.executionContextCreated`,而 React 通常在 frameStoppedLoading 后立刻开始渲染。**Race 很难赢**(JS 执行栈优先级 + React 初始化代码先于我们注入)。

## 已有调研

- galaxywk223/codex-plugin-unlocker 上游(MIT)同样未解决,接受了这次闪烁
- agent 调研报告(PR #191 触发)确认 P0 范围内只能缓解(daemon 提早启动 + observer 停止),不能物理消除
- CDP `Runtime.executionContextCreated` 事件确实比 `Page.loadEventFired` 早,但比 React 初始化代码晚(React 通常在 main bundle 加载完 inline 执行)— 需 verify Codex Desktop 实际 bundle 结构

## 风险 / 不确定性

- **跨 Codex Desktop 版本不稳定**:hook React 初始化 + AuthContext 默认值的方案对 Codex Desktop bundle 结构敏感,每次 Codex Desktop 升级可能挂
- **Electron Fuses 限制**:Codex Desktop 可能锁了 `EnableNodeOptionsEnvironmentVariable`,无法通过 NODE_OPTIONS 注入预启动脚本
- **法律风险**:DLL injection / preload 脚本 hook 可能触犯 OpenAI Codex Desktop EULA
- **不切实际预期**:即便完全消除闪烁,首次开 Codex Desktop 的整体启动闪烁还是有(Electron app 启动有 splash → 主界面跳变),Plugins 闪烁可能被淹没在大盘启动闪烁中

## 建议方向

下次接手第 1 步(优先级低,P2):

1. **查 Codex Desktop bundle 是否暴露 preload script 入口**(unpack .app/Contents/Resources/app.asar 看 main.js 是否引用 preload `webPreferences.preload`)
2. 如果有 preload 入口,**可尝试**用 DLL/dylib swap 或 `NODE_OPTIONS=--require=...` 在 Electron main process 启动时注入一段 JS 把 chatgpt 写入 IndexedDB / localStorage(让 React 初始化时直接读到已登录态)— **跨 Codex Desktop 版本兼容性需逐版本验**
3. **备用思路**:CDP 注入加在 `Runtime.executionContextCreated` 而不是 `Page.loadEventFired`,主动 evaluate `Object.defineProperty(window, '__chatgpt_authed', ...)` 之类的 sentinel,赌 React init 代码读到 sentinel 走已登录分支(需要 reverse engineering Codex Desktop init flow)
4. **现实接受**:跟用户沟通,把"~1-2s 锁定 + 一次刷新解锁" 作为长期 accepted 行为,不再投入消除

## 关联资源

- 触发 PR:#191(P0-A + P0-B 缓解版)
- 关联 issue:#190
- 关联 followup:[#27 二次 splash](27-codex-desktop-double-splash-on-plugin-unlock.md)(相关但不同症状 — 双 splash vs 单次闪烁)、[#33 Windows MSIX](33-windows-plugin-unlock-msix-store.md)
- 上游参考:[`galaxywk223/codex-plugin-unlocker`](https://github.com/galaxywk223/codex-plugin-unlocker) MIT,见 `codex_plugin_unlocker.rs:389-401`
