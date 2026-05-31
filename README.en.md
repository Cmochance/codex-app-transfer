# Codex App Transfer

> [!IMPORTANT]
> 🔴 **Test coverage notice**
>
> This project has currently completed **end-to-end real-world testing only for Kimi For Coding and Xiaomi MiMo (Token Plan)**.
>
> Other built-in chat-completions-compatible providers (including **DeepSeek, Kimi (Moonshot Platform), Xiaomi MiMo (Pay for Token), Zhipu GLM, Aliyun Bailian (API Key / Token Plan), MiniMax**) **have not undergone long-term real-world regression** — they sit at unit-test + occasional user-report level only.
>
> If you'd be willing to **provide an API key from another provider for testing**, it would be deeply appreciated! Reach out via **QQ: `3216202644`** or email — the author guarantees the **API key will only be used for actual testing of this project**.

<p align="center">
  <a href="README.md">简体中文</a> |
  <a href="README.en.md">English</a> |
  <a href="CHANGELOG.md">Changelog</a> |
  <a href="https://cmochance.github.io/codex-app-transfer/">Code Graph</a>
</p>

<p align="center">
  <a href="https://github.com/Cmochance/codex-app-transfer/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/Cmochance/codex-app-transfer?style=social"></a>
  <a href="LICENSE.txt"><img alt="License" src="https://img.shields.io/github/license/Cmochance/codex-app-transfer"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust"></a>
  <a href="https://v2.tauri.app/"><img alt="Tauri" src="https://img.shields.io/badge/Tauri-2.x-24C8DB?logo=tauri"></a>
  <a href="https://github.com/Cmochance/codex-app-transfer/releases"><img alt="Downloads" src="https://img.shields.io/github/downloads/Cmochance/codex-app-transfer/total?label=downloads"></a>
</p>

Codex App Transfer is a lightweight desktop config + forwarding tool for the **OpenAI Codex App**. It runs a local gateway that translates Codex App's Responses API requests (HTTP streaming / non-streaming + `/responses`) into Chat Completions and other upstream formats, then forwards them to your chosen provider. The desktop UI manages providers, model mappings, the forwarding port, and the logs panel, letting Codex App talk to any third-party chat/completions inference service.

After starting forwarding, Codex App talks to this tool at `127.0.0.1:18080`. Closing the window minimizes the app to the system tray; right-click the tray icon and choose "Exit" to fully quit.

