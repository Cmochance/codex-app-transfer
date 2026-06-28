//! Codex plugins 管理 — 扫 `~/.codex/plugins/cache/<market>/<plugin>/<ver>/` 列出
//! 已安装 plugin,读 `.codex-plugin/plugin.json`(fallback `.claude-plugin/plugin.json`)
//! 拿 capabilities;读写 `~/.codex/config.toml` 的 `[plugins."name@market"]` 节
//! 控制 enabled / per-tool policy。
//!
//! 跟 mcp_servers.rs 互补:`mcp_servers` 管直配单 server,`codex_plugins` 管 plugin
//! bundle(plugin 内部含多 MCP server + skill + app + hook)。

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table};

use super::mcp_servers;

const DEFAULT_VERSION: &str = "local";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    /// plugin 在 user TOML 里的 key — `name@marketplace`
    pub key: String,
    pub name: String,
    pub marketplace: String,
    pub version: String,
    /// plugin 内部声明
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub mcp_server_names: Vec<String>,
    #[serde(default)]
    pub skill_names: Vec<String>,
    #[serde(default)]
    pub app_count: usize,
    #[serde(default)]
    pub hook_count: usize,
    /// `[plugins."key"]` 节里的 enabled(默认 true)
    pub enabled: bool,
    /// plugin 安装目录(reveal 用)
    pub install_dir: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// plugin manifest 里 mcpServers / mcp_servers 都接受
    #[serde(alias = "mcp_servers")]
    pub mcp_servers: Option<serde_json::Value>,
    pub skills: Option<serde_json::Value>,
    pub apps: Option<serde_json::Value>,
    pub hooks: Option<serde_json::Value>,
    /// `interface.logo` 是各 plugin 自定的图标相对路径(cloudflare.png / app-icon.png / logo.png…)
    pub interface: Option<PluginInterface>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginInterface {
    pub logo: Option<String>,
    pub composer_icon: Option<String>,
}

fn resolve_home() -> Option<PathBuf> {
    // [MOC-277 followup] 用 workspace 统一 home 解析(`$CODEX_APP_TRANSFER_HOME` 覆盖 → HOME →
    // USERPROFILE),与 `CodexPaths` 同源。否则 e2e 隔离运行(设了 CODEX_APP_TRANSFER_HOME)时
    // 这里仍解析真实 HOME,plugin 安装/卸载/cache 检测会误动真实用户的 ~/.codex/plugins。
    codex_app_transfer_registry::paths::resolve_home()
}

pub fn codex_home() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex"))
}

pub fn plugins_cache_root() -> Result<PathBuf, String> {
    Ok(codex_home()?.join("plugins").join("cache"))
}

fn read_doc() -> Result<DocumentMut, String> {
    let path = mcp_servers::config_path()?;
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read config.toml: {e}"))?;
    raw.parse::<DocumentMut>()
        .map_err(|e| format!("parse config.toml: {e}"))
}

fn write_doc(doc: &DocumentMut) -> Result<(), String> {
    let path = mcp_servers::config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir config dir: {e}"))?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, doc.to_string()).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename tmp: {e}"))?;
    Ok(())
}

/// 解析 plugin key:`name@marketplace` → (name, marketplace);兼容无 @ 的旧 key
fn parse_key(key: &str) -> (String, String) {
    if let Some((name, market)) = key.split_once('@') {
        (name.to_owned(), market.to_owned())
    } else {
        (key.to_owned(), "official".to_owned())
    }
}

fn load_manifest(dir: &Path) -> Option<PluginManifest> {
    for rel in [".codex-plugin/plugin.json", ".claude-plugin/plugin.json"] {
        let p = dir.join(rel);
        if p.exists() {
            let raw = fs::read_to_string(&p).ok()?;
            return serde_json::from_str::<PluginManifest>(&raw).ok();
        }
    }
    None
}

