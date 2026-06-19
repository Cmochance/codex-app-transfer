//! 连接器市场(展示镜像)— 从私有 storage 仓库拉 registry.json + 图标代理(MOC-7 / phase2)。
//!
//! 源:`Cmochance/codex-app-transfer-storage`(**private**,镜像自 OpenAI Codex 插件目录的展示
//! 数据)。前端无法直连私有仓库,故后端持 token 代拉 + 缓存。token 解析顺序:
//! 1. build-baked `CODEX_APP_TRANSFER_STORAGE_TOKEN`(release 时 build.rs/CI 注入)
//! 2. 运行时 env `CODEX_APP_TRANSFER_STORAGE_TOKEN`
//! 3. dev 文件 `~/.codex-app-transfer/storage_token`(只读 PAT,本机开发用)
//!
//! - `GET /api/marketplace/connectors` → registry.json(内存缓存 30min)
//! - `GET /api/marketplace/icon?path=icons/<f>.png` → 图标原始字节(磁盘缓存,路径白名单 icons/)

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::Query,
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::common::err;

const STORAGE_REPO: &str = "Cmochance/codex-app-transfer-storage";
const REGISTRY_PATH: &str = "registry.json";
const CACHE_TTL: Duration = Duration::from_secs(60 * 30);

/// home 目录(对齐 src-tauri 其它处:`HOME` → `USERPROFILE`,不引 `dirs` crate)。
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// token:build-baked → 运行时 env → dev 文件。任一非空即用。
fn storage_token() -> Option<String> {
    if let Some(t) = option_env!("CODEX_APP_TRANSFER_STORAGE_TOKEN") {
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    if let Ok(t) = std::env::var("CODEX_APP_TRANSFER_STORAGE_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let p = home_dir()?
        .join(".codex-app-transfer")
        .join("storage_token");
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// GitHub contents API 取私有仓库 raw 文件字节(`Accept: application/vnd.github.raw`)。
/// 注:GitHub raw 响应 content-type 恒为 `application/vnd.github.raw`(非具体 mime),故调用方
/// 不依赖响应 content-type、按端点语义自定(registry=json / icon=png)。
async fn fetch_raw(token: &str, path: &str) -> Result<Vec<u8>, String> {
    let url = format!("https://api.github.com/repos/{STORAGE_REPO}/contents/{path}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("reqwest build: {e}"))?;
    let resp = client
        .get(&url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::ACCEPT, "application/vnd.github.raw")
        .header(header::USER_AGENT, "codex-app-transfer")
        .send()
        .await
        .map_err(|e| format!("fetch: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("github {} for {path}", status.as_u16()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("read body: {e}"))
}

fn registry_cache() -> &'static Mutex<Option<(Instant, String)>> {
    static C: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// `GET /api/marketplace/connectors` — 私有 storage 仓库的 registry.json(连接器展示目录),内存缓存 30min。
pub async fn connectors() -> impl IntoResponse {
    {
        let c = registry_cache().lock().unwrap();
        if let Some((at, body)) = c.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return ([(header::CONTENT_TYPE, "application/json")], body.clone())
                    .into_response();
            }
        }
    }
    let Some(token) = storage_token() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage token 未配置(env CODEX_APP_TRANSFER_STORAGE_TOKEN 或 ~/.codex-app-transfer/storage_token)",
        )
        .into_response();
    };
    match fetch_raw(&token, REGISTRY_PATH).await {
        Ok(bytes) => {
            let body = String::from_utf8_lossy(&bytes).to_string();
            *registry_cache().lock().unwrap() = Some((Instant::now(), body.clone()));
            ([(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct IconQuery {
    pub path: String,
}

/// `GET /api/marketplace/icon?path=icons/<f>.png` — 图标代理(路径白名单 `icons/` + 磁盘缓存)。
pub async fn icon(Query(q): Query<IconQuery>) -> impl IntoResponse {
    let path = q.path;
    // 路径白名单:仅 `icons/` 下的**单层 .png**(registry 里图标都是 `icons/<id>.png` 扁平布局)。
    // 收紧到 .png + 单 `/` 段 —— 防目录穿越、也防经此端点读 storage 仓库的非图标文件。
    // axum Query 已 percent-decode,故编码后的 `..` 也会被这里的检查命中。
    let valid = path.starts_with("icons/")
        && path.ends_with(".png")
        && path.matches('/').count() == 1
        && !path.contains("..");
    if !valid {
        return err(StatusCode::BAD_REQUEST, "invalid icon path").into_response();
    }
    let cache_file = home_dir().map(|h| {
        h.join(".codex-app-transfer")
            .join("marketplace-cache")
            .join(path.replace('/', "_"))
    });
    if let Some(cf) = &cache_file {
        if let Ok(bytes) = std::fs::read(cf) {
            return ([(header::CONTENT_TYPE, "image/png")], bytes).into_response();
        }
    }
    let Some(token) = storage_token() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "storage token 未配置").into_response();
    };
    match fetch_raw(&token, &path).await {
        // 路径已限定 .png,200 即图标字节;统一回 image/png(GitHub raw content-type 不具体)。
        Ok(bytes) => {
            if let Some(cf) = &cache_file {
                if let Some(parent) = cf.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                // 缓存是优化非正确性,写失败软降级(回退到每次 fetch),但不静默 —— 留痕便于排查。
                if let Err(e) = std::fs::write(cf, &bytes) {
                    tracing::debug!("marketplace icon cache write failed for {path}: {e}");
                }
            }
            ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e).into_response(),
    }
}
