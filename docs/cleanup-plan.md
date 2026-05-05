# Python → Rust 旧码清理方案

> 状态:**已确认,启动 Phase 1**
> 起草:2026-05-05
> 适用范围:`codex-app-transfer` 全仓
> 前置:`docs/migration-plan.md` 全部 7 个 Stage 已落地,v2.0.0 已发版

---

## 1. 背景

v2.0.0 完成 Python → Rust/Tauri 全栈重写后,仓库里仍残留两类旧码:

1. **已被 Rust 完全替换的死码**:`backend/`、`main.py`、PyInstaller spec / NSIS / 老 PS1 脚本等。
2. **形式上是 Python 但仍在产线上的工具**:`scripts/release_assets.py`、`scripts/gen_registry_fixtures.py`、`tests/replay/`、PyInstaller release pipeline (`make {mac,linux,win}-release`)。

直接一次性切换会砍断签名 release 通道。需要分阶段推进,每个 Phase 一次 PR(走仓库的 main 分支保护流程)。

## 2. 现状盘点

### 活码(保留)

| 路径 | 角色 |
|---|---|
| `src-tauri/` | Tauri 2 壳 + 内嵌 axum admin server + cas:// scheme |
| `crates/{registry,proxy,adapters,codex_integration}` | ~7k 行 Rust,替代 backend/*.py |
| `frontend/` | Bootstrap + 原生 JS,字节级保留,通过 `include_dir!` 编进二进制 |
| `feedback-worker/worker.js` | Cloudflare Worker(独立微服务,不进 app 二进制) |

### 待清理 Python / 老打包

| 类别 | 路径 | 仍被引用? |
|---|---|---|
| 旧后端 | `backend/` 全部 18 个 .py | 不被 Rust 引用,但 `macos/build-macos.sh` / Dockerfile / Makefile 还在导版本号 |
| 旧入口 | `main.py` (root) | 仅老 PyInstaller 用 |
| 老打包 | `build.spec`、`build.bat`、`start.bat`、`installer.nsi` | 仅老路径用 |
| 老 Mac 打包 | `macos/build-macos.{sh,spec}`、`make-{dmg,pkg}.sh`、`prepare-icon.py`、`entitlements.plist` | **`make mac-release` 仍在调** |
| 老跨平台打包 | `docker/{linux,windows}-builder/Dockerfile`、`scripts/build-{linux,windows}-on-mac.sh` | `make {linux,win}-release` 仍在调,基于 PyInstaller + Wine |
| 老签名 | `scripts/{Invoke-CodeSigning,New-Release,Test-ReleaseSignature}.ps1` | 老 PowerShell 路径 |
| 老 Python 测试 | `tests/test_{deepseek_thinking_tool_history,isolation,kimi_real,tool_call}.py` | 全部 import `backend.*`,backend 删掉就死 |
| 配置/缓存 | `requirements.txt`、`pyproject.toml`、`.venv/`、`.pytest_cache/`、`Codex-App-Transfer-Setup-1.0.3.exe` | 残留 |
| 跨语言契约 | `scripts/gen_registry_fixtures.py`、`tests/replay/{fixture,player,recorder}.py`、`tests/test_replay_smoke.py` | **CI 还在跑,`crates/registry/tests/python_compat.rs` 依赖产物** |
| Release 打包 | `scripts/release_assets.py` | **`make *-release` 还在调,签名 + latest.json 由它生成** |
| Tauri 配置 | `src-tauri/tauri.conf.json` 里 `"version": "2.0.0-stage0"` | 跟 README v2.0.0 不一致 |
| Workspace | `Cargo.toml` 注释里还提 `ui` crate (Leptos),实际没目录 | 死注释 |
| CI | `.github/workflows/ci.yml` 里 `python-replay-tests` job + `ui-wasm-build` job(目录已不存在) | 半死 |

## 3. 分 Phase 清理方案

### Phase 1 — 无争议直接删

零风险,只是把 v2 路径下已经没用的东西干掉。**目标**:删掉 backend/ + 老 PyInstaller spec + 老 Python 集成测试 + Setup .exe + 老 PS1 脚本 + 死 CI job + tauri 版本号修正。

清单:
- `backend/` 全目录(18 个 .py)
- `main.py`(root)
- `tests/test_deepseek_thinking_tool_history.py`、`test_isolation.py`、`test_kimi_real.py`、`test_tool_call.py`
- `build.spec`、`build.bat`、`start.bat`、`installer.nsi`
- `Codex-App-Transfer-Setup-1.0.3.exe`(committed 二进制)
- `scripts/Invoke-CodeSigning.ps1`、`New-Release.ps1`、`Test-ReleaseSignature.ps1`
- CI workflow:删 `ui-wasm-build` job(`ui/` 目录已不存在)
- `Cargo.toml`:删 `# ui` 死注释
- `src-tauri/tauri.conf.json`:`2.0.0-stage0` → 真实版本号

**不删**(留给后续 Phase):
- `requirements.txt`、`pyproject.toml`(release_assets.py 还要用)
- `.venv/`、`.pytest_cache/`(本来就在 .gitignore)
- `macos/build-macos.*` 等 release pipeline(Phase 2)
- `scripts/gen_registry_fixtures.py`、`tests/replay/`(Phase 3)
- `python-replay-tests` CI job(Phase 3)

**验收**:
- `cargo build --workspace` 通过
- `cargo test --workspace` 通过(Rust 测试不依赖 backend/)
- `make mac-app` 仍能本地出 `.app`(纯 Rust 路径)
- Phase 1 PR 不动 release pipeline,`make mac-release` 暂时会因 `backend/config.py` 缺失而失败 —— 这是预期,Phase 2 修复

### Phase 2 — 用 `cargo tauri build` 替掉 PyInstaller release pipeline

**目标**:三平台 release 全部走 Tauri 原生 bundler,删掉 PyInstaller / Wine / NSIS 旧链路。

- macOS:`cargo tauri build` 出 `.app` / `.dmg` / `.pkg`(Tauri bundler 原生支持)。`macos/build-macos.sh` 改写成调 cargo tauri build,删 `build-macos.spec`、`make-{dmg,pkg}.sh`。`entitlements.plist` 保留,被 `tauri.conf.json` 引用。
- Windows:Tauri 2 原生支持 NSIS / MSI。`docker/windows-builder/Dockerfile`(Wine + PyInstaller + pywebview hack)直接换成 GitHub Actions windows-latest runner 上跑 `cargo tauri build`。`installer.nsi` 删掉(Tauri 自动生成)。
- Linux:Tauri 直接出 `.deb`/`.AppImage`。`docker/linux-builder/` 重写为 Rust + webkit2gtk-4.1 编译环境(不再需要 Python + GTK 全套)。
- 倾向:**改成在 GitHub Actions 上跑三个原生 runner**(macos-14/ubuntu-22.04/windows-latest),Tauri 官方推荐方式,不需要 Wine,Docker 路径只剩"本机 Mac 上跨编译 Win/Linux"的便利,但 v2 已经是单二进制,CI 里直接出三套更省心。

**迁移期建议**:保留老 Python 路径并行一个版本,等新管线签名验证通过再删。

**验收**:
- 至少出一份 v2.0.x release,三平台全部由 Tauri bundler 产出
- 老 PyInstaller / Wine 路径删除后,`make help` 输出干净

### Phase 3 — 跨语言契约工具改造

**`scripts/gen_registry_fixtures.py` + `crates/registry/tests/python_compat.rs`**
- 当前作用:Python 端用 `backend/config.py` / `backend/registry.py` 生成 4 份 JSON fixture,Rust 端读取后做字节级 round-trip 断言,**双源真相校验**。
- backend/ 删了之后,这条路就断了。Python 不再是"权威源",fixture 只剩 Rust 一边的 golden file。
- **方案**:fixtures 改成 Rust 自己生成 + 入库 golden,`gen_registry_fixtures.py` 改写成 `xtask gen-fixtures`(`cargo run -p xtask -- gen-fixtures`),CI 步骤改成"Rust 生成一遍 → `git diff --exit-code`"。删掉 `python_compat.rs` 里跟 Python 比对的部分,只留 round-trip。

**`tests/replay/{fixture,player,recorder}.py` + `test_replay_smoke.py`**
- 当前作用:fixture JSON schema 是语言中立的(`crates/proxy/src/fixture.rs` 已经复刻 schema);Python 这套主要给"录制新 fixture"用,player 给老 Python 测试驱动用。
- 现 Rust 集成测试(`crates/proxy/tests/streaming_passthrough.rs`、`crates/adapters/tests/responses_streaming.rs`)直接读 fixture JSON,不走 Python player。
- **方案**:删 `player.py`、`fixture.py`、`test_replay_smoke.py`。`recorder.py` 是录制工具,以后录新 fixture 改用一次性 `curl + tee` 或一次性 Rust 工具。fixture JSON 文件保留(Rust 测试要读)。
- CI 里删 `python-replay-tests` job。

**`scripts/release_assets.py`**
- 作用:扫 `release/` 算 sha256 + RSA-3072 PKCS#1 v1.5 + SHA-256 签名 + 生 `latest.json`。
- 这个跟 Python 还是 Rust 没本质关系,是个独立 ~380 行的本地资产打包工具(不进二进制)。
- **方案**:Phase 2 完成后顺手用 Rust 重写为 `xtask release-bundle`,这样仓库就真正只有 Rust + JS(frontend + worker) + 少量 shell。`ring` 或 `rsa` crate 移植 RSA-3072 + SHA-256 不复杂。

### Phase 4 — 收尾

- 删 `requirements.txt`、`pyproject.toml`(Phase 3 完成后无 Python 文件)
- `.gitignore` 加 `.venv/`、`.pytest_cache/`(若未加),并删除已 track 的副本
- `README.md`:更新所有"Python"提法
- `Makefile`:更新注释,version 来源改为 `Cargo.toml`
- `docs/migration-plan.md`:在文末追加"清理已完成"修订日志
- 仓库里 `find . -name '*.py'` 应只剩 0 个或仅 `feedback-worker/`(无)

## 4. 时间线 / 里程碑

| Phase | 内容 | 风险 | 估时 | 触发条件 |
|---|---|---|---|---|
| 1 | 删死码 + tauri 版本号 + 死 CI job | 零 | 1 PR / 1 小时 | **现在** |
| 2 | release pipeline 全部切 Tauri bundler | 中(签名 / 公证 / 三平台 bundler 验证) | 1 PR + 1 个 v2.0.x release 周期 | 下个 release 之前 |
| 3 | 契约工具改造(xtask gen-fixtures + xtask release-bundle) | 低(已有 Rust 替身) | 1-2 PR | Phase 2 落地后 |
| 4 | 收尾(删 requirements.txt 等) | 零 | 1 PR | Phase 3 落地后 |

## 5. 修订日志

| 日期 | 来源 | 偏差 | 原因 |
|---|---|---|---|
| 2026-05-05 | 初稿 | — | 用户确认整体方向,启动 Phase 1 |
