//! Marketplace registry — fetch curated server / plugin list,跨多 source 索引,
//! 简单内存缓存(1h TTL)。
//!
//! 默认源:`Cmochance/codex-app-transfer-registry`(用户名下官方源)
//! 用户自定义源持久化在 `~/.codex-app-transfer/marketplace-sources.json`。

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const OFFICIAL_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/Cmochance/codex-app-transfer-registry/main/registry.json";
const SOURCES_STORE_FILE: &str = "marketplace-sources.json";
const CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceSource {
    pub id: String,
    pub name: String,
    pub url: String,
    /// true = 官方源(只读,用户不能删)/ false = 用户自定义
    #[serde(default)]
    pub official: bool,
    /// true = 启用(默认所有源都启用,关掉跳过)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct SourcesStore {
    #[serde(default)]
    pub sources: Vec<MarketplaceSource>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegistryServer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub transport: String, // "stdio" | "streamable_http"
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env_vars: Option<Vec<RegistryEnvVar>>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub bearer_token_env_var: Option<String>,
    /// source id(由 marketplace 注入,不来自 registry json)
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEnvVar {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPlugin {
    pub id: String,
    pub marketplace: String,
    pub version: String,
    pub tarball_url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Option<RegistryCapabilities>,
    /// source id(由 marketplace 注入)
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RegistryCapabilities {
    #[serde(default)]
    pub mcp_servers: usize,
    #[serde(default)]
    pub skills: usize,
    #[serde(default)]
    pub apps: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct RegistryDoc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    servers: Vec<RegistryServer>,
    #[serde(default)]
    plugins: Vec<RegistryPlugin>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RegistryIndex {
    pub servers: Vec<RegistryServer>,
    pub plugins: Vec<RegistryPlugin>,
    /// source.id → 错误消息(fetch 失败的源,UI 显警告)
    pub errors: HashMap<String, String>,
}

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

fn sources_store_path() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex-app-transfer").join(SOURCES_STORE_FILE))
}

/// 默认源列表 — 始终注入"官方源",用户不能删
fn default_sources() -> Vec<MarketplaceSource> {
    vec![MarketplaceSource {
        id: "official".to_owned(),
        name: "官方源 / Official".to_owned(),
        url: OFFICIAL_REGISTRY_URL.to_owned(),
        official: true,
        enabled: true,
    }]
}

pub fn load_sources_raw() -> Result<SourcesStore, String> {
    let path = sources_store_path()?;
    if !path.exists() {
        return Ok(SourcesStore {
            sources: default_sources(),
        });
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read sources: {e}"))?;
    let mut store: SourcesStore = serde_json::from_str(&raw).unwrap_or_default();
    // 确保 official 始终存在
    if !store.sources.iter().any(|s| s.official) {
        store.sources.insert(0, default_sources().remove(0));
    }
    Ok(store)
}

pub fn save_sources(store: &SourcesStore) -> Result<(), String> {
    let path = sources_store_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir sources parent: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(store).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

pub fn list_sources() -> Result<Vec<MarketplaceSource>, String> {
    Ok(load_sources_raw()?.sources)
}

pub fn add_source(name: String, url: String) -> Result<MarketplaceSource, String> {
    if !url.starts_with("https://") {
        return Err("marketplace source 必须 https".into());
    }
    let mut store = load_sources_raw()?;
    let id = format!(
        "custom-{:x}",
        SystemTimeHash::now()
            .fold(name.as_bytes())
            .fold(url.as_bytes())
            .value()
    );
    let src = MarketplaceSource {
        id: id.clone(),
        name,
        url,
        official: false,
        enabled: true,
    };
    if store.sources.iter().any(|s| s.url == src.url) {
        return Err(format!("source 已存在: {}", src.url));
    }
    store.sources.push(src.clone());
    save_sources(&store)?;
    Ok(src)
}

pub fn remove_source(id: &str) -> Result<bool, String> {
    let mut store = load_sources_raw()?;
    let before = store.sources.len();
    store.sources.retain(|s| !(s.id == id && !s.official));
    let removed = store.sources.len() != before;
    if removed {
        save_sources(&store)?;
    }
    Ok(removed)
}

pub fn toggle_source(id: &str, enabled: bool) -> Result<bool, String> {
    let mut store = load_sources_raw()?;
    let mut found = false;
    for s in &mut store.sources {
        if s.id == id {
            s.enabled = enabled;
            found = true;
            break;
        }
    }
    if found {
        save_sources(&store)?;
    }
    Ok(found)
}

// 简易内存缓存,每 source 1h
struct CacheEntry {
    fetched_at: Instant,
    doc: RegistryDoc,
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static C: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn fetch_one(source: &MarketplaceSource, force_refresh: bool) -> Result<RegistryDoc, String> {
    if !force_refresh {
        let cache = cache().lock().unwrap();
        if let Some(entry) = cache.get(&source.id) {
            if entry.fetched_at.elapsed() < CACHE_TTL {
                return Ok(entry.doc.clone());
            }
        }
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("reqwest build: {e}"))?;
    let resp = client
        .get(&source.url)
        .send()
        .await
        .map_err(|e| format!("fetch: {e}"))?
        .error_for_status()
        .map_err(|e| format!("http: {e}"))?;
    let raw = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    let doc: RegistryDoc =
        serde_json::from_str(&raw).map_err(|e| format!("parse registry json: {e}"))?;
    let mut cache = cache().lock().unwrap();
    cache.insert(
        source.id.clone(),
        CacheEntry {
            fetched_at: Instant::now(),
            doc: doc.clone(),
        },
    );
    Ok(doc)
}

/// 聚合所有 enabled source 的 servers/plugins,失败 source 进 errors map
pub async fn index(force_refresh: bool) -> Result<RegistryIndex, String> {
    let sources = list_sources()?;
    let mut out = RegistryIndex::default();
    for s in sources {
        if !s.enabled {
            continue;
        }
        match fetch_one(&s, force_refresh).await {
            Ok(mut doc) => {
                for item in &mut doc.servers {
                    item.source = s.id.clone();
                }
                for item in &mut doc.plugins {
                    item.source = s.id.clone();
                }
                out.servers.extend(doc.servers);
                out.plugins.extend(doc.plugins);
            }
            Err(e) => {
                out.errors.insert(s.id.clone(), e);
            }
        }
    }
    out.servers.sort_by(|a, b| a.id.cmp(&b.id));
    out.plugins.sort_by(|a, b| {
        (a.id.clone(), a.marketplace.clone()).cmp(&(b.id.clone(), b.marketplace.clone()))
    });
    Ok(out)
}

// 简单 hash 给 source id 用 — 不要为此引 sha2 重复 dep,自己写个 fnv-like
struct SystemTimeHash {
    state: u64,
}
impl SystemTimeHash {
    fn now() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self {
            state: 0xcbf2_9ce4_8422_2325 ^ nanos,
        }
    }
    fn fold(mut self, bytes: &[u8]) -> Self {
        for b in bytes {
            self.state ^= *b as u64;
            self.state = self.state.wrapping_mul(0x100_0000_01b3);
        }
        self
    }
    fn value(self) -> u64 {
        self.state
    }
}
