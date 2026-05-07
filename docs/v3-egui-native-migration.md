# v3.0.0 — Tauri webview → egui 原生 widget 迁移

> **Status**: 起步阶段(W0)。本文档是 PR 内单一推进 source of truth,所有 W1-W8 子任务都按本表格推进,完成后我在对应 commit 里更新本文档的 checkbox 状态。
>
> **目标**: 抛 Tauri webview,改用 `eframe + egui` 原生 widget,**体积减半 / 启动快 4 倍 / RAM 减 70%**,功能 1:1 等价。
>
> **PR 模式**: 全部 W1-W8 commits 都推到这一个 PR,**不合并**直到用户手测确认。

---

## 0. 决议参数(锁定)

| 项 | 选择 | 备注 |
|---|---|---|
| UI 框架 | `eframe` 0.31+(底层 `egui` + `wgpu`) | 纯 Rust,不引 DSL,与现有 5 crate workspace 类型直接共享 |
| 图形后端 | `wgpu`(eframe 默认) | macOS Metal / Windows DX12 / Linux Vulkan |
| 表格 / 网格 | `egui_extras::TableBuilder` | provider 列表 + 日志面板虚拟滚动 |
| 图标字体 | `egui_phosphor` | 6000+ glyph,与现 Bootstrap Icons 1:1 替换 |
| 中文字体 | Source Han Sans CN VF subset | ~1.5 MB,内嵌打包 |
| 等宽字体(日志) | JetBrains Mono subset | ~250 KB |
| 托盘 | `tray-icon` 0.17+ | 跨三平台 |
| 菜单栏(macOS) | `muda` | tray-icon 同作者,Cmd+Q/Cmd+H native |
| 文件对话框 | `rfd` | 配置导入用 |
| 剪贴板 | `arboard` | 复制环境变量命令用 |
| 自动更新 | `self_update` crate | 走现 `latest.json`,只换客户端 |
| 单实例 | `single-instance` | 替代 `tauri-plugin-single-instance` |
| 系统通知 | `notify-rust` | 后台事件用 |
| markdown | `egui_commonmark` | Guide 页静态富文本 |
| 包格式 | `cargo-bundle` (mac) + `cargo-wix`+NSIS (win) + `cargo-deb` + `appimage-builder` | 抛 Tauri bundler |

## 1. 体积 / 性能目标

| 指标 | v2.0.9(现 Tauri) | v3.0.0(egui)目标 | 验证方式 |
|---|---|---|---|
| Mac arm64 binary | 27 MB | **≤ 12 MB** | `ls -la dist/mac/.../MacOS/codex-app-transfer` |
| 整 .app bundle | 28 MB | **≤ 14 MB** | `du -sh "Codex App Transfer.app"` |
| Windows setup.exe | 9.5 MB | **≤ 6 MB** | release.yml artifact |
| 冷启动到主窗口可交互 | ~600 ms (WebView2) | **≤ 150 ms** | Instruments 启动 timeline |
| 切页响应 | ~80 ms (DOM rebuild) | **≤ 16 ms**(1 frame @ 60Hz) | egui frame_time 直方图 |
| 空闲 RAM | ~110 MB (WKWebView + WebContent) | **≤ 35 MB** | Activity Monitor |
| 日志面板滚动 10k 行 | DOM 卡顿 | **流畅 60 fps** | TableBuilder virtualized |

## 2. 当前应用功能盘点(迁移 reference,**逐条不漏**)

### 2.1 七个页面

- [ ] Dashboard `#dashboard` — provider 卡片网格 + 三个 status hero(桌面/代理/当前 provider) + 行动按钮 + 最近活动
- [ ] Providers/Add `#providers/add` — name/baseUrl/apiKey/auth + 高级折叠(API format) + 模型映射 grid + preset 列表
- [ ] Providers `#providers` — 表头 + 可重排 provider 列表 + Claude 模型菜单切换
- [ ] Desktop `#desktop` — config 列表 + JSON 预览 + apply/clear + 3 步 mini-step
- [ ] Proxy `#proxy` — 启停 + 端口输入 + 实时日志面板(终端式)+ 自动滚动 + stats
- [ ] Settings `#settings` — 主题 / 语言 / 双端口 / 4 开关 / 兼容性检测 / 配置备份+导出+导入 / 反馈 / About
- [ ] Guide `#guide` — 静态富文本(用 `egui_commonmark`)

### 2.2 三个 modal

- [ ] deleteModal — "确认删除"
- [ ] restartReminderModal — "立即重启 Codex App?"
- [ ] feedbackModal — textarea + 截图上传 + 日志附加开关 + 提交

