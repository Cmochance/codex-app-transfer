# Codex App Transfer — Onboarding Runbook

> 这份 runbook 把 "代码看不出来但你必须知道" 的隐性知识写下来:release pipeline 怎么签名、macOS / Windows native 集成的坑、Plugin Unlocker 为什么走 CDP、provider 的奇葩 quirk、followup 制度怎么运作。
>
> **目标读者**:第一次接手这个项目的维护者(包括半年后忘了细节的你自己)。
>
> **配套阅读**:
> - 架构纪律 → `ARCHITECTURE_PROTOCOL_GUIDE.md`
> - 协议演进历史 → 维护者本地 `docs/`(gitignored,Phase 1/4 已 ship,Phase 5 Anthropic Messages active)
> - 构建流程 → 维护者本地 `docs/build.md`(release 细节)
> - Followup 制度 → Linear workspace `Mochance`(team `Mochance`,label `Improvement`)
> - 上游借鉴致谢 → `ACKNOWLEDGEMENTS.md`
>
> **关于 `docs/`**:整目录已 gitignored(2026-05-24)— 维护者本地放调研 / RFC / refactor / archive / 上游 reference / agent review / wire dump / 反馈分析,**不入 remote**。只有 `README*.md` / `CHANGELOG.md` / `AGENTS.md` / `ACKNOWLEDGEMENTS.md` / `ARCHITECTURE_PROTOCOL_GUIDE.md` / `ONBOARDING.md` 6 文件 + `release-notes/` + `img/` 在 git。

---

## 1. 项目是什么(60 秒)

**Codex App Transfer** 是一个面向 **OpenAI Codex CLI** 的本机 Tauri 桌面应用:

```
Codex CLI ──HTTP──► 本机 127.0.0.1:18080 (我们的 axum 网关) ──HTTPS──► 上游 provider
                    │
                    └─► 协议转换 (Responses ↔ Chat / Gemini / Anthropic / Grok Web)
                        + previous_response_id 历史回放
                        + 多轮工具调用 + autocompact 注入
                        + 模型映射 (gpt-5.4 → deepseek-v4-pro 等)
```

桌面 UI 不绑 HTTP 端口,走 Tauri 自定义 scheme `cas://localhost/` 把 webview 请求直接喂进同进程的 axum router(`src-tauri/src/main.rs:register_asynchronous_uri_scheme_protocol`),避免 v1.x 的 18081 admin 端口冲突。

---

## 2. 第一天:把环境跑起来

### 2.1 prerequisites

| 平台 | 必备 |
|---|---|
| 全平台 | Rust stable ≥ 1.80(`rust-toolchain.toml` 锁定),`cargo install tauri-cli --version "^2" --locked` |
| macOS | Xcode CLT(`xcode-select --install`) |
| Linux | `libwebkit2gtk-4.1-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf` |
| Windows | MSVC 工具链 + WebView2(Win11 自带) |

### 2.2 跑起来

```bash
git clone https://github.com/Cmochance/codex-app-transfer.git
cd codex-app-transfer

# 跑测试(无 Tauri 系统依赖,1-2 min)
cargo test --workspace --no-fail-fast --exclude codex-app-transfer

# 跑桌面 dev 模式(自动 reload)
cargo tauri dev

# 本地出 .app 自测(macOS)
make mac-app
# → dist/mac/Codex App Transfer.app
```

**第一次启动后**:配置文件落在 `~/.codex-app-transfer/`,日志在 `~/.codex-app-transfer/logs/proxy-*.log`,会话历史 sqlite 在 `sessions.db`(30 天 TTL)。

### 2.3 git worktree 工作流(必读)

主仓 `~/alysechen/github/codex-app-transfer/` 永远 checkout `main`,**禁止**在 main 直接 commit(分支保护 + `enforce_admins`)。所有改动用 worktree:

```bash
# 起新 feature
git worktree add ../codex-app-transfer-worktrees/<branch-name> -b <branch-name>

# 完成后 push + PR
cd ../codex-app-transfer-worktrees/<branch-name>
gh pr create --base main --title "..." --body "..."

# squash-merge 后清理
git worktree remove ../codex-app-transfer-worktrees/<branch-name>
git -C ~/alysechen/github/codex-app-transfer pull --ff-only
```

**Why**:多 PR 并发隔离编译产物,主仓 `target/` 不被搞脏;每次 squash-merge 后主仓 `main` 跟齐远端,worktree 一弃即清。

---

