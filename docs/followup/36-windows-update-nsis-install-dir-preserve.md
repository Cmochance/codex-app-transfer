---
id: 36
priority: P3
type: bug
status: active
created: 2026-05-17
related_pr: null
---

# Windows update 走 NSIS `/D=<install_dir>` 保持安装目录(借鉴 AiMaMi)

## 触发上下文

2026-05-17 调研 AiMaMi 自动更新借鉴清单时发现,AiMaMi `/private/tmp/AiMaMi/src-tauri/src/platform/update.rs:7-23` 有专门的 `windows_current_install_dir_arg` / `windows_install_dir_arg_from_exe` 两个 helper,通过 `std::env::current_exe()` 反向定位安装目录,拼成 NSIS 标准的 `/D=<dir>` 参数传给 setup.exe,确保升级 in-place 不换位置。

本项目 `src-tauri/src/admin/handlers/update.rs:198-208 install_command_parts` Windows 分支直接返 `vec![path.to_owned()]` —— 只传 installer 路径,**不传 `/D=`,NSIS 默认会显示"选择安装目录"对话框 / 默认装回 `C:\Program Files\Codex App Transfer\`**。

## 问题描述

### 现状

`update.rs:199-200`:
```rust
if platform.starts_with("windows-") {
    return Ok(vec![path.to_owned()]);
}
```

`update.rs:240-247 launch_update_installer`:
```rust
Command::new(program)
    .args(args)
    .stdin(Stdio::null())
    ...
    .spawn()
