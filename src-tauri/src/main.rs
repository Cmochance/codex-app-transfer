// Stage 6:Tauri 自定义 URI scheme `cas://localhost/` → in-process axum,
// frontend/ 整目录 include_dir 进二进制。frontend 零改动(v1.4 Bootstrap 视觉)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod admin;
mod anyrouter_quota;
mod codex_plugin_unlocker;
mod codex_quota_injector;
mod codex_real_account;
mod codex_theme_injector;
mod deepseek_quota;
mod glm_quota;
mod kimi_code_quota;
mod kimi_session;
mod macos_dock;
mod mcp_webfetch_server;
mod mimo_quota;
mod mimo_session;
mod moonshot_quota;
mod opencode_go_quota;
mod opencode_session;
mod provider_quota;
mod proxy_runner;
#[cfg(target_os = "macos")]
mod single_instance;
mod system_proxy;
mod telemetry_bridge;
mod trace_viewer;
mod trae_quota;
mod web_session_quota;
#[cfg(target_os = "windows")]
mod windows_msix;

use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use proxy_runner::ProxyManager;
use std::io::Write;

use tauri::menu::{Menu, MenuBuilder, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, WindowEvent};
use tauri_plugin_deep_link::DeepLinkExt;
use tower::ServiceExt;

use admin::{build_app_router, handlers, AdminState};

