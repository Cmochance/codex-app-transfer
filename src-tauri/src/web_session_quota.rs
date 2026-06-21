//! web-session 额度 provider 的共用框架(MOC-211 + CAT-256 整合)。
//!
//! 某些 coding-plan 的套餐用量只在各自**网页控制台**后面(走账号登录、非 inference API key),
//! 必须用 app 内嵌 Tauri `WebviewWindow` 登一次抓 session cookie 落库,daemon 再带它查用量。
//! MiMo(MOC-211)和 OpenCode Go(CAT-256)是同一套路,本模块把**重复的部分**收成共用:
//!
//! - **登录抓取**([`login_and_capture`]):开内嵌窗 → 轮询 URL/cookies → 命中捕获信号 → 拼
//!   `Cookie:` 头(+ 可选从 URL 抓 workspace id)→ 关窗返回。各 provider 的差异(登录 URL /
//!   捕获信号 / cookie 域与白名单 / 是否抓 workspace)全收进 [`SessionLoginSpec`]。
//! - **injector 接线**([`active_session`] / [`clear_cookie`]):活动 provider 命中 host gate +
//!   有存储 cookie → 返回会话;session 失效时清 cookie 让前端转「未登录」。差异收进 [`QuotaSourceSpec`]。
//!
//! 新增同类 provider = 填一个 [`SessionLoginSpec`] + [`QuotaSourceSpec`] + 写一个 fetcher,
//! 不再 copy 整个模块。体积零增量:复用主窗口已在用的系统 WebView + 已链接的 tauri/wry。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// setup 阶段注入(AdminState 建 router 时尚无 AppHandle,走全局),供各 provider 开登录窗用。
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

