---
id: 41
priority: P3
type: refactor
status: active
created: 2026-05-23
related_pr: 245
---

# `~/.codex/config.toml` 并发 RMW 加 fs2 lock(理论 race,实际无 trigger 场景)

## 触发上下文

PR #245 Devin pre-merge review 标 🟡:`mcp_servers::upsert_server` / `codex_plugins::set_enabled` / `install_tarball` 等多处对 `~/.codex/config.toml` 走 `read_doc → modify → write_doc` 序列,无 file locking。理论上两个并发 axum handler 同时 RMW 会丢失更新。

## 当前 PR 拒绝采纳的理由

1. **Tauri single-instance**:已配 `tauri-plugin-single-instance`,同 user 不可能开多个 Codex App Transfer.app(start argv 转发到已运行实例)。跨 user 同 HOME 共享 config 也不在支持范围
2. **UI 操作天然顺序**:用户点 Save → fetch wait → toast → reload list,期间不会并发触发第二个 Save。所有 mcp servers 操作走 modal / 表单提交,模式上是 sequential
3. **Deeplink 跟 UI 也是用户顺序**:用户点浏览器 deeplink → app focus + 弹 confirmation modal → 用户点确认 → install。一次只能处理一个 deeplink confirmation
4. **现有 codex CLI 也无锁**:codex 自己也是 RMW 写 config.toml(`codex-rs/config/src/plugin_edit.rs`),只用 atomic rename 保证写半文件不出现。我们这层加锁也只是 atomic write,跟 codex 自身一致

实际威胁面:第三方进程同时写 config.toml(罕见 — 编辑器 / `codex` CLI 后台跑)。但加 fs2 lock 也只能保护 codex-app-transfer 自己进程之间,跟外部 codex CLI 写仍然有 race(对方不持本 app 的 lock)。

## 激活前置条件

发现真实 trigger 场景:
- 用户报"我同时用 UI 加 server 跟点 deeplink 装 plugin,结果某个改动丢了"
- 或者本 app 加了后台 task 自动跑(目前没有)
- 或者跨 user / 多 app instance 并发场景出现(目前不支持)

## 实施方案(激活时)

```rust
// services/config_toml_lock.rs(新)
use fs2::FileExt;
use std::fs::OpenOptions;

pub fn with_locked_doc<F, R>(f: F) -> Result<R, String>
where F: FnOnce(&mut DocumentMut) -> Result<R, String>
{
    let path = mcp_servers::config_path()?;
    let lock_path = path.with_extension("lock");
    let lock_file = OpenOptions::new().create(true).write(true).open(&lock_path)?;
    lock_file.lock_exclusive().map_err(|e| format!("lock: {e}"))?;
    let doc = read_doc()?;
    let result = f(&mut doc)?;
    write_doc(&doc)?;
    drop(lock_file); // release lock
    Ok(result)
}
```

把 mcp_servers / codex_plugins 所有 read_doc → modify → write_doc 都改成 `with_locked_doc` 包起来。

## 不在范围

- 不修 codex CLI 自己的 RMW(我们不能改 codex 源码)
- 不加跨 user 锁(单 user 用例外)

## 相关锚点

- 反例 sites:
  - `src-tauri/src/admin/services/mcp_servers.rs::upsert_server`
  - `src-tauri/src/admin/services/mcp_servers.rs::delete_server`
  - `src-tauri/src/admin/services/mcp_servers.rs::write_raw`
  - `src-tauri/src/admin/services/mcp_servers.rs::restore_from_history`
  - `src-tauri/src/admin/services/codex_plugins.rs::set_enabled`
  - `src-tauri/src/admin/services/codex_plugins.rs::uninstall`
  - `src-tauri/src/admin/services/codex_plugins.rs::install_tarball`(末尾写 [plugins.*])
- fs2 dep 已在 `src-tauri/Cargo.toml`