/// [MOC-257 review] 读 `restoreCodexOnExit`(默认 true)。stash 真账号还原(startup self-heal / exit)跟
/// `restore_codex_if_enabled` 同 gate 在它上:=false 表示「退出不还原、保留 transfer 状态」,此时**不**该把
/// stash 真账号挪回活动(否则违背意图、real-active 与 persisted off/synthetic 不一致;且 restore_codex 跳过
/// 后 un-stash 也补不到 managed key)。
fn restore_codex_on_exit_enabled() -> bool {
    handlers::settings::load_registry_for_startup_language_sync()
        .ok()
        .and_then(|cfg| {
            cfg.get("settings")
                .and_then(|s| s.get("restoreCodexOnExit"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(true)
}

fn main() {
    // MCP stdio server 模式 (MOC-144): Codex 把本二进制作为 mcp_server spawn 时带
    // `--mcp-serve-webfetch`。必须在任何可能写 stdout 的初始化(含 telemetry)之前分流 ——
    // MCP stdio 要求 stdout 只能是 JSON-RPC 消息, 且此模式不启 Tauri window。
    if std::env::args().any(|a| a == "--mcp-serve-webfetch") {
        mcp_webfetch_server::run();
        return;
    }
    // 必须在所有可能 emit tracing event 的代码之前 init,否则 startup 阶段
    // (registry healing / desktop apply / proxy 拉起)的 tracing event 会被 drop。
    telemetry_bridge::init_global_subscriber();

    // MOC-256:在 Tauri / webview / HTTP 起来**之前**就把无 Chrome 新装的 webFetchBackend
    // 落为 off,确保前端首次 GET /api/settings(及任何 save 响应)就读到 off —— 避免迁移
    // 落盘前显示陈旧 auto、点已选中的 auto 触发 early-return 而非门控。临时 current-thread
    // runtime 跑一次即 drop,不与 Tauri 自身 runtime 冲突;`--mcp-serve-webfetch` 子进程路径
    // 已在 run() 内自行落盘,故此处只覆盖 GUI 进程。幂等 + 跨进程锁。
    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        rt.block_on(crate::admin::services::mcp_servers::default_web_fetch_off_if_no_chrome());
    }

    // [MOC-196] macOS 自管单实例:flock 持锁 + 就绪态握手 + 超时接管,根治
    // 「僵尸主实例 → 后续启动被插件无条件 exit(0) 静默杀」(#436)。guard 持有
    // flock 至进程结束;第二实例路径在内部 exit、不返回。Windows/Linux 仍走
    // tauri_plugin_single_instance(下方 cfg 分支)。
    #[cfg(target_os = "macos")]
    let _instance_lock = single_instance::acquire_or_exit();

    let proxy_manager = Arc::new(ProxyManager::new());
    // [MOC-169] 诊断流量查看器:独立端口 SSE 服务,默认关,gate 开时随 app 自启(见 setup)。
    let trace_viewer_manager = Arc::new(trace_viewer::TraceViewerManager::new());
    let admin_state = AdminState {
        proxy_manager: proxy_manager.clone(),
        trace_viewer_manager: trace_viewer_manager.clone(),
    };
    let app_router = Arc::new(build_app_router(admin_state));
    let app_router_for_protocol = app_router.clone();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init());
    // [MOC-196] macOS 用上面的自管单实例(插件的 socket 方案会被僵尸主实例
    // 骗过且第二实例无条件 exit(0) 静默死,见 single_instance.rs 模块注释);
    // Windows(CreateMutex)/Linux(DBus)实现不同、无此故障实证,保留插件。
    #[cfg(not(target_os = "macos"))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        show_main_window(app);
        // single-instance 启动时如果带 deeplink URL,argv 里会有,转发给前端
        for arg in _argv.iter().skip(1) {
            if arg.starts_with("codex-app-transfer:") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.emit("codex-deeplink", arg.clone());
                }
            }
        }
    }));
    let app = builder
        .manage(proxy_manager)
        .manage(trace_viewer_manager)
        .register_asynchronous_uri_scheme_protocol("cas", move |ctx, request, responder| {
            // [MOC-211 安全] cas:// 同进程 admin API 只允许**主窗口**访问。MiMo 小米账号登录等
            // 加载**外部页面**的 webview(label != "main")若能发 cas://localhost/api/... 即可篡改
            // 本地 provider/settings(外部页面/被篡改脚本/重定向/MITM 拿到本地 admin API,P1);
            // 故非主窗口一律 403、绝不转进 admin router。
            if ctx.webview_label() != "main" {
                let resp = tauri::http::Response::builder()
                    .status(403)
                    .body(b"forbidden: cas scheme is restricted to the main webview".to_vec())
                    .unwrap();
                responder.respond(resp);
                return;
            }
            let router = app_router_for_protocol.clone();
            tauri::async_runtime::spawn(async move {
                let response = handle_cas_request(router, request).await;
                responder.respond(response);
            });
        })
        .setup(|app| {
            let startup_proxy_manager = app.state::<Arc<ProxyManager>>().inner().clone();
            // [MOC-211 / CAT-256] 存全局 AppHandle 供 web-session provider(MiMo / OpenCode)内嵌
            // webview 登录开窗用(AdminState 在建 router 时尚无 AppHandle,故走全局 OnceLock)。
            web_session_quota::init(app.handle().clone());
            // [Dock 隐藏] 存全局 AppHandle 供 save_settings hot-reload 切 activation policy。
            macos_dock::init(app.handle().clone());

            // [dev] tauri.conf.json 的 window url 是 cas://localhost/(prod 同进程 axum 派发)。
            // cas:// 是自定义协议,Tauri 不会用 build.devUrl 替换它(devUrl 只对 app-relative
            // URL 生效),故 dev 模式手动把主窗口导航到 vite dev server,享受 HMR;前端 /api
            // 请求经 vite proxy → 127.0.0.1:18900 的 debug TCP listener(见下方 app.run 前)。
            // release 不编译此段,窗口仍走 cas://localhost/。
            #[cfg(debug_assertions)]
            if let Some(w) = app.get_webview_window("main") {
                if let Ok(url) = "http://localhost:1420".parse::<tauri::Url>() {
                    let _ = w.navigate(url);
                }
            }
            // [MOC-257 三态] self-heal restore:上次若被强杀(退出 hook 没跑)、真账号被 OFF/synthetic 移到
            // stash、~/.codex 无 auth.json,整文件还原回去。**顺序同 exit:先 restore_codex_if_enabled,再
            // un-stash 真账号**([MOC-257 review] 反转,真账号最终写)——原序让 restore_codex 在 stash 还原出的
            // 真账号上 merge 旧快照 managed auth key,快照拍于 ChatGPT 登录前(无 auth_mode)时会抹掉真账号 genuine
            // 的 `auth_mode=chatgpt`。改 restore_codex 先跑(还原 config + strip transfer key),再 restore_stashed
            // 整文件覆盖回真账号 → auth_mode/tokens 全胜过旧快照。原「补 managed key 防空壳」由真账号在后覆盖解决。
            // 同步版(无 block_on/AUTH_LOCK):此刻在任何 auth task spawn 之前、无并发。失败留痕。
            // [review] un-stash gate 在 restoreCodexOnExit:=false 表示保留 transfer 状态、跳过。
            let startup_restore_ok = handlers::desktop::restore_codex_if_enabled("startup")
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // [MOC-257 review] 同 exit:un-stash 只在 restore 成功后跑;失败则真账号留 stash 待下次 self-heal。
            if restore_codex_on_exit_enabled() && startup_restore_ok {
                if let Err(e) = crate::codex_real_account::restore_stashed_real_auth_blocking() {
                    tracing::error!(
                        "[PluginUnlock] 启动 stash 真账号还原失败(真账号仍在 stash 待下次重试): {e}"
                    );
                }
            }

            // #262:加载 `settings.language` 一次,同步到 adapters 全局,确保
            // startup 后第一个 user 请求的 prompt 注入就是正确语言。后续 user
            // 切语言由 `save_settings` 内的 hot reload(同模块 fn)处理。
            if let Ok(cfg) = handlers::settings::load_registry_for_startup_language_sync() {
                let settings = cfg.get("settings").cloned().unwrap_or_else(|| serde_json::json!({}));
                handlers::settings::sync_user_language_from_settings(&settings);
                // 启动时按持久化的 `hideDockIcon` 应用 macOS Dock 图标显隐。
                macos_dock::apply_from_settings(&settings);
            }

            // [MOC-185] 诊断流量查看器:仅 env `CAS_DIAG_TRACE` 显式开发者入口随 app 自启。
            // 「诊断模式」UI 开关已改为 **session 级一次性**(退出 transfer 即关、不持久化、不
            // 随启动自启 —— 见 app.js toggle / renderSettings),故启动**不再读** traceViewerEnabled。
            // 运行时采集 gate 由 `vm.start`/`stop_silent` 在 start_lock 内与 viewer 生命周期原子
            // 绑定(成功才开 gate、失败不开 → 无残留),这里不单独动 gate。
            if codex_app_transfer_proxy::diagnostics::forward_trace_enabled()
            {
                let vm = app
                    .state::<Arc<trace_viewer::TraceViewerManager>>()
                    .inner()
                    .clone();
                match vm.start(trace_viewer::DEFAULT_TRACE_VIEWER_PORT) {
                    Ok(addr) => codex_app_transfer_proxy::proxy_telemetry().logs.add(
                        "INFO",
                        format!("[trace-viewer] 诊断流量查看器已启动 http://{addr}"),
                    ),
                    Err(e) => codex_app_transfer_proxy::proxy_telemetry()
                        .logs
                        .add("WARN", format!("[trace-viewer] 启动失败: {e}")),
                }
            }

            // [MOC-204] 额度条目注入 daemon:每 tick 读 settings.codexQuotaEnabled
            // + proxy rate limit 快照,经 CDP 推进 Codex Environment 卡片。
            // 开关关 / CDP 不可达时 tick 内静默跳过,常驻无负担。
            tauri::async_runtime::spawn(codex_quota_injector::run_quota_daemon());

            // [MOC-231] GC 旧的上下文明细缓存(context-breakdown/<uuid>.json,每对话一个;
            // >14 天的陈旧对话删除,下次有请求会重建)。fire-and-forget,不阻塞 startup。
            tauri::async_runtime::spawn(async {
                codex_app_transfer_adapters::responses::gc_context_breakdown(
                    std::time::Duration::from_secs(14 * 24 * 60 * 60),
                );
            });

            // Deep link scheme handler:codex-app-transfer://v1/import?...
            // 转发 URL 给前端 codexMcpHandleDeeplink() 弹 confirmation modal。
            let app_handle_for_deeplink = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                let urls = event.urls();
                for url in urls {
                    if let Some(window) = app_handle_for_deeplink.get_webview_window("main") {
                        let _ = window.set_focus();
                        let _ = window.emit("codex-deeplink", url.to_string());
                    }
                }
            });
            // follow-up #29:GC ~/.codex-app-transfer/codex-snapshots/trash/ 下
            // mtime > TRASH_RETENTION_DAYS 天的软删 bucket。fire-and-forget,
            // 失败 warn 不阻塞 startup。retention 给用户"误点 cleanup_all 后
            // 还有窗口期可在 trash/ 手动恢复"的安全网。
            //
            // always log:`removed=0/failed=0` = trash 空 / 无东西要清(健康),
            // `removed=0/failed=N` = GC 跑了但全失败(权限 / 锁 / 异常 FS),
            // 必须区分让运维诊断 trash 持续 grow 的根因。
            tauri::async_runtime::spawn(async {
                use codex_app_transfer_codex_integration::{
                    gc_trash_older_than, CodexPaths, TRASH_RETENTION_DAYS,
                };
                match CodexPaths::from_home_env() {
                    Ok(paths) => {
                        let (removed, failed) =
                            gc_trash_older_than(&paths, TRASH_RETENTION_DAYS);
                        if failed > 0 {
                            tracing::warn!(
                                removed,
                                failed,
                                retention_days = TRASH_RETENTION_DAYS,
                                "snapshot trash GC: some buckets failed to remove (检查 trash/ 目录权限 / 文件锁)"
                            );
                        } else {
                            tracing::info!(
                                removed,
                                retention_days = TRASH_RETENTION_DAYS,
                                "snapshot trash GC: removed expired buckets"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "snapshot trash GC skipped: CodexPaths::from_home_env() failed"
                        );
                    }
                }
            });

            // MOC-170:sessions.db 存量一次性迁移(旧 inline 大行 → 内容寻址引用,
            // 回收历史膨胀)。独立 std 线程后台静默跑,幂等(标志位),失败下次启动
            // 重试 —— fire-and-forget,不阻塞 startup,不在 tokio worker 上跑阻塞 IO。
            codex_app_transfer_adapters::responses::session::start_background_session_migration();

            // [MOC-257 三态] 在 auto_apply 之前按生效三态预置 proxy 伪造(synthetic→开),避免「relay
            // 已通但伪造未开」窄窗里旧 Codex 实例发 /backend-api 透传假 token 被 401。post-apply 的
            // apply_plugin_unlock_mode 会再校正一次。
            // [MOC-257 review] **先 migrate 再 resolve**:旧版只有 realAccountModeEnabled=false 等老布尔
            // 的 legacy config,若不先折叠成 pluginUnlockMode,这里早期 resolve 会按缺键默认从残留 tokens
            // 推成 real/synthetic、预置错的伪造态(post-apply 的 migrate 才纠正,中间窄窗已发错报文)。
            let _ = handlers::settings::migrate_plugin_unlock_mode_v1();
            // [MOC-257 review] 只在 synthetic **且 startup 真会 apply**(autoApplyOnStart + 有 active
            // provider)时预开伪造器:apply 被跳过(autoApplyOnStart=false / 无 provider)时没写 relay,若仍
            // 开伪造,旧 / 残留 Codex config 指向本 proxy 的 /backend-api 会被伪造(尽管解锁档没真应用)。
            let will_apply_synthetic = crate::codex_real_account::resolve_plugin_unlock_mode()
                == crate::codex_real_account::PluginUnlockMode::Synthetic
                && handlers::settings::load_registry_for_startup_language_sync()
                    .ok()
                    .and_then(|cfg| {
                        cfg.get("settings")
                            .and_then(|s| s.get("autoApplyOnStart"))
                            .and_then(|v| v.as_bool())
                    })
                    .unwrap_or(true)
                && admin::services::desktop::snapshot::active_provider_supports_relay();
            // [MOC-257 review] 伪造器开 = 本次将 apply synthetic **或** 盘上已是合成态(上次 apply 后
            // restoreCodexOnExit=false 没还原、auth.json 仍合成 + relay 仍指 proxy)。后者即使本次跳过 apply
            // 也要保持伪造开,否则 /backend-api 经现存 relay 透传假 token 撞 401。非合成态(active 不是合成
            // 且不 apply synthetic)则关,避免残留 config 误伪造。
            codex_app_transfer_proxy::set_fake_account_mode(
                will_apply_synthetic || crate::codex_real_account::active_is_synthetic(),
            );

            // #MOC-54:保留 JoinHandle,让下面的残留扫描能 await auto_apply
            // 真正跑完(确定性),而不是用固定 sleep 猜它有没有落盘。
            let auto_apply_handle = tauri::async_runtime::spawn(async move {
                handlers::desktop::auto_apply_on_startup_if_enabled(startup_proxy_manager).await
            });

            // #268 启动时自检 Codex 原配置完整性:`auto_apply_on_startup_if_enabled`
            // 与 `restore_codex_if_enabled("startup")` 跑完后,扫一次
            // `~/.codex/config.toml` + active/recovery snapshots,看是否含
            // transfer apply 残留字段(model_catalog_json 指向 app_home /
            // openai_base_url 指向 transfer proxy)。发现污染 → emit Tauri
            // event 让前端弹 banner 提示用户「针对性清除」;干净 → 静默 info!
            // 日志一条便于诊断。
            //
            // #MOC-54:scan 必须在 auto_apply 真正落盘之后才跑,否则会把"刚
            // apply 完、含 transfer 字段的 live config"误判成残留。apply 是异步,
            // 旧实现用固定 `sleep(3s)` 猜时机:赢了竞态就误报,用户「针对性清除」
            // 后重启又赢一次再误报 —— 正是"清掉→重启→又脏"的根因。改成直接
            // await auto_apply 的 JoinHandle:apply 在写 transfer 字段前就先建好
            // snapshot,await 到 task 结束即保证 `transfer_currently_applied`
            // 反映 apply 后的状态。30s 上限兜底,避免 apply 卡死时 scan 永不执行。
            let app_handle_for_residual_scan = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use codex_app_transfer_codex_integration::{
                    scan_residual_pollution, CodexPaths,
                };
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_secs(30),
                    auto_apply_handle,
                )
                .await;
                let paths = match CodexPaths::from_home_env() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "residual scan skipped: CodexPaths::from_home_env() failed"
                        );
                        return;
                    }
                };
                // 复用 handler 一致的 port 列表(当前 settings.proxyPort + 历史默认 18080)
                let ports = handlers::desktop::known_transfer_proxy_ports_for_startup();
                match scan_residual_pollution(&paths, &ports) {
                    Ok(report) => {
                        if report.is_clean() {
                            tracing::info!(
                                "residual config scan: clean (transfer_currently_applied={})",
                                report.transfer_currently_applied
                            );
                        } else {
                            tracing::warn!(
                                polluted_count = report.polluted.len(),
                                "residual config scan: pollution detected, emitting event to UI"
                            );
                            if let Some(window) =
                                app_handle_for_residual_scan.get_webview_window("main")
                            {
                                let _ = window.emit("residual-scan-report", &report);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "residual config scan failed");
                    }
                }
                // #MOC-62:复用同一 post-apply task —— 此刻已 await 完 auto_apply 且
                // residual scan 跑完,config.toml 的 apply 写已落定,这里再同步 MCP
                // 凭据"可移植保险箱"(file 模式 + 镜像)不会与 apply 抢写 config.toml。
                // 开关关闭时内部直接返回 0。startup_sync 负责 ensure file 模式 + 镜像跟随;
                // "live 整文件丢失需恢复"的提示**不**走一次性 event(会在前端 listener 注册
                // 前 emit 丢失,见 chatgpt-codex-connector P2),改由前端 load 时轮询
                // `GET /api/desktop/mcp-credentials/status` 决定是否弹确认。
                let _ = handlers::desktop::mcp_credentials_startup_sync("startup");

                // MOC-144:启动时把 web_fetch MCP server 注册态对齐当前 webFetchBackend
                // (config 已是某后端但还没注册过 → 补注册;off → 移除)。幂等:已一致则
                // 不写 config.toml。Codex 需重启才会加载/卸载该 server。
                {
                    // MOC-256:webFetchBackend 已在 main()(Builder.run() 之前)对无 Chrome 新装
                    // 落为 off,此处直接读当前值对齐 cat-webfetch MCP server 注册态(off → 不暴露
                    // 联网工具)。有 Chrome 但未设置 → key 仍 absent → 沿用 schema 默认 auto。
                    let backend = codex_app_transfer_registry::config_file()
                        .and_then(|p| codex_app_transfer_registry::load_raw_config(&p).ok())
                        .and_then(|c| {
                            c.get("settings")
                                .and_then(|s| s.get("webFetchBackend"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        // 有 Chrome 但未设置 → helper no-op、key 仍 absent → 沿用 schema 默认 auto
                        .unwrap_or_else(|| {
                            codex_app_transfer_registry::schema::DEFAULT_WEB_FETCH_BACKEND.to_string()
                        });
                    if let Err(e) =
                        crate::admin::services::mcp_servers::sync_web_fetch_server(&backend)
                    {
                        tracing::warn!("startup sync web_fetch mcp_server 失败: {e}");
                    }
                }

                // [MOC-104 req#2/#5 启动调谐] **必须在 auto_apply 落盘之后**才跑 —— 否则
                // 跟 auto_apply 抢写 `~/.codex/auth.json`:reconcile 先跑会看到上次退出
                // 恢复的旧 chatgpt 态而 no-op,随后 auto_apply 写 apikey 把导入镜像的恢复
                // 默默吞掉;反之又会撤销 auto_apply 的 apikey。复用本 post-apply task(已
                // await 完 auto_apply)确定性串在 apply 之后,不再用 sleep 猜时机(MOC-54)。
                // best-effort:无真实账号 / token 未过期则 no-op,失败只 log。
                // [MOC-104 分流] reconcile **不再刷新 token**(不构造 HTTP client)——刷新权
                // 归源头 Codex(Official 本机自刷 / Imported 源那边刷)与「登录」入口。启动
                // 只做「检测 + 必要时从导入镜像恢复」,杜绝跟外部 Codex 抢 single-use
                // refresh_token(实测撞刷会触发 refresh_token_reused 把账号烧死)。
                {
                    // [MOC-257 三态] 统一插件解锁:迁移旧三开关 → pluginUnlockMode;解析生效三态;apply。
                    // 本 task 已 await 完 auto_apply,确定性串在 apply 之后(不抢写 auth.json)。
                    let _ = handlers::settings::migrate_plugin_unlock_mode_v1();
                    let mode = crate::codex_real_account::resolve_plugin_unlock_mode();
                    let st = AdminState {
                        proxy_manager: app_handle_for_residual_scan
                            .state::<Arc<ProxyManager>>()
                            .inner()
                            .clone(),
                        trace_viewer_manager: Arc::new(trace_viewer::TraceViewerManager::new()),
                    };
                    // [MOC-257 review] autoApplyOnStart=false:用户不想启动就自动应用配置 → unlock apply
                    // 也跳过(否则仅启动就重写 ~/.codex,违背意图;provider auto-apply 已据此跳过,这里对齐)。
                    let auto_apply_on = handlers::settings::load_registry_for_startup_language_sync()
                        .ok()
                        .and_then(|cfg| {
                            cfg.get("settings")
                                .and_then(|s| s.get("autoApplyOnStart"))
                                .and_then(|v| v.as_bool())
                        })
                        .unwrap_or(true);
                    // synthetic/real 需 relay(chatgpt_base_url→proxy)才自洽;无 active provider 时 relay
                    // 起不来,若仍写合成/真 auth.json,Codex 会直连 chatgpt.com(合成 token 全 401、真账号也
                    // 绕过 proxy)。无 provider → 跳过 apply(对齐 set 端点 provider gate)。
                    let relay_ok =
                        admin::services::desktop::snapshot::active_provider_supports_relay();
                    if !auto_apply_on {
                        tracing::info!(
                            "[PluginUnlock] 启动跳过 unlock apply:autoApplyOnStart=false(尊重「启动不自动应用」)"
                        );
                    } else if matches!(
                        mode,
                        crate::codex_real_account::PluginUnlockMode::Synthetic
                            | crate::codex_real_account::PluginUnlockMode::Real
                    ) && !relay_ok
                    {
                        tracing::warn!(
                            "[PluginUnlock] 启动跳过 apply {mode:?}:无 active provider、relay 起不来,不写无 relay 的解锁态"
                        );
                    } else {
                        // [MOC-257 review] real 意图:先跑真账号 reconcile(镜像恢复 / relogin 检测,MOC-104
                        // 保留)。gate 在**持久 Real 意图**而非 resolved mode —— persisted=real 但账号过期/撤销时
                        // resolve 已把 mode 降级成 synthetic,按 mode 判会跳过 reconcile + relogin 提示,用户选了
                        // Real 却不知账号失效、不被提示重登(对齐手动 set 的降级消息)。降级后仍 apply synthetic。
                        if crate::admin::handlers::settings::read_plugin_unlock_mode().as_deref()
                            == Some("real")
                        {
                            if let Ok(
                                crate::codex_real_account::ReconcileOutcome::ReloginRequired { .. },
                            ) = crate::codex_real_account::reconcile_on_startup(Some(true)).await
                            {
                                if let Some(window) =
                                    app_handle_for_residual_scan.get_webview_window("main")
                                {
                                    let _ = window.emit("real-account-relogin-required", ());
                                }
                            }
                        }
                        match admin::services::desktop::snapshot::apply_plugin_unlock_mode(&st, mode)
                            .await
                        {
                            Ok(()) => tracing::info!("[PluginUnlock] 启动调谐:已应用三态 {mode:?}"),
                            Err(e) => {
                                // [MOC-257 review] auth/proxy 回滚**不在这里做**:apply_plugin_unlock_mode
                                // 已事务化自清理(失败还原 pre-apply 字节 + 视情 re-stash + 关伪造),活动态已
                                // 回 apply 前。再叠一层 un-stash(restore_stashed)会在自回滚之上重复动作 ——
                                // 如从「已是合成态 + 真账号在 stash」re-apply synthetic 失败时,把真账号挪回活动、
                                // 留 persisted synthetic + real-active 无 relay。只 log。
                                tracing::error!("[PluginUnlock] 启动调谐 apply {mode:?} 失败: {e}");
                            }
                        }
                    }
                }
                // [MOC-104] reconcile 已把活动账号 settle 完。relay 模式下真实 chatgpt
                // 活动 → Codex 据 `auth_mode==chatgpt` **原生**显示 Plugins 入口(实测:
                // bundle `pluginsDisabledTooltip` descriptor「API-key users → disabled
                // Plugins nav」),**不再需要 CDP daemon 注入**(消除 MOC-100 高延迟)。
                // 这里不启 daemon;daemon 只留给「无真实账号 + 显式强制开启」档(下方 task)。
            });

            // [MOC-257 三态] CDP 自动解锁 daemon 已废弃(synthetic 三态更可靠、自洽取代)。
            // 插件解锁三态由上面 post-apply task 的 apply_plugin_unlock_mode 统一应用,不再启 CDP daemon。

            let menu = build_tray_menu(app)?;
            // 菜单栏专属图标:白+透明猫剪影模板图(非全彩 app 图标缩小, 后者在 22px 菜单栏糊成一团)。
            // icon_as_template=true → macOS 按菜单栏明暗自动反色渲染(原生 menubar 风格)。
            let tray_rgba = image::load_from_memory(include_bytes!("../icons/tray-icon.png"))
                .expect("tray-icon.png 解码失败")
                .to_rgba8();
            let (tray_w, tray_h) = tray_rgba.dimensions();
            let tray_icon = tauri::image::Image::new_owned(tray_rgba.into_raw(), tray_w, tray_h);
            let _ = TrayIconBuilder::with_id("main")
                .tooltip("Codex App Transfer")
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&menu)
                // macOS 习惯:左键点图标 = 切窗口可见性,右键才弹菜单
                .show_menu_on_left_click(false)
                .on_menu_event(handle_tray_menu)
                .on_tray_icon_event(|tray, event| {
                    log_tray_event(&event);
                    let app = tray.app_handle();
                    // **不要**在每个事件(尤其右键 Click/Move/Enter)里
                    // 调用 `refresh_tray_menu`:Windows 平台正在呈现菜单时
                    // 把菜单引用替换会让选项点不动 / 不显示(2026-05-06
                    // 现场实测)。菜单在 handle_tray_menu 切 provider 之后
                    // 已经会刷新一次,那是真正会变内容的时机;其他事件
                    // (左键开窗 / 悬停 / 双击)菜单不变,没必要重建。
                    match event {
                        // 左键单击(Up)= 显示窗口并 focus
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } => show_main_window(app),
                        // 双击 = 同左键
                        TrayIconEvent::DoubleClick {
                            button: MouseButton::Left,
                            ..
                        } => show_main_window(app),
                        _ => {}
                    }
                })
                .build(app)?;

            // [MOC-271] Bug 2:tray 的 provider ✓ 之前只在「托盘内切 provider」后 refresh_tray_menu
            // (handle_tray_menu);主界面(REST set_default_provider/activate_provider)切 provider 不刷新
            // → ✓ 停在旧值、与主界面实际启用不符。这里起后台 watcher 轮询 `activeProvider`,变化即
            // refresh_tray_menu —— 覆盖**所有**切换源(托盘 / 主界面 / 外部直接改 config)。用轮询而非事件:
            // REST 层 AdminState 无 AppHandle、无法直接回调 tray;且 provider 变化仅随用户操作发生(非高频),
            // 1.5s 读一个小 JSON 开销可忽略。set_menu 发生在「菜单未呈现」的后台时机(变化只在用户切换时,
            // 不会与托盘菜单正在呈现重叠),避开 Windows 呈现中热替换雷区(见上 on_tray_icon_event 注释)。
            let watch_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut last = current_active_provider();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    let cur = current_active_provider();
                    if cur != last {
                        last = cur;
                        refresh_tray_menu(&watch_handle);
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // [MOC-211] 只有主窗口走「关闭=隐藏到托盘」;其它窗口(如 MiMo 小米账号登录
                // webview)应正常关闭销毁,否则红叉会连主 app 一起隐藏、窗口也不被销毁。
                if window.label() != "main" {
                    return;
                }
                api.prevent_close();
                // [MOC-271] close-to-tray 一律用**窗口级** `window.hide()`(NSWindow.orderOut:),
                // 不再用 app 级 `app.hide()`(NSApp.hide:)。原注释以为「官方 menubar-app 用 NSApp.hide:」
                // 实为反的:menubar app(尤其 hideDockIcon=Accessory 模式)应只隐藏**窗口**、保持 app +
                // 状态栏存活。app 级 NSApp.hide/unhide 跟 `.accessory` + NSStatusItem 组合在 macOS 上脆弱
                // ——点状态栏 item 使 app 失活、app 级隐藏状态机错乱 → 窗口被连带隐藏(纯显示 bug,后台正常)。
                // 改窗口级后关窗只影响该窗口,跨平台一致。
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Codex App Transfer");

    // [dev] vite dev server 在 http://localhost:1420 提供前端(HMR),其 /api 请求经
    // vite proxy(vite.config.ts server.proxy)转发到这里的 TCP 监听 —— 因为 dev 下
    // webview 在 devUrl 而非 cas://,相对路径 /api 打不到同进程 cas scheme 派发。
    // prod 走 cas:// 同进程 axum,不绑任何 TCP 端口;故此监听仅 debug 编译。
    #[cfg(debug_assertions)]
    {
        let dev_router = app_router.clone();
        tauri::async_runtime::spawn(async move {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 18900));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!(
                        "[dev] admin API listening on http://{addr} (vite proxy /api → here)"
                    );
                    if let Err(e) =
                        axum::serve(listener, (*dev_router).clone().into_make_service()).await
                    {
                        tracing::warn!("[dev] admin API listener exited: {e}");
                    }
                }
                Err(e) => tracing::warn!("[dev] failed to bind {addr} for admin API: {e}"),
            }
        });
    }

    app.run(|app_handle, event| {
        // [MOC-196] 窗口创建成功(Ready)→ 单实例握手开始回 OK(此前回 STARTING)。
        // 僵尸(setup hang)到不了这里,第二实例据此识别并接管。
        #[cfg(target_os = "macos")]
        if matches!(event, RunEvent::Ready) {
            single_instance::mark_ready(app_handle.clone());
        }
        if matches!(event, RunEvent::Exit) {
            let manager = app_handle.state::<Arc<ProxyManager>>();
            manager.stop_silent();
            // gate 状态要在 stop_silent 清除前读(用于决定是否需停 Codex 页内 recorder)。
            let diag_was_on = codex_app_transfer_proxy::diagnostics::forward_trace_enabled();
            app_handle
                .state::<Arc<trace_viewer::TraceViewerManager>>()
                .stop_silent();
            // [MOC-169] 诊断开着退出:优雅停 plugin-unlock daemon,让它退出前 best-effort 停掉
            // Codex 页内 MCP recorder(Codex 仍开时 recorder 否则留在渲染进程继续抓流量到下次
            // reload)。stop 发 Stop 命令后短暂等 daemon 处理(发停采 eval + 退出);bounded
            // timeout 防退出 hang;daemon 没在跑(relay/未启)时 stop 直接返回、无副作用。
            if diag_was_on {
                let _ = tauri::async_runtime::block_on(async {
                    tokio::time::timeout(std::time::Duration::from_millis(600), async {
                        handlers::plugin_unlock::get_service().await.stop().await;
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    })
                    .await
                });
            }
            // 取消任何 in-flight OAuth login —— 防 user 在 OAuth 5min 等待
            // 期间 Cmd+Q 退出 app,后台 task 残留 5min 后才超时(浪费资源,
            // 而且 callback 还可能触发 token persist 写入磁盘但 user 已经
            // 退出 app,产生 ghost 状态)
            let outcome = handlers::gemini_oauth::cancel_in_flight_login();
            if let (true, Some(target_epoch)) = (outcome.cancelled, outcome.cancelled_epoch) {
                tracing::info!(
                    target_epoch,
                    "app exit: cancelled in-flight OAuth login,等 epoch={target_epoch} task 真退出 (≤2s) 防 partial token persist"
                );
                // **C1 chatgpt-codex P1+P2 修**:wait_for_login_epoch_complete 用
                // watch::channel sticky 状态等 specific epoch 完成。比 notify
                // 强:① guard.drop 在 await 之前发生时仍能"读到" sticky 值
                //   立即返(P2 持久化完成信号);② preemption 场景下不被另一
                //   newer login 完成事件误唤醒(P1 specific epoch wait)。
                // timeout 2s 兜底防 task 异常 hang
                let _ = tauri::async_runtime::block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        handlers::gemini_oauth::wait_for_login_epoch_complete(target_epoch),
                    )
                    .await
                });
                tracing::info!(target_epoch, "app exit: epoch={target_epoch} 已退出或 timeout");
            }
            // 同理取消 in-flight zai/bigmodel OAuth login(MOC-252 Stage 3):防退出期间
            // 后台 OAuth task 残留 + 落盘 ghost 凭证
            let zai_outcome = handlers::zai_oauth::cancel_in_flight_login();
            if let (true, Some(target_epoch)) = (zai_outcome.cancelled, zai_outcome.cancelled_epoch)
            {
                tracing::info!(target_epoch, "app exit: cancelled in-flight zai OAuth login");
                let _ = tauri::async_runtime::block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        handlers::zai_oauth::wait_for_login_epoch_complete(target_epoch),
                    )
                    .await
                });
            }
            // [CAT-257] 同理取消 in-flight Trae OAuth login:防退出期间后台 OAuth task 残留 +
            // 落盘 ghost 凭证(与 zai/antigravity 同语义)
            let trae_outcome = handlers::trae_oauth::cancel_in_flight_login();
            if let (true, Some(target_epoch)) =
                (trae_outcome.cancelled, trae_outcome.cancelled_epoch)
            {
                tracing::info!(target_epoch, "app exit: cancelled in-flight trae OAuth login");
                let _ = tauri::async_runtime::block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        handlers::trae_oauth::wait_for_login_epoch_complete(target_epoch),
                    )
                    .await
                });
            }
            // [CAT-250] 同理取消 in-flight antigravity OAuth login:此前 antigravity 漏接这套
            // app-exit 取消(gemini/zai 都有),用户登 antigravity 期间退出 app 时后台 task 残留 +
            // ghost 凭证落盘。机器(cancel_in_flight_login + wait_for_login_epoch_complete)已在,补接线。
            let ag_outcome = handlers::antigravity_oauth::cancel_in_flight_login();
            if let (true, Some(target_epoch)) = (ag_outcome.cancelled, ag_outcome.cancelled_epoch) {
                tracing::info!(target_epoch, "app exit: cancelled in-flight antigravity OAuth login");
                let _ = tauri::async_runtime::block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        handlers::antigravity_oauth::wait_for_login_epoch_complete(target_epoch),
                    )
                    .await
                });
            }
            // [devin review] 同理取消 in-flight `codex login`(真实账号登录):否则 user 在
            // OAuth 等待期间退出 app,孤儿 codex login 进程可能在下面 restore_codex_if_enabled
            // 恢复原配置**之后**才写 ~/.codex/auth.json,把刚恢复的状态又改脏(数据完整性)。
            if crate::codex_real_account::cancel_login() {
                tracing::info!("app exit: 已取消 in-flight codex login,防孤儿进程退出后改写 auth.json");
            }
            // [MOC-257 三态] 退出前先把 OFF/synthetic 移走的真账号 stash 整文件还原回 ~/.codex/auth.json
            // 顺序:**先 restore_codex_if_enabled,再 un-stash 真账号**([MOC-257 review] 反转,真账号最终写)。
            // 原序(先 un-stash 再 restore_codex)让 restore_codex 在 stash 还原出的真账号上 merge 旧快照 managed
            // auth key:若快照拍于用户**登录 ChatGPT 之前**(无 `auth_mode`),merge 会把真账号 genuine 的
            // `auth_mode=chatgpt` 抹掉 → tokens 在但 standalone Codex 不再认作 ChatGPT。改为 restore_codex 先跑
            // (还原 config + strip transfer auth key),再 restore_stashed 把**完整真账号整文件覆盖**回活动 →
            // 真账号的 auth_mode/tokens 全胜过旧快照。原「补 managed key 防空壳」顾虑由「真账号在后覆盖」解决。
            // **同步版**:exit 期 async runtime 可能正在 shutdown,block_on 异步锁会 panic → 同步纯文件操作避开。
            // [review] un-stash gate 在 restoreCodexOnExit:=false 表示退出保留 transfer 状态,不该 un-stash。
            let exit_restore_ok = handlers::desktop::restore_codex_if_enabled("exit")
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // [MOC-257 review] un-stash 只在 restore **成功**后跑:restore_codex 失败(快照不可读 / ~/.codex 锁)
            // 时仍 un-stash 会留真账号 active + 没还原成功的 transfer relay/config 在盘上 → 下次启动 persisted
            // off/synthetic 却 real-active 不一致。失败则真账号留 stash,下次启动 self-heal 重试。
            if restore_codex_on_exit_enabled() && exit_restore_ok {
                if let Err(e) = crate::codex_real_account::restore_stashed_real_auth_blocking() {
                    tracing::error!(
                        "[PluginUnlock] 退出 stash 真账号还原失败(真账号仍在 stash 待下次启动重试): {e}"
                    );
                }
            }
            // MOC-144:transfer 注入的 web_fetch MCP server 在退出时从 Codex config.toml 移除
            // —— 它是 transfer 管理的工具, transfer 不在时不该残留 [mcp_servers.cat-webfetch]
            // (注入/移除对称;下次 transfer 启动 re-sync 会按 webFetchBackend 重新注册)。
            // 顺带清掉历史误用的 cas-webfetch 名(未发布)+ MOC-139 误改的大写 CAT-WEB-MCP。
            let _ = crate::admin::services::mcp_servers::delete_server(
                crate::admin::services::mcp_servers::WEB_FETCH_SERVER_NAME,
            );
            let _ = crate::admin::services::mcp_servers::delete_server("cas-webfetch");
            let _ = crate::admin::services::mcp_servers::delete_server("CAT-WEB-MCP");
        }
    });
}