/// 找 plugin 的 active version 目录 — 简单实现:取 lexicographic 最大 version dir,
/// 找不到则用 `local`。codex 真实实现在 `core-plugins/src/store.rs` 更复杂(active
/// pointer),本工具暂用简化版。
/// 扫 `<install_dir>/skills/` 的子目录名 = plugin 真实 skill 列表。manifest.skills 只是 `"./skills/"`
/// 路径取不到名;且对齐 Codex loader:只算**含 `SKILL.md`** 的子目录(排除共享库 / references 等非 skill 目录)。
fn scan_skill_dirs(install_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(install_dir.join("skills"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("SKILL.md").is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

fn active_version_dir(plugin_root: &Path) -> Option<(String, PathBuf)> {
    if !plugin_root.is_dir() {
        return None;
    }
    let local = plugin_root.join(DEFAULT_VERSION);
    if local.is_dir() {
        return Some((DEFAULT_VERSION.to_owned(), local));
    }
    let mut versions: Vec<(String, PathBuf)> = fs::read_dir(plugin_root)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.is_dir() {
                return None;
            }
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_owned())?;
            Some((name, p))
        })
        .collect();
    versions.sort_by(|a, b| b.0.cmp(&a.0)); // 最大版本号在前
    versions.into_iter().next()
}

fn extract_names_from_value(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Object(m) => m.keys().cloned().collect(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .map(|s| s.to_owned())
                    .or_else(|| x.get("name").and_then(|n| n.as_str()).map(|s| s.to_owned()))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn count_top(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(m) => m.len(),
        serde_json::Value::Array(a) => a.len(),
        _ => 0,
    }
}

/// 列已安装 plugin(扫 ~/.codex/plugins/cache/)
pub fn list_installed() -> Result<Vec<PluginEntry>, String> {
    let root = plugins_cache_root()?;
    let mut out = Vec::new();
    let Ok(markets) = fs::read_dir(&root) else {
        return Ok(out);
    };
    // 读 user toml 的 plugins 节,拿 enabled 状态
    let doc = read_doc()?;
    let plugins_tbl = doc
        .get("plugins")
        .and_then(|i| i.as_table())
        .cloned()
        .unwrap_or_default();

    for market_entry in markets.flatten() {
        let market_dir = market_entry.path();
        if !market_dir.is_dir() {
            continue;
        }
        let marketplace = market_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        let Ok(plugins) = fs::read_dir(&market_dir) else {
            continue;
        };
        for plugin_entry in plugins.flatten() {
            let plugin_root = plugin_entry.path();
            if !plugin_root.is_dir() {
                continue;
            }
            let name = plugin_root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_owned();
            let Some((version, install_dir)) = active_version_dir(&plugin_root) else {
                continue;
            };
            let key = format!("{name}@{marketplace}");
            let manifest = load_manifest(&install_dir).unwrap_or_default();
            let mcp_server_names = manifest
                .mcp_servers
                .as_ref()
                .map(extract_names_from_value)
                .unwrap_or_default();
            let skill_names = scan_skill_dirs(&install_dir);
            let app_count = manifest.apps.as_ref().map(count_top).unwrap_or(0);
            let hook_count = manifest.hooks.as_ref().map(count_top).unwrap_or(0);
            let enabled = plugins_tbl
                .get(&key)
                .and_then(|i| i.as_table())
                .and_then(|t| t.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            out.push(PluginEntry {
                key,
                name,
                marketplace: marketplace.clone(),
                version,
                description: manifest.description.clone(),
                mcp_server_names,
                skill_names,
                app_count,
                hook_count,
                enabled,
                install_dir: install_dir.to_string_lossy().into_owned(),
            });
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// key → 已安装 plugin 的 install_dir(**直接**按 `cache/<market>/<name>/<ver>/` 定位,
/// 不再走 list_installed 全盘扫 —— 否则每图标请求一次全扫 = N²,延迟高)。
fn installed_dir(key: &str) -> Result<PathBuf, String> {
    let (name, marketplace) = parse_key(key);
    // 防穿越:name/marketplace 直接进 path join。
    for seg in [&name, &marketplace] {
        if seg.is_empty() || seg.contains("..") || seg.contains('/') || seg.contains('\\') {
            return Err("invalid plugin key".to_owned());
        }
    }
    let plugin_root = plugins_cache_root()?.join(&marketplace).join(&name);
    active_version_dir(&plugin_root)
        .map(|(_, dir)| dir)
        .ok_or_else(|| format!("plugin {key} 未安装"))
}

/// 规范化 path 并确认仍在 base 目录内 —— 防 symlink 逃逸(如 SKILL.md / icon 被 symlink 到
/// `~/.codex/auth.json` 等目录外文件,`fs::read` 跟 symlink 会读出并泄露)。
fn canonical_within(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let canon = fs::canonicalize(path).map_err(|e| format!("resolve path: {e}"))?;
    let canon_base = fs::canonicalize(base).map_err(|e| format!("resolve base: {e}"))?;
    if !canon.starts_with(&canon_base) {
        return Err("path escapes plugin directory".to_owned());
    }
    Ok(canon)
}

/// plugin 图标字节 + content-type。各 plugin 图标路径不同 → 读 manifest `interface.logo`
/// (cloudflare.png / app-icon.png / logo.png…),取不到再退 `assets/app-icon.png`。
pub fn plugin_icon_bytes(key: &str) -> Result<(Vec<u8>, &'static str), String> {
    let dir = installed_dir(key)?;
    let rel = load_manifest(&dir)
        .and_then(|m| m.interface)
        .and_then(|i| i.logo)
        .unwrap_or_else(|| "./assets/app-icon.png".to_owned());
    let rel = rel.trim_start_matches("./");
    // 防穿越(含 Windows 绝对 `C:\` / UNC `\\`):禁 `..` / 前导 `/` / 反斜杠 / 盘符冒号 —— 否则
    // `dir.join(rel)` 遇绝对路径会越界读 plugin 目录外任意文件。合法 logo 是 `assets/x.png` 形态。
    if rel.contains("..") || rel.starts_with('/') || rel.contains('\\') || rel.contains(':') {
        return Err("invalid logo path".to_owned());
    }
    let icon = canonical_within(&dir, &dir.join(rel))?;
    let bytes = fs::read(&icon).map_err(|e| format!("read icon: {e}"))?;
    let ct = if rel.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "image/png"
    };
    Ok((bytes, ct))
}

#[derive(Debug, Serialize)]
pub struct SkillDoc {
    pub name: String,
    pub description: String,
    pub content: String,
}

/// 读 plugin 某 skill 的 `skills/<skill>/SKILL.md`(解析 frontmatter name/description + 正文)。
pub fn read_plugin_skill(key: &str, skill: &str) -> Result<SkillDoc, String> {
    // skill 名来自 manifest,但仍校验防穿越。
    if skill.is_empty() || skill.contains("..") || skill.contains('/') || skill.contains('\\') {
        return Err("invalid skill name".to_owned());
    }
    let dir = installed_dir(key)?;
    let md = canonical_within(&dir, &dir.join("skills").join(skill).join("SKILL.md"))?;
    let raw = fs::read_to_string(&md).map_err(|e| format!("read SKILL.md: {e}"))?;
    Ok(parse_skill_md(skill, &raw))
}

/// 解析 SKILL.md 的 YAML frontmatter(`--- name/description ---`)+ 正文。
fn parse_skill_md(fallback_name: &str, raw: &str) -> SkillDoc {
    let mut name = fallback_name.to_owned();
    let mut description = String::new();
    let mut content = raw.trim_start().to_owned();
    if let Some(rest) = raw.trim_start().strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    name = v.trim().to_owned();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().to_owned();
                }
            }
            content = rest[end + 4..].trim_start().to_owned();
        }
    }
    SkillDoc {
        name,
        description,
        content,
    }
}

