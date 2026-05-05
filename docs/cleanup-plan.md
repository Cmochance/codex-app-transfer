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

**目标**:三平台 release 全部走 Tauri 原生 bundler,删掉 PyInstaller / Wine / NSIS / Docker 旧链路;**构建载体从"本机 Mac + Docker"切到 GitHub Actions matrix**(macos-14 / ubuntu-22.04 / windows-latest)。

#### 2.1 设计决策矩阵

| 决策点 | 选项 | 选择 | 理由 |
|---|---|---|---|
| 构建载体 | (a) 本机 Mac + Docker 沿用 / (b) GitHub Actions matrix | **(b)** | Tauri 官方推荐;免 Wine 5GB 镜像;签名 secret 用 GH Secrets;CI 自然出三平台 artifact |
| `release_assets.py` 何时退役 | (a) Phase 2 顺手用 Rust 重写 / (b) Phase 2 只调输入路径,Phase 3 再 xtask 重写 | **(b)** | Phase 2 已经动 release pipeline + GH Actions + 签名密钥三件大事,工具不变更安全;~30 行调路径 vs ~380 行重写 |
| `.pkg` 去留 | (a) 保留(Tauri 出 .app 后手动 productbuild) / (b) 退役只发 .dmg | **(b)** | Tauri 2 macOS bundler 不直接出 .pkg;.dmg 是 macOS 主流分发;.pkg 主要给企业 MDM,不在本项目场景 |
| `entitlements.plist` 去留 | (a) 删 / (b) 留,改由 `tauri.conf.json` 引用 | **(b)** | 沙箱权限/钥匙串访问声明仍需要;tauri 支持 `bundle.macOS.entitlements` 字段 |
| 产物命名 | (a) 接受 Tauri 默认名 / (b) workflow 内 mv 成老命名 | **(b)** | 用户既有下载链接 / 镜像不应失效;`release_assets.py` 的 PLATFORM_PATTERNS 也基于老命名,改 mv 比改正则简单 |
| `latest.json` 自签 RSA 密钥位置 | (a) 仍 `.release-signing/` 本地 / (b) GitHub Secret | **(b)** | CI 上签需要;本地仍可用 secret 备份的同一份私钥手动签 |
| Linux 出哪些格式 | (a) 仅 .tar.gz(沿用) / (b) .deb + .AppImage(Tauri 默认) | **(b) + 兼容性** | 接受 Tauri 默认 .deb / .AppImage,**额外**保留 `.tar.gz` 作为最低依赖兜底(workflow 内 tar 一下 .AppImage 解出的目录) |
| `make` 留多少 | (a) 全删走 GH Actions / (b) 保留 `mac-app` 本地自测 | **(b)** | 本地开发自测仍需快速出 .app;`make help` 收敛到 1-2 个 target |

#### 2.2 新增 `.github/workflows/release.yml` 蓝图