/// 把 Tauri 协议层的 http::Request<Vec<u8>> 喂进 axum,response 转回 http 类型.
async fn handle_cas_request(
    router: Arc<axum::Router>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    // 1. http::Request<Vec<u8>> → axum::Request<Body>
    let (parts, body_bytes) = request.into_parts();
    let axum_req = axum::http::Request::from_parts(parts, Body::from(Bytes::from(body_bytes)));

    // 2. router.oneshot
    let response = match (*router).clone().oneshot(axum_req).await {
        Ok(r) => r,
        Err(e) => {
            return tauri::http::Response::builder()
                .status(500)
                .body(format!("router error: {e}").into_bytes())
                .unwrap();
        }
    };

    // 3. axum::Response<Body> → http::Response<Vec<u8>>
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b.to_vec(),
        Err(e) => {
            return tauri::http::Response::builder()
                .status(500)
                .body(format!("body read error: {e}").into_bytes())
                .unwrap();
        }
    };
    tauri::http::Response::from_parts(parts, bytes)
}

fn handle_tray_menu(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref();
    if let Some(provider_id) = id.strip_prefix("provider:") {
        if provider_id != "none" {
            let provider_id = provider_id.to_owned();
            let app_handle = app.clone();
            let proxy_manager = app.state::<Arc<ProxyManager>>().inner().clone();
            let trace_viewer_manager = app
                .state::<Arc<trace_viewer::TraceViewerManager>>()
                .inner()
                .clone();
            tauri::async_runtime::spawn(async move {
                let _ =
                    handlers::desktop::switch_provider_and_sync(proxy_manager.clone(), provider_id)
                        .await;
                // [MOC-257 review] 托盘切 provider 走 switch_provider_and_sync 直调(绕过 set_default/
                // add_provider handler 的 re-apply)→ 这里补上:切 provider 后 relay 可用,重 apply 当前
                // 生效三态(非 off),否则无 provider 时启动跳过的 synthetic/real unlock 仍不生效。
                let mode = crate::codex_real_account::resolve_plugin_unlock_mode();
                if !matches!(mode, crate::codex_real_account::PluginUnlockMode::Off) {
                    let st = AdminState {
                        proxy_manager,
                        trace_viewer_manager,
                    };
                    if let Err(e) =
                        admin::services::desktop::snapshot::apply_plugin_unlock_mode(&st, mode)
                            .await
                    {
                        tracing::warn!(
                            "[PluginUnlock] 托盘切 provider 后 apply {mode:?} 失败: {e}"
                        );
                    }
                }
                refresh_tray_menu(&app_handle);
            });
        }
        return;
    }

    match id {
        "show" => show_main_window(app),
        "hide" => hide_main_window(app),
        "restart_codex" => {
            // 同应用内「重启 Codex」按钮:先同步活动 provider 配置 → 重启 Codex → 通知 plugin daemon 重注入。
            let app_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let st = AdminState {
                    proxy_manager: app_handle.state::<Arc<ProxyManager>>().inner().clone(),
                    trace_viewer_manager: app_handle
                        .state::<Arc<trace_viewer::TraceViewerManager>>()
                        .inner()
                        .clone(),
                };
                // 与 HTTP restart_codex_app handler 一致:sync 尝试且失败时不重启,
                // 避免用 stale/错误的 provider 配置拉起 Codex。
                let sync =
                    admin::services::desktop::snapshot::sync_desktop_for_active_provider(&st).await;
                let sync_failed = sync.get("attempted").and_then(|v| v.as_bool()) == Some(true)
                    && sync.get("success").and_then(|v| v.as_bool()) != Some(true);
                if sync_failed {
                    tracing::warn!(
                        "[tray] restart-codex: desktop sync 失败, 跳过重启(避免 stale 配置)"
                    );
                    return;
                }
                if admin::services::desktop::process::launch_codex_app_restart(std::env::consts::OS)
                    .is_ok()
                {
                    handlers::plugin_unlock::get_service()
                        .await
                        .reinject()
                        .await;
                }
            });
        }
        "quit" => {
            let manager = app.state::<Arc<ProxyManager>>();
            manager.stop_silent();
            app.state::<Arc<trace_viewer::TraceViewerManager>>()
                .stop_silent();
            app.exit(0);
        }
        _ => {}
    }
}

