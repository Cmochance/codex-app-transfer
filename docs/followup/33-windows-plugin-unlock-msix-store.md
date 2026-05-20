---
id: 33
priority: P1
type: bug
status: active
created: 2026-05-17
related_pr: 191, 194, 201
remaining_issue: 226
last_updated: 2026-05-20
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

### 第一轮 agent 调研(6 方案对比)

| Method | 结论 | Evidence |
|---|---|---|
| 1. `IApplicationActivationManager::ActivateApplication` (WinAPI COM) | **App-specific** — TradingView Desktop MSIX 成功,Claude Desktop 失败;需对 Codex Desktop 真机 empirical test | [emremigh/tradingview-mcp-windows-msix-fix](https://github.com/emremigh/tradingview-mcp-windows-msix-fix/blob/main/launch_msix_debug.ps1) 成功案例;[zstnbb/PCE-Core ADR-018](https://github.com/zstnbb/PCE-Core/blob/main/Docs/docs/engineering/adr/ADR-018-msix-store-app-capture-strategy.md) 记录 Claude Desktop 失败 |
| 2. 直接 `.exe` (`WindowsApps/...`) | **ACL 阻断** | PCE-Core ADR-018 §2.1 实测 "WindowsApps 路径 ACL 拒绝执行" |
| 3. Runtime CDP attach | **不支持** | [electron/electron#10445](https://github.com/electron/electron/issues/10445) — debug port 只在进程启动时生效 |
| 4. DLL / Frida injection | **风险高,被开源项目拒绝** | PCE-Core ADR-018 §3.3 拒采用,AV false-positive ≥5% + ToS reverse-engineering 风险 |
| 5. `.lnk` 快捷方式劫持加参数 | **MSIX activation 剥所有 cmdline args** | tmurgent / advancedinstaller / Microsoft 官方 docs 一致 |
| 6. **检测非-Store 直装 .exe**(OpenAI 也提供 direct download) | **可行,最稳** | OpenAI 官方提供 [非 Store 直装版本](https://developers.openai.com/codex/app/windows);wallneradam/claude_autoapprove 同模式 (target `%LOCALAPPDATA%\AnthropicClaude\claude.exe`) |

### 第二轮(2026-05-17 用户指示查 AiMaMi / codex++)— 找到金矿 `BigPizzaV3/CodexPlusPlus`

**`BigPizzaV3/CodexPlusPlus`**(MIT, 2699 stars, Python 实现, 最新 commit 2026-05-17): **跟本项目 100% 同道的 CDP 路线,已完整解决 Windows MSIX 注入**。这个项目的 evidence 直接把 Method 1 从 "需 empirical test" 升级为 "可对照实现"。

关键 file:line + 实现要点:

1. **AUMID 自动解析**(`codex_session_delete/launcher.py:298-304`):从 `Get-AppxPackage OpenAI.Codex` 的 InstallLocation 反推 AUMID 不用手填:
   ```python
   if not package_dir.name.startswith("OpenAI.Codex_") or "__" not in package_dir.name:
       return None
   identity_name = package_dir.name.split("_", 1)[0]
   publisher_id = package_dir.name.rsplit("__", 1)[1]
   return f"{identity_name}_{publisher_id}!App"
   ```

2. **`IApplicationActivationManager::ActivateApplication` 完整 ctypes COM binding**(`launcher.py:347-395`):
   - CLSID `45BA127D-10A8-46EA-8AB7-56EA9078943C`
   - IID `2e941141-7f97-4756-ba1d-9decde894a3d`
   - vtable index 3 = `ActivateApplication`
   - 用 `ole32.CoCreateInstance` + 手搓 vtable 调用

3. **CDP 启动参数序列化** (`launcher.py:283-287, 411`):**MSIX 不能 CreateProcess + argv 数组**,必须把 args 序列化成单一字符串传 ActivateApplication 的 `arguments` 参数:
   ```python
   def build_codex_arguments(debug_port: int) -> list[str]:
       return [f"--remote-debugging-port={debug_port}",
               f"--remote-allow-origins=http://127.0.0.1:{debug_port}"]
   # 调用时:
   activate_packaged_app(aumid, subprocess.list2cmdline(build_codex_arguments(debug_port)))
   ```

4. **MSIX 安装定位** (`codex_session_delete/app_paths.py:30-49`):
   ```python
   cmd = 'Get-AppxPackage -Name "OpenAI.Codex" | Select-Object -ExpandProperty InstallLocation'
   r = subprocess.run(["powershell", "-NoProfile", "-Command", cmd], ...)
   root = Path(p); app = root / "app"
   ```

5. **端口冲突处理** (`launcher.py:267-281`):用 `SO_EXCLUSIVEADDRUSE` 探测 9229 占用,占用就分配随机端口,不报错继续

6. **Codex.exe 优雅清理** (`launcher.py:434-451`):PowerShell `Get-CimInstance Win32_Process -Filter "Name='Codex.exe'"` 而非 `taskkill /F`,避免 packaged app 状态污染

### 同时也调研的项目(结论 negative,留 evidence 防再调研浪费时间)

- **AiMaMi (`borawong/AiMaMi`,Apache 2.0)**:仅 macOS 完整覆盖,Windows 只做 `CREATE_NO_WINDOW` 防黑窗(`src-tauri/src/platform/windows.rs:1-22`),0 处 MSIX 处理。本项目从 AiMaMi 借鉴的是 UI 架构(marker / history / 四合一管理页, follow-up #24/#25),Windows MSIX 维度**无可借鉴**
- **b-nnett/codex-plusplus (TS/Bun, MIT, 1678 stars)**:走 ASAR 改包路线(`packages/loader/loader.cjs`),跟本项目 CDP 路线不同;Windows 用 `Get-AppxPackage` + robocopy 镜像 WindowsApps 到 `%LOCALAPPDATA%` 再 patch(`packages/installer/src/platform.ts:200-258`),仅 MSIX 安装位置探测部分可参考

## 风险 / 不确定性

- ~~IApplicationActivationManager 跟 Codex Desktop 兼容性未知~~ — **第二轮调研已 close**,BigPizzaV3 实证可工作
- **AUMID 字符串获取**:已知方案 — `Get-AppxPackage` PowerShell 调用反推(launcher.py:298-304 模式),Rust 实现可用 `std::process::Command::new("powershell")` 或 windows crate
- **法律**:借鉴 BigPizzaV3 / TradingView 的 ActivateApplication 模式,无 reverse engineering,**合规**;Apache-2.0 / MIT license 友好
- **直装版本(Method 6 fallback)的需求性降低**:Method 1 已实证可工作,Method 6 仅作 last-resort fallback

## 实施进展(2026-05-20 更新)

### ✅ P0 — 已不再需要(2026-05-17,PR #191 实施 Method 1 核心)

原"在 Windows 上 plugin_unlock status 加错误文案 'Windows MSIX 注入实施中'"是止血方案,但 PR #191 同日 merge 直接实施了 Method 1,Windows 实际能注入,无需文案。

### ✅ P1 大部分已实施(PR #191 / #194 / #201)

| PR | 范围 | 文件 |
|---|---|---|
| #191 (v2.1.11) | Method 1 `IApplicationActivationManager` COM activation 核心 + AUMID 自动解析(对照 `BigPizzaV3 launcher.py:298-304`) | `src-tauri/src/windows_msix.rs::activate_packaged_app` + `resolve_codex_aumid` |
| #191 (v2.1.11) | `restart_codex_app` 联动 `service.reinject()` — Codex Desktop 重启后立即触发 plugin_unlock daemon 重连(reset backoff),把解锁延迟从 5-8s 压到 ~1s | `src-tauri/src/admin/handlers/desktop.rs:1016-1033` |
| #194 (v2.1.11) | `resolve_codex_aumid` PowerShell 调用加 `CREATE_NO_WINDOW` flag 防黑窗 flash | `src-tauri/src/windows_msix.rs` |
| #201 (v2.1.11) | Windows quit / 进程清理改走 PowerShell CIM(`Get-CimInstance Win32_Process \| Invoke-CimMethod Terminate`)替 `taskkill /F`,绕 MSIX access-denied | `src-tauri/src/admin/handlers/desktop.rs::quit_command` |

借鉴出处:`BigPizzaV3/CodexPlusPlus`(MIT)— 已在 README + ACKNOWLEDGEMENTS.md 致谢。

### 修复后野外验证(0520-1 反馈)

2026-05-18 用户(v2.1.10)报"内置重启后 Plugins 未解锁",根因是 `restart_codex_app` v2.1.10 版本没联动 reinject,daemon 走自然 backoff(5-8s)retry,用户当成"不工作"。**v2.1.11 起 PR #191 的 reinject 联动已修**,延迟压到 ~1s。反馈归档于 `反馈/已解决/0520-1/`(详 fb-44c5eb6d),用户需升级到 v2.1.11+。

### ⏳ P2 — 剩余工作拆分到 Issue #226

Issue #226 跟踪两项:

1. **端口冲突探测**(对照 `BigPizzaV3 launcher.py:267-281` `SO_EXCLUSIVEADDRUSE`)— 用户 9222 端口被 Chrome / Edge / 其它 Electron 占用时,当前实现硬写 9222 → 注入失败。需要探测 + fallback OS 分配空闲端口
2. **非-Store .exe fallback**(Method 6)— 企业用户用直装 .exe(无 AUMID,COM activation 不可用)需要 fallback 到 `Command::new(exe).args(...).spawn()`(对照 `wallneradam/claude_autoapprove claude_autoapprove.py:161-163`)

本 followup `status: active` 不变(P2 未完成),`remaining_issue: 226` 字段指向剩余工作。两项都完成 + main merge 后,本 followup 移到 Resolved 段。

## 关联资源

- 触发 PR:#191(macOS P0 闪烁优化,本 followup 是它的"out of scope")
- 关联 issue:#190
- 关联 followup:[#32 macOS setAuthMethod React 重渲调研](32-plugin-unlock-react-context-rerender.md)
- 上游参考(按 evidence 强度排序):
  - **`BigPizzaV3/CodexPlusPlus`** (MIT, Python) — **本项目 1:1 翻译对照参考**,完整 CDP + COM ActivateApplication 实现
    - `codex_session_delete/launcher.py:283-451`(launcher 主体)
    - `codex_session_delete/app_paths.py:30-49`(MSIX 路径解析)
    - `codex_session_delete/cdp.py:53-91, 121-200`(CDP 注入)
    - `codex_session_delete/windows_installer.py:52-87`(Windows shortcut 安装,本项目暂不需)
  - [`b-nnett/codex-plusplus`](https://github.com/b-nnett/codex-plusplus) (MIT, TS) — ASAR 路线(不同道),仅 MSIX 安装位置探测可参考 `packages/installer/src/platform.ts:200-258`
  - [`emremigh/tradingview-mcp-windows-msix-fix`](https://github.com/emremigh/tradingview-mcp-windows-msix-fix) — 另一个 MSIX ActivateApplication 成功案例
  - [`zstnbb/PCE-Core ADR-018`](https://github.com/zstnbb/PCE-Core/blob/main/Docs/docs/engineering/adr/ADR-018-msix-store-app-capture-strategy.md) — Claude Desktop 失败案例 + 各方案对比 ADR
  - [`wallneradam/claude_autoapprove`](https://github.com/wallneradam/claude_autoapprove) `claude_autoapprove.py:161-163` — Method 6 非-Store 直装路径检测同模式
  - [Microsoft Learn: IApplicationActivationManager](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-iapplicationactivationmanager-activateapplication)
  - [openai/codex#21538](https://github.com/openai/codex/issues/21538) — 企业用户请求非 Store installer
