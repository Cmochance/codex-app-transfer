//! proxy 集成测试共享初始化:把 workspace home 解析重定向到临时目录,隔离真机数据。
//!
//! [MOC-195] 集成测试进程会触发 `global_response_session_cache()` 惰性初始化,
//! 其启动 GC(MOC-168 `sweep_orphan_messages` / `sweep_orphan_blobs`)全表扫描
//! `~/.codex-app-transfer/sessions.db` —— 开发机上几百 MB 真实数据时 debug build
//! 扫描超 5s 且持 db mutex,把走 adapter 转换路径的用例直接拖过 client timeout
//! (reqwest `TimedOut` / WS `Elapsed`);测试 write-through 还会写真机 db。CI
//! runner HOME 干净所以从不复现,属本地专属陷阱。
//!
//! `#[ctor]` 在 main 前的单线程阶段 set env,规避 `std::env::set_var` 的多线程
//! 数据竞争(对照 `forward_trace.rs` 被迫把两 leg 合进一个 `#[test]` 的折衷)。
//! 设 [`HOME_OVERRIDE_ENV`](codex_app_transfer_registry::HOME_OVERRIDE_ENV)
//! (`resolve_home` 最高优先级)而非 `HOME`,只重定向 workspace 自己的路径
//! 解析,不波及测试进程里其他读 HOME 的逻辑。
//!
//! **新增 `tests/*.rs` 集成测试 binary 时必须同步挂 `mod common;`**(自带
//! home 隔离的 `forward_trace.rs` 除外),否则该 binary 静默回到真机数据。

/// 所有 `mod common;` 引入本模块的集成测试 binary,main 前自动执行一次。
// ctor 1.0 起要求显式 `unsafe` 标注(ctor 在 main 前的单线程阶段运行,语义未变)。
#[ctor::ctor(unsafe)]
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
