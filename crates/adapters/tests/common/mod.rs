//! adapters 集成测试共享初始化:把 workspace home 解析重定向到临时目录,隔离
//! 真机数据 —— 与 `crates/proxy/tests/common/mod.rs` 同款,机制详见该文件。
//!
//! [MOC-195] 本 crate 的 `anthropic_messages_response.rs` 直接调
//! `global_response_session_cache()`:不隔离时启动 GC 全表扫真机
//! `~/.codex-app-transfer/sessions.db`(几百 MB 时单测拖到 ~9s),write-through
//! `save()` 还会把测试 session 写进真机 db。
//!
//! **新增 `tests/*.rs` 集成测试 binary 时必须同步挂 `mod common;`**,否则该
//! binary 静默回到真机数据。

/// 所有 `mod common;` 引入本模块的集成测试 binary,main 前自动执行一次。
#[ctor::ctor]
fn isolate_test_home() {
    let dir = tempfile::tempdir().expect("create temp home for test isolation");
    std::env::set_var(codex_app_transfer_registry::HOME_OVERRIDE_ENV, dir.path());
    // set 完立刻端到端自证:override key 改名漂移 / 优先级回归都会让所有用例
    // 静默回到真机数据,必须 fail-closed 当场 panic。
    assert_eq!(
        codex_app_transfer_registry::resolve_home().as_deref(),
        Some(dir.path()),
        "home isolation did not take effect — resolve_home no longer honors HOME_OVERRIDE_ENV"
    );
    // env 在进程存活期间一直指向该目录,不能让 TempDir drop 时删掉;
    // 进程退出后由 OS 临时目录策略回收。
    std::mem::forget(dir);
}