## 3. 仓库地图

```
codex-app-transfer/
├── src-tauri/                  # Tauri shell (二进制入口 codex-app-transfer)
│   ├── src/main.rs             # tauri::Builder + tray + cas:// 注册
│   ├── src/admin/              # 内部 axum router (/api/*)
│   │   ├── handlers/           # CRUD: providers / proxy / update / feedback / plugin_unlock ...
│   │   └── signature.rs        # 客户端验签 (RSA-3072 PKCS1v15 SHA-256)
│   ├── src/proxy_runner.rs     # ProxyManager: 起停 axum 网关 + 端口释放
│   ├── src/codex_plugin_unlocker.rs  # CDP 守护进程,见 §8
│   └── src/windows_msix.rs     # IApplicationActivationManager COM 启动 Codex Desktop
│
├── crates/
│   ├── adapters/               # ★ 协议转换核心 (97K 行 Rust 大头在这)
│   │   ├── src/core/           # 协议无关: routes / input / events
│   │   ├── src/mapper/         # 协议特有: chat / gemini_native / cloud_code / grok_web / anthropic_messages
│   │   ├── src/responses/      # Responses 协议适配 (request 5396 行 — 待拆)
│   │   ├── src/grok_web/       # Grok Web 反代 (实验性, TOS 灰色)
│   │   ├── src/{gemini_native,gemini_cli,anthropic_messages,openai_chat,passthrough}/
│   │   ├── src/registry.rs     # AdapterRegistry::lookup(api_format)
│   │   └── tests/              # contract + golden + streaming
│   ├── proxy/                  # axum server + forward / resolve / telemetry / fixture
│   ├── registry/               # ~/.codex-app-transfer/config.json schema + presets + healing
│   ├── codex_integration/      # 守护 ~/.codex/{config.toml,auth.json} 快照 / 还原
│   └── gemini_oauth/           # Gemini CLI + Antigravity OAuth flow
│
├── frontend/                   # 前端 (静态文件被 include_dir! 编进二进制)
│   ├── css/{tokens,base,responsive}.css + components/<name>.css + pages/<route>.css
│   ├── gallery.html            # 组件预览页 (不需 dev server, 直接 open)
│   └── js/api.js               # 调 cas://localhost/api/*
│
├── xtask/                      # release-bundle (签名 + latest.json) + gen-fixtures
├── release/                    # 内置公钥 PEM (build-time include_str!)
├── .release-signing/           # ★ private key (gitignored, secret 源)
│
├── docs/                       # ★ gitignored — 维护者本地放调研 / RFC / archive,不入 remote
├── release-notes/              # 每版 GitHub release body 模板(tracked,user-facing)
├── img/                        # README 截图(tracked,user-facing)
├── CHANGELOG.md                # 用户面向 release notes 索引(tracked,user-facing)
│
├── tests/replay/fixtures/      # 反向 diff fixture (xtask gen-fixtures 产生)
├── ARCHITECTURE_PROTOCOL_GUIDE.md  # ★ 架构纪律 (新协议必读)
├── AGENTS.md                   # AI agent 工作规范
├── ACKNOWLEDGEMENTS.md         # 上游借鉴致谢 (23KB, 法律安全网)
└── README.md / README.en.md    # 用户文档
```

---

## 4. 架构 30 秒速记

(详细版看 `ARCHITECTURE_PROTOCOL_GUIDE.md`,这里只放速记)

```
                  ┌─ core ──────────────────────────┐
                  │ routes.rs  input.rs  events.rs  │
                  │ (协议无关: 会话恢复 / SSE / 路由) │
                  └────────────────┬────────────────┘
                                   │ 共享
       ┌───────────────────────────┼───────────────────────────┐
       │                           │                           │
┌──────▼──────┐  ┌──────▼──────┐  ┌──────▼──────┐  ┌──────────▼──────────┐
│ mapper/     │  │ mapper/     │  │ mapper/     │  │ mapper/             │
│ chat.rs     │  │ gemini_     │  │ cloud_code  │  │ anthropic_messages  │
│             │  │ native.rs   │  │ .rs         │  │ .rs                 │
└──────┬──────┘  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘
       │                │                │                    │
┌──────▼──────┐  ┌──────▼──────┐  ┌──────▼──────┐  ┌──────────▼──────────┐
│ responses/  │  │ gemini_     │  │ gemini_cli/ │  │ anthropic_messages/ │
│ mod.rs      │  │ native/     │  │ mod.rs      │  │ mod.rs              │
│ (薄编排)    │  │ mod.rs      │  │ (薄编排)    │  │ (薄编排)            │
└─────────────┘  └─────────────┘  └─────────────┘  └─────────────────────┘
```