```

→ Windows 用户升级时执行 `Codex-App-Transfer-v?.?.?-Windows-Setup.exe`,NSIS UI 弹"选择安装目录":
- 默认值 `C:\Program Files\Codex App Transfer\`(或上次保存到注册表的值,看 NSIS 模板)
- 用户若首装在 `D:\Apps\` / `E:\Portable\` —— NSIS UI 显示默认 `C:\Program Files\` → 用户不小心一路 Next 就会**双装**(C: + D: 都有,但只有 D: 那份有用户数据,启动 C: 那份就是空白配置)
- 即使 NSIS 模板记了上次目录,UI **每次还是要点 Next 跳过** —— 体感"为什么升级要再选位置"

### 期望

`/D=<install_dir>` NSIS 参数(NSIS Chapter 3.2 标准 https://nsis.sourceforge.io/Docs/Chapter3.html#3.2):
- `<install_dir>` 必须是绝对路径,**不能有引号**(NSIS bug),必须是命令行最后一个参数(NSIS parser quirk)
- 设置后 NSIS 跳过"选目录"对话框,直接装到该目录

### 差距

`update.rs:199-200` Windows 分支只返 `[path]`,**没有 `/D=` 也没有 `/S` (silent)**;`/S` 上不上是体验权衡,`/D=` 是基本要做的。

## 已有调研

### AiMaMi 实现

`/private/tmp/AiMaMi/src-tauri/src/platform/update.rs:7-23`:

- **L7-10 `windows_current_install_dir_arg()`** —— 顶层入口,内部调 `windows_install_dir_arg_from_exe(std::env::current_exe()?)`
- **L13-23 `windows_install_dir_arg_from_exe(exe_path)`** ——
  - `exe_path.parent()` 拿 install dir
  - 转 String,trim trailing backslash
  - 拼 `/D=<dir>` 返 String

注意 NSIS 规范:
- `/D=` 必须**整个**作为**一个**命令行 token(无内部空格,有空格的路径不能加引号,直接传)
- 必须是 args 数组的**最后一项**

### 本项目当前 Windows install 链路

`update.rs:146-159 pick_windows_installer`:
- 只接受 filename `endsWith("windows-setup.exe")`(line 154-156)
- 排除 `Codex-App-Transfer-Windows-Portable.exe` / `Codex-App-Transfer-Windows-x64.exe` —— 这两种 portable 不走 NSIS,本就不在 in-place install 流程

`update.rs:198-208 install_command_parts`:
- 只返 `[installer_path]`,没有任何 NSIS args

`update.rs:232-248 launch_update_installer`:
- `Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()`
- 第一个 program 就是 installer.exe,args 是空 —— 等于不带任何 flag 跑 setup.exe

### NSIS 模板出处

Tauri 2 默认 Windows installer 走 NSIS,`tauri.conf.json bundle.targets: "all"` 会同时产 nsis + msi(本项目 release/ 只放 nsis Setup.exe,line 23 验证)。NSIS 模板就是 Tauri 内置的标准 NSIS 脚本,支持 `/D=` 标准 flag,无需自定义模板修改。

## 风险 / 不确定性

### 路径有空格

`C:\Program Files\Codex App Transfer\` 里有空格 —— NSIS `/D=` 处理空格的规则是"整个 /D=...path 作为单个 token,不能用引号":

- ❌ `setup.exe /D="C:\Program Files\Codex App Transfer"` (引号失败)
- ✅ `setup.exe /D=C:\Program Files\Codex App Transfer` (作为命令行最后一个 arg)
- Rust `Command::args` 会自动 escape,需要测试 actual 命令是否被 escape 成 `"/D=C:\Program Files\Codex App Transfer"`(带引号)还是裸 token —— **Tauri 2 build target Windows 真机测试不可省**

### 当前 exe 路径 vs install dir

`std::env::current_exe()` 在 Windows portable 模式下指向 portable 解压目录 —— 但我们已经在 `pick_windows_installer` 过滤只用 Setup.exe(installed 版本),portable 用户不会走这条 update 路径,无问题。

### NSIS UI 是否仍然显示

`/D=<dir>` 跳过"选目录"步骤,但**其他步骤(欢迎页 / 许可证 / 完成页)默认仍显示**。如果用户希望完全静默(`/S`),那是更激进的体验改动,需要权衡:
- `/S` silent → 用户感知"突然装好了",但不知道什么时候完成
- 默认 `/D=` only → 用户看几个 Next,但不需要选目录
- 推荐 `/D=` only,不上 `/S`

### Tauri 内置 updater 是否已经处理

不适用 —— 本项目 `src-tauri/Cargo.toml` **没**用 `tauri-plugin-updater`,update flow 完全自写(`update.rs` 942 行),所以 Tauri 默认 NSIS in-place 逻辑不会自动 kick in,必须客户端代码自己拼参数。

## 建议方向

下次接手按这个顺序:

1. **新增 Windows helper** —— `src-tauri/src/platform/windows_update.rs`(若 platform/ 目录不存在则建),复刻 AiMaMi `update.rs:7-23` 的 `windows_install_dir_arg_from_exe(exe_path: PathBuf) -> Option<String>`,返 `Some("/D=<dir>")` 或 None
2. **修 install_command_parts** —— `update.rs:198-208` Windows 分支:
   ```rust
   if platform.starts_with("windows-") {
       let mut parts = vec![path.to_owned()];
       if let Some(d_arg) = windows_install_dir_arg_from_exe(std::env::current_exe().ok()) {
           parts.push(d_arg);  // /D= 必须最后一个
       }
       return Ok(parts);
   }
   ```
3. **install_after_quit_command_parts 同步处理** —— `update.rs:211-230` 目前 Windows 分支直接复用 `install_command_parts`,自动会带 /D=,无需额外改
4. **现有单测覆盖** —— `update.rs:639-704 update_platform_version_and_installer_selection_match_legacy` 已经 assert `install_command_parts(C:\..., "windows-x64") == ["C:\\Codex-App-Transfer-Windows-Setup.exe"]`(line 683-685);改完后这个 assert 需要更新成 `[".exe", "/D=C:\\"]` 形式或拆 case
5. **真机测试** —— Windows 11 真机,装在非默认路径(如 `D:\Apps\Codex App Transfer\`),触发 update,观察 NSIS UI 是否还问选目录 + 装完后是否仍在 `D:\Apps\` 而不是 `C:\Program Files\`
6. **README 同 PR 加致谢段** —— 借鉴自 AiMaMi `update.rs:7-23`(memory feedback_credit_upstream_in_readme)

## 关联资源

- 上游借鉴:`/private/tmp/AiMaMi/src-tauri/src/platform/update.rs:7-23`(MIT 假设,需 LICENSE 确认)
- NSIS 文档:https://nsis.sourceforge.io/Docs/Chapter3.html#3.2 `/D=` 标准 flag
- 关联 followup: #34(客户端 RSA 验签)、#35(macOS translocation/quarantine)— 同次借鉴调研派生
- 代码锚点:
  - 本项目 `src-tauri/src/admin/handlers/update.rs:146-159 pick_windows_installer`
  - 本项目 `src-tauri/src/admin/handlers/update.rs:198-208 install_command_parts`
  - 本项目 `src-tauri/src/admin/handlers/update.rs:232-248 launch_update_installer`
  - 本项目 `src-tauri/src/admin/handlers/update.rs:639-704 现有单测`
- Tauri 2 NSIS 模板:tauri-cli 内置,默认支持 `/D=`(无需自定义)