### 2.3 二十个 action(逐条迁移)

| Action | 类别 | 异步? |
|---|---|---|
| `apply-desktop` | Codex 应用 + 复制环境变量 | 是 |
| `apply-provider-desktop` | apply + 设默认 | 是 |
| `backup-config` | registry::create_backup | 同步 |
| `check-provider-compatibility` | 串行 curl /v1/models 上报 | 是,SSE 推进度 |
| `check-update` | self_update fetch | 是 |
| `choose-import-config` | rfd file picker + import | 是 |
| `clear-desktop` | restore_codex_state | 是 |
| `clear-logs` | telemetry().logs.clear | 同步 |
| `export-config` | rfd save + export | 是 |
| `fetch-form-models` | curl `<base>/v1/models` | 是 |
| `install-update` | self_update download + 重启 | 是 |
| `open-feedback` | open feedback dialog | 同步 |
| `open-log-dir` | opener::open(log_dir) | 同步 |
| `proxy-start` | ProxyManager::start | 是 |
| `proxy-stop` | ProxyManager::stop | 同步 |
| `test-provider-form` | curl probe | 是 |
| `toggle-baseurl-menu` | UI state | 同步 |
| `toggle-key` | UI state | 同步 |
| `toggle-model-menu-mode` | PUT /api/settings persist | 是 |
| `view-logs` | 跳到 #proxy | 同步 |

### 2.4 二十八个 admin REST endpoint(全保留,headless 用)

```
GET  /api/version                   GET  /api/status
GET  /api/settings                  PUT  /api/settings
GET  /api/providers                 POST /api/providers
GET  /api/providers/compatibility   POST /api/providers/test
POST /api/providers/models/available
PUT  /api/providers/reorder
GET  /api/presets
GET  /api/proxy/status              POST /api/proxy/start
POST /api/proxy/stop                GET  /api/proxy/logs
POST /api/proxy/logs/clear          POST /api/proxy/logs/open-dir
GET  /api/desktop/status            POST /api/desktop/configure
GET  /api/desktop/snapshot-status   POST /api/desktop/clear
POST /api/desktop/restart-codex-app
GET  /api/config/backups            POST /api/config/backup
GET  /api/config/export             POST /api/config/import
POST /api/feedback                  POST /api/update/install
```

### 2.5 系统集成

- [ ] cas:// URI scheme(macOS Info.plist / Windows Registry / Linux .desktop)
- [ ] 系统托盘:provider 切换 + 主窗口显隐 + 退出
- [ ] macOS native app menu(Cmd+Q/Cmd+H/About)
- [ ] 单实例(`single-instance` crate)
- [ ] 自动启动(autoStart 设置 → `auto-launch` crate)
- [ ] 自动更新(`self_update` crate;沿用 latest.json)

### 2.6 七个主题

`default` / `green` / `orange` / `gray` / `dark` / `white` + 内置 dark mode 变体。
17 个 CSS `--xxx` 变量映射到 egui Visuals/Style。**颜色逐字搬,不重设计调色板**。

### 2.7 双语 i18n

zh / en 两套字典共 571 行(`frontend/js/i18n.js`)。逐 key 搬到 TOML,build.rs 用 `phf_codegen` 生成 O(1) 静态表。

## 3. 新 workspace 结构

```
crates/
  registry/          ← 保留(provider data + config IO)
  proxy/             ← 保留(axum proxy + SSE 适配)
  adapters/          ← 保留(协议转换)
  codex_integration/ ← 保留(Codex CLI apply/restore)
  admin_api/         ← W1 新建:从 src-tauri/src/admin/ 抽出
  desktop_state/     ← W1 新建:跨 desktop_app + admin_api 共享 state machine
  desktop_app/       ← W2 新建:eframe 主程序(替代 src-tauri)
xtask/               ← 保留;W7 扩展 bundle 任务
src-tauri/           ← W1-W7 期间存活共存,W8 删
frontend/            ← W1-W7 期间存活共存,W8 删
```

## 4. W1-W8 推进表

> 每个 W 完成时:更新对应 checkbox + push commit 到 PR + 在 PR comment 给 changelog + 把"决策点"标 ⚠️ 的项**先汇报后动手**。

### W1: 抽 admin_api + desktop_state

- [ ] 新建 `crates/admin_api/`,把 `src-tauri/src/admin/{mod.rs, handlers.rs, registry_io.rs}` 整体搬过来
- [ ] `crates/admin_api/Cargo.toml` 依赖 axum + serde + 现 4 crate
- [ ] `src-tauri/src/main.rs` 改成引 `admin_api::build_router`
- [ ] 新建 `crates/desktop_state/`,定义 `Action` / `Effect` / `Model` 三个核心 enum / struct
- [ ] workspace test 全绿(54 admin handlers test 不动)
- [ ] **现 Tauri app `make mac-app` 仍能 build 仍能跑**(回滚保护)

