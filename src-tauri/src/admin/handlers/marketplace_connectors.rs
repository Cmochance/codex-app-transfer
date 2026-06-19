//! 连接器市场(多源)— 官方源(私有 storage 仓库镜像)+ 用户自加源,聚合展示(MOC-7 phase2)。
//!
//! - **官方源**:`Cmochance/codex-app-transfer-storage`(private),走 GitHub contents API + token。
//! - **自加源**:用户加的公开 `registry.json` URL(同 `{connectors,categories}` schema),直取。
//! token 解析:build-baked `CODEX_APP_TRANSFER_STORAGE_TOKEN` → 运行时 env → `~/.codex-app-transfer/storage_token`。
//!
//! - `GET  /api/marketplace/connectors`       → 聚合所有启用源(返 sources[含 count] + connectors + categories + errors)
//! - `GET  /api/marketplace/sources`          → 源列表(管理用)
//! - `POST /api/marketplace/sources/add`      → 加自加源 {name,url}
//! - `POST /api/marketplace/sources/remove`   → 删自加源 {id}(官方源不可删)
//! - `POST /api/marketplace/sources/toggle`   → 启用/停用 {id,enabled}
//! - `GET  /api/marketplace/icon?source=&path=` → 图标代理(官方走 token+icons/;自加源走其 base)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::Query,
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::common::err;

const STORAGE_REPO: &str = "Cmochance/codex-app-transfer-storage";
const REGISTRY_PATH: &str = "registry.json";
const SOURCES_FILE: &str = "marketplace-connector-sources.json";
const OFFICIAL_ID: &str = "official";
const CACHE_TTL: Duration = Duration::from_secs(60 * 30);
/// fetch 响应体上限(registry / 图标都远小于此)—— 封顶防恶意/异常自加源返超大体 OOM / 撑爆缓存。
const MAX_FETCH_BYTES: u64 = 16 * 1024 * 1024;

/// 流式读响应体并封顶(先看 Content-Length,再边读边累计防谎报)。
async fn read_body_capped(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    use futures::StreamExt;
    if let Some(len) = resp.content_length() {
        if len > MAX_FETCH_BYTES {
            return Err(format!("response too large: {len} bytes"));
        }
    }
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read body: {e}"))?;
        if buf.len() as u64 + chunk.len() as u64 > MAX_FETCH_BYTES {
            return Err("response exceeds size cap".to_owned());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// home 目录(对齐 src-tauri 其它处:`HOME` → `USERPROFILE`,不引 `dirs` crate)。
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// 官方源 token:build-baked → 运行时 env → dev 文件。任一非空即用。
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

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        // 只跟 https 重定向 —— 否则 https 源 / 图标可 30x 降级到 http,绕过 add_source 的 https 强制。
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.error("refusing redirect to non-https (downgrade)")
            }
        }))
        .build()
        .map_err(|e| format!("reqwest build: {e}"))
}

// ── Sources store ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct ConnectorSource {
    id: String,
    /// 自加源:用户填的名字;官方源留空(前端按 `official` 用 i18n「官方源」渲染)。
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    official: bool,
}
fn yes() -> bool {
    true
}

fn official_source() -> ConnectorSource {
    ConnectorSource {
        id: OFFICIAL_ID.to_string(),
        name: String::new(),
        url: None,
        enabled: true,
        official: true,
    }
}

fn sources_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".codex-app-transfer").join(SOURCES_FILE))
}

/// 读源列表(官方源恒在,首次/缺失自动置顶补回)。
fn read_sources() -> Vec<ConnectorSource> {
    let mut list: Vec<ConnectorSource> = sources_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !list.iter().any(|s| s.id == OFFICIAL_ID) {
        list.insert(0, official_source());
    }
    list
}

fn write_sources(list: &[ConnectorSource]) -> Result<(), String> {
    let p = sources_path().ok_or("HOME 未设置")?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let body = serde_json::to_string_pretty(list).map_err(|e| format!("serialize: {e}"))?;
    // atomic:写 tmp 再 rename,崩溃也不会留半截 JSON(否则下次 read 静默 unwrap_or_default 丢光自加源)。
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename: {e}"))
}

/// FNV-1a 稳定哈希(跨版本确定),用于源 id / 缓存文件名 —— 避免有损字符净化导致不同输入碰撞。
fn stable_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 自加源 id:对 url 稳定哈希(同 url 唯一、不碰撞)。
fn source_id_from_url(url: &str) -> String {
    format!("src_{}", stable_hash(url))
}

/// 源文件 read-modify-write 串行化(本地单进程 app,进程内 Mutex 足够防丢更新)。
fn sources_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

// ── fetch + 每源 body 缓存 ───────────────────────────────────────────────────

