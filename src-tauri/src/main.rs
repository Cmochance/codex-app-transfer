// Stage 6:Tauri 自定义 URI scheme `cas://localhost/` → in-process axum,
// frontend/ 整目录 include_dir 进二进制。frontend 零改动(v1.4 Bootstrap 视觉)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod admin;
mod codex_plugin_unlocker;
mod codex_real_account;
mod codex_theme_injector;
mod proxy_runner;
mod telemetry_bridge;
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

fn main() {
    // 必须在所有可能 emit tracing event 的代码之前 init,否则 startup 阶段
    // (registry healing / desktop apply / proxy 拉起)的 tracing event 会被 drop。
    telemetry_bridge::init_global_subscriber();

    let proxy_manager = Arc::new(ProxyManager::new());
    let admin_state = AdminState {
        proxy_manager: proxy_manager.clone(),
    };
    let app_router = Arc::new(build_app_router(admin_state));
    let app_router_for_protocol = app_router.clone();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
            // single-instance 启动时如果带 deeplink URL,argv 里会有,转发给前端
            for arg in _argv.iter().skip(1) {
                if arg.starts_with("codex-app-transfer:") {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("codex-deeplink", arg.clone());
                    }
                }
            }
        }))
        .manage(proxy_manager)
        .register_asynchronous_uri_scheme_protocol("cas", move |_app, request, responder| {
            let router = app_router_for_protocol.clone();
            tauri::async_runtime::spawn(async move {
                let response = handle_cas_request(router, request).await;
                responder.respond(response);
            });
        })
        .setup(|app| {
            let startup_proxy_manager = app.state::<Arc<ProxyManager>>().inner().clone();
            let _ = handlers::desktop::restore_codex_if_enabled("startup");

            // #262:加载 `settings.language` 一次,同步到 adapters 全局,确保
            // startup 后第一个 user 请求的 prompt 注入就是正确语言。后续 user
            // 切语言由 `save_settings` 内的 hot reload(同模块 fn)处理。
            if let Ok(cfg) = handlers::settings::load_registry_for_startup_language_sync() {
                let settings = cfg.get("settings").cloned().unwrap_or_else(|| serde_json::json!({}));
                handlers::settings::sync_user_language_from_settings(&settings);
            }

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
                    use crate::codex_real_account::RefreshOutcome;
                    match crate::codex_real_account::reconcile_on_startup().await {
                        // [MOC-104] 真实账号失效(镜像 token 本地 JWT 已过期、无法恢复)→ 自动
                        // 关「自动解锁」开关 + emit 事件让前端提示重新登录。
                        Ok(RefreshOutcome::ReloginRequired { .. }) => {
                            tracing::warn!(
                                "[RealAccount] 真实账号已失效(需重新登录),自动关闭自动解锁开关"
                            );
                            // 关开关只在原本是 on 时有动作;但事件**无论开关原态都 emit**
                            // (review #6)—— 前端靠这个事件标记「账号已失效」,开关已是
                            // off 的新装用户也得知道账号失效,否则 detect 见 token 在就误报。
                            let _ = handlers::settings::disable_auto_unlock_codex_plugins().await;
                            if let Some(window) =
                                app_handle_for_residual_scan.get_webview_window("main")
                            {
                                let _ = window.emit("real-account-relogin-required", ());
                            }
                        }
                        Ok(outcome) => tracing::info!(
                            "[RealAccount] 启动调谐(检测 + 必要时从导入镜像恢复,不刷新): {outcome:?}"
                        ),
                        Err(e) => {
                            tracing::warn!("[RealAccount] 启动调谐失败(忽略): {e}")
                        }
                    }
                }
                // [MOC-104] reconcile 已把活动账号 settle 完。relay 模式下真实 chatgpt
                // 活动 → Codex 据 `auth_mode==chatgpt` **原生**显示 Plugins 入口(实测:
                // bundle `pluginsDisabledTooltip` descriptor「API-key users → disabled
                // Plugins nav」),**不再需要 CDP daemon 注入**(消除 MOC-100 高延迟)。
                // 这里不启 daemon;daemon 只留给「无真实账号 + 显式强制开启」档(下方 task)。
            });

            // ── [MOC-104] 真实账号解锁一次性迁移 ──
            // 老版本 autoUnlockCodexPlugins 默认 true 会直接拉起 CDP 伪造注入 daemon;真实
            // 账号模式上线后,**无真实账号时**的高延迟 CDP 路径改为「显式强制开启」才走。
            // 同步执行(在 daemon 决策 + 任何 Codex 启动前),硬重置升级用户残留的旧 true。幂等。
            if handlers::settings::migrate_real_account_unlock_v1() {
                tracing::info!(
                    "[RealAccount] 一次性迁移:硬重置 autoUnlockCodexPlugins=false(无真实账号时的高延迟 CDP 改为显式强制开启)"
                );
            }

            // ── Plugin Unlock 守护进程自动启动 ──
            // [MOC-104] daemon = CDP 注入 `setAuthMethod('chatgpt')`,把 React authMethod
            // 伪造成 chatgpt 来解锁 Plugins 入口。它**只**对「活动 auth.json 是 apikey」的
            // 用户有意义,且伪造态与磁盘 apikey 不匹配 → Codex 重新初始化登录态(MOC-100
            // ~5.8s 高延迟)。relay 模式上线后:
            // ① 活动是真实 chatgpt → Codex 据 `auth_mode==chatgpt` **原生**显示 Plugins,
            //    **不启 daemon**(无注入、无不匹配、无高延迟);apply.rs relay 分支保证切
            //    provider 时也保留 chatgpt 态,入口持续可见。
            // ② 活动是 apikey + 用户显式强制开启 → 跑 daemon(伪造,用户已接受高延迟);
            // ③ 活动是 apikey + 未强制开启 → 不跑(消除「默默高延迟」,用户原始诉求)。
            // 复用 handlers::plugin_unlock 的 OnceCell 单例(否则跟前端手动 start 各跑一份)。
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                // [MOC-104] 真实 chatgpt 活动 → Codex 原生显示 plugins,绝不启 daemon
                // (relay 模式核心:无注入、无不匹配、无 MOC-100 高延迟)。
                if crate::codex_real_account::active_is_real_chatgpt_now() {
                    tracing::info!(
                        "[PluginUnlock] 真实 chatgpt 账号活动,Codex 原生显示 plugins,不启 daemon(relay 模式)"
                    );
                    return;
                }

                // 无真实账号:仅用户显式强制开启(高延迟 CDP 伪造注入)才跑 daemon。
                // 迁移后默认 false;只有强制开启 / 用户手动开开关才 true。
                let force_cdp = match crate::admin::registry_io::load() {
                    Ok(cfg) => cfg
                        .get("settings")
                        .and_then(|s| s.get("autoUnlockCodexPlugins"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    Err(_) => false,
                };
                if force_cdp {
                    tracing::info!("[PluginUnlock] 无真实账号 + 用户强制开启(高延迟 CDP),启动 daemon");
                    handlers::plugin_unlock::get_service().await.start();
                } else {
                    tracing::info!(
                        "[PluginUnlock] 无真实账号且未强制开启,不启 daemon(避免默默高延迟)"
                    );
                }
            });

            let menu = build_tray_menu(app)?;
            let _ = TrayIconBuilder::with_id("main")
                .tooltip("Codex App Transfer")
                .icon(app.default_window_icon().unwrap().clone())
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
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                // macOS:用 NSApp.hide:(`app.hide()`)而不是 NSWindow.orderOut:
                // (`window.hide()`)。NSApp.hide/unhide 是 Apple 提供的 app 级
                // 隐藏 API,状态切换比 NSWindow.orderOut 干净;且与 NSStatusItem
                // 组合的官方 menubar-app 模式就是这样写的。
                // 非 macOS 仍用 window.hide()。
                #[cfg(target_os = "macos")]
                {
                    let app_handle = window.app_handle().clone();
                    let _ = window.run_on_main_thread(move || {
                        let _ = app_handle.hide();
                    });
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Codex App Transfer");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit) {
            let manager = app_handle.state::<Arc<ProxyManager>>();
            manager.stop_silent();
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
            // [devin review] 同理取消 in-flight `codex login`(真实账号登录):否则 user 在
            // OAuth 等待期间退出 app,孤儿 codex login 进程可能在下面 restore_codex_if_enabled
            // 恢复原配置**之后**才写 ~/.codex/auth.json,把刚恢复的状态又改脏(数据完整性)。
            if crate::codex_real_account::cancel_login() {
                tracing::info!("app exit: 已取消 in-flight codex login,防孤儿进程退出后改写 auth.json");
            }
            let _ = handlers::desktop::restore_codex_if_enabled("exit");
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
            tauri::async_runtime::spawn(async move {
                let _ =
                    handlers::desktop::switch_provider_and_sync(proxy_manager, provider_id).await;
                refresh_tray_menu(&app_handle);
            });
        }
        return;
    }

    match id {
        "show" => show_main_window(app),
        "hide" => hide_main_window(app),
        "quit" => {
            let manager = app.state::<Arc<ProxyManager>>();
            manager.stop_silent();
            app.exit(0);
        }
        _ => {}
    }
}

fn build_tray_menu<R: Runtime, M: Manager<R>>(manager: &M) -> tauri::Result<Menu<R>> {
    let mut providers = SubmenuBuilder::new(manager, "Switch provider");
    let entries = tray_provider_entries();
    if entries.is_empty() {
        providers = providers.text("provider:none", "No providers");
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
        .text("show", "Show window")
        .text("hide", "Hide window")
        .separator()
        .item(&providers)
        .separator()
        .text("quit", "Quit Codex App Transfer")
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
    // macOS:NSApp.unhide:(`app.show()`)反向唤醒 app + 全部窗口,
    // 与关窗时的 NSApp.hide: 配对。
    #[cfg(target_os = "macos")]
    let _ = app.show();

    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }

    // macOS 14+:`NSApplicationActivateIgnoringOtherApps` 已废弃,改用
    // `NSRunningApplication.activate(.activateAllWindows)` 强制带到前台。
    #[cfg(target_os = "macos")]
    activate_macos_app();
}

#[cfg(target_os = "macos")]
fn activate_macos_app() {
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
