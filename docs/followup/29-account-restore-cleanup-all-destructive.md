---
id: 29
priority: P0
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# 账号还原 C5:`desktop_restore` 接 `cleanup_all=true` 物理删光所有 snapshot,无二次确认 / 无软删除

## 触发上下文

2026-05-17 用户主动要求审查"账号还原逻辑是否会导致用户原始账号信息丢失"。Agent 调研 + 主对话 verify 后确认是真实风险路径(用户显式触发的破坏性操作,但缺守门 UX)。

代码 evidence:
- `src-tauri/src/admin/handlers/desktop.rs:924-940` `desktop_restore` 端点接受 `cleanup_all: bool` 参数,透传到 `restore_codex_snapshot(&paths, &snapshot_id, payload.cleanup_all)`
- `crates/codex_integration/src/apply.rs:189-191` `if drop_remaining_snapshots { drop_all_snapshots(paths)?; }`
- `crates/codex_integration/src/snapshot.rs:212-223` `drop_all_snapshots` 实现:`remove_dir_all` 三个目录:`active_snapshots_dir + recovery_snapshots_dir + snapshot_dir(legacy)` —— **全部物理删除,无软删除 / 无 trash 子目录保留**

## 问题描述

### 现状

UI 上用户点 "Restore" 时勾选 "同时清理其他 snapshot"(`cleanup_all=true`),后端**无条件**调 `drop_all_snapshots` 物理删除 `~/.codex-app-transfer/codex-snapshots/{active,recovery}/*` 全部内容,**包括用户最早的真原始账号备份**。

### 复现路径

1. 用户 v2.1.0 装本 app,首次 apply → 创建 snapshot `SA`(快照内容 = 真原始,含 `OPENAI_API_KEY=sk-user-very-old`)
2. 用户长期使用本 app,经过**多次 session**(每次 app 重启都有新 session_id),`snapshot.rs:361` `move_stale_active_snapshots_to_recovery` 把 `SA` 晋级到 `recovery/<old-session-id>/`,**真原始安全保留**
3. 用户某天**手动**修改 `~/.codex/config.toml`(可能误以为这是新原始,或为了测试别的 provider)
4. 下次 apply → 创建 snapshot `SB`(快照内容 = 步骤 3 修改后的版本)
5. 用户在 UI 上点 "Restore" + 选 SB + **勾选 cleanup_all=true** → 后端 `drop_all_snapshots` → `recovery/` 里的 `SA` 跟所有其他 recovery 内容 **全部物理删除** → 用户真原始账号信息 **永久丢失**

### 期望

至少 3 层守门:

1. **后端默认拒收**:`desktop.rs:930` 收到 `cleanup_all=true` 时先返 dry-run preview(会被删除的 snapshot 列表 + provider_name + timestamp + apikey 后 4 位 hash),require 二次确认 token 才真删
2. **软删除**:`snapshot.rs:212-223` `drop_all_snapshots` 改成移到 `~/.codex-app-transfer/codex-snapshots/trash/<timestamp>/`,保留 N 天(默认 30 天)或 N 个(默认 10 份)再物理删
3. **UI 二次确认弹窗**:列出"将被删除的所有 snapshot 详情"(不仅是 count),强制用户阅读才能确认

## 已有调研

- agent 调研报告 §C5 完全一致(critically verified)
- `snapshot.rs:212-223` `drop_all_snapshots` 实现路径 audit 过,**无任何 backup / soft-delete 兜底**
- recovery/ 机制本身设计就是为了"用户长期保留真原始备份",cleanup_all=true 直接抹掉违反这个设计 invariant

## 风险 / 不确定性

- 触发要用户显式勾选 `cleanup_all=true` checkbox,所以**不是 silent bug**
- 但 UI 上是否有显眼的 confirm dialog 需要 verify;按现有 frontend 代码风格(`frontend/js/app.js`)推测可能只有 `confirm("确认?")` 一行
- 软删除会增加磁盘占用(每个 snapshot ~10-50 KB,30 天保留通常 < 10 MB,可接受)

## 建议方向

下次接手第 1 步:

1. **快查 frontend 调用入口**:`rg "cleanup_all|cleanupAll" frontend/` 找 UI 入口,evaluate 现有 confirm dialog 严重程度
2. **修法 P0**:
   - 后端 `desktop.rs:924-940` `desktop_restore` 改成默认 `cleanup_all=false`,即便 frontend 传 true 也要先返 dry-run preview(列表 + count + 总磁盘占用)
   - 前端弹 modal 列出会被删除的所有 snapshot 详情,user 必须再点 "确认删除" 才真删
3. **修法 P0(并行)**:`snapshot.rs:212-223` `drop_all_snapshots` 改成移到 `trash/`,加 GC 任务(每次 daemon 启动清 > 30 天旧 trash)
4. 加 integration test 覆盖 cleanup_all 行为(预设 active+recovery+legacy 三种 snapshot,跑 desktop_restore + cleanup_all=true,验所有数据**最终**都丢失但 trash 内有备份)

## 关联资源

- 审查 session:2026-05-17 用户主动要求账号还原审查
- 关联代码:`src-tauri/src/admin/handlers/desktop.rs:924-940`、`crates/codex_integration/src/apply.rs:189-191`、`crates/codex_integration/src/snapshot.rs:212-223, 361`
- 关联 followup:[#28 D1 desktop_clear 无 guard](28-account-restore-desktop-clear-no-snapshot-guard.md)、[#30 A2+C1 跨版本 managed_keys 升级](30-account-restore-cross-version-managed-keys.md)、[#31 D4 单点存储无冗余](31-account-restore-single-storage-no-redundancy.md)