```yaml
name: release
on:
  push:
    tags: ['v*']
  workflow_dispatch:
    inputs:
      version: { description: '不带 v 前缀(e.g. 2.0.1)', required: true }

permissions: { contents: write }   # gh release create

jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: macos-14,        target: aarch64-apple-darwin,   bundles: 'app,dmg' }
          - { os: ubuntu-22.04,    target: x86_64-unknown-linux-gnu, bundles: 'deb,appimage' }
          - { os: windows-latest,  target: x86_64-pc-windows-msvc, bundles: 'nsis,msi' }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: ${{ matrix.target }} }
      - uses: Swatinem/rust-cache@v2
      - if: runner.os == 'Linux'
        run: |   # 复用 ci.yml 已经打磨好的 retry 模式
          set -e
          for i in 1 2 3; do
            sudo apt-get update && sudo apt-get install -y --no-install-recommends \
              libwebkit2gtk-4.1-dev libssl-dev libayatana-appindicator3-dev \
              librsvg2-dev patchelf build-essential file && break || sleep 30
          done
      - name: cargo tauri build
        env:
          # 仅 Mac/Win 需要;Linux 无平台原生签名
          APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE_P12_BASE64 }}
          APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
          APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
          APPLE_API_KEY: ${{ secrets.APPLE_API_KEY }}
          APPLE_API_KEY_ID: ${{ secrets.APPLE_API_KEY_ID }}
          APPLE_API_ISSUER: ${{ secrets.APPLE_API_ISSUER }}
          # Windows: 通过 tauri.conf.json bundle.windows.signCommand 调 signtool
        run: |
          cargo install tauri-cli@^2 --locked
          cargo tauri build --target ${{ matrix.target }} --bundles ${{ matrix.bundles }}
      - name: rename to project naming
        shell: bash
        run: bash .github/workflows/scripts/rename-bundles.sh "${{ matrix.target }}"
      - uses: actions/upload-artifact@v4
        with: { name: bundle-${{ matrix.target }}, path: dist-renamed/ }

  release-bundle:
    needs: build
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with: { path: release/, merge-multiple: true }
      - uses: actions/setup-python@v5
        with: { python-version: '3.13' }
      - run: pip install cryptography
      - name: write signing key from secret
        env: { KEY: ${{ secrets.RELEASE_PRIVATE_KEY_PEM }} }
        run: |
          mkdir -p .release-signing
          printf '%s\n' "$KEY" > .release-signing/release-private-key.pem
      - run: python scripts/release_assets.py --version "${VERSION}" --include macos linux windows --repo "${{ github.repository }}"
      - uses: softprops/action-gh-release@v2
        with: { files: 'release/*', tag_name: 'v${{ env.VERSION }}' }
```

> **实施修正**:Phase 2 落地时 `tauri.conf.json` 加了 `bundle.fileName: "codex-app-transfer"`,Tauri 默认产物名变成 `codex-app-transfer_<V>_<arch>.<ext>`(无空格),rename 逻辑直接写在 `release.yml` build step 的 ~30 行 bash case,不再需要单独的 `rename-bundles.sh`。

#### 2.3 文件级 diff 清单

**新增**:
- `.github/workflows/release.yml`(三平台 matrix build + release-bundle 收口)
- `src-tauri/tauri.conf.json` 加 `bundle.fileName`、`bundle.macOS.entitlements`、`bundle.linux.deb.depends`、`bundle.windows.{wix, nsis}` 子段

**修改**:
- `Makefile`:删除 `mac-release` `linux-release` `win-release` `release` `release-bundle` `linux-image` `win-image`、变量 `PYTHON` `VERSION` `WIN_IMAGE_TAG` `LINUX_IMAGE_TAG` `REPO_FLAG`;只留 `mac-app`(本地自测)+ `clean` + `help`
- `docs/build.md`:大幅重写。主线变 GitHub Actions / `gh workflow run release.yml`,本地路径只剩 `make mac-app`
- `scripts/release_assets.py`:**整体重写适配 Tauri 输出形态** — 删除假设 PyInstaller 输出的 `collect_windows/mac/linux`(`dist/{mac,linux-folder,linux-onefile}` 路径),换成统一的 `collect_from_incoming(dist-incoming/, ...)`;`PLATFORM_PATTERNS` 改:macOS `.pkg` 退役、Linux `.tar.gz`/无后缀退役改 `.deb`/`.AppImage`、Windows `Portable.zip`/`-x64.exe` 退役改 `-Setup.exe`/`.msi`;新增 `--incoming-dir` CLI 参数

**删除**:
- `macos/build-macos.sh`、`build-macos.spec`、`make-dmg.sh`、`make-pkg.sh`、`prepare-icon.py`、`pkg-scripts/`、`README.md`(本目录的)
- `macos/entitlements.plist`:**保留**,被 `tauri.conf.json` 引用
- `docker/linux-builder/`、`docker/windows-builder/`(整个 docker/ 目录可空,`rmdir docker`)
- `scripts/build-linux-on-mac.sh`、`scripts/build-windows-on-mac.sh`