Current version **v2.1.18** (see [Changelog](CHANGELOG.md) and [Releases](https://github.com/Cmochance/codex-app-transfer/releases)).

## Preview

| Dashboard | Providers |
|---|---|
| ![Board](img/Board.png) | ![Providers](img/Providers.png) |
| **Settings** | **Logs** |
| ![Settings](img/Settings.png) | ![Logs](img/Logs.png) |

### Codex App in action

With any provider enabled, Codex App's model picker shows `<provider> / <real-model>`-style real model names. Tool loops / `previous_response_id` history replay / thinking-mode reasoning_content injection are all handled transparently by the local proxy:

![Codex App real conversation](img/codex-cli-real-chat.png)

### Codex Desktop background themes (optional)

Inject background image + frosted-glass panel CSS into Codex Desktop (the Electron client). Five built-in anime themes plus user upload. The Codex binary is not modified — runtime injection via Chromium DevTools Protocol; turn the toggle off on the Theme page to restore the native UI.

| Changli | Azur Lane |
|---|---|
| ![Changli](img/codex-theme/codex-theme-changli.jpg) | ![Azur Lane](img/codex-theme/codex-theme-azurlane.jpg) |
| **Nailin** | **Zani** |
| ![Nailin](img/codex-theme/codex-theme-nailin.jpg) | ![Zani](img/codex-theme/codex-theme-zani.jpg) |

A sixth theme (Carton) carries a floating mascot in the bottom-right that reacts to the cursor. **Custom backgrounds**: Theme page → "+ Add custom" → pick a JPG/PNG → 1:1 crop modal (drag + scroll to zoom) → apply. If the toggle is on at Codex launch, the selected theme auto-injects — no manual step needed.

## What it does

- Manage multiple providers; map OpenAI model names (`gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.3-codex` / `gpt-5.2`) to the provider's real model IDs
- Translate Codex App's Responses API streaming / non-streaming requests into upstream protocols: Chat Completions, Gemini Native (`:streamGenerateContent`), Gemini CLI OAuth (Cloud Code Assist), Anthropic Messages (`/v1/messages`), Grok Web (`/rest/app-chat/conversations/new`), Responses passthrough, etc.
- Multi-turn tool conversation context + `previous_response_id` history replay + autocompact expansion + thinking / reasoning_content injection — all aligned with the OpenAI Responses API protocol
- Codex App's freeform `apply_patch` tool (edit-file +/- diff UI) works on chat-completions providers: the adapter bridges Responses `custom_tool_call` ↔ chat `function_call` wire forms, the model emits V4A-format patches, Codex App renders the diff (issue #235); Gemini-family providers (gemini_native + Cloud Code Assist / Antigravity, using generateContent) now have the same bridge via MOC-75: on the request side, freeform `custom` tools are downgraded to a function with an `input` string parameter (V4A description reuses the chat constants); on the response side, Gemini's `functionCall` is repacked into a `custom_tool_call` wire
- When Gemini-family providers (gemini_native + Cloud Code Assist / Antigravity) return an upstream 4xx/5xx, the proxy translates it into a Codex-recognizable `response.failed` whose `error.code` is aligned to Codex's retry whitelist: **unambiguously-permanent** errors (400 INVALID_ARGUMENT / 401 auth / 403 permission) surface to the user and stop immediately (so you can switch models), instead of Codex re-sending the same request and deadlocking; **transient-or-uncertain** errors (timeout / rate-limit / quota / 5xx) keep retry semantics (exponential backoff; truly-unrecoverable ones surface after the retry cap) (MOC-79)
- Grok Web upstream 4xx/5xx are now similarly aligned to Codex's retry whitelist: 401 auth / 403 permission → `invalid_prompt` (permanent, Codex surfaces and stops), no longer deadlocking on repeated retries; transient errors (timeout / rate_limited / server_error) keep retry semantics (MOC-90)
- **Two-layer session history persistence**: L1 in-memory LRU + L2 sqlite with 30-day TTL (`~/.codex-app-transfer/sessions.db`), preserving history across `.app` restarts
- **Usage stats** (Sidebar → Usage): parses `~/.codex/sessions/` rollout JSONL, aggregating token usage by conversation / day / model (parser vendored from ryoppippi/ccusage). The by-conversation view shows each conversation's **cache hit rate**; clicking the number opens a **per-prompt hit-rate histogram** (cached contained within total, two-tone; hover for cached / total input / output). The proxy locally records `session → real upstream model` (for conversations run after this version), so the by-conversation model column shows the real upstream model instead of Codex's client placeholder (`gpt-5.x`)
- Codex App config guardrails: snapshots `~/.codex/{config.toml,auth.json}` before apply; restores via per-key smart merge on exit / next start; **Portable MCP auth vault** (on by default): switches MCP OAuth credential storage to a portable file (`~/.codex/.credentials.json`, mode 0o600) and keeps a mirror outside `~/.codex` (`~/.codex-app-transfer/mcp-credentials.json`); when the whole file is wiped by an account switch / accidental delete / new machine, the next launch prompts you to restore from backup (an intentional per-server logout is respected, never resurrected; note: does not fix natural OAuth expiry)
- **Codex Doc Management** (Sidebar → Codex):
  - **Agents**: raw read/write `AGENTS.md` at any path with file system picker; auto-classify project-root / subdir via `.git/` detection with chip labels
  - **Memories**: fixed two entries `~/.codex/memories/MEMORY.md` (main index) + `memory_summary.md` (auto summary) — the only two user-editable AI memory indexes that codex actually reads
  - **Skills**: scan `~/.codex/skills/<name>/SKILL.md` for raw editing; "Open folder" button shells out to `open` so users edit non-SKILL.md companion files (scripts / examples / templates) in Finder/Explorer
  - **MCP**: structured JSON editing on the `[mcp_servers.*]` section of `~/.codex/config.toml` (`toml_edit` round-trip preserves comments + sibling config sections); Plugins sub-tab scans `~/.codex/plugins/cache/` for installed bundles (enable toggle / uninstall); all writes are atomic + independent history per SHA-256 path hash (no cross-tab interference)
- Real-time logs panel auto-refreshing every 2s; unified `tracing::warn!(error_id, detail)` with stable tokens — operators can grep / aggregate
- Feedback dialog automatically attaches diagnostic material (environment info, sanitized config, recent error snapshot with full request / response) — fewer back-and-forth follow-ups
- Chinese / English UI; light / dark / green / orange / gray / white themes
- **Injected system prompts follow the UI language**: the `apply_patch` chat-path rules + autocompact summarization prompt that this project injects for non-OpenAI providers track the `语言 / Language` setting (Chinese users → Chinese prompts, avoiding mixed-language model thinking); V4A keywords (`*** Begin Patch` / `@@ <header>` etc.) + Codex CLI error message originals stay in English (parser / matcher does not accept translations)
- **Codex Desktop Theme (optional, off by default)**: Theme page ships 11 built-in anime themes (`carton` with a floating mascot, plus `changli` / `azurlane` / `nailin` / `zani` / `frost` / `nocturne` / `duet` / `rose` / `sonata` / `studio`), each individually colour-matched to its artwork (per-theme glass + accent). Injects design-token overrides (`--color-token-*` + the runtime `--color-*` layer) + a background image into Codex Desktop via CDP, covering chat / settings / collapsed-sidebar / popovers. Toggle is independent from Plugin Unlock; page reload re-applies automatically
- Cross-platform single-instance lock (double-click brings the existing window forward) + cross-process file lock prevents multi-instance config-write lost-updates
- Windows / macOS / Linux system tray

## Download

Latest: `https://github.com/Cmochance/codex-app-transfer/releases/latest`

Recommended asset naming:

```text
Codex-App-Transfer-v<version>-Windows-x64-Setup.exe       Windows NSIS installer (recommended)
Codex-App-Transfer-v<version>-Windows-x64.msi             Windows MSI (enterprise MDM / GPO)
Codex-App-Transfer-v<version>-macOS-arm64.dmg             macOS Apple Silicon
Codex-App-Transfer-v<version>-macOS-x64.dmg               macOS Intel x64 (v2.1.0+, closes #61)
Codex-App-Transfer-v<version>-Linux-x86_64.deb            Debian / Ubuntu
Codex-App-Transfer-v<version>-Linux-x86_64.AppImage       Generic Linux x86_64; `chmod +x` and run
```

Each binary ships with `.sha256` and `.sig` (RSA-3072 PKCS#1 v1.5 + SHA-256); the public key `Codex-App-Transfer-release-public.pem` is published as a release asset — download it from [Releases](https://github.com/Cmochance/codex-app-transfer/releases) to verify signatures.

Windows builds are not Authenticode-signed yet, so Windows may show an "unknown publisher" warning — use the `.sha256` / `.sig` to verify download integrity.

macOS builds are **not yet signed with an Apple Developer ID** and **not yet Notarized**, so Gatekeeper blocks first launch with "cannot be opened because it is from an unidentified developer". Workarounds: `right-click → Open` to allow once; or verify download integrity with `.sha256` / `.sig`, then click "Open Anyway" under `System Settings → Privacy & Security`.

## Quick Start

1. Launch Codex App Transfer; the desktop window opens
2. On the dashboard, click the top-right "+" → pick a preset or add a custom provider; fill in API Base URL, API Key, then "Fetch models" and add model mappings
3. Click the **Apply** button at the bottom — config is written and a toast confirms sync (if a provider is already configured, just click **Apply** on its card on the home page)
4. To make Codex Desktop pick up the new config, click the ↻ **Restart Codex** button at the top-right (decoupled from Apply since #281 to avoid losing in-flight context on misclicks)

## Provider compatibility matrix

| Provider | Multi-turn | autocompact | tool_call_repair | Notes |
|---|---|---|---|---|
| Kimi (Moonshot Platform / Kimi For Coding) | ✅ | ✅ | ✅ | Thinking 3-layer defense |
| DeepSeek V4 (incl. Max thinking) | ✅ | ✅ | ✅ | Vision input stripped to avoid 400; xhigh → real max effort (#254) |
| Xiaomi MiMo (Token Plan / Pay for Token) | ✅ | ✅ | ✅ | Image-only requests get space text-part fallback |
| MiniMax M2.x / Text-01 | ✅ | ✅ | ✅ | `role=system` → user (v2.1.6 fix for 400) |
| Google AI Studio (`gemini_native`) | ✅ | ✅ | ✅ | Auto-selects Gemini 3 `/v1alpha` + Gemini 2.x `/v1beta` |
| Google Gemini CLI OAuth | ✅ | ✅ | ✅ | Browser login once; no API key needed |
| Anthropic Messages (custom Claude-compatible) | ✅ (PR #153) | ✅ (PR #153) | ✅ (PR #153) | `apiFormat=anthropic_messages`; Claude preset pending real validation |
| Grok Web (SuperGrok / X Premium+) | ✅ | ✅ | ✅ (v2.1.6 adds tool_calls flatten) | Experimental, TOS gray area, personal use only |
| Google Antigravity OAuth | ✅ | ✅ | ✅ | Backend ready, UI pending |
| Zhipu GLM (5.1 / 4.7) | ✅ | ✅ | ✅ | OpenAI Chat-compatible reverse proxy |
| Alibaba Cloud Bailian (Qwen 3.6 Plus / Flash) | ✅ | ✅ | ✅ | OpenAI Chat-compatible reverse proxy |
| Responses passthrough (custom) | — | — | — | Direct upstream connection, bypasses proxy (suitable for OpenAI official / native Responses reverse proxy); ⚠️ Plugins/MCP `namespace` tool bundle is NOT flattened — some upstreams silently drop tools |

> **MCP tools (Codex 0.130+ `tool_search` mechanism)**: Codex 0.130+ defers server-side MCP tools (`mcp__notion__*` / `mcp__linear__*`, etc.) to `tool_search` instead of placing them directly in `tools[]`. The proxy wires the full chain on the **chat path** — discovering tools from `tool_search_output` → injecting them into chat `tools[]` → routing back upstream by `namespace` (#293). **Applies to all chat-compat providers in the table above**; only the Responses passthrough row (last row, bypasses the proxy) is excluded.

## Reasoning effort mapping (chat-compat `reasoning_effort`)

How Codex's `low/medium/high/xhigh` is dispatched per chat-completions upstream (issue #254):

| Provider | `xhigh` / `max` | Other tiers | Notes |
|---|---|---|---|
| **DeepSeek V4** | `reasoning_effort: "max"` | `low/medium/high` → `"high"` | Only chat upstream that accepts a true max tier |
| **Kimi / Kimi Code / GLM / Bailian Qwen / Xiaomi MiMo / MiniMax** | field dropped | field dropped | Upstreams don't accept `reasoning_effort`; default thinking applies. Set provider-native fields in `requestOptions` to control explicitly |
| **Custom chat-compat** | clamp to `"high"` | passthrough | OpenAI standard enum conservative fallback |

## Model mapping

Codex App prompts by OpenAI model names; third-party providers use real IDs like `deepseek-v4-pro` / `kimi-k2.6` / `glm-5.1` / `gemini-3-pro`.

This tool maps via `provider.models[slot]` (`gpt-5.5` → `deepseek-v4-pro` etc.); Codex App's model picker shows `<provider> / <real-model>` real names. Upstream `chatcmpl-...` response IDs are auto-rewritten to Codex App-validatable `resp_<base64>`, preserving deployment-affinity encoding so `previous_response_id` is consistent across turns.

## Development (v2 / Rust)

```bash
git clone https://github.com/Cmochance/codex-app-transfer.git
cd codex-app-transfer
cargo tauri dev          # launch desktop window with hot-reload
cargo test --workspace --lib   # run unit tests
make mac-app             # local macOS bundle to dist/mac/
```

Fixture reverse-diff (contract tests):

```bash
cargo run --bin xtask -- gen-fixtures
```

Bundling (refer to `.github/workflows/release.yml`):

```bash
cargo tauri build --bundles app,dmg          # macOS arm64
cargo tauri build --bundles nsis,msi         # Windows x64
cargo tauri build --bundles deb,appimage     # Linux x86_64
```

### Tweaking the UI

`frontend/css/` is organized as a small component library — no need to grep the whole `style.css`:

| What to tweak | Where to edit |
|---|---|
| Theme colors / radius / shadow / spacing (design tokens) | `frontend/css/tokens.css` (129 vars + 6 themes) |
| Global reset / body font / focus ring | `frontend/css/base.css` |
| Buttons / cards / forms / badges / modals etc. | `frontend/css/components/<name>.css` |
| Page-specific styles for dashboard / providers / proxy / settings / guide | `frontend/css/pages/<route>.css` |
| Responsive breakpoints (1100px / 720px) | `frontend/css/responsive.css` |

Preview every component + variant + theme switching:

```bash
# Open directly in your browser (no dev server needed)
open frontend/gallery.html        # macOS
xdg-open frontend/gallery.html    # Linux
start frontend/gallery.html       # Windows
```

`gallery.html` has a theme picker + dark/light toggle at the top, refresh after editing component css to see changes. `frontend/index.html`'s `<link href="css/style.css">` does not need to change — `style.css` is just an `@import` entry that aggregates every sub-file.

To add a new component: create `components/<name>.css` + add a line `@import url("components/<name>.css");` to `style.css` + add a section in `gallery.html`.

## Troubleshooting

### Codex models can't run `curl` and similar network commands / approval prompt stuck

`curl` requires elevated permissions. Third-party models currently cannot trigger the macOS escalation prompt, so this app writes `sandbox_mode = "danger-full-access"` + `approval_policy = "never"` into `~/.codex/config.toml` by default on apply. On Windows, or if you have other reasons, you can turn this off in Settings → "Allow Codex network tools (full-access mode)" (#215).

> **⚠️ Security trade-off**: full-access mode lets the model read/write any file and run every command without approval = **fully trust the model** (equivalent to Codex's official "Full access" tier). With the toggle off, Codex falls back to the read-only sandbox + on-request approval. macOS still cannot trigger the elevation prompt, so there is no network access — only the selected model's built-in `web_search` capability is usable. If the model doesn't support `web_search`, all search calls return empty results.

### Codex App reports `404 Not Found url: http://127.0.0.1:18080/responses`

Old versions only exposed `/v1/responses`; Codex CLI 0.126+ falls back to `/responses` (without `/v1/`). This tool added the route alias — **v1.0.1 and later all support it** (current v2.x series ships it by default, no extra config needed).

### Codex App reports `stream disconnected before completion`

Usually means `response.id` / `response.model` weren't returned in the shape Codex App expects. This tool rewrites upstream `chatcmpl-...` to `resp_<base64>` while preserving the requested model name — confirm forwarding logs show `resp_...` instead of `chatcmpl-...`.

### Upstream 400: `thinking is enabled but reasoning_content is missing`

Kimi / DeepSeek with thinking enabled require historical assistant messages with `tool_call` to carry `reasoning_content`. This tool **auto-fills a default empty string since v1.0.1** and maps reasoning items from Responses input to the corresponding assistant message.

### Upstream 400: `'reasoning_effort' does not support 'xhigh'`

v2.1.14 and earlier clamped `xhigh` / `max` to `high` for all providers (issue #254). **v2.1.15+ uses a per-provider policy** — DeepSeek truly reaches max; Kimi / GLM / MiMo / MiniMax / Qwen drop the field (upstream doesn't accept it); custom proxies clamp conservatively. See the [reasoning effort mapping table](#reasoning-effort-mapping-chat-compat-reasoning_effort) above for the full matrix.

`auto` / `none` / `disabled` and similar values that Chat endpoints do not accept are always dropped.

### MiniMax 400: `invalid message role: system (2013)`

v2.1.5 and earlier did not convert `role=system` to `role=user`, causing MiniMax `/v1/chat/completions` to 400 the entire request. v2.1.6+ fixes this (closes #139): all `role=system` messages are converted to `role=user` with content prefixed by `[System]\n`.

### Upstream 404 / can't connect (Base URL includes the full endpoint)

Set the provider Base URL to the root or `/v1` only (e.g. `https://api.example.com/v1`); do **not** paste the full endpoint path. The tool appends `/chat/completions`, `/v1/messages`, or `/responses` per protocol automatically. If the Base URL already ends with one of these (e.g. pasting `https://opencode.ai/zen/go/v1/chat/completions` verbatim), the path doubles into `…/chat/completions/chat/completions` and the upstream returns 404. Trim the extra endpoint suffix and keep it at `/v1`.

### Codex shows `Failed to revert changes`

This is Codex's own client-side "revert changes" message and does **not** go through this tool's proxy (the revert is performed by Codex against the local file snapshots it maintains, unrelated to the selected model or relay). Common causes: (1) the changed files are locked by an editor / IDE / antivirus, so the rollback can't write on Windows; (2) the files were modified externally after Codex edited them, so the snapshot no longer matches; (3) apply_patch wrote files into a nested subdirectory this session, so Codex can't locate the originals to revert. Close any program holding the files, verify the changes landed in the expected directory, then retry; revert manually if it still fails.

### Port conflicts

v2 only listens on `18080` (forwarding) by default; the admin UI now uses Tauri in-process `cas://` and no longer occupies 18081. Use `netstat -ano | findstr :18080` to find usage, or change the port in Settings → Port and restart forwarding.

### Windows "unknown publisher" warning

The current Windows build is not Authenticode-signed. The Release page provides `.sha256` and `.sig` to verify the installer hasn't been tampered with.

### Self-host / custom update URL

From v2.1.12+ the client **enforces** RSA-3072 PKCS#1-v1.5-SHA256 verification on `latest.json` and every installer. The upgrade flow fetches `<url>.sig` alongside the file and verifies against the build-time embedded official public key (`release/Codex-App-Transfer-release-public.pem`). Failure is fatal — no SHA256-only fallback.

**A custom update URL must be self-signed to work**:

1. Fork the repo and replace `release/Codex-App-Transfer-release-public.pem` with your own public key.
2. Run `cargo run -p xtask --release -- release-bundle` with the matching private key to sign `latest.json` and every installer.
3. Rebuild the client so the public key is embedded.
4. Users point Settings → Update URL at your `latest.json` endpoint.

Design intent: the client trusts only the build-time embedded public key and never lets a runtime URL replace it, blocking MITM rewrites of `latest.json` (the public PEM lives in `release/` but pulling it from the same origin as the update URL would dissolve the trust anchor).

### Logs

- App UI: real-time panel below the forwarding page, auto-refreshes every 2s
- Disk: `~/.codex-app-transfer/logs/proxy-YYYY-MM-DD.log` — click "View Logs" to open directly
- "Clear logs" moves the current log to `logs/backup/` with a timestamp suffix — never deletes outright

## Tech Stack

- **Backend / forwarding**: Rust 1.80+ · axum 0.8 · reqwest 0.12 (rustls-tls) · tokio
- **Protocol adapters**: `crates/adapters/` — Responses ↔ Chat / Gemini Native / Gemini CLI OAuth / Anthropic Messages / Grok Web (request body + streaming response state machine + reasoning_content + tool_calls)
- **Frontend**: HTML + CSS + vanilla JavaScript + Bootstrap 5.3.3 (localized, no CDN dependency)
- **Desktop shell**: Tauri 2 + tray-icon 0.23; the `cas://` URI scheme glues frontend/ and axum in-process, no TCP loopback
- **Storage**: `~/.codex-app-transfer/config.json` (config, compatible with v1.x), `~/.codex-app-transfer/sessions.db` (L2 sqlite session persistence), `~/.codex/{config.toml,auth.json,.credentials.json}` (Codex App integration), `~/.codex-app-transfer/mcp-credentials.json` (MCP credential mirror, outside `~/.codex`)
- **Packaging**: `cargo tauri build` single command produces dmg/AppImage/deb/exe/msi; `xtask release-bundle` finalizes sha256 + RSA-3072 sig + latest.json + draft GitHub release

## Disclaimer

This project focuses on **OpenAI Codex App** integration; it is **not** an official OpenAI / Anthropic / Google / xAI project and does not reuse their trademarks / logos / release identities.

Upstream API keys / OAuth tokens are stored locally in `~/.codex-app-transfer/` (Unix 0600 + atomic write); the forwarding service only listens on `127.0.0.1` and does not hijack the system proxy. Apart from the feedback feature, this tool performs no third-party network access.

Some experimental providers (Grok Web / Gemini CLI OAuth / Antigravity OAuth) involve upstream TOS gray areas — Grok Web reverse-proxies grok.com's Web backend, Gemini CLI OAuth uses the undocumented internal endpoint `cloudcode-pa.googleapis.com/v1internal` — strictly limited to **personal use**, **must not** be deployed as a public service, **carries a real account-ban risk**, **users assume the risk**. These gray-area providers are **hidden by default** in the "Add provider" list; enable the **Show gray-area providers** setting to reveal them (MOC-91).

## Acknowledgements

> Overview list below. For the full **borrowing form / itemized list / corresponding file:line in this codebase**, see [ACKNOWLEDGEMENTS.md](./ACKNOWLEDGEMENTS.md).

<!-- Acknowledgements overview rule: each entry's description (after " — ") is a terse tag, ≤ 40 chars; full borrowing form / license / file:line goes in ACKNOWLEDGEMENTS.md. Enforced in CI by scripts/check_acknowledgements.py — over budget fails the build. -->

- [`farion1231/cc-switch`](https://github.com/farion1231/cc-switch) — provider switching paradigm
- [`lonr-6/cc-desktop-switch`](https://github.com/lonr-6/cc-desktop-switch) — v1.x desktop shell skeleton
- [`BerriAI/litellm`](https://github.com/BerriAI/litellm) — bidirectional protocol translation
- [`tauri-apps/tauri`](https://tauri.app/) — v2 + `cas://` architecture base
- [`openai/codex`](https://github.com/openai/codex) — compact prompt base structure
- [`Piebald-AI/claude-code-system-prompts`](https://github.com/Piebald-AI/claude-code-system-prompts) — autocompact anchor bullets
- [`7as0nch/mimo2codex`](https://github.com/7as0nch/mimo2codex) — MiMo protocol reference
- [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI) — Gemini OAuth wire-level reference
- [`chenyme/grok2api`](https://github.com/chenyme/grok2api) — Grok Web reverse-engineering ref
- [`galaxywk223/codex-plugin-unlocker`](https://github.com/galaxywk223/codex-plugin-unlocker) — Codex Desktop Plugins unlock script
- [`QwenLM/qwen-code`](https://github.com/QwenLM/qwen-code) — Qwen Token Plan model registry
- [`BigPizzaV3/CodexPlusPlus`](https://github.com/BigPizzaV3/CodexPlusPlus) — Windows MSIX CDP injection path
- [`borawong/AiMaMi`](https://github.com/borawong/AiMaMi) — managed-block six-op design
- [`ryoppippi/ccusage`](https://github.com/ryoppippi/ccusage) — rollout JSONL token-usage parser

### Community contributors

Contributors who improved this project via PRs (in reverse-chronological order of first commit; full list at [Contributors](https://github.com/Cmochance/codex-app-transfer/graphs/contributors)):

- [@Alpaca233114514](https://github.com/Alpaca233114514) — theme CDP drain_until_response + update check gzip/OnceLock fixes ([#278](https://github.com/Cmochance/codex-app-transfer/pull/278) / [#285](https://github.com/Cmochance/codex-app-transfer/pull/285))
- [@lukegood](https://github.com/lukegood) — MiniMax M2.x compatibility ([#47](https://github.com/Cmochance/codex-app-transfer/pull/47))
- [@cw881014](https://github.com/cw881014) — early protocol-layer fixes, 3 PRs ([#1](https://github.com/Cmochance/codex-app-transfer/pull/1) / [#7](https://github.com/Cmochance/codex-app-transfer/pull/7) / [#12](https://github.com/Cmochance/codex-app-transfer/pull/12))

If you've submitted a PR and want to rename / add a link / remove yourself, open an issue.

## License

MIT License. Full text at [LICENSE.txt](LICENSE.txt).

## Project Activity

<table>
<tr>
<td width="50%" align="center">
<a href="https://github.com/Cmochance/codex-app-transfer/releases"><img src="https://cmochance.github.io/codex-app-transfer/downloads.svg" alt="Download trend" width="100%"></a>
<br/><sub>Download trend</sub>
</td>
<td width="50%" align="center">
<a href="https://star-history.com/#Cmochance/codex-app-transfer&Date"><img src="https://api.star-history.com/svg?repos=Cmochance/codex-app-transfer&type=Date" alt="Star history" width="100%"></a>
<br/><sub>Star history</sub>
</td>
</tr>
</table>