/// tray 菜单语言:跟随 `settings.language`(显式 "en" → 英文,其余/未设 → 中文,
/// 与应用 Chinese-first 默认一致)。tray 是 Rust 原生菜单,不走前端 i18n 字典,
/// 故在此内联中英双串按语言选取。
fn tray_lang_is_zh() -> bool {
    admin::registry_io::load()
        .ok()
        .map(|cfg| {
            !matches!(
                cfg.get("settings")
                    .and_then(|s| s.get("language"))
                    .and_then(|v| v.as_str()),
                Some("en")
            )
        })
        .unwrap_or(true)
}

fn build_tray_menu<R: Runtime, M: Manager<R>>(manager: &M) -> tauri::Result<Menu<R>> {
    let zh = tray_lang_is_zh();
    let tr = |cn: &'static str, en: &'static str| if zh { cn } else { en };

    let mut providers = SubmenuBuilder::new(manager, tr("切换提供商", "Switch provider"));
    let entries = tray_provider_entries();
    if entries.is_empty() {
        providers = providers.text("provider:none", tr("无提供商", "No providers"));
    } else {
        for entry in entries {
            let label = if entry.active {
                format!("✓ {}", entry.name)
            } else {
                entry.name
            };
            providers = providers.text(format!("provider:{}", entry.id), label);
        }
    }
    let providers = providers.build()?;
    MenuBuilder::new(manager)
        .text("show", tr("显示窗口", "Show window"))
        .text("hide", tr("隐藏窗口", "Hide window"))
        .separator()
        .item(&providers)
        .text("restart_codex", tr("重启 Codex", "Restart Codex"))
        .separator()
        .text(
            "quit",
            tr("退出 Codex App Transfer", "Quit Codex App Transfer"),
        )
        .build()
}

