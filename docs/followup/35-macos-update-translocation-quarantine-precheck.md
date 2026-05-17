---
id: 35
priority: P3
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# macOS update 加 translocation / quarantine 前置检查(借鉴 AiMaMi)

## 触发上下文

2026-05-17 调研 AiMaMi "自动更新借鉴清单"。AiMaMi `/private/tmp/AiMaMi/src-tauri/src/platform/update.rs` 145 行虽然 update.rs 自身只是 sanity check(实际 update 流程走 `tauri-plugin-updater` 标准 + AiMaMi 的 `tauri.conf.json plugins: {}` 还是空的 stub),但它的 macOS 前置检查思路本项目 update.rs 942 行**完全没做**,值得借鉴。

## 问题描述

### 现状

本项目 `src-tauri/src/admin/handlers/update.rs:198-208 install_command_parts` macOS 分支:

```rust
if platform.starts_with("macos-") {
    return Ok(vec!["open".to_owned(), path.to_owned()]);
}
```

`update.rs:232-248 launch_update_installer` 直接 `Command::new("open").arg(path).spawn()`,没有任何前置检查。

### 问题场景

#### 场景 1: App Translocation 拒绝 `open .pkg`

用户从 `.dmg` 双击挂载后,不拖到 `/Applications/`,直接从 mount point(`/Volumes/...`)运行 `Codex App Transfer.app` —— macOS Gatekeeper 把 .app 移到临时只读路径 `/private/var/folders/.../AppTranslocation/<uuid>/d/Codex App Transfer.app` 运行(translocation 安全机制)。

此时 `update.rs:376-381` 把 installer 下到 `std::env::temp_dir().join("Codex-App-Transfer/updates/")` —— translocation 下 `std::env::temp_dir()` 解析到 `/private/var/folders/<UID>/T/`,**跨 sandbox 边界 `open` 仍能跑**,但用户操作的 .pkg 安装目标默认是 `/Applications/Codex App Transfer.app` —— 跟当前正在运行的 translocated bundle 不是同一个,**安装完仍然旧版本在跑,用户感知"升级失败"**。

#### 场景 2: Quarantine 二次 Gatekeeper 弹窗

`.pkg` 下载后带 `com.apple.quarantine` xattr(`reqwest` 走 HTTP 下来 macOS 自动 attach),`open <pkg>` 会触发 Gatekeeper 二次确认对话框("Codex-App-Transfer.pkg 来自互联网,确定打开吗?")。**虽然不是 bug,但用户体感"为什么升级要再点一次允许"**。

### 期望

升级前置检查 + 友好引导:

- 检测当前 `.app` 路径 contains `/AppTranslocation/` → 不进 update 流程,弹窗"请先把 Codex App Transfer 拖到 `/Applications/` 再升级"
- installer 下载后主动清除 quarantine attribute,避免二次弹窗

## 已有调研

### AiMaMi 上游实现

`/private/tmp/AiMaMi/src-tauri/src/platform/update.rs`(145 行):

- **L47-83 `macos_update_installability()`** —— 主入口,返回 `UpdateInstallabilityPayload`
- **L86-92 `resolve_app_bundle_from_exe()`** —— 从 `std::env::current_exe()` 反向遍历 3 级父目录,找 `.app` 扩展名
- **L95-97 `is_app_translocation_path()`** —— `path.to_string_lossy().contains("/AppTranslocation/")`
- **L100-102 `is_disk_image_mount_path()`** —— `path.to_string_lossy().starts_with("/Volumes/")`
- **L105-113 `has_quarantine_attribute()`** —— shell out `xattr -p com.apple.quarantine <path>`,检查 exit code

### 本项目当前 update.rs 涉及 macOS 的代码

- `update.rs:139-141, 161-184` `pick_macos_installer` 优先 .pkg 后 fallback .dmg(已正确)
- `update.rs:202-203, 217-228` install_command_parts macOS 分支 / install_after_quit_command_parts macOS 分支(test-only)
- `update.rs:610-625` update_install handler 末尾 macOS 友好文案 "Installer downloaded. App will exit and launch the installer."
- **没有任何 translocation / disk-image / quarantine 检测**