**铁律**:
1. **不依赖 provider 的逻辑**必须放 `core/`;**协议特有**逻辑必须放 `mapper/`;`adapter` 层**只做薄编排**。
2. 新协议进来必须经过 RFC(写 goal / 边界 / 风险 / 回滚)再写代码,不要静默加 if-else 分支。
3. canonical 协议名固定不变(`openai_chat`/`responses`/`gemini_native`/`gemini_cli_oauth`/`antigravity_oauth`/`grok_web`/`anthropic_messages`),老别名归一靠 `AdapterRegistry::lookup` 内的 match。

---

## 5. Release Pipeline(高 bus-factor 区,重点)

### 5.1 触发方式

```bash
# 推荐: tag 触发
git tag v2.1.12
git push --tags

# 备用: 手动触发 (适合 rc 测试)
gh workflow run release.yml -f version=2.1.12-rc1
```

### 5.2 必需 secret

仓库 Settings → Secrets and variables → Actions:

| Secret | 内容 | 缺失后果 |
|---|---|---|
| `RELEASE_PRIVATE_KEY_PEM` | RSA-3072 PKCS#8 PEM 私钥全文 | release-bundle job fail-fast |
| `APPLE_CERTIFICATE` (可选) | Developer ID .p12(base64) | macOS .dmg 降级 ad-hoc 签名 |
| `APPLE_CERTIFICATE_PASSWORD` (可选) | 上面 .p12 密码 | 同上 |
| `APPLE_SIGNING_IDENTITY` (可选) | `Developer ID Application: Foo (TEAMID)` | 同上 |
| `APPLE_API_KEY_BASE64` (可选) | App Store Connect .p8(base64) | 跳过 notarization,首次启动会被 Gatekeeper 拦 |
| `APPLE_API_KEY` / `APPLE_API_ISSUER` (可选) | 配套 ID | 同上 |

**Windows 签名**:暂未接入。后续接 EV/OV 证书走 `tauri.conf.json bundle.windows.signCommand`。

### 5.3 私钥从哪来

私钥就是 `.release-signing/release-private-key.pem`(本地 gitignored)。**首次** release 时如果还没生成:

```bash
# 跑一次,故意会失败(没产物),但路径上先生成 keypair
cargo run -p xtask --release -- release-bundle \
  --version 0.0.0-init --include macos --incoming-dir /tmp/empty-dir
# → 在 .release-signing/ 生成 keypair
# 把 release-private-key.pem 全文粘进 GitHub secret RELEASE_PRIVATE_KEY_PEM
```

公钥 `release/Codex-App-Transfer-release-public.pem` build-time `include_str!` 编进二进制(`src-tauri/src/admin/signature.rs`),客户端**只信这个公钥**,运行时不可替换 —— 故意为之,防 attacker 改本地 .pem 绕过验签。

### 5.4 签名算法对称性(改算法必读)

- 签名(CI 端):`xtask/src/release_bundle.rs::sign_file` 用 `rsa::Pkcs1v15Sign::new::<Sha256>()`
- 验证(客户端):`src-tauri/src/admin/signature.rs::verify_signed_bytes` 用同一 crate 同一版本
- **算法对称是硬约束**:`sha2` 必须带 `oid` feature,`rsa` 跟 `sha2` 版本两边对齐(`src-tauri/Cargo.toml` 注释里写明)

改任何一边都要同步改另一边,否则发出去的包客户端立刻验签失败。

### 5.5 macOS runner image — 历史血泪

**PR #75 教训**:误用了**已退役**的 `macos-13` runner。GitHub 在 runner 退役后会**静默丢弃** job(`status=queued, steps=[]` 但永远不分配机器),release 卡死 24h+ 无人发现。

当前 release.yml 用:
- `macos-14`(Apple Silicon arm64)
- `macos-15-intel`(Intel x64,GitHub 2025-12 后给 Intel 续命的新 label,官方支持到 2027-08)

