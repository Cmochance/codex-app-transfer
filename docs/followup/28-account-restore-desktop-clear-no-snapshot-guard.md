---
id: 28
priority: P0
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# 账号还原 D1:`desktop_clear` 无 `has_snapshot` guard,无快照时会删用户手写的 managed key

## 触发上下文

2026-05-17 用户主动要求审查"账号还原逻辑是否会导致用户原始账号信息丢失"。Agent 调研 + 主对话 verify 后确认该路径为真实 HIGH 风险(后修正为 MEDIUM —— 需要用户主动触发,但仍属可能丢失用户数据的隐式 path)。

代码 evidence:
- `src-tauri/src/admin/handlers/desktop.rs:902-911` `desktop_clear` 端点直接调 `restore_codex_state`,**无 `has_snapshot` 前置检查**
- `crates/codex_integration/src/apply.rs:144-147` `restore_codex_state` 实现:`if !has_snapshot(paths) { clear_managed_codex_state(paths)?; return Ok(false); }`
- `crates/codex_integration/src/apply.rs:198-213` `clear_managed_codex_state` 删除全部 `MANAGED_TOML_KEYS` (apply.rs:23-29: `openai_base_url, model_context_window, model_catalog_json, model, model_provider`) + `MANAGED_AUTH_KEYS` (apply.rs:20: `auth_mode, OPENAI_API_KEY`)

## 问题描述

### 现状

`desktop_clear` 在 frontend UI 上对应"清除桌面配置"按钮(假设有)。后端无差别对所有调用走 `restore_codex_state`,而 `restore_codex_state` 在没快照时 fallback 到 `clear_managed_codex_state` —— **直接删除 `~/.codex/config.toml` 和 `auth.json` 里全部 7 个 managed key**。

### 复现路径

1. 用户**从未用过本 app**,但自己**手工**在 `~/.codex/config.toml` 写过(常见场景:用户自己用 OpenAI 反代):
   ```toml
   openai_base_url = "https://my-proxy.example.com/v1"
   model_provider = "custom"
   model = "gpt-4o"
   ```
   `~/.codex/auth.json`:`{"OPENAI_API_KEY": "sk-user-original", "auth_mode": "ApiKey"}`
2. 用户装本 app,**没点 apply**(只是看看 UI)
3. 用户在 UI 上**手点 "清除桌面配置" 按钮**(假设存在) → `POST /api/desktop/clear` → `desktop_clear()` → `restore_codex_state(&paths)` → `has_snapshot=false`(从未 apply 过没快照)→ `clear_managed_codex_state` → 用户手写的 5 个 toml managed key + 2 个 auth managed key **被全部删除**
4. 用户 Codex CLI 直接使用立刻坏(没了 `openai_base_url` / `model_provider` / `OPENAI_API_KEY`)

### 期望

`desktop_clear` 在 `has_snapshot=false` 时应该:
- **非破坏性 fallback**:先把当前 `~/.codex/{config.toml, auth.json}` 备份到 `~/.codex-app-transfer/codex-snapshots/recovery/<timestamp>-pre-clear/` **再** 走 `clear_managed_codex_state`,让"清除"操作可逆
- 或者直接 noop + 返回结构化 error 让前端弹"无快照可清除,操作已跳过"

## 已有调研

- 与 agent 调研报告 §D1 完全一致(critically verified)
- 同样路径 `restore_codex_if_enabled` (`desktop.rs:777-804`) 在 `has_snapshot=false` 时走 desktop.rs:793 `return ... "no snapshot; skip"` ✓ 安全;**对比**`desktop_clear` 没这个 guard
- `restore_codex_snapshot` (`apply.rs:172`) `snapshot_id` 为空时也 fallback 到 `restore_codex_state` (apply.rs:178) → 同样会触发本风险路径

## 风险 / 不确定性

- **触发条件需 UI 主动操作**(无 silent 隐式触发),严重性取决于 UI 是否有显眼的"清除"按钮
- 需要 verify frontend `/api/desktop/clear` 调用入口在哪个组件,是否带二次确认。如果只在"高级设置 → 危险操作"段而且有 confirm dialog,严重性降低
- 修复时要小心不要 break `restore_codex_state` 在 "正常已 apply 然后 restore"路径的语义(那条路径必须保留 `clear_managed_codex_state` 行为)

## 建议方向

下次接手第 1 步:

1. **快查 frontend 调用入口**:`rg "/api/desktop/clear|desktop-clear|clearDesktop" frontend/` 找 UI 入口 + 是否有 confirm dialog
2. **修法 A(推荐)**:`desktop.rs:902-911` `desktop_clear` 内加 has_snapshot 前置判断:
   - has_snapshot=true → 走 `restore_codex_state`(原行为)
   - has_snapshot=false → 先 `snapshot_codex_state` 一次(把当前文件备份到 recovery/pre-clear),**再** 走 `clear_managed_codex_state` → 用户事后可在 UI 上手动恢复
3. **修法 B(更保守)**:has_snapshot=false 时直接返 `{"success": true, "restored": false, "message": "no snapshot to clear"}`,**完全不动文件** — 前端拦下用户的操作
4. 加 unit test 覆盖 has_snapshot=false 时 desktop_clear 不会丢用户手写 managed key

## 关联资源

- 审查 session:2026-05-17 用户主动要求账号还原审查
- 关联代码:`src-tauri/src/admin/handlers/desktop.rs:902-911`、`crates/codex_integration/src/apply.rs:143-213`
- 关联 followup:[#29 C5 cleanup_all 破坏性](29-account-restore-cleanup-all-destructive.md)、[#30 A2+C1 跨版本 managed_keys 升级](30-account-restore-cross-version-managed-keys.md)、[#31 D4 单点存储无冗余](31-account-restore-single-storage-no-redundancy.md)
