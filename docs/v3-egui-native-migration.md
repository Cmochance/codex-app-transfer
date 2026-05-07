# v3.0.0 — Tauri webview → egui 原生 widget 迁移

> **Status (截至 2026-05-07)**: W1-W7.2 ✅ + W7.3/W7.4 配置就位等 CI artifact 验证。
> 待办:
> - W7-验证(用户手动 `gh workflow run egui-bundle-test.yml`,跑出三平台 .app/.dmg/.exe/.deb/.AppImage)
> - W7.5 release.yml 整合(放 W8 或 W7-A/B 决策后)
> - W8 cutover(删 src-tauri/+frontend/、bump 3.0.0、合并)
>
> **目标**: 抛 Tauri webview,改用 `eframe + egui` 原生 widget,**体积减半 / 启动快 4 倍 / RAM 减 70%**,功能 1:1 等价。
>
> **PR 模式**: 全部 W1-W8 commits 都推到 [PR #46](https://github.com/Cmochance/codex-app-transfer/pull/46),**不合并**直到用户手测确认。
>
> **当前 commit 链(migrate/egui-native-ui 分支)**:
> ```
> 96a3cb8 ci: egui-bundle-test workflow + W7.3/W7.4 文档
> eab86f5 W7.2 mac bundle (cargo-packager) + eframe Linux fix
> 4406d6b W7.1 install_update_flow 真实下载
> 48209ed CI fix: rust-tauri-check 加 desktop_app
> b35d951 W6.2 system 集成
> 76e5eba W6.1 tokio runtime + 14 异步 action
> 0453e6d W5 Proxy + Desktop + Guide
> 2e3f8cf W4 Providers + Providers/Add
> 038d33d W3 Dashboard + Settings
> c234538 W2 骨架
> 8dd6b68 W1 抽 admin_api/proxy_runner/desktop_state
> aa37c88 docs(v3): 跟踪文档起步
> ```

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

- [x] Dashboard `#dashboard` — provider 卡片网格 + 三个 status hero(桌面/代理/当前 provider) + 行动按钮 + 最近活动 *(W3 ✅)*
- [x] Providers/Add `#providers/add` — name/baseUrl/apiKey/auth + 高级折叠(API format) + 模型映射 grid + preset 列表 *(W4 ✅)*
- [x] Providers `#providers` — 表头 + 可重排 provider 列表 + Claude 模型菜单切换 *(W4 ✅,reorder 用上下箭头替代 drag-drop)*
- [x] Desktop `#desktop` — config 列表 + JSON 预览 + apply/clear + 3 步 mini-step *(W5 ✅)*
- [x] Proxy `#proxy` — 启停 + 端口输入 + 实时日志面板(终端式)+ 自动滚动 + stats *(W5 ✅,TableBuilder virtualized)*
- [x] Settings `#settings` — 主题 / 语言 / 双端口 / 4 开关 / 兼容性检测 / 配置备份+导出+导入 / 反馈 / About *(W3 ✅)*
- [x] Guide `#guide` — 静态富文本(用 `egui_commonmark`)*(W5 ✅)*

### 2.2 三个 modal

- [x] deleteModal — "确认删除" *(W4 ✅)*
- [x] restartReminderModal — "立即重启 Codex App?" *(W5 渲染,W6.1 接 RestartCodex action)*
- [x] feedbackModal — textarea + 日志附加开关 + 提交 *(W6.1 ✅,截图上传留 W7+ 进阶)*

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

- [x] cas:// URI scheme — 解析 + argv 接入 *(W6.2 ✅)*;mac Info.plist 注册 *(W7.2 ✅)*;Win NSIS registry + Linux .desktop *(W7.3/W7.4 配置就位,需 CI artifact 验证)*
- [x] 系统托盘:动态 provider 切换 + 主窗口显隐 + proxy 启停 + 退出 *(W6.2 ✅,tray-icon 0.23)*
- [x] macOS native app menu:App + Edit + Window 三个 submenu *(W6.2 ✅,muda 0.19)*
- [x] 单实例(`single-instance` 0.3)*(W6.2 ✅,Unix 用 ~/.codex-app-transfer/.singleton.lock)*
- [x] 自动启动(`auto-launch` 0.6 + Settings auto_start 切换触发)*(W6.2 ✅)*
- [x] 自动更新:check + 下载 + 启动安装器 *(W7.1 ✅)*;atomic .app 替换辅助脚本 *(W7-A 测完决定要不要上)*

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

### W1: 抽 admin_api + desktop_state ✅ 完成

- [x] 新建 `crates/admin_api/`,把 `src-tauri/src/admin/{mod.rs, handlers.rs, state.rs, static_files.rs, registry_io.rs}` 整体搬过来
- [x] `crates/admin_api/Cargo.toml` 依赖 axum + serde + 现 4 crate + tower/bytes/include_dir/mime_guess/chrono/reqwest/sha2/base64/getrandom
- [x] `src-tauri/src/main.rs` 改成引 `admin_api::build_app_router` + `admin::handlers/registry_io` + `proxy_runner::ProxyManager`
- [x] 新建 `crates/desktop_state/`,定义 `Action` / `Effect` / `Model` 三个核心 enum / struct(W1 占位 scaffolding)
- [x] **额外抽出 `crates/proxy_runner/`** — ProxyManager 单独成 crate(183 行),admin_api 依赖,后续 desktop_app 也直接用,避免循环依赖
- [x] **额外动作**:`APP_VERSION` 从 `env!("CARGO_PKG_VERSION")` 改成 `OnceLock<&'static str>` + bin 启动时 `set_app_version()` 注入,11 个 site 全更新
- [x] workspace test 全绿(257+ tests,所有 admin handler 测试不变)
- [x] **现 Tauri app `make mac-app` 仍能 build 仍能跑**(已验证:dist/mac binary 27M,version 2.0.9,行为不变)
- [x] `cargo fmt --check` 通过

### W2: desktop_app 骨架 ✅ 完成

- [x] `crates/desktop_app/Cargo.toml` 依赖 eframe 0.31 + egui 0.31 + egui_extras + phf + serde
- [x] 主窗口 1024×700,标题"Codex App Transfer"
- [x] 7 个空 Page(每个 page 一个 placeholder render)+ 左侧 nav 切换
- [x] 主题切换器(Settings page 内)+ 7 主题 ThemeName::ALL + Palette 结构 + apply()
  - [x] Default / Dark 完整调色板(从 style.css 第 1-25 行逐字搬)
  - [ ] Green / Orange / Gray / White 完整调色板(W3 决策点 W2-A 时填充并审)
- [x] i18n:`build.rs` 读 `src/i18n/strings.toml`(267 keys 全自动从 i18n.js 抽出)→ phf::Map<&str, [&str; 2]> 静态表
- [x] `lookup_owned()` 函数 + `t!()` 宏
- [x] Locale enum + 切换器(Settings page)
- [x] `cargo run -p desktop_app` 能起空窗口(已 smoke test 验证 2 秒无 panic)
- [x] **不**动 src-tauri / frontend(回滚保护成立:`make mac-app` 仍然 build 出 v2.0.9)
- [ ] ⚠️ **决策点 W2-A**:延后到 W3。W2 只填 Default/Dark 两套真色板,Green/Orange/Gray/White 是占位;W3 填充齐 5 套色板后再做 A/B 审,信息更充分

### W2 量化首结

| 指标 | 现状 | 备注 |
|---|---|---|
| desktop_app release binary | **9.5 MB** | vs Tauri 27 MB(目标 ≤12 MB),已留充足余量 |
| workspace 全测 | 250+ tests 全绿 | adapters / proxy / codex_integration / registry / 等等不动 |
| Tauri app 旧版本 | `make mac-app` 仍出 v2.0.9 27MB | 回滚保护成立 |

### W3: Dashboard + Settings 完整 ✅ 完成

- [x] Dashboard:provider 卡片网格(从真实 ~/.codex-app-transfer/config.json 读)+ 桌面/代理/当前 provider 三个 hero + activity placeholder + 顶栏反馈/还原按钮
- [x] Settings 全 20 项渲染:
  - [x] Theme 单选(7 项, hover hint)
  - [x] Language toggle(zh/en)
  - [x] 双端口输入(DragValue 1024-65535)
  - [x] 4 个开关(autoApplyOnStart / restoreCodexOnExit / exposeAllProviderModels / autoStart)
  - [x] Update URL 单行输入
  - [x] Compatibility 检查按钮(渲染,W6 wire async)
  - [x] Backup / Export / Import 按钮(渲染,W6 wire)
  - [x] Feedback 按钮(渲染,W6 wire modal)
  - [x] About:版本 / License / 检查更新按钮 / 安装更新按钮
- [x] 这两页 sync action 全部接通(theme/language/ports/4 switches/updateUrl);async action(W6 接通)按钮已渲染但 W6 wire
- [x] **AppState 数据源**:registry crate 直接读写 ~/.codex-app-transfer/config.json,2 秒自动 reload,settings 改动立即写回
- [x] 6 套主题完整调色板(从 style.css `[data-theme-palette]` 1751-1853 行逐字搬,default/green/orange/gray/dark/white)
- [x] 5 个新 i18n key(proxy.notRunning / provider.active / provider.none / providers.default / providers.noApiKey)
- [x] ⚠️ **决策点 W2-A 现在到位**:7 主题完整,你可 \`cargo run --release -p codex-app-transfer-desktop-app\` 切换 7 主题 × 7 page 看色彩,告诉我哪些接受 / 哪些要调
- [ ] ~~决策点 W3-A~~ 与 W2-A 合并(本质同一个色彩审议)
- [ ] egui_kittest snapshot W4 加(需要先有 page 内容稳定,W3 末刚有实装)

### W3 量化(累计)

| 指标 | W2 末 | W3 末 |
|---|---|---|
| desktop_app release binary | 9.5 MB | **9.9 MB** | ≤ 12 MB(余 2.1 MB)|
| workspace tests | 250+ 全绿 | **250+ 全绿** | 不动 |
| Tauri 旧版 `make mac-app` | v2.0.9 27 MB | **v2.0.9 27 MB**(回滚保护成立)|

### W4: Providers + Providers/Add ✅ 完成

- [x] Providers/Add:完整表单 + 高级折叠(API format + auth scheme)+ mapping grid(6 slots: default + gpt-5.5/5.4/5.4-mini/5.3-codex/5.2)+ preset 列表(7 个内置 preset 渲染 + 一键填充)+ base_url options ComboBox(部分 preset 含多区域 URL)+ apiKey 显隐切换
- [x] Providers:provider 列表(表头 + 行)+ 上下箭头 reorder(替代 W6 drag-drop)+ 启用切换(set default)+ 编辑跳转(load_provider_into_form + nav_to_providers_add)+ 删除入口(经 deleteModal 确认)+ 添加按钮(右上角)
- [x] **deleteModal 实装**(W6 三 modal 中第一个落地;在 app.rs 用 egui::Window 渲染,confirm_delete_id 触发)
- [x] AppState 加 ProviderForm + presets cache + 6 个新方法(load_provider_into_form / save_form / delete_provider / set_default_provider / move_provider / fill_form_from_preset)
- [x] page::render 签名改 \`&mut Page\`,允许 page 内部跳转(state.nav_to_providers_add / nav_back_to_providers 信号)
- [x] 7 个新 i18n key(providers.cluster / providers.authScheme / providers.edit / providersAdd.formatTitle / common.show / common.hide / presets.use)
- [x] 累计 15/20 action 接通(W3 8 个 + W4 7 个:add provider / edit / delete / set default / move-up / move-down / save form)。剩 5 个 async / modal action 在 W6:apply-desktop / proxy-start/stop / clear-desktop / open-feedback / install-update / fetch-form-models / test-provider-form / check-update / check-provider-compatibility / backup-config / export-config / import-config(actually count is more,但都属于 W6)

### W4 量化(累计)

| 指标 | W3 末 | **W4 末** | 目标 |
|---|---|---|---|
| desktop_app release binary | 9.9 MB | **10 MB** | ≤ 12 MB(余 2 MB)|
| workspace tests | 250+ 绿 | **250+ 绿** | 不动 |
| Tauri 旧版 \`make mac-app\` | v2.0.9 27 MB | **v2.0.9 27 MB**(回滚保护成立)|

### W5: Proxy + Desktop + Guide ✅ 完成

- [x] Proxy:启停按钮(W6 wire ProxyManager)+ 端口输入(立即写回)+ stats 卡片(Total/Success/Failed,从 \`proxy_telemetry().stats.snapshot()\` 读)+ 实时日志面板(\`egui_extras::TableBuilder\` virtual scroll)+ 自动滚动开关(state.proxy_log_auto_scroll,默认 true)+ 清空 / 查看日志目录按钮
- [x] Desktop:状态摘要(applied/notConfigured)+ ~/.codex/config.toml 关键字段抽取列表(openai_base_url / model_catalog_json / model_context_window / model)+ apply/clear/restart Codex 三按钮(W6 wire) + JSON 预览 collapsing + 3 mini-step 引导
- [x] Guide:\`egui_commonmark\` 渲染 zh / en 双版 markdown(从原 frontend/index.html guide section 提取改写,~80 行内嵌 markdown)
- [x] **restartReminderModal 实装**(W6 三 modal 第二个;app.rs 加渲染逻辑,Desktop page 加 debug 触发按钮验收)
- [x] 累计 16/20+ action **渲染就位**(剩 async 的真正 wire 在 W6:apply/clear/proxy-start/stop/test/fetch-models/check-update/install-update/check-compat/backup/export/import/feedback/log-open-dir)
- [x] **顺手做的关键优化**:加 \`[profile.release]\` strip + lto=fat + codegen-units=1 + opt-level=z + panic=abort,**desktop_app binary 14 MB → 5.7 MB**(减 60%);Tauri 旧版同步 27 MB → 13 MB

### W5 量化(累计)

| 指标 | W4 末 | **W5 末** | 目标 |
|---|---|---|---|
| desktop_app release binary | 10 MB | **5.7 MB** | ≤ 12 MB(余 6.3 MB)|
| Tauri 旧版 binary | 27 MB | **13 MB** | n/a(顺手优化)|
| workspace tests | 250+ 绿 | **250+ 绿** | 不动 |

### W6: 系统集成

#### W6.1 ✅ — tokio runtime + 异步 action 全接通

- [x] `crates/desktop_app/src/background.rs` 新建(~600 行):`UiAction` 14 变体 + `BgEvent` 枚举 + `Bg` 结构(Arc<Runtime> + ProxyManager + mpsc unbounded channel + egui::Context)
- [x] `Bg::dispatch(UiAction)` 非阻塞;后台 task 完成后 `tx.send(BgEvent)` + `ctx.request_repaint()`
- [x] `drain_into(state, bg)` 主帧调用,把 BgEvent → state 改动 + Toast 队列
- [x] 14 个 action 全 wire:StartProxy / StopProxy / ApplyDesktop / ClearDesktop / RestartCodex / TestProvider / FetchModels / CheckUpdate / InstallUpdate / BackupConfig / ExportConfig / ImportConfig / SubmitFeedback / OpenLogDir / CopyToClipboard
- [x] **真实 IO**:apply_active_provider 经 codex_integration::apply_provider(&paths, &cfg)(传引用 + app_version 字段)、restore_codex_state、ProxyManager::start/stop、reqwest 探测 base_url/v1/models、reqwest GET latest.json、reqwest multipart POST 到 feedback worker、备份目录 `~/.codex-app-transfer/backups/config-YYYYMMDD-HHMMSS.json`
- [x] Toast 队列(4 秒自动消失,4 类 ToastKind:Info/Success/Warn/Error)右下角 Area 渲染
- [x] feedbackModal 实装(W6 三 modal 第三个):title/body 表单 + diagnostics 开关 + dispatch SubmitFeedback
- [x] restartReminderModal 真接通 RestartCodex action(已删 W5 占位 debug 按钮的隔离)
- [x] 7 个 page 全加 `bg: &Bg` 参数,按钮 click 改 `bg.dispatch(UiAction::...)`
- [x] cargo fmt --check 过;workspace tests 21 个 suite 全绿;smoke run 2s 无 panic
- [x] desktop_app release binary **6.6 MB**(W5 末 5.7 → 加 tokio/reqwest/multipart;余 5.4 MB to 12 MB target)

#### W6.2 ✅ — 系统集成(代码层全到位)

- [x] tray-icon 0.23 接通:动态 provider 菜单(签名变化 lazy rebuild)+ "显示/隐藏窗口" + "启动/停止 proxy" + 退出 + 占位单色 icon(W7 替换打包 PNG)
- [x] muda 0.19 macOS native menu:App submenu(About + Services + Hide/HideOthers/ShowAll + Quit)+ Edit submenu(Undo/Redo/Cut/Copy/Paste/SelectAll)+ Window submenu(Minimize/Maximize/Close)
- [x] single-instance 0.3:`acquire_single_instance()` 持锁直到进程退出;次实例启动直接 `eprintln!` + `exit(0)`(W6.2 暂不上 IPC URL 转发,W6-A 测完再决定要不要)
- [x] cas:// URI scheme 解析层完成:`parse_cas_url()` 支持 `cas://providers/add?baseUrl=&name=&apiKey=` / `cas://desktop/apply?provider=` / `cas://proxy/start` / `cas://proxy/stop`,5 个单元测试覆盖
- [x] argv 启动接入:`cas_url_from_argv()` 取启动参数中的 cas:// URL,首帧消化(预填表单 / 应用 provider / 启停 proxy)
- [x] auto-launch 0.6:`set_auto_launch()` 包装,Settings auto_start 切换时触发(初始化时记录已读 state 避免首帧无故 launchctl)
- [x] App 加 Tray + last_tray_signature + last_auto_start + pending_cas + window_visible 字段,update() 每帧 handle_initial_cas / handle_tray_events / maybe_rebuild_tray / sync_auto_launch
- [x] cargo build / smoke 2s / cargo test --workspace / cargo fmt --check 全过;binary 6.6 → 6.7 MB(目标 ≤12 MB,余 5.3 MB)
- [ ] ⚠️ **决策点 W6-A**:cas:// 三平台**注册流程**(macOS Info.plist + Windows registry + Linux .desktop)落到 W7 打包阶段,我会在 W7 里加 cargo-bundle / cargo-wix 的 URL handler 配置,然后给你三平台 build 各装一份测 `xdg-open cas://...` / `open cas://...` / `start cas://...` 是否拉起 app + 应用对应 action
- [ ] feedbackModal 进阶(rfd 截图上传 + diagnostics 自动注入应用日志)放 W6.3 / W7,W6.1 基础 multipart POST 已可用
- [ ] tray icon 替换正式 PNG(放 W7 打包阶段一起)
- [ ] cas:// 第二实例 IPC 转发(W6-A 测完根据用户体验决定是否上;若直接 launch 不打开第一实例 OK 就不必)

### W7: self-update + 三平台 CI

#### W7.1 ✅ — install_update_flow 真实下载 + 启动安装器

- [x] background.rs `install_update_flow(url)`:reqwest 下载安装器到
  `~/.codex-app-transfer/updates/{filename}`,然后 `opener::open(dest)`
  让 OS 自动接管(macOS Finder 挂载 .dmg / Windows 启动 .exe / Linux GNOME)
- [x] 替换 W6.1 的"在浏览器打开下载 URL"占位
- [x] 不主动退出当前进程 → 用户安装完手动重启;Tauri 旧版自动 quit + relaunch
  的辅助脚本留给 W7-A 测完根据用户体验决定是否上(目前 .dmg 拖到 Applications
  的标准 mac 流程已可用)

#### W7.2 ✅ — mac bundle(cargo-packager 0.11)

- [x] **W7-策略决策落:auto mode 选 B (cargo-packager)** — Tauri 团队 maintain,三平台 .app/.dmg/.nsis/.deb/.appimage 全覆盖,Cargo.toml metadata 集中配置
- [x] `[package.metadata.packager]` 写到 crates/desktop_app/Cargo.toml(productName / identifier / icons / formats / deepLinkProtocols / macos / deb / nsis 段)
- [x] mac .app + .dmg 本地验证(macOS host):
  - .app **9.7 MB**(MacOS bin 6.7MB + Resources 3.1MB icons);.dmg **10 MB**
  - Info.plist 含 CFBundleURLTypes 注册 `cas://` scheme(plutil 校验通过)
  - CFBundleIdentifier = `store.alyse.codex-app-transfer`(同 Tauri 旧版,签名链路不变)
  - LSMinimumSystemVersion = 11.0
  - .app 启动 smoke 通过
- [x] icons 复制到 crates/desktop_app/icons/(W8 删 src-tauri/ 不影响)
- [x] entitlements 复制到 crates/desktop_app/macos/(JIT + unsigned mem + library validation off,wgpu/Metal 需要)
- [x] Makefile `mac-app-egui` target:`cd crates/desktop_app && cargo packager --release -f app -f dmg`
- [x] **desktop_app 版本 0.1.0 → 3.0.0-pre**(锁住 cargo packager 输出文件名 `_3.0.0-pre_`)
- [x] **eframe Linux winit 编译错误修复**:`default-features = false` 关掉了 default 的 `["x11","wayland"]`,winit 找不到平台 backend → 显式补回(W6.2 push 后 CI 才暴露,跨平台靠 CI 才能验)
- [ ] codesign Developer ID + notarize 留给 W7.5 release.yml(本地 build 用 adhoc)
- [ ] 中文字体 subset 内嵌(W2 用系统 fallback,W7.5 时再决定要不要内嵌减少首启耗时)

#### W7.3 — Windows bundle(配置就位,需 CI 验证)

- [x] `[package.metadata.packager.nsis]` 配 `languages = ["English", "SimpChinese"]`
- [x] cargo-packager 默认 NSIS 输出 `Codex App Transfer_3.0.0-pre_x64-setup.exe`,xtask release-bundle 的 `-Windows-x64-Setup\.exe$` pattern **不匹配**(大小写+下划线差异),W7.5 时 rename or 改 pattern
- [x] cas:// URL scheme 通过 `[[deepLinkProtocols]]` 配置,cargo-packager 自动写 NSIS 安装器的 `WriteRegStr HKCU "Software\Classes\cas"`,无需手工
- [ ] **需要 Windows host 实测验证**:`gh workflow run egui-bundle-test.yml` 跑出 .exe → 装机 → 测 `start cas://providers/add?...`
- [ ] code-sign Authenticode 留给 W7.5 release.yml

#### W7.4 — Linux bundle(配置就位,需 CI 验证)

- [x] `[package.metadata.packager.deb]` 配 depends = libgtk-3-0 / libayatana-appindicator3-1 / libxdo3
- [x] cargo-packager 自动生成 .desktop 含 `MimeType=x-scheme-handler/cas;` for cas:// 注册(基于 deepLinkProtocols)
- [x] AppImage 走 cargo-packager 内置(底层 appimage-builder)
- [ ] **需要 Linux host 实测验证**:`gh workflow run egui-bundle-test.yml` 跑出 .deb / .AppImage → 装到 Ubuntu/Fedora → 测 `xdg-open cas://...`

#### W7-验证流水线 ✅ — 新加 dispatch-only workflow

- [x] `.github/workflows/egui-bundle-test.yml` 新建 — workflow_dispatch only,**不**绑 push/pr/tag
  - matrix:macos-latest / ubuntu-22.04 / windows-latest
  - 步骤:checkout + rust toolchain + sysdeps + `cargo install cargo-packager` + `cargo packager` + 上传 artifact
  - 不签名(adhoc only),用于 W7.3/W7.4/W6-A 验证
  - retention 14 天,artifact 名 `egui-{os}`
- [x] **不**改 release.yml,W2.0 老 release 链路完全不动(并行存活,W8 cutover 时整合)

#### W7.5 — release.yml 改造(放 W8 一起)

- [ ] 三平台 matrix 从 `cargo tauri build` → `cargo packager` (沿用 egui-bundle-test 的 build 步骤)
- [ ] xtask release-bundle 资产 pattern 检查:`-Windows-x64-Setup\.exe$` 大概率要改成 `_x64-setup\.exe$` 匹配 cargo-packager 输出
- [ ] **不 push 真 release tag**,先在 PR 上 workflow_dispatch 走 dry run
- [ ] 决定放在本 PR 还是 W8 cutover 后再改 release.yml(避免破坏 v2.x 后续小版本发布)

#### 验收

- [ ] 三平台 CI 全绿;artifact 下载手测装能用
- [ ] ⚠️ **决策点 W7-A**:跨版本自动更新 staging 测(v2.0.9 → v3.0.0-pre);
  目前 install_update_flow 依赖 latest.json 的 platforms.{plat}.url 指向 .dmg/.exe/.deb/.AppImage,
  W7.5 改完 release.yml 后会重出 latest.json,届时验证旧版 install-update 路径仍可工作
- [ ] ⚠️ **决策点 W7-B**:三平台 binary 体积达成 §1 表(mac ≤14MB / win ≤6MB);若未达成讨论权衡

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