/// 设 `[plugins."name@market"] enabled = <bool>`
pub fn set_enabled(key: &str, enabled: bool) -> Result<(), String> {
    let mut doc = read_doc()?;
    if !doc.contains_key("plugins") {
        let mut t = Table::new();
        t.set_implicit(true);
        doc["plugins"] = Item::Table(t);
    }
    let plugins = doc["plugins"]
        .as_table_mut()
        .ok_or_else(|| "plugins is not a table".to_owned())?;
    plugins.set_implicit(true);
    if !plugins.contains_key(key) {
        plugins.insert(key, Item::Table(Table::new()));
    }
    let tbl = plugins
        .get_mut(key)
        .and_then(|i| i.as_table_mut())
        .ok_or_else(|| format!("plugins.{key} not a table"))?;
    tbl["enabled"] = value(enabled);
    write_doc(&doc)
}

/// 卸载 plugin — 删 cache 目录 + 删 `[plugins."name@market"]` 节
///
/// **安全**:parse_key 后的 name/marketplace 都进 path join + `remove_dir_all`,必须防
/// path traversal — 攻击者可以提交 `../../foo@../../bar` 类 key 删除任意 user 目录。
pub fn uninstall(key: &str) -> Result<(), String> {
    let (name, marketplace) = parse_key(key);
    for (label, val) in [("name", &name), ("marketplace", &marketplace)] {
        if val.is_empty() {
            return Err(format!("plugin {label} 不能为空"));
        }
        if val == "." || val == ".." {
            return Err(format!("plugin {label} '{val}' 不安全(禁止 . / ..)"));
        }
        for c in val.chars() {
            if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
                return Err(format!("plugin {label} '{val}' 含非法字符"));
            }
        }
    }
    let cache_dir = plugins_cache_root()?.join(&marketplace).join(&name);
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir).map_err(|e| format!("rm cache dir: {e}"))?;
    }
    let mut doc = read_doc()?;
    let mut toml_removed = false;
    if let Some(plugins) = doc.get_mut("plugins").and_then(|i| i.as_table_mut()) {
        toml_removed = plugins.remove(key).is_some();
    }
    // 只有 toml 真有 entry 被删才写盘 — 避免对不存在 plugin 调 uninstall 也 churn config.toml
    if toml_removed {
        write_doc(&doc)?;
    }
    Ok(())
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InstallInput {
    pub name: String,
    pub marketplace: String,
    pub version: String,
    /// HTTPS URL,必须 https 防 MITM(JSON 接 camelCase `tarballUrl`)
    pub tarball_url: String,
}