pub fn init(handle: AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

const POLL_TIMEOUT: Duration = Duration::from_secs(180);

/// 登录窗「抓到 session」的判定信号。
pub enum CaptureSignal {
    /// 等某 cookie 名出现且非空(MiMo:httpOnly `api-platform_serviceToken`)。
    CookiePresent(&'static str),
    /// 等 webview URL 含某片段(OpenCode:登录后跳 `/workspace`)。
    UrlContains(&'static str),
}

/// 一个 web-session provider 的登录抓取规格(声明式,差异都在这)。
pub struct SessionLoginSpec {
    pub login_url: &'static str,
    pub win_label: &'static str,
    pub win_title: &'static str,
    pub inner_size: (f64, f64),
    /// 抓哪个域的 cookie(日志 + 全量拼头时按它过滤)。
    pub cookie_domain: &'static str,
    pub signal: CaptureSignal,
    /// 限定 cookie 名(MiMo:按名匹配、按此顺序拼头);**空 = 该域全部 cookie**(OpenCode)。
    pub want_cookies: &'static [&'static str],
    /// 是否从 authed URL 抓 `/workspace/<id>`(OpenCode 查用量端点需要)。
    pub extract_workspace_from_url: bool,
}

/// 登录抓取结果。
pub struct CapturedSession {
    /// 拼好的 `Cookie:` 头。
    pub cookie: String,
    /// 从 URL 抓的 workspace id(`spec.extract_workspace_from_url` 时;否则 None)。
    pub workspace_id: Option<String>,
}

/// 从控制台 authed URL(`.../workspace/<wrk_id>[/...]`)抽 workspace id(到下一个 `/ ? #`)。
fn extract_workspace_id(url: &str) -> Option<String> {
    let marker = "/workspace/";
    let start = url.find(marker)? + marker.len();
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let id = &rest[..end];
    (!id.is_empty()).then(|| id.to_string())
}

/// 从 tauri 的 cookie 提取需要的字段(避开命名 tauri 的 cookie 类型 + 解耦其 API)。
struct CookieInfo {
    name: String,
    value: String,
    domain: String,
}

/// 按 spec 拼 `Cookie:` 头:`want_cookies` 非空 → 按名匹配(不卡 domain,与 MiMo 原实现一致)、
/// 按 want 顺序;空 → 取 `cookie_domain` 域的全部非空 cookie。
fn build_cookie_header(cookies: &[CookieInfo], spec: &SessionLoginSpec) -> String {
    if spec.want_cookies.is_empty() {
        cookies
            .iter()
            .filter(|c| c.domain.contains(spec.cookie_domain) && !c.value.is_empty())
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    } else {
        spec.want_cookies
            .iter()
            .filter_map(|want| {
                cookies
                    .iter()
                    .find(|c| c.name == *want && !c.value.is_empty())
                    .map(|c| format!("{want}={}", c.value))
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// 捕获信号是否命中。
fn signal_triggered(spec: &SessionLoginSpec, cur_url: &str, cookies: &[CookieInfo]) -> bool {
    match spec.signal {
        CaptureSignal::CookiePresent(name) => cookies
            .iter()
            .any(|c| c.name == name && !c.value.is_empty()),
        CaptureSignal::UrlContains(frag) => cur_url.contains(frag),
    }
}

/// 打开内嵌登录窗,轮询抓 session(参数全在 `spec`)。
/// - `Ok(Some(captured))`:命中信号、拿到非空 cookie。
/// - `Ok(None)`:用户关窗 / 超时未完成(非错误,前端显「未登录」不弹错)。
/// - `Err(_)`:真错误(开窗失败 / AppHandle 未初始化)。
pub async fn login_and_capture(spec: &SessionLoginSpec) -> Result<Option<CapturedSession>, String> {
    let app = APP_HANDLE.get().ok_or("AppHandle 未初始化")?.clone();

    // 防连点:已有同名登录窗先关掉重开。
    if app.get_webview_window(spec.win_label).is_some() {
        close_win(&app, spec.win_label);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // 窗口创建放主线程(macOS 在非主线程建 webview 会 panic);结果经 oneshot 回传。
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let app_build = app.clone();
    let login_url = spec.login_url;
    let win_label = spec.win_label;
    let win_title = spec.win_title;
    let inner_size = spec.inner_size;
    app.run_on_main_thread(move || {
        let res = (|| -> Result<(), String> {
            let url: tauri::Url = login_url
                .parse()
                .map_err(|e| format!("URL 解析失败: {e}"))?;
            WebviewWindowBuilder::new(&app_build, win_label, WebviewUrl::External(url))
                .title(win_title)
                .inner_size(inner_size.0, inner_size.1)
                .build()
                .map_err(|e| format!("创建登录窗口失败: {e}"))?;
            Ok(())
        })();
        let _ = tx.send(res);
    })
    .map_err(|e| format!("主线程派发失败: {e}"))?;
    rx.await.map_err(|e| format!("窗口创建回传失败: {e}"))??;

    // 轮询 URL + cookies(每秒一次,≤3min)。命中 spec.signal → 拼 Cookie 头返回。
    let started = Instant::now();
    let mut last_url = String::new();
    loop {
        if started.elapsed() > POLL_TIMEOUT {
            tracing::info!(
                win = spec.win_label,
                "[WebSession] 登录超时(未捕获 session),关闭登录窗"
            );
            close_win(&app, spec.win_label);
            return Ok(None);
        }
        if app.get_webview_window(spec.win_label).is_none() {
            tracing::info!(win = spec.win_label, "[WebSession] 登录窗口被关闭,放弃捕获");
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(win) = app.get_webview_window(spec.win_label) else {
            tracing::info!(win = spec.win_label, "[WebSession] 登录窗口被关闭,放弃捕获");
            return Ok(None);
        };

        let cur_url = win.url().map(|u| u.to_string()).unwrap_or_default();
        let cookies: Vec<CookieInfo> = match win.cookies() {
            Ok(cs) => cs
                .iter()
                .map(|c| CookieInfo {
                    name: c.name().to_string(),
                    value: c.value().to_string(),
                    domain: c.domain().unwrap_or("").to_string(),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, win = spec.win_label, "[WebSession] 读取 cookies 失败,下轮重试");
                continue;
            }
        };

        // 诊断:URL 变化时记一次当前 URL + 此刻该域 cookie 名(**只记名/域,不记值**),
        // 追踪跳转流 + session cookie 何时出现,给排障/后续抓包定位。
        if cur_url != last_url {
            last_url = cur_url.clone();
            let seen: Vec<String> = cookies
                .iter()
                .filter(|c| c.domain.contains(spec.cookie_domain))
                .map(|c| format!("{}@{}", c.name, c.domain))
                .collect();
            tracing::info!(win = spec.win_label, cookies = seen.len(), names = ?seen, url = %cur_url, "[WebSession] 登录窗 URL/cookie 快照");
        }

        if !signal_triggered(spec, &cur_url, &cookies) {
            continue;
        }
        let header = build_cookie_header(&cookies, spec);
        if header.is_empty() {
            // 命中信号但还没拿到 cookie(跳转中),下轮再试。
            continue;
        }
        let workspace_id = spec
            .extract_workspace_from_url
            .then(|| extract_workspace_id(&cur_url))
            .flatten();
        tracing::info!(win = spec.win_label, workspace_id = ?workspace_id, url = %cur_url, "[WebSession] 已捕获 session cookie,关闭登录窗");
        close_win(&app, spec.win_label);
        return Ok(Some(CapturedSession {
            cookie: header,
            workspace_id,
        }));
    }
}

/// 关登录窗 —— `destroy()`(强制销毁,绕过 CloseRequested 拦截)且在**主线程**执行
/// (macOS 窗口操作须主线程),确保登录窗一定被销毁不残留。
fn close_win(app: &AppHandle, win_label: &'static str) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(win_label) {
            let _ = w.destroy();
        }
    });
}

// ───────────────────────── injector 侧共用接线 ─────────────────────────

/// 一个 web-session 额度源的 injector 接线规格(host gate + config 存储字段)。
pub struct QuotaSourceSpec {
    /// 活动 provider 的 baseUrl host 后缀(`xiaomimimo.com` / `opencode.ai`)。
    pub host_suffix: &'static str,
    /// 额外要求 baseUrl 含此片段(MiMo:`token-plan`,区分按量 MiMo);None=只看 host。
    pub path_contains: Option<&'static str>,
    /// config 里存 session cookie 的字段名(`mimoCookie` / `opencodeCookie`)。
    pub cookie_field: &'static str,
    /// config 里存 workspace id 的字段名(OpenCode:`opencodeWorkspaceId`);None=该 provider 不需。
    pub workspace_field: Option<&'static str>,
}

/// 命中的活动会话:provider id + cookie + 可选 workspace id。
pub struct ActiveSession {
    pub provider_id: String,
    pub cookie: String,
    pub workspace_id: Option<String>,
}

/// 活动 provider 命中 `spec` 的 host gate + 有存储 cookie(+ 若需 workspace id)→ 返回会话;否则 None。
pub fn active_session(spec: &QuotaSourceSpec) -> Option<ActiveSession> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))?,
        None => providers.first()?,
    };
    let base_url = p.get("baseUrl").and_then(|v| v.as_str())?;
    let host = crate::codex_quota_injector::host_of(base_url)?;
    if !host.ends_with(spec.host_suffix) {
        return None;
    }
    if let Some(frag) = spec.path_contains {
        if !base_url.contains(frag) {
            return None;
        }
    }
    let provider_id = p.get("id").and_then(|v| v.as_str())?.to_string();
    let cookie = field_nonempty(p, spec.cookie_field)?;
    let workspace_id = match spec.workspace_field {
        Some(field) => Some(field_nonempty(p, field)?),
        None => None,
    };
    Some(ActiveSession {
        provider_id,
        cookie,
        workspace_id,
    })
}

fn field_nonempty(p: &Value, field: &str) -> Option<String> {
    p.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// session 失效(Auth)时清掉该 provider 存的 cookie 字段,让前端 `has*Cookie` 转 false 显「未登录」、
/// 提示重新登录(这类网页 session 无 refresh,只能重登)。仅当当前存的 cookie 仍是本次失败用的那把
/// 才清(避免请求在途时用户已重登存了新 session、被这条迟到的 Auth 误删)。
pub fn clear_cookie(provider_id: &str, cookie_field: &str, used_cookie: &str) {
    let _ = crate::admin::registry_io::with_config_write(|cfg| {
        let Some(providers) = cfg
            .as_object_mut()
            .and_then(|o| o.get_mut("providers"))
            .and_then(|v| v.as_array_mut())
        else {
            return Ok(crate::admin::registry_io::ConfigMutation::Unchanged(()));
        };
        let mut changed = false;
        for p in providers.iter_mut() {
            if p.get("id").and_then(|v| v.as_str()) == Some(provider_id) {
                if let Some(obj) = p.as_object_mut() {
                    let still_same =
                        obj.get(cookie_field).and_then(|v| v.as_str()) == Some(used_cookie);
                    if still_same && obj.remove(cookie_field).is_some() {
                        changed = true;
                    }
                }
                break;
            }
        }
        if changed {
            Ok(crate::admin::registry_io::ConfigMutation::Modified(()))
        } else {
            Ok(crate::admin::registry_io::ConfigMutation::Unchanged(()))
        }
    });
}
