---
id: 30
priority: P1
type: refactor
status: active
created: 2026-05-17
related_pr: null
---

# 账号还原 A2 + D4:snapshot 单点存储无冗余,卸载 / 换机 / 用户清除 → 一起丢

## 触发上下文

2026-05-17 用户主动要求审查"账号还原逻辑"。Agent 报告中 A2 / D4 是两条独立 entry,审查时合并为同一类问题(单点存储 + 无冗余备份)处理,优先级 P1。

代码 evidence:
- `crates/codex_integration/src/paths.rs:38-42` snapshot 唯一存储位置:`~/.codex-app-transfer/codex-snapshots/active|recovery/`
- 全 repo grep 无 macOS `~/Library/Application Support/` 备份代码 / 无 Windows `%APPDATA%/...` 备份代码 / 无导出按钮代码

## 问题描述

### 现状

所有 snapshot(active + recovery + legacy)全部存在 `~/.codex-app-transfer/codex-snapshots/`,这个目录被任何以下事件清除 → 用户**真原始账号备份永久丢失**:

1. **用户卸载本 app** — 卸载脚本(如有)+ 用户手动 rm
2. **用户换机器**(从 macOS A 迁到 macOS B) — Time Machine 之外无法迁移 snapshot
3. **用户清理磁盘**误删 `~/.codex-app-transfer/`
4. **磁盘损坏 / 文件系统 corrupt** —— 没 redundant copy

### 复现路径(典型场景)

- **场景 A**(换机器):用户 v2.1.0 装 app 在 MacBook Air,首次 apply 备份了真原始。后来换 MacBook Pro 重新装本 app —— **新 ~/.codex-app-transfer/ 是空的,首次 apply 把当前 ~/.codex 当 baseline**,真原始永远找不回(除非 Time Machine)
- **场景 B**(用户清理):用户某天磁盘满,看到 `~/.codex-app-transfer/` 占 200MB,rm 整个目录腾空间。下次开 app → 重新建 baseline → 原备份丢失

### 期望

至少 1 层冗余 + 用户可控的导出/恢复入口:

- **冗余备份**:macOS `~/Library/Application Support/CodexAppTransfer/snapshot-backups/`、Windows `%APPDATA%\CodexAppTransfer\snapshot-backups\`(系统级用户配置目录,常规清理不会动到)
- **导出按钮**:UI 加 "Export all snapshots as zip" 入口,用户可手动备份到 iCloud / Dropbox / 公司网盘
- **导入按钮**:UI 加 "Import snapshots from zip",支持跨机器 / 重装恢复
- **卸载脚本明确询问**:卸载 .app 时弹"是否保留 snapshot?",不应静默删

## 已有调研

- agent 调研报告 §A2 + §D4 合并(两条都是"单点存储无冗余"的不同表现面)
- `paths.rs:38-42` audit 过,无任何冗余备份代码
- 现有 UI 端口:`GET /api/desktop/snapshots`、`POST /api/desktop/restore`、`GET /api/desktop/snapshot-status`(`desktop.rs:913-949`)— **无导出 / 导入端口**

## 风险 / 不确定性

- 用户操作风险(rm / 卸载 / 换机)不是 code bug,严重度比 #28 / #29 低 → P1 而非 P0
- 冗余备份会增加磁盘占用(每个 snapshot ~10-50 KB,长期 < 几 MB,可接受)
- 跨平台冗余路径需各自实现(macOS / Linux / Windows 系统目录约定不同)
- 导出/导入 zip 涉及序列化格式 + 兼容性(snapshot manifest schema 升级时旧 zip 怎么处理)

## 建议方向

下次接手第 1 步:

1. **冗余备份**(P1):每次 `snapshot_codex_state` 写入 active/ 时,同时复制一份到平台冗余目录(`dirs::data_dir() / "CodexAppTransfer/snapshot-backups/"`)。失败仅 warn 不 fail(冗余非主路径)
2. **导出按钮**(P1):新增 `POST /api/desktop/snapshots/export` 端点,把 `~/.codex-app-transfer/codex-snapshots/` 整目录 zip 后返 download URL;前端 Settings 加 "导出快照" 按钮
3. **导入按钮**(P2):配套 `POST /api/desktop/snapshots/import` 接受 zip,解压到 recovery/(不动 active/ 防 race)
4. **README / 卸载文档**(P0 小补丁):增补"重装/换机前先在 UI 导出快照"提示;卸载脚本(如有)加保留确认

## 关联资源

- 审查 session:2026-05-17 用户主动要求账号还原审查
- 关联代码:`crates/codex_integration/src/paths.rs:38-42`、`src-tauri/src/admin/handlers/desktop.rs:913-949`、`crates/codex_integration/src/snapshot.rs:148-174`(snapshot 写入入口)
- 关联 followup:[#28](28-account-restore-desktop-clear-no-snapshot-guard.md)、[#29](29-account-restore-cleanup-all-destructive.md)、[#31 C1 跨版本 managed_keys](31-account-restore-cross-version-managed-keys.md)