**Bus-factor 风险**:这些 label 也会退役。每年 Q1 查一次 [GitHub Actions runner image changelog](https://github.blog/changelog/),提前换标签。

### 5.6 Release 发布流程(默认 draft)

`release.yml` 产出的 GitHub release **默认 draft**(`isDraft=true`)。

```bash
# 验证 build 成功后,先手测 assets:
gh release view v2.1.12

# 用户确认无问题后转 Latest:
gh release edit v2.1.12 --draft=false --prerelease=false --latest
```

**规则**(用户硬性偏好):
- 不主动转 Latest,等用户说"发为 Latest"
- 不主动改 release body 加 disclaimer / 退回声明
- 历史版本不动 body

### 5.7 Release notes 模板

`release-notes/<version>.md`(root,tracked)走严格模板(参考 `v2.1.7.md`):
- **单一 `###` 主题**(多修复合并成综合主题 + bullets)
- 允许 inline `code` / code block 区分技术名词
- **禁用** 粗体 / 斜体 / 删除线 / 中文引号等强调
- 单文件 ~3000 字符上限

---

## 6. CI 链路

```
push tag v*  / workflow_dispatch
        │
        ▼
 ┌───────────────────────────────────────────────────────────────┐
 │ matrix build (4 runner 并发)                                  │
 │   macos-14 (arm64) │ macos-15-intel │ ubuntu-22.04 │ win-latest │
 │   bundles=app,dmg  │ bundles=app,dmg│ deb,appimage │ nsis,msi   │
 └───────────────────────────┬───────────────────────────────────┘
                             ▼
              ┌──────────────────────────────────┐
              │ release-bundle (收口 job)        │
              │ download artifacts → xtask sign  │
              │ → .sha256 + .sig + latest.json   │
              │ → softprops/action-gh-release@v2 │
              │ → DRAFT GitHub release           │
              └──────────────────────────────────┘
```

### 6.1 PR CI(`.github/workflows/ci.yml`)

两层门禁:
1. **rust-fast-check**(always run,1-2 min,无 apt):fmt + workspace check/test(exclude `codex-app-transfer`)+ xtask gen-fixtures 反向 diff
2. **rust-tauri-check**(条件 run,仅 Tauri 路径变动):apt 装 webkit2gtk + cargo check/test `-p codex-app-transfer`

**Why 拆两层**:早期单一 job 每 PR 都 apt(~7 min),后来拆分让纯 doc / xtask / 非 Tauri crate 的 PR 1-2 min 跑完。

### 6.2 第三个 workflow:`no-ai-coauthor.yml`

仓库强制 commit 不带 `Co-Authored-By: Claude / GPT / AI` trailer 的 gate workflow。**Why**:用户硬性偏好 — contributor 列表不能混入 AI 账号稀释作者贡献统计。所有 commit 必须以 git user.name / user.email(GitHub 账号)署名。

### 6.3 ChatGPT Codex auto-review threads(必须处理)

仓库集成了 `chatgpt-codex-connector` auto-review bot,PR push 后会留 review threads。这些 thread **默认 unresolved**,如果分支保护开了 `required_conversation_resolution=true`,会**直接 block squash merge**(`mergeStateStatus: BLOCKED`,报错只说 "base branch policy prohibits the merge",不指明原因)。

**SOP**:PR push 后等 CI 启动期间立刻查:

```bash
gh api graphql -f query='
{
  repository(owner: "Cmochance", name: "codex-app-transfer") {
    pullRequest(number: PR_NUM) {
      reviewThreads(first: 50) {
        nodes { id isResolved path comments(first: 1) { nodes { author { login } body } } }
      }
    }
  }
}'
```

看到 `chatgpt-codex-connector` 留的 unresolved thread → reply + `resolveReviewThread` mutation 关闭。

---

## 7. 平台特有的坑

### 7.1 macOS — Gatekeeper Translocation

**问题**:macOS Gatekeeper 看到 `.dmg` 里的 `.app` 没 quarantine attr 清干净,会把 `.app` "translocate" 到 `/private/var/folders/.../AppTranslocation/<uuid>/` 只读临时挂载点运行。此时跑应用内置的 update 流程必然失败(目标只读)。

**实现**:`src-tauri/src/admin/handlers/update.rs:macos_translocation_precheck()`(followup #35a)— 检测当前 binary 路径含 `/AppTranslocation/` 就 hard fail,引导用户先把 `.app` 拖进 `/Applications/`。

**Bus-factor 笔记**:这个 precheck **必须在 update install 之前调用**(`update.rs:759`),不能依赖用户判断。绝不要做"试试看,失败回滚" — translocation 下连 read 都可能假成功。

### 7.2 Windows — MSIX Store 启动 + 参数剥离

**问题**:Codex Desktop 在 Windows 是 Microsoft Store 分发的 **MSIX packaged app**。老的 `explorer.exe shell:AppsFolder\<AUMID>` 启动协议在 OS 层面**剥离所有命令行参数**(微软官方 docs 明确记载)。所以 `--remote-debugging-port=9222 --remote-allow-origins=*` 静默丢失,Plugin Unlock daemon 永远连不上 CDP。

**解决**:`src-tauri/src/windows_msix.rs::activate_packaged_app` 走 Win32 Shell COM:

```rust
IApplicationActivationManager::ActivateApplication(aumid, args, AO_NONE, &out_pid)
```

`arguments` 是**单一 raw 命令行字符串**(不是 argv 数组),必须先按 Windows cmdline 规则 quote 拼好。借鉴 `BigPizzaV3/CodexPlusPlus` Python 实现(MIT),用 `windows` crate 官方 binding 而非手搓 ctypes。

**踩坑要点**:
- COM 必须 `COINIT_APARTMENTTHREADED`(Shell COM 硬性要求,不是 multi-threaded)
- AUMID 形如 `OpenAI.Codex_<publisher_id>!App`,用 `Get-AppxPackage -Name "OpenAI.Codex"` 反推 — 见 `windows_msix.rs::resolve_codex_aumid`
- 进程清理用 **PowerShell CIM**(`Get-CimInstance Win32_Process | Invoke-CimMethod Terminate`)而非 `taskkill`,绕 MSIX access-denied(PR #201)

### 7.3 Windows NSIS — 安装目录保留(PR #205)

NSIS installer 升级时默认会用全新路径,不读用户上次选的目录。已加 `/D=install_dir` 参数处理(followup #36)— 升级 NSIS 包必须验证用户旧目录被尊重。

---

## 8. Plugin Unlocker(CDP 守护)

### 8.1 目的

Codex Desktop 默认隐藏 `Plugins` 选项卡(只对 ChatGPT 账号开放)。Plugin Unlocker 通过 **Chrome DevTools Protocol** 注入 JavaScript 调用 React state 的 `setAuthMethod('chatgpt')`,把 Plugins 选项卡解锁出来,让用户在 Codex Desktop 里挂 MCP 服务。

### 8.2 工作流(`src-tauri/src/codex_plugin_unlocker.rs`)

```
1. 检测 Codex Desktop 进程是否存在
2. GET http://127.0.0.1:9222/json/list  ← CDP HTTP endpoint
3. 解析出 webSocketDebuggerUrl
4. WS 连接 → Page.enable + Page.loadEventFired 订阅
5. 注入 setAuthMethod JS
6. 监听刷新 → 自动重注入
7. 断开 → 指数退避重连 (1s → 30s 上限)
```

### 8.3 关键 followup 历史

| 编号 | 问题 | 状态 |
|---|---|---|
| #32 | setAuthMethod 触发 React 整树重渲(物理消除可行性) | PR #191 已 P0 缓解,长期消除需 hook Codex Desktop preload(跨版本不稳) |
| #33 | Windows MSIX 启动限制 — 6 方案对比 | PR #191/#201 已实施 COM activation + PowerShell CIM |
| #190 | macOS 首次开启 Plugins 锁定→刷新→解锁闪烁 | 已 fix |

**Bus-factor 警告**:Plugin Unlocker 强依赖 Codex Desktop 的 React 内部 state,Codex Desktop 任何 minor 升级都可能让注入 JS 失效。**每次 Codex Desktop 出新版必须真机回归**。

---

## 9. Provider 配置自愈(`crates/registry/src/healing.rs`)

**Why 这个文件存在**:v1.x 老配置 / 用户手改会让 `apiFormat` / `authScheme` / `extraHeaders` 字段缺失或错配,导致:

- **Kimi For Coding Windows 403**:`extraHeaders` 空 → 不注入 KimiCLI UA → Kimi 反爬
- **MiMo Token Plan 404**:`apiFormat` 缺失 → fallback `responses` → 直连上游(完全跳过代理,零日志)

**策略**(2026-05-08 之后):用户 `provider.baseUrl` 经 `normalize_base_url` 命中任一 builtin preset 的 baseUrl/baseUrlOptions 集合,就**强制覆盖**协议路由字段(`apiFormat` / `authScheme` / `extraHeaders` / `isBuiltin`),保留用户可改字段(`id` / `name` / `apiKey` / `models` / `baseUrl` 等)。

**改 preset 必读**:
- preset 字段定义 → `crates/registry/src/presets.rs`
- 强制覆盖范围 → `healing.rs::ENFORCED_BUILTIN_FIELDS`(目前 `apiFormat` + `authScheme` + `extraHeaders`)
- 加新字段进强制覆盖范围前必须想清楚:**用户是否可能想自定义**?如果可能,不能放进 `ENFORCED_BUILTIN_FIELDS`。

### 9.1 已知 provider quirk 速查

| Provider | Quirk |
|---|---|
| Kimi For Coding | 必须注入 `User-Agent: KimiCLI/...`,否则 Windows 403 反爬 |
| MiMo / DeepSeek | `apiFormat=openai_chat`,不支持 `/responses`,缺字段会 404 |
| MiniMax M2.x | `role=system` 必须转 `user` + 加 `[System]\n` prefix(PR #141,close #139)|
| DeepSeek V4 | tool_calls 缺 reply 必须用 `<repaired>` 占位补齐,否则 400(PR #182 #180) |
| Gemini 3 | `/v1alpha` endpoint(不是 `/v1beta`),Gemini 2.x 仍 `/v1beta`,自动选 |
| Grok Web | 实验性 / TOS 灰色,仅本机个人使用,cookie 鉴权,connector 走 server-side state |
| Anthropic Messages | canonical 名 `anthropic_messages`,老别名 `anthropic` / `claude` / `messages` / `claude_messages` 归一 |

---

## 10. 加新 Provider / 新协议(SOP)

### 10.1 加新 Provider(已有协议复用)

1. 改 `crates/registry/src/presets.rs::builtin_presets()`,加 preset JSON(`baseUrl` / `baseUrlOptions` / `apiFormat` / `authScheme` / `extraHeaders` / `models` 默认值)
2. 跑 `cargo test -p codex-app-transfer-registry` 验证 preset schema
3. 如有专属 header(反爬 UA 等)加进 `extraHeaders`
4. 在 README.md 兼容矩阵加一行(中英都要)
5. 用真机 config(`~/.codex-app-transfer/config.json`)跑一遍 ad-hoc test,验完删

### 10.2 加新协议(新 mapper)

按 `ARCHITECTURE_PROTOCOL_GUIDE.md §3` 严格执行:

1. **写 RFC**(维护者本地 `docs/protocol-unification-rfc-phase<N>.md`,`docs/` 已 gitignored):目标 / 边界 / 风险 / 回滚
2. 新建 `crates/adapters/src/mapper/<protocol>.rs` 实现 `RequestMapper` + `ResponseMapper` trait
3. 新建 `crates/adapters/src/<protocol>/mod.rs` 薄编排,**禁止**写复杂 provider 分支
4. 在 `AdapterRegistry::lookup`(`crates/adapters/src/registry.rs`)加 canonical name + 老别名归一
5. 补测试:
   - 单测:请求 / 响应 / 错误分支
   - 契约测试:`mapper/mod.rs` 共性断言
   - golden / replay:`tests/replay/fixtures/`
6. 跑 `cargo run -p xtask --release -- gen-fixtures` 反向 diff 验证 fixture 仍能从 Rust 重生成
7. 同 PR 改 README.md(中英)+ ACKNOWLEDGEMENTS.md(若借鉴上游)

### 10.3 借鉴上游开源 — 法律安全网

**铁律**:任何代码 / prompt / 架构借鉴自上游开源时,必须**同 PR**:
1. 在借鉴文件顶部 module doc 写明来源 + 上游 file:line + license
2. 在 `ACKNOWLEDGEMENTS.md` 加致谢段(中英)
3. README.md 致谢段同步加注明

已致谢的上游(`ACKNOWLEDGEMENTS.md` 23KB):
- `BigPizzaV3/CodexPlusPlus`(MIT)— Windows MSIX COM activation
- `router-for-me/CLIProxyAPI`(Go, MIT)— Gemini CLI OAuth wire
- `farion1231/cc-switch` — UI 借鉴
- `BerriAI/litellm` — 协议参考(`litellm/` 目录是只读参考目录,**禁修改**,见 `AGENTS.md`)

---

## 11. 调试入口

### 11.1 日志

- 默认路径:`~/.codex-app-transfer/logs/proxy-*.log`
- 格式:`tracing` 结构化日志,每个 error 带稳定 `error_id`(可 grep / 聚合)
- UI 内置日志面板,2 秒刷新

### 11.2 `error_id` 约定

任何用户可见错误必须 `tracing::warn!(error_id="xxx_yyy", detail=..., "msg")`:
- `error_id` 是**稳定 token**(改了等同 breaking change,运维 grep 会断)
- `detail` 字段放上下文(provider 名 / model / status code 等),不要拼进 msg

### 11.3 反馈弹窗

`src-tauri/src/admin/handlers/feedback.rs`:用户点反馈时自动附:
- 环境信息(OS / 版本)
- 脱敏 config(apiKey 替换为 `***`)
- 最近 N 条错误快照 + 完整请求 / 响应

不要往里加未脱敏字段。

### 11.4 多轮 / autocompact 调试

- 多轮历史 L1 内存 LRU + L2 sqlite(`~/.codex-app-transfer/sessions.db`,30 天 TTL)
- `previous_response_id` 漂移 / session miss 都会 `tracing::warn!` 带 `error_id`
- `core/input.rs` 是历史拼接入口,问题多发在这里

---

## 12. Followup 制度

**2026-05-24 起改走 Linear**(workspace `Mochance`,team `Mochance`)。任何"当前 PR 范围内不修但值得跟踪"的问题**必须**:

1. 开 Linear issue:`mcp__linear__save_issue(team="Mochance", labels=["Improvement"], priority=P1=1/P2=3/P3=4, state="Todo" 或 "Backlog")`
2. description 写够详细(触发上下文 + 问题描述 + 已有调研 + 风险 + 建议方向 + 关联资源,半年后回看不用重新调研,含 file:line / 真实数据样本路径 / 决策推导链)
3. 关联 PR / GitHub issue 挂 `links` 字段

**resolve 时**:`mcp__linear__save_issue(id="MOC-N", state="Done")`,body 末尾追加 `- resolved by PR #N (YYYY-MM-DD)`

**为什么这个制度重要**:这个项目唯一对抗 bus-factor 的机制就是这套 followup 制度。Cmochance 离开后,新维护者读 Linear backlog 就知道每个未决问题的完整背景。**不要省略任何条目**。

**历史制度**:2026-05-24 前用 `docs/followup-tracker.md` + `docs/followup/<id>-<slug>.md` 详情两级结构,迁 Linear + `docs/` 整目录 gitignored 后停用。旧详情文件维护者本地 `docs/archive/followup/` 仍可读。首批迁移 MOC-5..MOC-11 跨原 #32 / #39 / #40 / #41 / #42 / #44 / #45。

---

## 13. PR / Issue 流程

### 13.1 先开 issue 再起 PR

任何代码改动**先**`gh issue create` 写问题 + 方案,再起 PR:
- 多大改动都归**一个 issue**,多 PR 各 `Refs #N` / `Closes #N`
- issue body 给上下文:现状 / 期望 / 已知风险 / 建议方案
- PR description 简短指向 issue

### 13.2 PR 提交规范

- commit author 用 git user.name / user.email(GitHub 账号),**禁** AI Co-Authored-By trailer
- PR title 短(<70 字符),细节进 body
- body 必含 `## Summary` + `## Test plan` 两段
- push 后立即起 step-level CI 监控(`gh run watch` / Monitor),不询问
- 4 分 30 秒内必须有一次输出(查询或文本汇报),防 Anthropic prompt cache TTL 失效

### 13.3 微 PR 攒批

单行 yaml / typo / dep bump 等微改动**不立即 PR**:记 pending 攒到下一任务搭车,或 ≥5 条 / ≥7 天起一个 `chore: misc micro-fixes` PR。hotfix / security / 用户显式要求例外。

### 13.4 合并收尾流程

用户说"merge 收尾"时走固定 5 步:
1. squash-merge PR
2. macOS 把 worktree `dist/mac/*.app` 转主仓 `dist/mac/`
3. 主仓 `git pull --ff-only` 回 main
4. 删除远端 + 本地分支 + worktree
5. 关闭关联 issue

不在没听到指令时主动 merge。

---

## 14. 常见 gotcha

| 现象 | 根因 / 怎么查 |
|---|---|
| `cargo tauri build` Linux 失败 | apt 装 `libwebkit2gtk-4.1-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf` |
| Plugin Unlock 不工作 | `lsof -i :9222` 看 CDP 是否就绪;Win 看 AUMID 是否解出;真机 Codex Desktop 升级后必回归 |
| Release workflow job 永远 queued | **runner image 退役**!查 GitHub Actions changelog,不要默认假设排队 |
| `cargo run -p xtask -- release-bundle` 报 "no platform artifacts found" | 这是预期,首次跑生成 keypair 即可 |
| MiMo / Kimi 突然连不通 | 先查 `~/.codex-app-transfer/config.json` 里 `apiFormat` / `extraHeaders` 是否完整,缺失就是 healing 没生效 |
| 客户端验签失败 | `rsa` / `sha2` crate 版本是否两边对齐(`src-tauri` vs `xtask`),`sha2` 必须带 `oid` feature |
| `cargo tauri dev` 改前端不刷新 | frontend 是 `include_dir!` 编进二进制的,dev 模式仍走文件系统;改 `frontend/*.html|css|js` 不需重编译,改 Rust 需 |
| 单实例锁死(双开打不开) | `tauri_plugin_single_instance` + `fs2::FileExt` 跨进程 file lock 双层;查 `~/.codex-app-transfer/.lock`(或类似) |

---

## 15. 人 + 历史

### 15.1 主要贡献者

- **Cmochance** / **Xinlong Wu**(同一人,`.mailmap` 合并 alias)— ~98% commit
- Will Chen / cloudcollector — 个位数 commit
- 一些 PR 描述风格表明大量 AI 协作(Claude / Codex CLI / ChatGPT auto-review)

### 15.2 时间线速记

- **v1.x**(2026-05-01 前后)— Python backend + Tauri shell,有独立 18081 admin HTTP 端口
- **Phase 2**(2026-05 中)— 三平台 release pipeline 切到 GitHub Actions,删 Docker/Wine/PyInstaller
- **Phase 3**(PR #4)— 签名脚本从 Python 切到 Rust xtask
- **v2.0.0**— `cas://` scheme 同进程 admin,删除 18081 端口
- **Phase 4**(2026-05-11 完结)— adapters 引入 `core + mapper + thin adapter` 三层
- **Phase 5**(进行中)— Anthropic Messages 协议
- **v2.1.x**(当前)— Plugin Unlocker / latest.json 签名验签 / Gemini OAuth / Grok Web

### 15.3 ⚠️ 必读文档清单

接手前**一定**全读一遍:

1. `ARCHITECTURE_PROTOCOL_GUIDE.md`(架构纪律)
2. `CHANGELOG.md`(版本演进概览)
3. `ACKNOWLEDGEMENTS.md`(法律 / 上游借鉴)
4. `AGENTS.md`(AI 协作规范)
5. `README.md` + `README.en.md`(用户视角)
6. 本文档
7. Linear workspace `Mochance`(未决 followup 全景 — Todo / Backlog)
8. 维护者本地 `docs/build.md` + `docs/protocol-unification-rfc-phase{1,4,5}.md`(`docs/` gitignored;build 细节 + 架构演进 RFC)

---

## 附:30 天接手 checklist

第 1 周:
- [ ] 全平台都 build 一遍(本机 macOS + 借朋友 Windows 机 + Linux VM)
- [ ] 配一遍真实 provider(至少 OpenAI / Kimi / Gemini / Claude 各一)
- [ ] 跑通端到端:Codex CLI → 本工具 → 上游 → 回来
- [ ] 读完上面 8 份必读文档
- [ ] 把 `RELEASE_PRIVATE_KEY_PEM` 私钥**备份到自己的密码管理器**

第 2 周:
- [ ] 跑一次 rc release(`gh workflow run release.yml -f version=X.Y.Z-rc1`),全平台手测
- [ ] 真机 Codex Desktop 测 Plugin Unlocker(macOS + Windows MSIX)
- [ ] 验证 macOS translocation precheck(把 .app 留在 .dmg 里挂载点跑)
- [ ] 把 Linear workspace `Mochance` 的 Active(Todo / Backlog)followup 每条点开读详情

第 3-4 周:
- [ ] 拆 `crates/adapters/src/responses/request.rs`(5396 行)— 第一个大文件
- [ ] 拆 `src-tauri/src/admin/handlers/desktop.rs`(1778 行)— 第二个大文件
- [ ] 把 Active followup 减到 ≤10 条
- [ ] 跟 Cmochance(如能联系)做一次 1h 视频过架构关键点,把"代码看不出来的"补到本文档

---

**最后**:这份 runbook 是活文档,改了就 PR(跟代码同 PR 提交,见 `feedback_readme_with_pr` 规则)。半年后你忘了为什么这么写,翻到对应段加一句"Why: ..."就好。