#### 2.4 签名密钥迁移到 GH Secrets

| 用途 | Secret 名 | 来源 / 必须? |
|---|---|---|
| `release_assets.py` RSA-3072 自签 | `RELEASE_PRIVATE_KEY_PEM` | 复制 `.release-signing/release-private-key.pem` 全文 / **必须** |
| Apple Developer ID 证书 | `APPLE_CERTIFICATE_P12_BASE64` | `base64 -i Cert.p12` / 可选(无则 ad-hoc 签名) |
| 同上密码 | `APPLE_CERTIFICATE_PASSWORD` | / 可选 |
| 同上 identity | `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Name (TEAMID)` / 可选 |
| Apple Notary API Key | `APPLE_API_KEY` (P8 全文)、`APPLE_API_KEY_ID`、`APPLE_API_ISSUER` | App Store Connect 生成 / 可选 |
| Windows Authenticode 证书 | `WIN_CODESIGN_CERT_BASE64`、`WIN_CODESIGN_PASSWORD` | / 可选 |

**首次切换**:用户在 GH 仓库 Settings → Secrets and variables → Actions 配置;**必须只配 `RELEASE_PRIVATE_KEY_PEM` 一条**就能跑通(Mac 退化为 ad-hoc 签名,Win 退化为未签名,Linux 本来就不平台签)。

#### 2.5 产物命名映射

`tauri.conf.json` 设 `bundle.fileName: "codex-app-transfer"` 后,Tauri 默认产物名变成 `codex-app-transfer_<V>_<arch>.<ext>` (无空格)。`release.yml` 的 rename step 直接 cp 成项目老命名:

| Tauri (fileName=codex-app-transfer) | → 项目老名(staging/) |
|---|---|
| `target/<T>/release/bundle/dmg/codex-app-transfer_<V>_aarch64.dmg` | `Codex-App-Transfer-v<V>-macOS-arm64.dmg` |
| `target/<T>/release/bundle/deb/codex-app-transfer_<V>_amd64.deb` | `Codex-App-Transfer-v<V>-Linux-x86_64.deb` |
| `target/<T>/release/bundle/appimage/codex-app-transfer_<V>_amd64.AppImage` | `Codex-App-Transfer-v<V>-Linux-x86_64.AppImage` |
| `target/<T>/release/bundle/nsis/codex-app-transfer_<V>_x64-setup.exe` | `Codex-App-Transfer-v<V>-Windows-x64-Setup.exe` |
| `target/<T>/release/bundle/msi/codex-app-transfer_<V>_x64_en-US.msi` | `Codex-App-Transfer-v<V>-Windows-x64.msi` |

**调整说明**:
- `.pkg` 退役 (Tauri 2 macOS bundler 不直出);macOS 只发 `.dmg`
- Linux 不再用 `.tar.gz` (PyInstaller folder build 时代的形态);改 `.deb`+`.AppImage`
- Windows `Portable.zip` 退役 (Tauri 不直出);改 `-Setup.exe`(NSIS)+ `.msi`(WiX)
- `release_assets.py` 的 `PLATFORM_PATTERNS` 同步更新

#### 2.6 验收

- 本地 `make mac-app` 仍出 `.app`(无回归)
- `gh workflow run release.yml -f version=2.0.1-rc1` 触发,三平台全跑成功,artifact 全部生成 + 签名 + `latest.json` 含 3 个 platforms 项
- 用 `release/Codex-App-Transfer-release-public.pem` 验签所有 `.sig` 通过(沿用 `Test-ReleaseSignature.ps1` 的协议)
- `find . -name 'Dockerfile' -o -name '*.spec'` 应为空
- `make help` 输出只剩 `mac-app` / `clean`

#### 2.7 回滚策略

