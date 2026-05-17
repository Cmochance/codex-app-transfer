---
id: 27
priority: P3
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# 打开 Plugins 后启动 Codex Desktop 看到两次启动界面 — 根因诊断

## 触发上下文

2026-05-17 用户反馈:
> "Plugins 打开后启动 codex app 时会跳两次启动界面,帮我梳理一下逻辑看是什么原因导致的。"

注意:这里"codex app"指的是 **上游 Codex Desktop**(Electron app `/Applications/Codex.app`),不是 Codex App Transfer 本身。Codex App Transfer 自身没有 splash window 也没有 splash 路由(`src-tauri/src/main.rs` 只 setup tray + 单一 main window,`frontend/index.html` 没 splash 元素)。

只在 `autoUnlockCodexPlugins=true` 时出现;关掉这个开关后(待用户实测确认)splash 应该只跳一次。

## 问题描述

### 现状(Codex App Transfer 侧不主动 restart Codex Desktop)

`src-tauri/src/admin/handlers/desktop.rs` 全文搜索 `launch_codex_app_restart` 只有一个 caller —— `restart_codex_app` (`desktop.rs:960-961`),对应前端"重启 Codex"按钮显式点击。**startup 流程不调它。**

启动序列(`src-tauri/src/main.rs:50-85`):

```
打开 Codex App Transfer
  │
  ├─ L52 同步: restore_codex_if_enabled("startup")
  │     └─ 有快照就把 ~/.codex/config.toml + auth.json 还原到 apply 前
  │
  ├─ L53-55 async: auto_apply_on_startup_if_enabled
  │     └─ sync_desktop_for_active_provider (desktop.rs:695)
  │         ├─ 写新 ~/.codex/config.toml + auth.json (apply_desktop_target)
  │         └─ 必要时拉起本地 proxy
  │     [NOT launch / restart Codex Desktop]
  │
  └─ L63-85 async (+5s): plugin_unlock daemon
        └─ CDP 连接已运行的 Codex Desktop
            └─ inject_unlock_script (codex_plugin_unlocker.rs:379-541)
                ├─ enablePluginEntry → DOM disabled=false
                ├─ spoofChatGPTAuthMethod (line 434-441)
                │     └─ auth.setAuthMethod('chatgpt')  ← 关键嫌疑点
                └─ MutationObserver 持续重跑(line 532-537)
```

### 期望

启动 Codex Desktop 只显示 1 次 splash,不因 Plugins 解锁产生 visual flicker。

### 差距

差距具体在哪未百分百确认,需要走"验证方法"一节里的实验。最强嫌疑见下。

## 已有调研

### 候选根因 1(最可能):setAuthMethod 触发 React AuthContext 重 mount

`src-tauri/src/codex_plugin_unlocker.rs:392-433` 的脚本沿 React fiber `return` 链向上爬,找带 `setAuthMethod` 和 `authMethod` 字段的 Context.Provider value。注释明确写"找 AuthContext.Provider value"(line 393-397)。

`line 438` 直接调 `auth.setAuthMethod('chatgpt')`。AuthContext 是顶层 Provider,value 变化会让整棵子树 re-render,Codex Desktop 的 router 很可能因 authMethod 切换走"已登录后的初始化"分支,触发 splash → 主界面的二次跳转。

注意 `line 437` 已经有 short-circuit `if (auth.authMethod === 'chatgpt') return true;` —— 但**首次注入时 authMethod 不可能已经是 chatgpt**(否则按钮本来就不会 disabled),所以首次注入必然触发一次切换,这次切换就是 splash 的来源。

### 候选根因 2(次可能):config 文件抖动触发 Codex Desktop 内部 reload

用户先开 Codex Desktop、再开 Codex App Transfer 的场景:

- `main.rs:52` 同步 `restore_codex_if_enabled("startup")` 可能把 ~/.codex/config.toml 还原到 apply 前
- `main.rs:53-55` async 接着写新 config(apply_desktop_target → `desktop.rs:676`)
- 加上 `maybe_wake_codex_pet` (`desktop.rs:268-308`) 改 `~/.codex/.codex-global-state.json` 里 `electron-avatar-overlay-open` —— **但 maybe_wake_codex_pet 只在 `open_codex_app` 路径被调(`desktop.rs:336`),startup 路径不调用**

如果 Codex Desktop 监听 `~/.codex/config.toml` / `auth.json` 变化(闭源 Electron app,行为未知),restore→apply 之间的连续两次写可能让它 reload → 二次 splash。

### 候选根因 3(低概率):CDP `--remote-debugging-port=9222` 让 Electron 不一样

`desktop.rs:320-329` `should_attach_debug_port` 在 autoUnlockCodexPlugins=true 时给 Codex Desktop 加 `--remote-debugging-port=9222 --remote-allow-origins=*`。但用户描述的是"启动 Codex Desktop 时两次 splash",而 debug port 是用户**从 Codex App Transfer 主动启动 Codex** 时加的(`open_codex_app`),如果用户是手动开 `/Applications/Codex.app` 这个 flag 不会注入 → 该候选只覆盖"用户用 Codex App Transfer 启动 Codex"的子场景。

## 风险 / 不确定性

- **Codex Desktop 是闭源 Electron**,所有"它内部做了什么"都是黑盒推断。verify 必须靠外部 signal(splash 出现时间点、CDP devtools console、config 文件 mtime 与 splash 时间相关性)。
- **复现路径未严格 nail**:用户当前描述是"打开 Codex App Transfer + 打开 Codex Desktop"两个动作的总体观感,不知道哪个先 / 哪个后 / 间隔多久。需要让用户复述精确步骤。
- 如果根因 1 确认,**修法不一定在 unlock 脚本里**。`auth.setAuthMethod('chatgpt')` 是借鉴 `galaxywk223/codex-plugin-unlocker` 的核心做法(unlocker.rs:389-391 引用),换法可能导致 plugin 解锁失败。需要权衡"避免 splash 闪一次"vs"plugin 一定能解锁"。

## 建议方向

下次接手按这个顺序:

1. **零成本验证根因**:让用户在 settings 关掉 `autoUnlockCodexPlugins` 重启 Codex App Transfer + Codex Desktop,看二次 splash 是否消失。
   - 消失 → 根因 1 / 3 确认,继续下一步
   - 不消失 → 根因 2 或别的,改去看 config 文件 mtime
2. **若根因 1 确认**:开 Codex Desktop devtools(`open http://localhost:9222`,但要求 Codex App Transfer 已经带 debug port 启动它),在 splash 闪烁时间点看 Console 是否打印 setAuthMethod 调用 / React Router 路由跳转日志。
3. **若用户能接受闪一次**:不修代码,只在 settings 文案补一句"首次注入会导致 Codex Desktop 出现一次额外的 splash 跳转,属预期行为"。
4. **若必须消除**:研究是否能在 `enablePluginEntry` 跑前先 stash 当前 authMethod,setAuthMethod 后立刻 setAuthMethod(原值),让 React 看到"实际没变",依赖 React 的同值短路。但需要测会不会让 plugin 重新被 lock。

## 关联资源

- 触发 PR:#188 (bailian-token-plan preset 修复 — 此 followup 由同次调研派生)
- 关联 issue:#187
- 关联 follow-up:#26(Plugins / MCP 跟协议路由绑定的 UI 提示)
- 代码锚点:
  - `src-tauri/src/main.rs:50-85` startup 序列
  - `src-tauri/src/admin/handlers/desktop.rs:695, 746, 960-961, 268-308, 320-329, 335-357, 359-369` apply + launch + wake 链路
  - `src-tauri/src/codex_plugin_unlocker.rs:379-541` inject 脚本全文
- 上游借鉴:`galaxywk223/codex-plugin-unlocker` MIT,见 `codex_plugin_unlocker.rs:389-391` 注释