fn body_cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// GitHub contents API 取私有仓库 raw 文件(`Accept: application/vnd.github.raw`)。
async fn fetch_official(token: &str, path: &str) -> Result<Vec<u8>, String> {
    let url = format!("https://api.github.com/repos/{STORAGE_REPO}/contents/{path}");
    let resp = client()?
        .get(&url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::ACCEPT, "application/vnd.github.raw+json")
        .header(header::USER_AGENT, "codex-app-transfer")
        .send()
        .await
        .map_err(|e| format!("fetch: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("github {} for {path}", resp.status().as_u16()));
    }
    read_body_capped(resp).await
}

/// 公开 URL 直取(自加源 registry / 图标)。
async fn fetch_public(url: &str) -> Result<Vec<u8>, String> {
    let resp = client()?
        .get(url)
        .header(header::USER_AGENT, "codex-app-transfer")
        .send()
        .await
        .map_err(|e| format!("fetch: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http {} for {url}", resp.status().as_u16()));
    }
    read_body_capped(resp).await
}

/// 取某源 registry 文本(走缓存,除非 force)。
async fn source_registry(src: &ConnectorSource, force: bool) -> Result<String, String> {
    if !force {
        if let Some((at, body)) = body_cache().lock().unwrap().get(&src.id) {
            if at.elapsed() < CACHE_TTL {
                return Ok(body.clone());
            }
        }
    }
    let bytes = if src.official {
        let token = storage_token().ok_or(
            "storage token 未配置(env CODEX_APP_TRANSFER_STORAGE_TOKEN 或 ~/.codex-app-transfer/storage_token)",
        )?;
        fetch_official(&token, REGISTRY_PATH).await?
    } else {
        let url = src.url.as_deref().ok_or("源缺 url")?;
        fetch_public(url).await?
    };
    let text = String::from_utf8_lossy(&bytes).to_string();
    body_cache()
        .lock()
        .unwrap()
        .insert(src.id.clone(), (Instant::now(), text.clone()));
    Ok(text)
}

// ── Connectors 聚合 ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ConnectorsQuery {
    #[serde(default)]
    pub force_refresh: bool,
}

/// `GET /api/marketplace/connectors` — 聚合所有启用源。
pub async fn connectors(Query(q): Query<ConnectorsQuery>) -> impl IntoResponse {
    let sources = read_sources();
    let mut connectors: Vec<Value> = Vec::new();
    let mut categories: Vec<String> = Vec::new();
    let mut source_meta: Vec<Value> = Vec::new();
    let mut errors = serde_json::Map::new();

    for s in &sources {
        if !s.enabled {
            source_meta.push(
                json!({"id": s.id, "name": s.name, "official": s.official, "enabled": false, "count": 0}),
            );
            continue;
        }
        let before = connectors.len();
        match source_registry(s, q.force_refresh).await {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(reg) => {
                    if let Some(arr) = reg.get("connectors").and_then(|v| v.as_array()) {
                        for c in arr {
                            // 校验自加源条目:必须是带 string id + name 的对象,否则丢弃 —— 防恶意/畸形
                            // registry 把非对象/缺字段条目塞给前端,前端 c.id / displayName(c).charAt(0) 崩。
                            let valid = c.get("id").and_then(|v| v.as_str()).is_some()
                                && c.get("name").and_then(|v| v.as_str()).is_some();
                            if !valid {
                                continue;
                            }
                            let mut c = c.clone();
                            if let Some(obj) = c.as_object_mut() {
                                obj.insert("source".to_string(), json!(s.id));
                                if let Some(cat) = obj.get("category").and_then(|v| v.as_str()) {
                                    let cat = cat.to_string();
                                    if !categories.contains(&cat) {
                                        categories.push(cat);
                                    }
                                }
                            }
                            connectors.push(c);
                        }
                    }
                }
                Err(e) => {
                    errors.insert(s.id.clone(), json!(format!("parse: {e}")));
                }
            },
            Err(e) => {
                errors.insert(s.id.clone(), json!(e));
            }
        }
        source_meta.push(json!({
            "id": s.id, "name": s.name, "official": s.official,
            "enabled": true, "count": connectors.len() - before,
        }));
    }

    Json(json!({
        "sources": source_meta,
        "connectors": connectors,
        "categories": categories,
        "errors": errors,
    }))
    .into_response()
}

// ── Sources 管理 ─────────────────────────────────────────────────────────────

pub async fn sources() -> impl IntoResponse {
    let list: Vec<Value> = read_sources()
        .iter()
        .map(|s| json!({"id": s.id, "name": s.name, "url": s.url, "enabled": s.enabled, "official": s.official}))
        .collect();
    Json(json!({"sources": list})).into_response()
}

#[derive(Deserialize)]
pub struct AddSourceInput {
    pub name: String,
    pub url: String,
}