- **PR 内 commit 顺序**:先加 `release.yml` + `rename-bundles.sh`,再改 `tauri.conf.json` / `release_assets.py`,最后才删 `macos/build-macos.sh` / `docker/`。如果在合 PR 之前 dispatch 测试 release 跑挂,只 revert 最后那条删除 commit 即可继续用老路径
- **合 PR 之前的硬验收**:用户必须先用 `gh workflow run release.yml -f version=2.0.1-rc1` 在 feature 分支上完整跑通一次三平台 + 签名,artifact 落到 GH release(可以是 draft / pre-release),验过再 squash-merge
- **如果合 PR 后第一次 tag release 翻车**:`git revert` Phase 2 PR + 重新 tag 旧版本号 + 通过临时手工跑 `release_assets.py` 应急

#### 2.8 范围明确**不做**(留给 Phase 3+)

- 不重写 `release_assets.py` 为 Rust(只调输入路径,Phase 3 用 `xtask release-bundle` 统一替换)
- 不动 `tests/replay/*.py` 和 `gen_registry_fixtures.py`
- 不删 `requirements.txt`(`release_assets.py` 还要 `cryptography`;Phase 3 完成后 Phase 4 再删)
- 不改 `feedback-worker/`(独立微服务,不在 cleanup 范围)

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
| 2026-05-05 | Phase 1 PR CI 反馈 | 顺手在 Phase 1 修了 main 上 pre-existing 红:`cargo fmt` drift + pytest 9 import error(加 `pythonpath = ["."]`)+ `src-tauri` 跨平台 dep 误放 macos 块导致 Linux 编译失败 | 不修 CI 永远绿不了,后续 Phase 用不上回归门禁 |
| 2026-05-05 | Phase 1 → Phase 3 范围微调 | CI 中"Python 重生 registry fixture → diff" 步骤直接删除(原属 Phase 3 范围)。原因:Phase 1 删了 backend/ 后 `gen_registry_fixtures.py` 失去数据源,该 CI 步骤会一直红;`python_compat.rs` 仍读 commit 的 fixture 做 round-trip,反向校验(Rust → diff)留给 Phase 3 xtask 重建 | 最小化 CI 红区,保住回归门禁;真正的 xtask 替身仍按 Phase 3 计划做 |
| 2026-05-05 | Phase 1 后期 CI 修(3 个补 commit) | (1) `aebf8cd` 补 `Cargo.lock` 与 `src-tauri` dep 删除的同步(16bb9fb 漏);(2) `c53de00` `apt-get install` 加 `timeout-minutes:8` + 3 次 retry + `--no-install-recommends`,抗 archive.ubuntu.com 偶发抖动(上一次 run 卡 17 分钟);(3) `e4d3382` 修 `crates/registry/src/raw_io.rs::tests::tempdir()` 并发 race(共享 `cas-registry-test-{pid}` 目录,加 `AtomicU64` counter 保唯一) | 都是 Phase 1 范围内的 CI 修,记录在案以便 Phase 2 不重蹈覆辙 |
| 2026-05-05 | Phase 2 详细方案落地 | 把 §3 Phase 2 概念性段落扩充为 §2.1-2.8 子章节(决策矩阵 / release.yml 蓝图 / 文件 diff 清单 / 签名密钥迁移 / 产物命名映射 / 验收 / 回滚 / 不做项) | 概念描述不足以直接动手,细化后让 reviewer 在动代码之前先评设计 |
| 2026-05-05 | Phase 2 §2.3/§2.5 修正 | 实施时 `tauri.conf.json` 加 `bundle.fileName: "codex-app-transfer"` 让 Tauri 产物名摆脱空格,rename 逻辑直接写在 `release.yml` 的 ~30 行 bash case,**不再需要单独的 `rename-bundles.sh`**;`release_assets.py` 不是"微调路径"而是**整体重写**:删 `collect_windows/mac/linux`(假设 PyInstaller 输出),换 `collect_from_incoming(dist-incoming/)`,`PLATFORM_PATTERNS` 同步换为 `.dmg`/`.deb`/`.AppImage`/`-Setup.exe`/`.msi` | 调研 Tauri 2 实际产物形态后发现原设计估计过粗,落地修正 |