### 本项目其他相关已有处理

`src-tauri/src/admin/handlers/desktop.rs` `restart_codex_app` / `open_codex_app` 等流程是针对 Codex Desktop(`/Applications/Codex.app`),不是 self-update。self-update 路径全在 `update.rs`。

## 风险 / 不确定性

### xattr 命令依赖

`xattr` 是 macOS 系统自带 `/usr/bin/xattr`,无额外依赖。但 spawn shell command 失败时(罕见,文件权限 / SIP / sandbox)需要 graceful fallback —— 不能让 xattr 检测失败阻挡升级,做 best-effort 即可。

### 清除 quarantine 需要 entitlement?

`xattr -d com.apple.quarantine <pkg>` 在用户进程权限下应该可以(Codex App Transfer 自身已经被 Gatekeeper 接受,只是清自己下的文件的 attribute)。**但需要真机验证**,某些 hardened runtime + library validation 组合可能限制。

### 误判风险

`resolve_app_bundle_from_exe` 反向 3 级:`exe_path = .../Codex App Transfer.app/Contents/MacOS/codex-app-transfer` → 3 级父目录 = `.../Codex App Transfer.app`(对的)。但如果 user 自己改了目录结构(虽然非常少见),会拿错路径。AiMaMi 的实现 `update.rs:86-92` 假设 Tauri 标准 bundle 布局,本项目也是 Tauri 标准,可以直接复刻。

### 复现要求

- Translocation 检测:用户必须从 .dmg 双击运行 .app(不拖入 Applications)才能触发 —— 现实中用户体验很多人就是这么做,不算稀有
- Quarantine 弹窗:必现,每次首次启动新下的 .pkg

## 建议方向

下次接手按这个顺序:

1. **复刻 AiMaMi 4 个 helper** 到本项目 —— 新建 `src-tauri/src/platform/macos_update.rs`,复刻 `resolve_app_bundle_from_exe` / `is_app_translocation_path` / `is_disk_image_mount_path` / `has_quarantine_attribute`(file:line 见上),代码量 < 100 行
2. **加 `pre_install_check_macos()`** —— update_install 前先调,translocation → 返 friendly error,disk-image → 同上;quarantine → 清除而非报错
3. **接入点** —— `update.rs:553-628 update_install` 入口加分支:
   ```rust
   if cfg!(target_os = "macos") {
       if let Err(e) = pre_install_check_macos() {
           return err(StatusCode::PRECONDITION_FAILED, e).into_response();
       }
   }
   ```
4. **frontend 提示** —— `frontend/js/app.js renderUpdateBadge`(`app.js:294`)接收新错误码,展示引导用户拖到 /Applications 的中英双语提示
5. **测试** —— `update.rs` 现有 `#[cfg(test)]` 段已有 update install fixture,加 path-mocked 单测 `is_app_translocation_path` / `is_disk_image_mount_path` 正负 case
6. **README 同 PR 加上游致谢段** —— 借鉴自 AiMaMi `update.rs:47-113`(memory feedback_credit_upstream_in_readme)

## 关联资源

- 上游借鉴:`/private/tmp/AiMaMi/src-tauri/src/platform/update.rs:47-113`(MIT 假设,需 LICENSE 二次确认)
- 关联 followup: #34(客户端 RSA 验签 — 同次借鉴调研发现)、#36(Windows NSIS in-place)
- 代码锚点:
  - 本项目 `src-tauri/src/admin/handlers/update.rs:198-248 install_command_parts / launch_update_installer / install_after_quit_command_parts`
  - 本项目 `src-tauri/src/admin/handlers/update.rs:553-628 update_install` axum handler
  - 本项目 `frontend/js/app.js:294 renderUpdateBadge` UI
- Apple 文档:
  - App Translocation: https://eclecticlight.co/2017/02/02/app-translocation-the-final-piece-of-the-puzzle/
  - quarantine xattr: `man xattr` / TN2206