### W2: desktop_app 骨架

- [ ] `crates/desktop_app/Cargo.toml` 依赖 eframe + egui + egui_extras + egui_phosphor + tokio
- [ ] 主窗口 1024×700,标题"Codex App Transfer"
- [ ] 7 个空 Page(只有 placeholder 文本)+ 左侧 nav 切换
- [ ] 主题切换按钮 + 7 主题 Visuals 应用(`Theme::DEFAULT/GREEN/ORANGE/GRAY/DARK/WHITE`)
- [ ] i18n:`build.rs` + `crates/desktop_app/src/i18n/strings.toml`(含 571 行原表)+ phf 静态表 + `t!()` 宏
- [ ] `cargo run -p desktop_app` 能起空窗口
- [ ] **不**动 src-tauri / frontend
- [ ] ⚠️ **决策点 W2-A**:把 7 主题各自的 Dashboard 空骨架截图发给用户,确认色彩是否接受

### W3: Dashboard + Settings 完整

- [ ] Dashboard:provider 卡片(`provider-card-list`)+ 桌面/代理/当前 provider 三个 hero + activity list + 顶栏(反馈/还原)
- [ ] Settings:Theme 单选 + Language toggle + 双端口输入 + 4 个开关 + checkSnapshotStatus 状态 + Compatibility 列表 + Backup list + Feedback 入口 + About + 检查更新
- [ ] 这两页所有 data-action 全部接通(8/20 个)
- [ ] ⚠️ **决策点 W3-A**:Theme A/B 视觉对比 — 与现 webview 截屏并排对比给用户审,接受偏差范围
- [ ] egui_kittest snapshot 入仓库

### W4: Providers + Providers/Add

- [ ] Providers/Add:完整表单 + 高级折叠 + mapping grid(6 slots:default + gpt-5.5/5.4/5.4-mini/5.3-codex/5.2)+ preset 列表 + base_url options + apiKey 显隐
- [ ] Providers:可重排 provider 列表(rfd drag-drop 替代 SortableJS)+ 启用切换 + 编辑跳转
- [ ] deleteModal 实装
- [ ] 这两页所有 action 接通(7/20 累计 15/20)

### W5: Proxy + Desktop + Guide

- [ ] Proxy:启停 + 端口 + 实时日志(`egui_extras::TableBuilder` virtual scroll,10k 行流畅)+ 自动滚动 + stats
- [ ] Desktop:config list + JSON pre + apply/clear/restart Codex 三按钮 + 3 mini-step
- [ ] Guide:`egui_commonmark` 渲染原 guide 静态文案
- [ ] restartReminderModal 实装
- [ ] 累计 20/20 action 全部接通

### W6: 系统集成

- [ ] tray-icon:动态 provider 列表 + 主窗口显隐 + Quit
- [ ] muda macOS menu:Cmd+Q/Cmd+H/Cmd+W/About + 编辑菜单(Cut/Copy/Paste/SelectAll)
- [ ] cas:// URI scheme 三平台注册
- [ ] single-instance:第二实例把 cas:// URL 通过 ipc-channel 转发到第一实例
- [ ] auto-launch:autoStart 设置开关接通
- [ ] feedbackModal 实装(含 rfd 截图上传 + 日志附加 + multipart POST 到 feedback-worker)
- [ ] ⚠️ **决策点 W6-A**:cas:// 注册流程在三平台手测,需要你各装一份测一次

### W7: self-update + 三平台 CI

- [ ] self_update 替代 Tauri updater,沿用 latest.json
- [ ] xtask mac-bundle / win-msi+nsis / linux-deb / linux-appimage
- [ ] codesign / notarytool / WiX / RSA 自签 — 沿用现 secrets
- [ ] release.yml 改 `cargo tauri build` → `cargo bundle` 链路
- [ ] **不 push 真 release tag**;只验证 PR CI 三平台 build 跑通,artifact 下载手测装能用
- [ ] ⚠️ **决策点 W7-A**:跨版本自动更新 staging 测(用户旧 v2.0.9 装机 → 触发 self-update → 升到本 PR 出的 v3.0.0-pre);需要你手测确认才发外部
- [ ] ⚠️ **决策点 W7-B**:三平台 binary 体积达成 §1 表;若任一平台没达成,讨论权衡

### W8: cutover