/// 安装 plugin — 下载 tar.gz → 解压到 cache_dir(staged tempdir + atomic rename)+
/// 写 `[plugins."name@market"] enabled = true`
pub async fn install_tarball(input: &InstallInput) -> Result<PluginEntry, String> {
    if !input.tarball_url.starts_with("https://") {
        return Err(format!(
            "tarball_url 必须 https(防 MITM):{}",
            input.tarball_url
        ));
    }
    // name / marketplace / version 都 path join 进 cache 目录,必须严防 path traversal:
    // (1) 非空 (2) 不允许 `.` `..`(整字符串拒)(3) 不允许 `/` `\`(单 char 拒)
    //     (4) 其他 char 限制为 [A-Za-z0-9_.-]
    for (label, val) in [
        ("name", &input.name),
        ("marketplace", &input.marketplace),
        ("version", &input.version),
    ] {
        if val.is_empty() {
            return Err(format!("plugin {label} 不能为空"));
        }
        if val == "." || val == ".." {
            return Err(format!("plugin {label} '{val}' 不安全(禁止 . / ..)"));
        }
        for c in val.chars() {
            if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
                return Err(format!("plugin {label} '{val}' 含非法字符"));
            }
        }
    }
    // 下载 — 严防 OOM 攻击:
    // (1) timeout 60s(防慢速 / 永不结束的连接占内存)
    // (2) Content-Length 预检 > 50MB 直接拒(防 server 谎报大文件)
    // (3) streaming 累计字节,边读边检查 > 50MB 立即 abort(防 server 不发 Content-Length 但实际超大)
    const MAX_TARBALL_BYTES: usize = 50 * 1024 * 1024;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        // [MOC-96] connect 阶段封顶,坏系统代理下连接不再阻塞到 overall timeout。
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("reqwest build: {e}"))?;
    let resp = client
        .get(&input.tarball_url)
        .send()
        .await
        .map_err(|e| format!("download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("http error: {e}"))?;
    if let Some(cl) = resp.content_length() {
        if cl > MAX_TARBALL_BYTES as u64 {
            return Err(format!(
                "tarball too large per Content-Length: {cl} bytes (max {MAX_TARBALL_BYTES})"
            ));
        }
    }
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read chunk: {e}"))?;
        if buf.len() + chunk.len() > MAX_TARBALL_BYTES {
            return Err(format!(
                "tarball exceeded {MAX_TARBALL_BYTES} bytes mid-download — aborted"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    let bytes = buf;
    // 解压到 staged tempdir
    let target_dir = plugins_cache_root()?
        .join(&input.marketplace)
        .join(&input.name)
        .join(&input.version);
    // 不用 with_extension — 版本号 "1.0.0" 含点,with_extension 会替换最后段 ".0" →
    // "1.0.staged.tmp",跟 "1.0.1" 撞 staged path。改成显式拼父目录 + "<ver>.staged.tmp"。
    let staged = target_dir
        .parent()
        .ok_or_else(|| "target_dir 无父目录".to_owned())?
        .join(format!("{}.staged.tmp", input.version));
    if staged.exists() {
        fs::remove_dir_all(&staged).map_err(|e| format!("clean staged: {e}"))?;
    }
    fs::create_dir_all(&staged).map_err(|e| format!("mkdir staged: {e}"))?;
    let cursor = Cursor::new(&bytes[..]);
    let decoder = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);
    // path traversal + symlink safety:
    // - tar crate unpack() 拒绝绝对路径 / .. 但不拒绝 symlink。
    //   恶意 tarball 可含指向 /etc/passwd 或 ../.ssh/ 的 symlink,
    //   后续 archive 条目若跟 symlink 同 path 就会覆写外部文件。
    // - 改用 entries() 迭代,每条校验:
    //   (1) path 不逃逸 staged 目录(canonicalize 后仍以 staged 为前缀)
    //   (2) 拒绝 symlink 类型(平台通用;即使 target 合法,也防止后续条目
    //       写到 symlink 指向的外部路径)
    //   (3) 拒绝硬链接(unix 才出现;防御多引用攻击)
    //   任何违反 → 清 staged 报错,绝不部分解压。
    //
    // 不信任 archive.unpack() 的默认防护:tar crate 0.4 的 unpack_internal
    // 只 reject 绝对路径 + windows 盘符 + `..` 组件,不拦截 symlink 创建。
    {
        let staged_canonical = staged
            .canonicalize()
            .map_err(|e| format!("cannot canonicalize staged dir: {e}"))?;
        for entry in archive
            .entries()
            .map_err(|e| format!("read tar entries: {e}"))?
        {
            let mut entry = entry.map_err(|e| format!("tar entry error: {e}"))?;
            let entry_path = entry
                .path()
                .map_err(|e| format!("tar entry path error: {e}"))?
                .into_owned();
            // (1) path escape check — 先拼完整路径再 canonicalize
            let full = staged.join(&entry_path);
            // canonicalize 要求路径存在;对新建文件/目录先检查 parent
            let check_path =
                if full.exists() {
                    full.canonicalize().map_err(|e| {
                        format!("cannot canonicalize entry {}: {e}", entry_path.display())
                    })?
                } else if let Some(parent) = full.parent() {
                    // parent 必须已存在(由 tar 条目顺序保证目录先于文件)
                    let parent_canon = if parent.exists() {
                        parent.canonicalize().map_err(|e| {
                            format!(
                                "cannot canonicalize parent of {}: {e}",
                                entry_path.display()
                            )
                        })?
                    } else {
                        parent.to_path_buf()
                    };
                    parent_canon.join(full.file_name().ok_or_else(|| {
                        format!("entry has no filename: {}", entry_path.display())
                    })?)
                } else {
                    return Err(format!("entry has no parent: {}", entry_path.display()));
                };
            if !check_path.starts_with(&staged_canonical) {
                let _ = fs::remove_dir_all(&staged);
                return Err(format!(
                    "tarball entry escapes staged dir: {} → {}",
                    entry_path.display(),
                    check_path.display()
                ));
            }
            // (2) reject symlinks and hard links
            let header = entry.header();
            if header.entry_type().is_symlink() || header.entry_type().is_hard_link() {
                let _ = fs::remove_dir_all(&staged);
                return Err(format!(
                    "tarball contains forbidden symlink/hardlink: {}",
                    entry_path.display()
                ));
            }
            // (3) extract this entry — 检查 unpack_in 的 bool 返回值:tar crate 对
            // 逃逸条目(`..`/绝对路径)返回 Ok(false) **静默跳过**,`?` 只传播 Err。
            // 前面手工 escape 校验已拦大部分,这里把 unpack_in 自带的 inside-dst 判定
            // 作为 fail-closed backstop(tarball 解包遇异常 entry 不静默放过)。
            let unpacked = entry
                .unpack_in(&staged)
                .map_err(|e| format!("untar entry {}: {e}", entry_path.display()))?;
            if !unpacked {
                let _ = fs::remove_dir_all(&staged);
                return Err(format!(
                    "tarball entry escaped staged dir (unpack_in rejected): {}",
                    entry_path.display()
                ));
            }
        }
    }
    // 验证 plugin.json 存在(防 marketplace 投毒)
    let candidate_a = staged.join(".codex-plugin").join("plugin.json");
    let candidate_b = staged.join(".claude-plugin").join("plugin.json");
    if !candidate_a.exists() && !candidate_b.exists() {
        // tar 内可能多包一层 wrapper dir(github tarball 默认行为),展开
        // **严格校验**:staged 根必须只含**单个**子目录,无任何 root-level 文件,
        // 且 collision check 防 inner 子条目跟 staged 根残留冲突。一旦异常立即清 staged
        // 报错(防 malformed tarball 走到后续校验)
        let entries: Vec<_> = fs::read_dir(&staged)
            .map_err(|e| format!("read staged: {e}"))?
            .flatten()
            .map(|e| e.path())
            .collect();
        let dirs: Vec<&PathBuf> = entries.iter().filter(|p| p.is_dir()).collect();
        let non_dirs: Vec<&PathBuf> = entries.iter().filter(|p| !p.is_dir()).collect();
        if !non_dirs.is_empty() {
            let _ = fs::remove_dir_all(&staged);
            return Err(format!(
                "malformed tarball:无 plugin.json 但根含 {} 个 root-level 文件",
                non_dirs.len()
            ));
        }
        if dirs.len() != 1 {
            let _ = fs::remove_dir_all(&staged);
            return Err(format!(
                "malformed tarball:无 plugin.json 且根含 {} 个子目录(应单一 wrapper)",
                dirs.len()
            ));
        }
        let inner = dirs[0].clone();
        if !inner.join(".codex-plugin/plugin.json").exists()
            && !inner.join(".claude-plugin/plugin.json").exists()
        {
            let _ = fs::remove_dir_all(&staged);
            return Err("malformed tarball:wrapper dir 内仍无 plugin.json".into());
        }
        // 先把 wrapper dir 移到 staged 兄弟位置(用 PID 防多并发 install 撞名),让 staged
        // 变空 — 这样 inner/child 跟 wrapper 同名也不会假阳性 collision(原 staged.join(name)
        // 会指到 wrapper 自己,误判 collision)
        let inner_holding = staged
            .parent()
            .ok_or_else(|| "staged 无父目录".to_owned())?
            .join(format!(
                ".inner-hold-{}-{}",
                std::process::id(),
                input.version
            ));
        if inner_holding.exists() {
            fs::remove_dir_all(&inner_holding).ok();
        }
        fs::rename(&inner, &inner_holding).map_err(|e| format!("move wrapper out: {e}"))?;
        // 现在 staged 是空目录,collision check 干净
        for entry in fs::read_dir(&inner_holding).map_err(|e| format!("flat iter init: {e}"))? {
            let entry = entry.map_err(|e| format!("flat iter: {e}"))?;
            let name = entry.file_name();
            if staged.join(&name).exists() {
                // 这种情况只有 staged 不是干净空 dir 才会触发,本不该发生(staged 是新 tempdir 解压结果)
                let _ = fs::remove_dir_all(&staged);
                let _ = fs::remove_dir_all(&inner_holding);
                return Err(format!(
                    "malformed tarball:扁平化时 inner/{} 跟 staged 冲突",
                    name.to_string_lossy()
                ));
            }
        }
        for entry in fs::read_dir(&inner_holding).map_err(|e| format!("flat iter: {e}"))? {
            let entry = entry.map_err(|e| format!("flat iter: {e}"))?;
            let from = entry.path();
            let to = staged.join(from.strip_prefix(&inner_holding).unwrap());
            fs::rename(&from, &to).map_err(|e| format!("flat mv: {e}"))?;
        }
        fs::remove_dir_all(&inner_holding).ok();
    }
    if !staged.join(".codex-plugin/plugin.json").exists()
        && !staged.join(".claude-plugin/plugin.json").exists()
    {
        let _ = fs::remove_dir_all(&staged);
        return Err(
            "tarball 内未找到 .codex-plugin/plugin.json 或 .claude-plugin/plugin.json".into(),
        );
    }
    // atomic rename:删旧 target_dir → mv staged
    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).map_err(|e| format!("rm old target: {e}"))?;
    }
    if let Some(parent) = target_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir target parent: {e}"))?;
    }
    fs::rename(&staged, &target_dir).map_err(|e| format!("rename staged: {e}"))?;
    // 写 [plugins."name@market"] enabled = true
    let key = format!("{}@{}", input.name, input.marketplace);
    set_enabled(&key, true)?;
    // 返新条目
    let installed = list_installed()?;
    installed
        .into_iter()
        .find(|e| e.key == key)
        .ok_or_else(|| format!("install ok but not found in list: {key}"))
}