pub async fn add_source(Json(input): Json<AddSourceInput>) -> impl IntoResponse {
    let name = input.name.trim().to_string();
    let url = input.url.trim().to_string();
    if name.is_empty() || url.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name 跟 url 都必填").into_response();
    }
    // 自加源强制 https(防明文 registry 在不可信网络被 MITM 篡改连接器展示数据 / 图标 URL)。
    // 不开 localhost-http 例外 —— 前缀判 localhost 可被 `http://localhost.evil.com` / `localhost@evil.com`
    // 绕过(真实 host 是 evil.com),整类去掉最稳。
    if !url.starts_with("https://") {
        return err(StatusCode::BAD_REQUEST, "自加源 URL 必须 https").into_response();
    }
    let _g = sources_lock().lock().unwrap();
    let mut list = read_sources();
    let id = source_id_from_url(&url);
    if list.iter().any(|s| s.id == id) {
        return err(StatusCode::BAD_REQUEST, "该源已存在").into_response();
    }
    list.push(ConnectorSource {
        id,
        name,
        url: Some(url),
        enabled: true,
        official: false,
    });
    match write_sources(&list) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SourceIdInput {
    pub id: String,
}

pub async fn remove_source(Json(input): Json<SourceIdInput>) -> impl IntoResponse {
    if input.id == OFFICIAL_ID {
        return err(StatusCode::BAD_REQUEST, "官方源不可删").into_response();
    }
    let _g = sources_lock().lock().unwrap();
    let mut list = read_sources();
    let before = list.len();
    list.retain(|s| s.id != input.id || s.official);
    if list.len() == before {
        return err(StatusCode::NOT_FOUND, "源不存在").into_response();
    }
    body_cache().lock().unwrap().remove(&input.id);
    match write_sources(&list) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ToggleSourceInput {
    pub id: String,
    pub enabled: bool,
}

pub async fn toggle_source(Json(input): Json<ToggleSourceInput>) -> impl IntoResponse {
    let _g = sources_lock().lock().unwrap();
    let mut list = read_sources();
    let mut found = false;
    for s in list.iter_mut() {
        if s.id == input.id {
            s.enabled = input.enabled;
            found = true;
        }
    }
    if !found {
        return err(StatusCode::NOT_FOUND, "源不存在").into_response();
    }
    match write_sources(&list) {
        Ok(()) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Icon 代理 ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct IconQuery {
    pub path: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// 把图标 path 解析成可取的 URL:绝对 URL 原样;相对则按源 registry url 的目录拼接。
fn resolve_icon_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
    format!("{}/{}", dir, path.trim_start_matches('/'))
}

/// `GET /api/marketplace/icon?source=&path=` — 图标代理 + 磁盘缓存。
/// 官方源(source 缺省 / `official`):path 限 `icons/<f>.png`,走 token。
/// 自加源:按该源 registry url 解析 path(绝对或相对),公开直取。
pub async fn icon(Query(q): Query<IconQuery>) -> impl IntoResponse {
    let source = q.source.unwrap_or_else(|| OFFICIAL_ID.to_string());
    let path = q.path;

    let cache_key = format!("{source}|{path}");
    // 缓存文件名用稳定哈希,避免有损字符净化把不同 (source,path) 坍缩到同一文件 → cache-hit 串图。
    let cache_file = home_dir().map(|h| {
        h.join(".codex-app-transfer")
            .join("marketplace-cache")
            .join(format!("{}.png", stable_hash(&cache_key)))
    });
    if let Some(cf) = &cache_file {
        if let Ok(bytes) = std::fs::read(cf) {
            return ([(header::CONTENT_TYPE, "image/png")], bytes).into_response();
        }
    }

    let fetched = if source == OFFICIAL_ID {
        // 官方源:仅 icons/ 下单层 .png(防穿越 + 防读非图标文件),走 token。
        if !path.starts_with("icons/")
            || !path.ends_with(".png")
            || path.matches('/').count() != 1
            || path.contains("..")
        {
            return err(StatusCode::BAD_REQUEST, "invalid icon path").into_response();
        }
        let Some(token) = storage_token() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "storage token 未配置").into_response();
        };
        fetch_official(&token, &path).await
    } else {
        // 自加源:按该源 url 解析 path(用户自配的源,本地 app 内 SSRF 风险可接受)。
        let sources = read_sources();
        let Some(src) = sources.iter().find(|s| s.id == source) else {
            return err(StatusCode::NOT_FOUND, "unknown source").into_response();
        };
        let Some(base) = src.url.as_deref() else {
            return err(StatusCode::BAD_REQUEST, "source 无 url").into_response();
        };
        let target = resolve_icon_url(base, &path);
        // 自加源图标也强制 https —— https registry 仍可给绝对 http logo_url,明文下载可被 MITM 篡改图标。
        if !target.starts_with("https://") {
            return err(StatusCode::BAD_REQUEST, "icon url 必须 https").into_response();
        }
        fetch_public(&target).await
    };

    match fetched {
        Ok(bytes) => {
            if let Some(cf) = &cache_file {
                if let Some(parent) = cf.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(cf, &bytes) {
                    tracing::debug!("marketplace icon cache write failed for {cache_key}: {e}");
                }
            }
            ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e).into_response(),
    }
}