- [ ] 删 `src-tauri/` 整目录
- [ ] 删 `frontend/` 整目录
- [ ] 改 `Cargo.toml` workspace.members:移除 src-tauri,加 desktop_app + admin_api + desktop_state
- [ ] 改 `Makefile` mac-app target 走 cargo bundle
- [ ] 改 `release.yml` 完成
- [ ] 改 `README.md` 介绍说明 v3 是原生 Rust
- [ ] 写 `docs/release-notes-v3.0.0.md`
- [ ] bump version 2.0.9 → 3.0.0
- [ ] **本 PR 此时 ready-for-merge**
- [ ] ⚠️ **决策点 W8-A**:你手测 → 你确认 → 你说 "merge",我才合(release tag 也等你说才打)

## 5. 风险登记 + 缓解

| ID | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | egui 主题做不到 Bootstrap 视觉精度 | 视觉退化 | W3-A 决策点截图 A/B 给用户 |
| R2 | Bootstrap modal 动画消失 | UX 略降 | egui::Window 自带 fade,接受 |
| R3 | macOS 公证后 cas:// 不工作 | 链接不响应 | W6-A 决策点测三平台,加 fallback 引导 |
| R4 | 中文 IME 候选窗错位 | 输入体验降 | eframe 0.31+ 已修主流问题,W3 测;加 `egui_inbox` fallback 监听 winit IME |
| R5 | 字体 subset 漏字符 | 显示豆腐块 | FontDefinitions fallback chain 加系统字体 |
| R6 | self_update 在 macOS 公证后无法替换 .app | 自动更新失败 | helper script 模式:下载 .dmg 提示用户挂载替换 |
| R7 | TableBuilder 10k 行卡 | 日志面板退化 | 启 `striped` virtualized;不达标改 ScrollArea 自实现 |
| R8 | Tauri 旧 artifact 自动更新链路被破坏 | 旧用户升级失败 | 沿用 latest.json schema + binary 名 + bundle ID |

## 6. 中途新增任务 / 优化(自动追加)

> 推进过程中发现需要的优化项,我会自动追加到本节,**不打断主线**;到达决策点时统一汇报,你裁决要不要本 PR 吸收。

(W0 起步阶段,空)

## 7. 决策点登记(到达时汇报)

| ID | 节点 | 决策内容 | 状态 |
|---|---|---|---|
| W2-A | 主题骨架做完 | 7 主题色彩是否接受 | 未到 |
| W3-A | Dashboard 完整后 | A/B 视觉差异接受范围 | 未到 |
| W6-A | 系统集成做完 | cas:// 三平台注册各自测试 | 未到 |
| W7-A | CI 跑通后 | 跨版本自动更新 staging 测 | 未到 |
| W7-B | CI 跑通后 | 三平台体积是否达成,任一未达讨论 | 未到 |
| W8-A | cutover ready | 你手测 + 确认 + 说 merge | 未到 |

## 8. 回滚预案

- W1-W7 期间 src-tauri/ + frontend/ 共存,`make mac-app` 默认仍能 build 旧 Tauri 版
- 每 W 完成 git tag `migrate-egui-Wn`(本地 tag,不推 remote)作为可回退点
- W8 切换前最后一刻 `pre-egui-cutover` tag(推 remote)
- W8 切换后发现 P0:`git revert <cutover-commit>` 一键回到 Tauri 版,恢复 src-tauri/ + frontend/

## 9. acceptance criteria(W8 末)

✅ 7 页全部存在且功能等价(逐条走 §2.1)
✅ 3 modal 都能正确打开/关闭/数据 round-trip(§2.2)
✅ 20 action 全部触发对应行为(§2.3)
✅ 28 admin endpoint headless curl 可用(§2.4)
✅ 7 主题切换实时生效(§2.6)
✅ zh/en i18n 切换无遗漏 key(build.rs 编译期检查)
✅ 系统托盘 provider 切换 + 重启 Codex 流程通(§2.5)
✅ macOS Cmd+Q/Cmd+H/Cmd+W 标准菜单
✅ cas:// 链接外部点击拉起应用并跳到对应页
✅ 自动更新 v2.0.x → v3.0.0-pre staging 链路通
✅ 二进制体积达成 §1 目标
✅ workspace 全套测试 + 新 egui_kittest snapshot 全绿

## 10. 通讯口径(W8 后 release notes 用)

> v3.0.0 是架构级变更:全 Rust 原生 widget,无 webview。
> 体积减半,启动快 4 倍,RAM 减 70%。
> 对用户:UI 视觉接近,功能 1:1,自动更新无缝。
> 对配置:配置文件 schema 不动,直接读旧 ~/.codex-app-transfer/config.json。