fn refresh_tray_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    if let Ok(menu) = build_tray_menu(app) {
        let _ = tray.set_menu(Some(menu));
    }
}

/// [MOC-271] 读当前活动 provider id(`activeProvider`),供 tray watcher 检测变化用。
fn current_active_provider() -> Option<String> {
    admin::registry_io::load().ok().and_then(|cfg| {
        cfg.get("activeProvider")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    })
}

struct TrayProviderEntry {
    id: String,
    name: String,
    active: bool,
}

fn tray_provider_entries() -> Vec<TrayProviderEntry> {
    let Ok(cfg) = admin::registry_io::load() else {
        return Vec::new();
    };
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    cfg.get("providers")
        .and_then(|v| v.as_array())
        .map(|providers| {
            providers
                .iter()
                .filter_map(|provider| {
                    let id = provider.get("id").and_then(|v| v.as_str())?;
                    let name = provider
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unnamed Provider");
                    Some(TrayProviderEntry {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        active: Some(id) == active_id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn show_main_window(app: &AppHandle) {
    // [MOC-271] 只做**窗口级**显示(配对窗口级 `window.hide()`)。不再调 app 级 `app.show()`
    // (NSApp.unhide:)—— close-to-tray 已改窗口级,无需 app 级唤醒;且 app 级 unhide 跟 `.accessory`
    // 组合是窗口被连带隐藏的脆弱点。
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }

    // macOS 14+:`NSApplicationActivateIgnoringOtherApps` 已废弃,改用
    // `NSRunningApplication.activate(.activateAllWindows)` 强制带到前台(`.accessory` 下也有效)。
    #[cfg(target_os = "macos")]
    activate_macos_app();
}

#[cfg(target_os = "macos")]
pub(crate) fn activate_macos_app() {
    // macOS 14(Sonoma)起 `NSApplicationActivateIgnoringOtherApps` 已被
    // deprecated 且**实际无效** —— 这就是我们之前 Tauri set_focus()
    // 失效的根本原因(Tauri 内部就用这个 flag 走 NSApp.activate)。
    // 改走 NSRunningApplication.activate(.activateAllWindows),Apple 推荐
    // 替代品,在所有 macOS 14+ 版本下强制有效。
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    unsafe {
        let app = NSRunningApplication::currentApplication();
        app.activateWithOptions(NSApplicationActivationOptions::NSApplicationActivateAllWindows);
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

/// 写每次 tray 事件到 `~/.codex-app-transfer/tray.log`,便于诊断 click 是否
/// 真的触发 / 被什么字段过滤掉.手测完可以删此函数.
fn log_tray_event(event: &TrayIconEvent) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home).join(".codex-app-transfer");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tray.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {event:?}");
    }
}