/// 从编译期内置(`include_dir!`)的 plugin 目录树安装 —— 不下载、不解 tar。
///
/// 复用 [`install_tarball`] 落盘后段范式:staged tempdir → 写内置树 → 校验
/// `.codex-plugin/plugin.json` → atomic rename 进 cache → 写 `[plugins."name@market"] enabled = true`。
/// 内置树可信(字节来自编译期、无 symlink/traversal),故省去下载与 tar 条目硬化校验。
///
/// `executable_rel_paths`:`include_dir` 不保留可执行位,装完在 unix 上对这些相对路径补
/// `chmod 0o755`(如 SessionStart hook 入口 `hooks/run-hook.cmd`,Codex 会直接执行)。
pub fn install_embedded(
    name: &str,
    marketplace: &str,
    version: &str,
    dir: &include_dir::Dir<'_>,
    executable_rel_paths: &[&str],
) -> Result<PluginEntry, String> {
    // 与 install_tarball 同款 path-traversal 校验(三段都 join 进 cache 路径)。
    for (label, val) in [
        ("name", name),
        ("marketplace", marketplace),
        ("version", version),
    ] {
        if val.is_empty() {
            return Err(format!("plugin {label} 不能为空"));
        }
        if val == "." || val == ".." {
            return Err(format!("plugin {label} '{val}' 不安全(禁止 . / ..)"));
        }
        for c in val.chars() {
            if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
                return Err(format!("plugin {label} '{val}' 含非法字符"));
            }
        }
    }
    let target_dir = plugins_cache_root()?
        .join(marketplace)
        .join(name)
        .join(version);
    let staged = target_dir
        .parent()
        .ok_or_else(|| "target_dir 无父目录".to_owned())?
        .join(format!("{version}.staged.tmp"));
    if staged.exists() {
        fs::remove_dir_all(&staged).map_err(|e| format!("clean staged: {e}"))?;
    }
    fs::create_dir_all(&staged).map_err(|e| format!("mkdir staged: {e}"))?;
    if let Err(e) = write_embedded_dir(dir, &staged) {
        let _ = fs::remove_dir_all(&staged);
        return Err(e);
    }
    // include_dir 丢失可执行位 → 在 staged(rename 前)补 chmod,使最终目录原子地带 +x。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for rel in executable_rel_paths {
            let p = staged.join(rel);
            if p.is_file() {
                match fs::metadata(&p) {
                    Ok(meta) => {
                        let mut perms = meta.permissions();
                        perms.set_mode(0o755);
                        if let Err(e) = fs::set_permissions(&p, perms) {
                            let _ = fs::remove_dir_all(&staged);
                            return Err(format!("chmod +x {rel}: {e}"));
                        }
                    }
                    Err(e) => {
                        let _ = fs::remove_dir_all(&staged);
                        return Err(format!("stat {rel}: {e}"));
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = executable_rel_paths;
    // fail-closed backstop:内置树理应含 plugin.json(若 include_dir 漏了 dotfile 会在此暴露)
    if !staged.join(".codex-plugin/plugin.json").exists()
        && !staged.join(".claude-plugin/plugin.json").exists()
    {
        let _ = fs::remove_dir_all(&staged);
        return Err(
            "内置 plugin 树缺 .codex-plugin/plugin.json(检查 include_dir 是否纳入 dotfile)".into(),
        );
    }
    // atomic rename:删旧 → mv staged
    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).map_err(|e| format!("rm old target: {e}"))?;
    }
    if let Some(parent) = target_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir target parent: {e}"))?;
    }
    fs::rename(&staged, &target_dir).map_err(|e| format!("rename staged: {e}"))?;
    let key = format!("{name}@{marketplace}");
    set_enabled(&key, true)?;
    list_installed()?
        .into_iter()
        .find(|e| e.key == key)
        .ok_or_else(|| format!("install ok but not found in list: {key}"))
}

/// 递归把 `include_dir` 目录树写进 `dest_root`(`File::path()` 是相对内置根的完整路径,
/// 直接 join 还原结构)。
fn write_embedded_dir(dir: &include_dir::Dir<'_>, dest_root: &Path) -> Result<(), String> {
    for file in dir.files() {
        let dest = dest_root.join(file.path());
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        fs::write(&dest, file.contents()).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    for sub in dir.dirs() {
        write_embedded_dir(sub, dest_root)?;
    }
    Ok(())
}
