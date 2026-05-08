# Codex App Transfer

[![GitHub stars](https://img.shields.io/github/stars/Cmochance/codex-app-transfer?style=social)](https://github.com/Cmochance/codex-app-transfer/stargazers)
[![License](https://img.shields.io/github/license/Cmochance/codex-app-transfer)](LICENSE.txt)
[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-24C8DB?logo=tauri)](https://v2.tauri.app/)
[![Downloads](https://img.shields.io/github/downloads/Cmochance/codex-app-transfer/total?label=downloads)](https://github.com/Cmochance/codex-app-transfer/releases)

Codex App Transfer 是一个面向 **OpenAI Codex CLI** 的轻量配置和转发工具。它在本机起一个网关，把 Codex CLI 发出的 Responses API（HTTP 流式 / 非流式请求，含 `/responses` HTTP 回退）翻译成 Chat Completions 格式，再转发到你选择的供应商，比如 Kimi Code、Kimi 月之暗面、DeepSeek V4、Xiaomi MiMo、智谱 GLM、阿里云百炼等。

和 `farion1231/cc-switch` 这类偏 Claude Code / CLI 的 Anthropic 工具不同，本项目专注 OpenAI Codex CLI 的接入：用桌面界面管理供应商、模型映射、转发端口和日志，让 Codex CLI 可以无缝使用第三方 OpenAI 兼容的推理服务。

Windows 安装版和便携版默认会打开独立桌面窗口；浏览器地址只作为调试和备用入口。点击窗口关闭按钮时，应用会缩小到系统托盘继续运行；需要完全退出时，请右键托盘图标选择"退出"。

启动转发后，Codex CLI 通过本机 `127.0.0.1:18080` 与本工具通信。本工具在转换协议、改写模型名、保留 `previous_response_id` 历史的同时，把上游真实错误体写到日志面板，方便排查兼容问题。

## 项目状态

- 当前版本:**v2.0.11**(Python → Rust/Tauri 全栈重写后的当前主线;UI 视觉与 v1.0.4 字节级一致)
- 已验证供应商:Kimi Code(`kimi-for-coding` UA 网关)、Kimi 月之暗面(Moonshot Platform)、DeepSeek V4(官方 baseUrl,含「Max 思维」思考模式)、Xiaomi MiMo (Token Plan / Pay for Token)、MiniMax M2.x(OpenAI-compatible chat,自动剥不兼容字段 + `reasoning_split` + `<think>` 兜底)
- 实验兼容:智谱 GLM / 阿里云百炼 / 其它 OpenAI Chat 兼容反代
- v2.0.x 链路稳定性改动(累计到 v2.0.10):reasoning_content 历史回放 + 全局 `TOOL_CALLS_CACHE` 在历史压缩后重建缺失的 assistant.tool_calls;`response.in_progress` 事件按协议补齐;chat usage → responses usage 字段规范化(含 `cached_tokens` 默认补零,避免 Kimi/MiMo 重连卡顿);DeepSeek 视觉(image)输入剥离避免 400;vision 白名单按模型级精确匹配(替换之前的 provider 子串匹配);Kimi/DeepSeek thinking 在 Codex CLI TUI 显示头修复 + `extraHeaders` 支持 `{apiKey}` 模板(解决 Kimi 403);MiniMax 兼容(请求侧白名单清洗 `reasoning_effort`/`response_format`/`parallel_tool_calls` 等不支持字段、`tool` 剥 `strict`、连续 `system` 合并;响应侧 `reasoning_details` 拆 reasoning + `<think>` 兜底拆分仅对 MiniMax 启用);**Codex CLI 客户端身份头零泄漏**(出站剔除 `originator`/`x-codex-*`/`x-openai-*`/`chatgpt-account-id`/`session_id`/`thread_id`/`user-agent`,reqwest 默认 UA 改为中性 `Codex-App-Transfer/<v>`,彻底解决 Kimi For Coding Windows 403 access_terminated_error);**配置自愈**(load config.json 时按 `baseUrl normalize` 命中 builtin preset,**不依赖 `isBuiltin` 字段或随机 hex id**,命中即强制覆盖 `apiFormat / authScheme / extraHeaders` + 把 `isBuiltin=true`,admin 路径**写回磁盘**根治旧残留 —— 适配真机里 v1.x 升级遗留的"name/baseUrl 是 builtin 但 isBuiltin=false、id 是随机 hex"的老配置);**MiMo Token Plan 404 修复**(`apiFormat == "responses"` 不再被误读为"上游原生 Responses 透传"路由到 direct_provider,所有 provider 永远走 local_proxy,Responses↔Chat 协议转换由代理在本地完成);**`apiFormat` fallback 全栈统一为 `openai_chat`**(后端 add+update+import + 前端 mapProvider/providerBody/getPresets/选预设/编辑共 8 处对齐 schema serde default);**Responses 会话历史持久化**(`ResponseSessionCache` 双层架构:L1 内存 1000×60min LRU + L2 sqlite 30 天 TTL,落盘 `~/.codex-app-transfer/sessions.db`,Tauri 重启 / TTL 续期不丢历史 → ws incremental delta=0 续轮直接命中 cache 走 ws 路径,避免 stream retry + fallback HTTP 浪费);**Codex CLI ws warmup 不再打上游 + 立即关闭 ws 让客户端 fallback HTTP**(`server.rs` ws handler 识别 `generate: false` / 空 input 帧后直接发送 ws **Close frame**(连接关闭语义),Codex CLI 立即放弃 ws session 转 HTTP `stream_responses_api`。**注意**:之前 v2.0.11 早期用 `{"type":"error",...}` 文本帧被 Codex CLI 当流内错误处理,卡 ws idle timeout ~5 分钟才 fallback —— 实测反馈 fb-8f5b51fb / fb-0c121681 卡 4分48秒,改 Close frame 后秒级 fallback);**cache miss 错误体对齐 OpenAI SDK**(`previous_response_id` 失效返 HTTP 400 + `code: "previous_response_not_found"` 字面对齐 OpenAI 服务端真实行为,SDK / Codex CLI 错误处理路径直接复用);**user-facing 字符串改回英文**(API 响应 message / proxy telemetry log / tray menu / SDK 错误体全部英文,对齐 OpenAI/Anthropic SDK 错误处理风格;注释 / test 仍保留中文);Windows 重启 Codex 不再 flash 终端窗口 + 托盘菜单交互修复;启用按钮即时反馈,不再等转发回包

> 如果使用过程中出现问题，欢迎提交 PR 协助作者完善，会及时处理，非常感谢。

- 平台:v2.0.0 首发只发 macOS arm64;v2.0.1 起发布链路生成 macOS arm64 / Windows x64 / Linux x86_64 资产。v2.0.0 ~ v2.0.5 已归档为中间版本(仅源代码,不提供二进制,详见各 Release 页),生产请用 v2.0.11。早期 Python 版本 v1.0.x 已不再作为推荐稳定版,新用户直接用 v2.0.x
- 数据兼容:`~/.codex-app-transfer/config.json` 与 v1.0.4 字节级互通,可来回切换不丢数据

### 更新日志

逐版本变更详见 [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) 或 `docs/release-notes/v*.md`。当前管理 API 与代理入口状态见 [`docs/api-route-status.md`](docs/api-route-status.md)。

## 能做什么

- 管理多套供应商，按 OpenAI 模型名（gpt-5.5 / gpt-5.4 / gpt-5.4-mini / gpt-5.3-codex / gpt-5.2）映射到供应商真实模型 ID。
- 把 Codex CLI 的 Responses API 流式 / 非流式请求转换为 Chat Completions 格式后转发，多轮工具对话上下文 + 思维内容流式展开均已对齐 OpenAI Responses API 协议。
- 兼容 Codex CLI 0.126+ 的 `responses_http` 路径和 `/responses` 路由别名；当前 Rust 主线对外承诺的是 HTTP/SSE 转发入口。
- 自动把上游 `chatcmpl-...` 应答 ID 重写成 Codex CLI 校验通过的 `resp_...`，并保留 deployment affinity 编码；session_cache 查询前自动 decode 回原始 ID。
- thinking 开启的上游（Kimi / DeepSeek 等）三层防御：单空格占位 `reasoning_content`、`reasoning_summary_part` 标准协议事件、全局 `TOOL_CALLS_CACHE` 在历史压缩后重建缺失的 assistant.tool_calls。
- 自动归一化 `reasoning_effort`（`xhigh` / `max` → `high`，`auto` / `none` 直接丢弃），适配只接受 `minimal/low/medium/high` 的供应商。
- Codex CLI 原配置守护：apply 前自动快照 `~/.codex/{config.toml,auth.json}`，退出 / 下次启动按 key 智能合并还原；切到不需转发的 provider 自动停转发服务。
- 实时日志面板：每 2 秒自动刷新；提供"查看日志"按钮直接打开 `~/.codex-app-transfer/logs/`；"清除日志"按钮会把当前日志备份到 `logs/backup/` 后开启新日志，不直接删除文件。
- 中文 / 英文界面，浅色 / 深色 / 绿色 / 橙色 / 灰色 / 白色多种主题（仅深色会改变背景色）。
- Windows / macOS / Linux 系统托盘 + 跨平台单实例锁定(双击启动会自动唤起已有窗口)。

## 界面预览

| 仪表盘 | 供应商 |
|---|---|
| ![Board](docs/img/Board.png) | ![Providers](docs/img/Providers.png) |
| **设置** | **日志** |
| ![Settings](docs/img/Settings.png) | ![Logs](docs/img/Logs.png) |

### Codex CLI 实际接入

启用任意供应商后，Codex CLI 模型选择器会显示「<provider> / <real-model>」形式的真实模型名（v2.0.5 起所有供应商均如此），对话过程中工具循环 / `previous_response_id` 历史回放 / thinking 模式 reasoning_content 注入全部由本地代理透明处理：

![Codex CLI 实际对话](docs/img/codex-cli-real-chat.png)

## 下载

最新已发布版本在 GitHub Release：

```text
https://github.com/Cmochance/codex-app-transfer/releases/latest
```

推荐普通用户下载（v2.0.x 起 Tauri bundler 直出，不再有 PyInstaller 时代的 `.tar.gz` / `Portable.zip` / `.pkg` / 无后缀 Linux 单文件）：

- `Codex-App-Transfer-v<版本>-Windows-x64-Setup.exe`：Windows NSIS 安装版（推荐）
- `Codex-App-Transfer-v<版本>-Windows-x64.msi`：Windows MSI（企业 MDM / GPO 部署）
- `Codex-App-Transfer-v<版本>-macOS-arm64.dmg`：macOS Apple Silicon 拖拽安装
- `Codex-App-Transfer-v<版本>-Linux-x86_64.deb`：Debian/Ubuntu 系，自动拉运行时依赖
- `Codex-App-Transfer-v<版本>-Linux-x86_64.AppImage`：通用 Linux x86_64，免安装，`chmod +x` 直接跑

每个二进制都附带 `.sha256` 和 `.sig`（基于 `release/Codex-App-Transfer-release-public.pem` 的 RSA-3072 PKCS#1 v1.5 + SHA-256 签名）。`release/latest.json` 是版本元数据，供应用内"检查更新"读取。

Windows 版目前还没有 Authenticode 代码签名证书，系统可能提示未知发布者；可用 `.sha256` / `.sig` 校验下载完整性。

如果这个工具对你有帮助，欢迎 Star 一下不迷路。遇到问题、想新增供应商支持，或想反馈兼容性 issue，可直接发到 [Issues](https://github.com/Cmochance/codex-app-transfer/issues)。

## 基本用法

1. 启动 Codex App Transfer，弹出桌面窗口。
2. 在仪表盘点右上角加号 → 选择预设或自定义供应商，填入 API Base URL、API Key、模型映射。
3. 在"转发"页面点"启动转发"，本机 `18080` 端口开始监听。
4. 在 Codex CLI 配置文件（`~/.codex/config.toml`）里把 `base_url` 指向 `http://127.0.0.1:18080`，把 API Key 设为本工具显示的 Gateway API Key。
5. 重新打开 Codex CLI，模型选项就会自动列出当前供应商的模型映射。
6. **务必把 Codex CLI 切到 Full access(`/approvals` → "Full access")**。第三方 OpenAI-compatible provider(Kimi / DeepSeek / MiMo / MiniMax 等)在 Codex CLI 默认 `auto` 审批模式下,发起任何工具调用都会被卡在审批弹窗,长会话会因为单次工具调用超时 / 用户没及时点同意而失败。Full access 模式直接放行工具调用,这是接入第三方 provider 的**事实必要前提**。

如果桌面窗口无法打开，可以手动访问备用地址：

```text
http://127.0.0.1:18081
```

## English Quick Start

Codex App Transfer is a lightweight desktop app that turns OpenAI Codex CLI into a multi-provider client. It runs a local gateway, translating Codex CLI's Responses API requests (HTTP streaming / non-streaming requests, including the `/responses` HTTP fallback) into Chat Completions format and forwarding them to providers such as Kimi Code, Kimi Moonshot, DeepSeek V4, Xiaomi MiMo, Zhipu GLM, and Alibaba Cloud Bailian.

Unlike `farion1231/cc-switch` and similar Anthropic-oriented Claude Code tools, this project focuses on OpenAI Codex CLI: manage providers, model mapping, forwarding ports, and logs from a desktop UI so Codex CLI can talk to any third-party OpenAI-compatible inference endpoint.

The Windows installer / portable build opens a standalone desktop window by default; the local browser URL is only a debug fallback. Closing the window minimizes the app to the system tray; right-click the tray icon and choose "Exit" to fully quit.

### Project status

- Current version: **v2.0.11** (current mainline after the full Python → Rust/Tauri rewrite; UI byte-identical to v1.0.4)
- Validated upstream: Kimi Code (`kimi-for-coding` UA gateway), Kimi Moonshot (Platform API), DeepSeek V4 (official baseUrl, with "Max thinking" mode), Xiaomi MiMo (Token Plan / Pay for Token), MiniMax M2.x (OpenAI-compatible chat — incompatible fields auto-stripped, `reasoning_split` enabled, `<think>` tag fallback)
- Experimental compatibility: Zhipu GLM / Alibaba Cloud Bailian / other OpenAI Chat-compatible reverse proxies
- Platforms: v2.0.0 launched on macOS arm64 only; v2.0.1+ release builds produce macOS arm64 / Windows x64 / Linux x86_64 assets. v2.0.0 ~ v2.0.5 are archived as intermediate releases (source-only, no binaries — see each Release page); use v2.0.11 in production. Earlier Python v1.0.x line is no longer recommended for new installs — go straight to v2.0.x.
- Data compatibility: `~/.codex-app-transfer/config.json` remains byte-identical between v1.0.4 and the v2.x Rust mainline — switch back and forth without data loss
- v2.0.x stability rollups (cumulative through v2.0.11): reasoning_content history replay + global `TOOL_CALLS_CACHE` rebuilds missing `assistant.tool_calls` after history compaction; emits the protocol-correct `response.in_progress` event; normalizes chat-style `usage` into Responses-API `usage` (with `cached_tokens` defaulted to 0 to fix Kimi/MiMo reconnect stalls); strips DeepSeek vision (image) inputs to avoid upstream 400; vision allowlist switched from provider substring to per-model exact match; Kimi/DeepSeek thinking header rendering fix in Codex CLI TUI + `extraHeaders` `{apiKey}` template; MiniMax compatibility (request-side allowlist + `<think>` fallback split scoped to MiniMax only); **zero leakage of Codex CLI client identity headers** (strips `originator`/`x-codex-*`/`x-openai-*`/`chatgpt-account-id`/`session_id`/`thread_id`/`user-agent` on outbound; reqwest default UA changed to neutral `Codex-App-Transfer/<v>` — fixes Kimi For Coding 403 access_terminated_error on Windows); **configuration self-healing** (on load, providers whose `baseUrl` (normalized) matches any builtin preset are recognized as that preset — **independent of the `isBuiltin` field or random-hex ids** — and have `apiFormat / authScheme / extraHeaders` force-overwritten + `isBuiltin=true` set; the admin path **persists changes to disk** so legacy residue heals on first launch — fixes real-world v1.x configs where name/baseUrl matched a builtin but `isBuiltin=false` with random hex ids); **MiMo Token Plan 404 fix** (`apiFormat == "responses"` is no longer misread as "upstream-native Responses passthrough" routing to `direct_provider`; every provider always goes through `local_proxy`, Responses↔Chat conversion happens locally in the proxy); **`apiFormat` fallback unified to `openai_chat` stack-wide** (backend add+update+import + frontend mapProvider/providerBody/getPresets/preset-pick/edit, 8 sites aligned with schema serde default); **persistent Responses session cache** (two-tier `ResponseSessionCache`: L1 in-memory 1000×60min LRU + L2 SQLite 30-day TTL at `~/.codex-app-transfer/sessions.db`; survives Tauri restarts; ws incremental delta=0 follow-ups hit cache and stream over ws directly, avoiding the stream-retry + HTTP fallback waste); **Codex CLI ws warmup no longer touches upstream + closes ws so client falls back to HTTP** (`server.rs` ws handler detects `generate: false` / empty-input frames and sends a ws **Close frame** (connection-close semantics); Codex CLI immediately abandons the ws session and goes through HTTP `stream_responses_api`. **Note**: an earlier v2.0.11 build sent `{"type":"error",...}` text frames, which Codex CLI treated as a stream-level error and waited the full ws idle timeout (~5 minutes) before falling back — feedback fb-8f5b51fb / fb-0c121681 measured 4m48s; switching to Close frame brings fallback down to seconds); **cache miss error body aligned with OpenAI SDK** (`previous_response_id` not found returns HTTP 400 + `code: "previous_response_not_found"` literally matching OpenAI server-side behavior, so SDKs / Codex CLI error paths apply directly); **user-facing strings switched back to English** (API response messages / proxy telemetry log / tray menu / SDK error bodies all in English to align with OpenAI/Anthropic SDK error styles; comments / tests still in Chinese); Windows Codex restart no longer flashes a terminal window + tray menu interactions fixed; enable-button now gives instant feedback instead of waiting for the forwarder roundtrip.

### Changelog

Per-version changes are tracked at [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases) (and locally under `docs/release-notes/v*.md`). The current management API and proxy surface are indexed in [`docs/api-route-status.md`](docs/api-route-status.md).

### Getting started

1. Download the latest installer or portable package from [GitHub Releases](https://github.com/Cmochance/codex-app-transfer/releases/latest).
2. Open Codex App Transfer — a desktop window appears.
3. On the dashboard click the top-right `+` and pick a preset (or define a custom provider). Fill in the API base URL, API key, and model mappings.
4. Open the **Proxy** page and click `Start forwarding` — the app listens on `127.0.0.1:18080`.
5. In Codex CLI's config (`~/.codex/config.toml`), point `base_url` at `http://127.0.0.1:18080` and set the API key to the gateway API key shown in the app.
6. Restart Codex CLI; the model picker now lists the model mappings for the active provider.

> Note (v2): the management UI is now in-process via Tauri's custom `cas://` URI scheme. The v1.x debug fallback at `http://127.0.0.1:18081` no longer exists; use the desktop window (or restart the app to recreate it).

### What it does

- Manages multiple providers and maps OpenAI model names (`gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.2`) to each provider's real model ID.
- Translates Codex CLI's Responses API requests (streaming and non-streaming) to Chat Completions format before forwarding.
- Compatible with the Codex CLI 0.126+ `responses_http` path and the `/responses` route alias (no `/v1/` prefix); the current Rust mainline exposes an HTTP/SSE forwarding surface.
- Re-encodes upstream `chatcmpl-*` IDs into Codex-friendly `resp_<base64>` while preserving deployment affinity, so `previous_response_id` keeps working.
- For thinking-enabled upstreams (Kimi / DeepSeek), automatically attaches `reasoning_content` to historical assistant tool-call messages to avoid `400 thinking is enabled but reasoning_content is missing`.
- Normalizes `reasoning_effort` (`xhigh` / `max` → `high`; `auto` / `none` dropped) so providers that only accept `minimal/low/medium/high` won't reject the request.
- Live log panel auto-refreshing every 2 seconds, with an `Open log folder` button that jumps to `~/.codex-app-transfer/logs/`. The `Clear logs` button archives the active log to `logs/backup/` with a timestamp suffix instead of deleting it.
- Chinese / English UI with light, dark, green, orange, gray, and white themes (only the dark theme changes background colors).
- System tray on Windows / macOS / Linux + cross-platform single-instance lock (a second launch auto-focuses the existing window).

### Security notes

- Provider API keys are stored only in `~/.codex-app-transfer/config.json` — do not upload it together with `logs/` to a public repo.
- The forwarding service binds to `127.0.0.1` only and never hijacks the system proxy. The v2 management UI is served through Tauri's in-process `cas://` scheme instead of a loopback HTTP admin port; `X-CAS-Request` is still sent by the frontend for compatibility, but it is not the current security boundary.
- Backup / export JSON files contain plaintext API keys — keep them on trusted devices only.
- Logs append to `~/.codex-app-transfer/logs/proxy-YYYY-MM-DD.log`. Clearing logs archives them to `logs/backup/` with a timestamp suffix (no deletion).
- **Conversation history persistence (v2.0.11+)**: the proxy persists Responses-API session messages (mapped to `previous_response_id`) to `~/.codex-app-transfer/sessions.db` (SQLite, 30-day TTL) so Codex CLI long sessions survive app restarts without triggering `previous_response_not_found`. The DB contains complete chat history (system / user / assistant / tool messages); to wipe it, either call the admin endpoint `POST /api/sessions/clear` or delete the file directly.
- Windows builds are not Authenticode-signed yet; verify downloads with the published `.sha256` / `.sig` and the verification snippet below.
- This project is not affiliated with OpenAI, Anthropic, CC-Switch, or `farion1231/cc-switch`.

## 默认端口

- **本机转发服务**:`18080`,Codex CLI 通过它访问上游供应商
- **管理界面(v2)**:Tauri 自定义 `cas://` URI scheme + 同进程 axum,**不再绑定 18081 端口**。v1.4 的 18081 调试入口已移除,管理界面只能通过应用窗口访问

可在 设置 → 端口 修改转发端口,修改后需要重启转发。

## 本地开发(v2 / Rust)

前置:Rust 1.80+(`rustup`)、Tauri CLI(`cargo install tauri-cli --version "^2"`)、macOS 上需要 Xcode CLT。

```bash
git clone https://github.com/Cmochance/codex-app-transfer.git
cd codex-app-transfer

# 启动桌面窗口(开发模式,代码改动自动重编译)
cargo tauri dev

# 单独跑后端测试(不开窗口)
cargo test --workspace
```

`frontend/` 是 v1.4 同款 Bootstrap + 原生 JS,改任何 HTML/CSS/JS 文件后**刷新窗口**即可生效(Tauri 的 cas:// 协议会重新读取 `include_dir!` 嵌入的资源,但需要重新构建二进制——dev 模式下会自动)。

### Fixture 反向 diff(契约测试)

```bash
cargo run -p xtask --release -- gen-fixtures
git diff --exit-code -- tests/replay/fixtures/registry/
```

`tests/replay/fixtures/` 下的 JSON fixture 是字节级契约 (Phase 3 起由 `xtask gen-fixtures` 维护权威源, 之前是 Python `gen_registry_fixtures.py`)。Rust crate 的集成测试 (`crates/registry/tests/golden_compat.rs` + `crates/proxy/tests/streaming_passthrough.rs` 等) 会读这些 fixture 做 round-trip 验证;CI 强制 `git diff --exit-code` 闭环。

## 历史版本(v1.x / Python)

v1.0.4 及更早版本基于 Python 3.11+ + FastAPI + pywebview + PyInstaller。Phase 1-3 清理后,主线不再保留 `backend/` / `main.py` / `scripts/*.py` / `requirements.txt` / `pyproject.toml`;如需运行历史 Python 版本,请切到对应 tag 或 GitHub Release 的 source archive:

```bash
git checkout v1.0.4
pip install -r requirements.txt
python main.py
```

v2.0.0 起主线只维护 Rust 实现。历史 Python 代码只通过 v1.x tag / release source archive 保留。

## 打包(v2)

只需一条命令(macOS):

```bash
make mac-app
```

内部跑 `cargo tauri build --bundles app`,产出落到 `dist/mac/Codex App Transfer.app`。**单二进制 27MB**,不需要 Python 解释器、不需要外部 frontend/ 目录(全部 `include_dir!` 嵌入二进制)、不需要 Docker/Wine。

正式发布走 GitHub Actions 的 `release.yml` 工作流,由 Tauri bundler 生成三平台资产并由 `xtask release-bundle` 生成 `.sha256` / `.sig` / `latest.json`。本地 `make mac-app` 仅用于 macOS 自测;Apple Developer ID notarize 仍是后续工作。

### Windows / Linux

v2.0.0 首发只 ship macOS arm64;v2.0.1 起发布工作流会生成 Windows NSIS / MSI、Linux `.deb` / AppImage 和 macOS `.dmg`。Tauri 2 原生支持跨平台编译,代码层面没有平台特异(除 macOS Apple-specific 的 NSApp.hide / NSRunningApplication.activate 等已 `#[cfg(target_os = "macos")]` 隔离),`cargo tauri build` 在对应平台直接出 native 产物。

### 历史(v1.x / Python)打包

v1.0.4 之前用 PyInstaller + Docker + Wine + NSIS 三平台打包。相关旧脚本已从主线清理;如需复现 v1.x 打包流程,请切到对应 v1.x tag 或 release source archive。主线后续正式发布链路会在 v2.0.x 中切到 Tauri bundler。

### 签名校验

任意有 Python + cryptography 的环境里都能验签 (Python 3.7+, `pip install cryptography`)。本仓库已不再依赖 Python, 但发布产物的验签协议是 RSA-3072 PKCS#1 v1.5 + SHA-256 RFC 标准, Python `cryptography` 是最方便的验证工具之一 (OpenSSL CLI / Rust `rsa` crate 也可):

```bash
python -c "
from pathlib import Path
import base64
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding
pub = serialization.load_pem_public_key(Path('release/Codex-App-Transfer-release-public.pem').read_bytes())
asset = 'release/Codex-App-Transfer-v2.0.6-macOS-arm64.dmg'  # 替换成你下载的产物
sig = base64.b64decode(Path(asset+'.sig').read_text())
pub.verify(sig, Path(asset).read_bytes(), padding.PKCS1v15(), hashes.SHA256())
print('OK')
"
```

## Troubleshooting

### Codex CLI 提示 `404 Not Found url: http://127.0.0.1:18080/responses`

老版本只有 `/v1/responses`，Codex CLI 0.126 起会回退到 `/responses`（不带 `/v1/`）。本工具已加路由别名，更新到 v1.0.1+ 即可。

### Codex CLI 提示 `stream disconnected before completion`

通常是 `response.id` / `response.model` 没有按 Codex CLI 期望填回。本工具会把上游 `chatcmpl-...` 重写成 `resp_<base64>` 并保留请求模型名，请确认转发日志中确实看到了 `resp_...` 而不是 `chatcmpl-...`。

### 上游 400：`thinking is enabled but reasoning_content is missing in assistant tool call message`

Kimi / DeepSeek 在开启 thinking 后强制要求历史中带 tool_call 的 assistant 消息提供 `reasoning_content`。v1.0.1 已自动补默认空字符串，并把 Responses 输入里的 reasoning items 映射到对应 assistant 消息。如果仍出现，请抓一份转发日志反馈。

### 上游 400：`'reasoning_effort' does not support 'xhigh'`

Codex 用户配置里若把 `model_reasoning_effort` 设成 `xhigh` / `max`，本工具会自动降级到 `high`。`auto` / `none` 等 Chat 端不接受的值会被丢弃。

### 端口冲突

v2 默认只监听 `18080`(转发);管理界面已改走 Tauri 同进程 `cas://` scheme,不再占用 18081 端口。如果 18080 被占:

```powershell
netstat -ano | findstr :18080
```

发现占用后，可以关闭占用进程，或在 设置 → 端口 改成空闲端口后重启转发。

### Windows 提示未知发布者

当前 Windows 构建还没有 Authenticode 代码签名证书，所以 Windows 可能提示未知发布者。Release 页面提供 `.sha256` 和 `.sig`，可用于校验安装包没有被替换。

### 日志去哪了

- 应用界面：转发页面下方实时面板，每 2 秒自动刷新。
- 磁盘文件：`~/.codex-app-transfer/logs/proxy-YYYY-MM-DD.log`，可点"查看日志"按钮直接打开。
- 清除日志：把当前日志移到 `logs/backup/` 并加时间戳后缀，不直接删除。

## 技术栈(v2)

- **后端 / 转发**:Rust 1.80+ · axum 0.8 · reqwest 0.12 (rustls-tls) · tokio
- **协议适配**:`crates/adapters` —— Responses ↔ Chat 互转(请求 body + 流式响应状态机,支持 reasoning_content / tool_calls)
- **前端**:HTML + CSS + 原生 JavaScript + Bootstrap 5.3.3(本地化,无 CDN 依赖)—— 与 v1.0.4 **视觉字节级一致**
- **桌面壳**:Tauri 2 + tray-icon 0.23,通过自定义 `cas://` URI scheme 把 frontend/ 与 axum 同进程串起来,无 TCP loopback
- **存储**:`~/.codex-app-transfer/config.json`(配置,与 v1.x 字节级兼容)、`~/.codex/{config.toml,auth.json}`(Codex CLI 集成)、`~/.codex-app-transfer/codex-snapshot/`(apply 前的备份快照)
- **打包**:`cargo tauri build` 单命令 → `dist/mac/Codex App Transfer.app`(27MB,unsigned 自测;v2.0.x 起接 Apple Developer ID + notarize)

## 重写过程

v2.0.0 是从 v1.0.4 (Python) 一次性重写而来,完整过程(7 阶段 + 30+ 修订日志)记录在 [`docs/refactor/migration.md`](docs/refactor/migration.md),核心结论 + 量化对比 + 关键 bug 修复见 [`docs/release-notes/v2.0.0.md`](docs/release-notes/v2.0.0.md)。

## 安全说明

- API Key 仅保存在本机 `~/.codex-app-transfer/config.json`，不要把它和 `logs/` 一起上传公开仓库。
- 转发服务只监听 `127.0.0.1`，不接管系统代理；v2 管理界面通过 Tauri 同进程 `cas://` scheme 提供，不再暴露 loopback HTTP 管理端口。前端仍会发送 `X-CAS-Request` 兼容头，但当前安全边界是 Tauri 自定义协议，而不是这个请求头。
- 备份 / 导出配置的 JSON 文件包含 API Key 明文，仅在可信设备上保存。
- 代码签名公钥位于 `release/Codex-App-Transfer-release-public.pem`，可用上文"签名校验"里的 Python 片段验证下载完整性。

## 致谢

本项目站在前人的肩膀上：

- **[CC-Switch](https://github.com/farion1231/cc-switch)** 提供了"轻量桌面 + 一键切换 API 提供商"的产品形态启发。
- **[CC Desktop Switch](https://github.com/lonr-6/cc-desktop-switch)** 提供了完整的桌面应用框架——pywebview 桌面壳、pystray 托盘、FastAPI 双端口（管理 / 转发）布局、PyInstaller / NSIS 打包脚本、`scripts/New-Release.ps1` 发布签名链路、GitHub Actions 自动构建工作流，以及 i18n / 主题 / 设置面板等前端模板都直接沿用了它的实现。
- **[litellm](https://github.com/BerriAI/litellm)** 提供了 Responses API ↔ Chat Completions 双向协议转换的核心思路。v1.x 的 `backend/responses_adapter.py` / `backend/openai_adapter.py` / `backend/base_adapter.py` 以及 v2.x 的 `crates/adapters/` 都直接参考了 litellm 的字段映射、消息归一化和 reasoning 处理策略。
- **[Tauri](https://tauri.app/)** 提供了 v2.0 桌面壳的全部基础设施 —— 单二进制打包、native webview、tray、IPC、单实例插件、自定义 URI scheme。v2.0 的"frontend/ 零改动 + cas:// 同进程 axum"架构靠 Tauri 2 的 `register_asynchronous_uri_scheme_protocol` 才能成立。

### 社区贡献者

感谢以下贡献者通过 Pull Request 直接改进过本项目(按首次提交时间倒序;完整列表见 [Contributors 图表](https://github.com/Cmochance/codex-app-transfer/graphs/contributors)):

- [@lukegood](https://github.com/lukegood) — MiniMax M2.x OpenAI-compatible 接入兼容性(请求侧字段白名单清洗 + 连续 system 合并 + tool `strict` 剥离;响应侧 `reasoning_details` 拆 reasoning + `<think>` 兜底;[#47](https://github.com/Cmochance/codex-app-transfer/pull/47))
- [@cw881014](https://github.com/cw881014) — 早期协议层修复:DeepSeek thinking 工具历史回放([#1](https://github.com/Cmochance/codex-app-transfer/pull/1))、Responses reasoning schema 与历史修复对齐([#7](https://github.com/Cmochance/codex-app-transfer/pull/7))、Codex 0.128 model catalog 路由加固([#12](https://github.com/Cmochance/codex-app-transfer/pull/12))

如果你提交过 PR 但希望改名 / 补链接 / 移除,直接开 issue 跟我说一声即可。

本项目专注 OpenAI Codex CLI 接入，不是 OpenAI、Anthropic、CC-Switch 或 `farion1231/cc-switch` 的官方项目，也不复用它们的商标、Logo 或发布身份。

## 许可证

MIT License。完整文本见 [LICENSE.txt](LICENSE.txt)。
