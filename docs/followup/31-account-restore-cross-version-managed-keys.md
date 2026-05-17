---
id: 31
priority: P1
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# 账号还原 C1:跨版本 `MANAGED_TOML_KEYS` 升级,restore 旧 snapshot 会按新 list 删除用户原有 key

## 触发上下文

2026-05-17 用户主动要求审查"账号还原逻辑"。Agent 报告 §C1 提出跨版本兼容性风险:`SnapshotManifest` 无 `managed_keys_at_snapshot` 字段,restore 时按**当前 binary 编译时的** MANAGED list 操作,而不是 snapshot 写入时的 list,跨版本升级时可能误删用户在 app 接管前就有的 key。

代码 evidence:
- `crates/codex_integration/src/apply.rs:23-29` `MANAGED_TOML_KEYS` 当前 5 个:`openai_base_url, model_context_window, model_catalog_json, model, model_provider`
- `crates/codex_integration/src/apply.rs:20` `MANAGED_AUTH_KEYS` 当前 2 个:`auth_mode, OPENAI_API_KEY`
- `crates/codex_integration/src/snapshot.rs:25-30` `SnapshotManifest` schema 当前**不含** `managed_keys_at_snapshot` 字段(只有 `config_existed, auth_existed, app_version, snapshot_at, schema_version`)
- `crates/codex_integration/src/apply.rs:215-237` `restore_from_snapshot_values` 遍历 `MANAGED_TOML_KEYS` 用 snapshot 字面量回填,snapshot 没有该 key 时调 `sync_root_value(key, None)` → **删除**

## 问题描述

### 现状

restore 行为是"对每个 managed key 用 snapshot 字面量回填;snapshot 里没有就删"。当 `MANAGED_TOML_KEYS` 数组**在 app 升级时新增条目**(典型场景:issue #178 / PR #181 把 `model_provider` 强制改为 `"openai"` 即可能是这种历史改动),旧 snapshot 不可能记新晋升 managed 的 key 的字面量,restore 时一律按"snapshot 没记 → 删" 处理。

### 复现路径

1. 用户 v2.1.4 装本 app,首次 apply:
   - 当时 `MANAGED_TOML_KEYS = [openai_base_url, model_context_window, model_catalog_json, model]`(假设 `model_provider` 尚未列入)
   - 用户 `~/.codex/config.toml` 原本含 `model_provider = "azure"`(自己手写过)
   - snapshot 写入 — `config.toml` 整文件 cp 完整保留(`snapshot.rs:162-174`),**但 manifest 不记 "我当时认 manage 哪些 key"**
2. 用户长期使用,某天升级到 v2.2.0:
   - `MANAGED_TOML_KEYS = [openai_base_url, model_context_window, model_catalog_json, model, model_provider]` (新增 model_provider)
3. v2.2.0 触发 restore(app 退出 / 用户主动) → `restore_from_snapshot_values` 遍历**新版 5 个 managed key**:
   - 前 4 个用 v2.1.4 snapshot 里的字面量正确回填
   - **第 5 个 `model_provider`** — v2.1.4 snapshot 字面量没存它(snapshot 只记快照写入时 managed 的 key 的字面量,虽然完整 config.toml 被 cp 保留,但 `read_snapshot_config` 等只读 managed 字段?需 verify)→ `sync_root_value(&paths.config_toml, "model_provider", None)` → **删除用户手写的 `model_provider = "azure"`**

### 期望

- `SnapshotManifest` 加 `managed_keys_at_snapshot: Vec<String>` 字段,snapshot 写入时刻 freeze 当时的 MANAGED list
- restore 时按**snapshot 的 managed_keys** 而非当前 binary 的 list 操作:
  - 旧 snapshot(无该字段)→ 用兜底 list(snapshot 写入时刻 v2.x app_version 推出的历史 list,硬编码兼容表)
  - 新 snapshot → 按 snapshot 记录的 list 精确还原

## 已有调研

- agent 报告 §C1 完全一致
- snapshot manifest 当前 schema:`snapshot.rs:25-30` 的 `SnapshotManifest` struct(需 verify 实际字段)
- 历史 `MANAGED_TOML_KEYS` 演化:`git log -p -- crates/codex_integration/src/apply.rs` 可查每次新增
- **关键未 verify**:`read_snapshot_config` (`snapshot.rs:242-261` 附近) 是读 manifest 记录的 key 字面量,还是 parse 完整 config.toml 副本?如果是后者,本风险**不存在**(用户手写的 key 会通过整文件 cp 被保留,restore 时仍能读出)

## 风险 / 不确定性

- 触发条件需要"已有用户 + 跨版本升级 + 新版本新增 managed key 在旧用户配置里也有手写值",窄但不为 0
- 严重性取决于 snapshot 实现细节,需要 read snapshot.rs 完整代码 verify 上面 "关键未 verify" 项
- 修法增加 schema 字段,需要 backward compat(老 snapshot 无该字段时 default 用历史兜底 list)

## 建议方向

下次接手第 1 步:

1. **Verify 实际风险**(零代价):read `crates/codex_integration/src/snapshot.rs` 完整,确认 `read_snapshot_config` / `read_snapshot_config_by_id` 究竟读什么。如果是整 config.toml 字符串(取自 cp 副本),用户手写的非 managed key 会保留 ✓ 安全;如果只读 manifest 里的 managed-key 字面量,本风险 valid → 走步骤 2
2. **修法**(P1,前提是步骤 1 确认 valid):
   - `SnapshotManifest` 加 `managed_keys_at_snapshot: Vec<String>` 字段,版本号 `schema_version` 升到 2
   - `snapshot_codex_state` 写入时 freeze 当时的 `MANAGED_TOML_KEYS.iter().chain(MANAGED_AUTH_KEYS).collect()`
   - `restore_from_snapshot_values` 按 manifest 里的 list 而不是当前 binary 的常量
   - 老 schema_version=1 snapshot:fallback 到硬编码的"v1 时代 MANAGED list" 兜底
3. **测试**:加 cross-version restore test(预设 v1 schema snapshot,跑新 binary restore,验用户手写的"新晋升 managed key" 不丢)

## 关联资源

- 审查 session:2026-05-17 用户主动要求账号还原审查
- 关联代码:`crates/codex_integration/src/apply.rs:20-29, 215-237`、`crates/codex_integration/src/snapshot.rs:25-30, 162-174`(snapshot 写入)
- 历史 issue:#178(`model_provider` 强制 openai 修复)— 可能是历史 MANAGED list 演化的实例
- 关联 followup:[#28](28-account-restore-desktop-clear-no-snapshot-guard.md)、[#29](29-account-restore-cleanup-all-destructive.md)、[#30](30-account-restore-single-storage-no-redundancy.md)
