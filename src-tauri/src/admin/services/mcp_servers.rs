//! `[mcp_servers.<name>]` 结构化读写 — `toml_edit::DocumentMut` round-trip 保留注释 +
//! decor + 其他 config 节(用户的 `[model_providers.*]` `[shell_environment_policy]`
//! 等都不动)。
//!
//! 跟 marker mode 的区别:
//! - marker mode 整段替换 `# managed: mcp` 块,丢注释 + 跟用户其他 MCP 节冲突
//! - 本 service 按 server name 粒度 upsert / delete,只动目标节
//!
//! schema 严格对齐 codex `config/src/mcp_types.rs` `RawMcpServerConfig`:
//! - Stdio: command + args[] + env{} + env_vars[] + cwd
//! - StreamableHttp: url + bearer_token_env_var + http_headers + env_http_headers
//! - 公共: enabled / required / experimental_environment / startup_timeout_sec /
//!   tool_timeout_sec / default_tools_approval_mode / enabled_tools / disabled_tools /
//!   supports_parallel_tool_calls / tools

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use toml_edit::{value, Array, DocumentMut, Item, Table};

use super::managed_block::HistoryEntry;

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSpec {
    /// server name(TOML table key, e.g. `[mcp_servers.vercel]` → "vercel")
    pub name: String,
    pub transport: McpTransport,
    /// stdio
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// streamable_http
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_http_headers: Option<HashMap<String, String>>,
    /// 公共
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub supports_parallel_tool_calls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental_environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

fn default_enabled() -> bool {
    true
}

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

pub fn config_path() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex").join("config.toml"))
}

pub fn history_file() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home
        .join(".codex-app-transfer")
        .join("managed-history")
        .join("mcp-config-toml.json"))
}

fn read_doc() -> Result<DocumentMut, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read config.toml: {e}"))?;
    raw.parse::<DocumentMut>()
        .map_err(|e| format!("parse config.toml: {e}"))
}

fn write_doc(doc: &DocumentMut) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir config dir: {e}"))?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, doc.to_string()).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename tmp: {e}"))?;
    Ok(())
}

/// 列所有 `[mcp_servers.<name>]` 节
pub fn list_servers() -> Result<Vec<McpServerSpec>, String> {
    let doc = read_doc()?;
    let Some(servers_item) = doc.get("mcp_servers") else {
        return Ok(Vec::new());
    };
    let Some(servers_tbl) = servers_item.as_table() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, item) in servers_tbl.iter() {
        if let Some(tbl) = item.as_table() {
            if let Some(spec) = parse_server_table(name, tbl) {
                out.push(spec);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn parse_server_table(name: &str, tbl: &Table) -> Option<McpServerSpec> {
    let has_command = tbl.contains_key("command");
    let has_url = tbl.contains_key("url");
    let transport = if has_command {
        McpTransport::Stdio
    } else if has_url {
        McpTransport::StreamableHttp
    } else {
        // 老配置可能用 type 字段
        match tbl.get("type").and_then(|v| v.as_str()) {
            Some("streamable_http") | Some("streamable-http") | Some("http") => {
                McpTransport::StreamableHttp
            }
            _ => McpTransport::Stdio,
        }
    };
    let s_string = |k: &str| tbl.get(k).and_then(|v| v.as_str()).map(|s| s.to_owned());
    let s_bool = |k: &str, default: bool| tbl.get(k).and_then(|v| v.as_bool()).unwrap_or(default);
    let s_u64 = |k: &str| {
        tbl.get(k)
            .and_then(|v| v.as_integer())
            .map(|i| i.max(0) as u64)
    };
    let s_arr_str = |k: &str| {
        tbl.get(k).and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect::<Vec<_>>()
        })
    };
    // env / http_headers 既可能写成 inline table `env = { K = "V" }`,也可能写成
    // regular table `[mcp_servers.foo.env]\nK = "V"`。两种 toml_edit 类型不同,
    // 都要支持(否则 user-edit-and-save 会丢字段)
    let s_map_str = |k: &str| {
        let item = tbl.get(k)?;
        if let Some(t) = item.as_inline_table() {
            return Some(
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.to_owned(), s.to_owned())))
                    .collect::<HashMap<_, _>>(),
            );
        }
        if let Some(t) = item.as_table() {
            return Some(
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.to_owned(), s.to_owned())))
                    .collect::<HashMap<_, _>>(),
            );
        }
        None
    };
    Some(McpServerSpec {
        name: name.to_owned(),
        transport,
        command: s_string("command"),
        args: s_arr_str("args"),
        env: s_map_str("env"),
        cwd: s_string("cwd"),
        url: s_string("url"),
        bearer_token_env_var: s_string("bearer_token_env_var"),
        http_headers: s_map_str("http_headers"),
        env_http_headers: s_map_str("env_http_headers"),
        enabled: s_bool("enabled", true),
        required: s_bool("required", false),
        supports_parallel_tool_calls: s_bool("supports_parallel_tool_calls", false),
        experimental_environment: s_string("experimental_environment"),
        startup_timeout_sec: s_u64("startup_timeout_sec"),
        tool_timeout_sec: s_u64("tool_timeout_sec"),
        default_tools_approval_mode: s_string("default_tools_approval_mode"),
        enabled_tools: s_arr_str("enabled_tools"),
        disabled_tools: s_arr_str("disabled_tools"),
    })
}

fn command_basename(command: &str) -> String {
    let trimmed = command.trim().trim_matches('"').trim_matches('\'');
    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(trimmed)
        // **先小写再剥 `.exe`** —— 反过来会让 `BASH.EXE`(大写扩展名)漏判:
        // trim_end_matches(".exe") 大小写敏感,不剥 `.EXE`,后续 lower 成 `bash.exe`
        // 不在黑名单里 → 绕过(MOC-68 review 复盘)。
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .to_string()
}

fn is_shell_interpreter(basename: &str) -> bool {
    matches!(
        basename,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "ash"
            | "csh"
            | "tcsh"
            | "cmd"
            | "powershell"
            | "pwsh"
            | "nu"
            | "xonsh"
            | "busybox"
    )
}

/// command(或 env/wrapper 解包后的真实 command)是否是 shell 解释器。
///
/// **定位:防御纵深 guardrail,不是安全边界**。MCP spec 最终写入 `~/.codex/config.toml`
/// 由 Codex CLI 直接 spawn `command + args`,"配置即可执行任意命令"本质成立 —— `node -e`
/// / `python -c` 这类无法靠黑名单拦(拦了会误伤正常 MCP server,绝大多数 server 就是
/// node/python/npx/uvx)。本检查只拦最直白的 `bash -c` 形态 + `env bash` 包装,降低被
/// 诱导粘贴恶意 spec 的概率;真正的边界是"写配置需用户在 UI 显式确认"(见 Linear followup)。
fn command_is_shell(command: &str, args: &[String]) -> bool {
    let base = command_basename(command);
    if is_shell_interpreter(&base) {
        return true;
    }
    // env / sudo / doas / wsl 等包装器:解包第一个非 `VAR=val`、非 flag 的实参作为真实
    // command 再判一次(拦 `env bash -c …` / `/usr/bin/env sh`)。
    if matches!(base.as_str(), "env" | "sudo" | "doas" | "wsl") {
        if let Some(real) = args
            .iter()
            .find(|a| !a.starts_with('-') && !a.contains('='))
        {
            if is_shell_interpreter(&command_basename(real)) {
                return true;
            }
        }
    }
    false
}

fn cwd_is_sensitive(cwd: &str) -> bool {
    let trimmed = cwd.trim();
    if trimmed.is_empty() {
        return true;
    }
    // 非绝对路径(相对 / `~` 未展开)一律拒。注意 Windows 盘符路径在非 Windows 上
    // `is_absolute()` 为 false → 也走这里拒,语义安全。
    if !PathBuf::from(trimmed).is_absolute() && !looks_like_windows_abs(trimmed) {
        return true;
    }
    let lowered = trimmed.replace('\\', "/").to_ascii_lowercase();
    // 去掉 Windows 盘符前缀 `c:`,统一成 `/...` 再做路径段匹配。
    let after_drive = if lowered.len() >= 2
        && lowered.as_bytes()[0].is_ascii_alphabetic()
        && lowered.as_bytes()[1] == b':'
    {
        &lowered[2..]
    } else {
        lowered.as_str()
    };
    let after_drive = after_drive.trim_end_matches('/');
    // 文件系统根 / 盘符根:敏感
    if after_drive.is_empty() {
        return true;
    }
    // 系统敏感根:cwd 落在这些目录**本身或其子目录**才算敏感。
    // 用「路径段前缀」匹配(`/etc` 命中 `/etc`、`/etc/x`,但**不**误伤 `/home/x/etc-tool`)。
    // **不含** `/users`、`/home`、`/appdata` —— 用户项目正常就在这些目录下,旧版用
    // `contains` 子串匹配把 macOS 全部 `/Users/<name>/…` 误判为敏感、封死整个 cwd 字段
    // (MOC-68 review 复盘)。
    const SENSITIVE_ROOTS: &[&str] = &[
        "/windows",
        "/system32",
        "/program files",
        "/program files (x86)",
        "/programdata",
        "/etc",
        "/private/etc",
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/boot",
        "/sys",
        "/proc",
    ];
    SENSITIVE_ROOTS
        .iter()
        .any(|root| after_drive == *root || after_drive.starts_with(&format!("{root}/")))
}

/// 在非 Windows 平台上识别 `C:\…` / `C:/…` 形式的 Windows 绝对路径
/// (`PathBuf::is_absolute()` 在 Unix 上对这类返回 false,但语义上是绝对路径)。
fn looks_like_windows_abs(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// 校验 spec — stdio 必须 command,http 必须 url。失败返带说明的 error。
pub fn validate_spec(spec: &McpServerSpec) -> Result<(), String> {
    if spec.name.is_empty() {
        return Err("server name 不能为空".into());
    }
    if !spec
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "server name '{}' 包含非法字符(仅允许字母数字 - _ .)",
            spec.name
        ));
    }
    match spec.transport {
        McpTransport::Stdio => {
            let command = spec.command.as_deref().unwrap_or("").trim();
            if command.is_empty() {
                return Err("stdio transport 必须填 command".into());
            }
            let args = spec.args.as_deref().unwrap_or(&[]);
            if command_is_shell(command, args) {
                return Err(format!(
                    "stdio transport 拒绝直接使用 shell 解释器 command={command:?};请改用明确的 MCP 可执行文件"
                ));
            }
            if let Some(cwd) = spec
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
            {
                if cwd_is_sensitive(cwd) {
                    return Err(format!(
                        "stdio transport cwd 不安全:{cwd};请选择具体项目目录或用户明确管理的数据目录"
                    ));
                }
            }
            if spec.url.is_some() {
                return Err("stdio transport 不允许设 url(请切换 streamable_http)".into());
            }
        }
        McpTransport::StreamableHttp => {
            if spec.url.as_deref().unwrap_or("").is_empty() {
                return Err("streamable_http transport 必须填 url".into());
            }
            if spec.command.is_some() {
                return Err("streamable_http transport 不允许设 command(请切换 stdio)".into());
            }
        }
    }
    Ok(())
}

/// upsert 一条 `[mcp_servers.<name>]` — 保留未建模字段(`tools` per-tool approval map /
/// `env_vars` / codex 未来新加字段),只 set/remove 本工具明确建模的字段。
///
/// **Why**: 之前"先清后写"会把用户手写的 `[mcp_servers.foo.tools] bash = "auto"` 等未建模
/// 字段静默删除。改成 read-modify-set,只覆盖建模字段,跨 transport 切换时显式删对端独有字段。
pub fn upsert_server(spec: &McpServerSpec) -> Result<(), String> {
    validate_spec(spec)?;
    let mut doc = read_doc()?;
    if !doc.contains_key("mcp_servers") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["mcp_servers"] = Item::Table(t);
    }
    let servers = doc["mcp_servers"]
        .as_table_mut()
        .ok_or_else(|| "mcp_servers is not a table".to_owned())?;
    servers.set_implicit(true);
    // 拿现有 table(保留未建模字段如 `tools` per-tool approval / `env_vars`)或新建空 table。
    // write_spec_to_table 内部 sweep MODELED_KEYS 后 conditional set,跨 transport 切换时
    // 对端独有字段(url/bearer 等)也在 sweep 范围,自然清理。
    let existing = servers.get(&spec.name).and_then(|i| i.as_table()).cloned();
    let mut tbl = existing.unwrap_or_default();
    write_spec_to_table(&mut tbl, spec);
    servers.insert(&spec.name, Item::Table(tbl));
    write_doc(&doc)
}

/// 本工具明确建模的 keys — 写前 sweep 清掉,防 spec.None 字段后旧值残留;
/// 不在此清单的 key(如 `tools` per-tool approval map / `env_vars` / codex 未来新字段)
/// 保留不动。
const MODELED_KEYS: &[&str] = &[
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "bearer_token_env_var",
    "http_headers",
    "env_http_headers",
    "enabled",
    "required",
    "supports_parallel_tool_calls",
    "experimental_environment",
    "startup_timeout_sec",
    "tool_timeout_sec",
    "default_tools_approval_mode",
    "enabled_tools",
    "disabled_tools",
];

fn write_spec_to_table(tbl: &mut Table, spec: &McpServerSpec) {
    // 先 sweep 建模 keys(防 spec 字段 None 时旧值残留;未建模字段不动)
    for k in MODELED_KEYS {
        tbl.remove(k);
    }
    match spec.transport {
        McpTransport::Stdio => {
            if let Some(cmd) = &spec.command {
                tbl["command"] = value(cmd);
            }
            if let Some(args) = &spec.args {
                let arr: Array = args.iter().map(|s| s.as_str()).collect();
                tbl["args"] = value(arr);
            }
            if let Some(env) = &spec.env {
                let mut t = toml_edit::InlineTable::new();
                for (k, v) in env {
                    t.insert(k, v.as_str().into());
                }
                tbl["env"] = value(t);
            }
            if let Some(cwd) = &spec.cwd {
                tbl["cwd"] = value(cwd);
            }
        }
        McpTransport::StreamableHttp => {
            if let Some(url) = &spec.url {
                tbl["url"] = value(url);
            }
            if let Some(env_var) = &spec.bearer_token_env_var {
                tbl["bearer_token_env_var"] = value(env_var);
            }
            if let Some(headers) = &spec.http_headers {
                let mut t = toml_edit::InlineTable::new();
                for (k, v) in headers {
                    t.insert(k, v.as_str().into());
                }
                tbl["http_headers"] = value(t);
            }
            if let Some(headers) = &spec.env_http_headers {
                let mut t = toml_edit::InlineTable::new();
                for (k, v) in headers {
                    t.insert(k, v.as_str().into());
                }
                tbl["env_http_headers"] = value(t);
            }
        }
    }
    // enabled 默认 true 时不写(toml 简洁)
    if !spec.enabled {
        tbl["enabled"] = value(false);
    }
    if spec.required {
        tbl["required"] = value(true);
    }
    if spec.supports_parallel_tool_calls {
        tbl["supports_parallel_tool_calls"] = value(true);
    }
    if let Some(env) = &spec.experimental_environment {
        tbl["experimental_environment"] = value(env);
    }
    if let Some(t) = spec.startup_timeout_sec {
        tbl["startup_timeout_sec"] = value(t as i64);
    }
    if let Some(t) = spec.tool_timeout_sec {
        tbl["tool_timeout_sec"] = value(t as i64);
    }
    if let Some(mode) = &spec.default_tools_approval_mode {
        tbl["default_tools_approval_mode"] = value(mode);
    }
    if let Some(tools) = &spec.enabled_tools {
        let arr: Array = tools.iter().map(|s| s.as_str()).collect();
        tbl["enabled_tools"] = value(arr);
    }
    if let Some(tools) = &spec.disabled_tools {
        let arr: Array = tools.iter().map(|s| s.as_str()).collect();
        tbl["disabled_tools"] = value(arr);
    }
}

pub fn delete_server(name: &str) -> Result<bool, String> {
    let mut doc = read_doc()?;
    let Some(servers) = doc.get_mut("mcp_servers").and_then(|i| i.as_table_mut()) else {
        return Ok(false);
    };
    let removed = servers.remove(name).is_some();
    if removed {
        write_doc(&doc)?;
    }
    Ok(removed)
}

// ── history snapshot:整个 config.toml 全文进 history(以便完整 rollback) ──

pub fn read_history() -> Vec<HistoryEntry> {
    let Ok(path) = history_file() else {
        return Vec::new();
    };
    if !path.exists() {
        return Vec::new();
    }
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn write_history(mut history: Vec<HistoryEntry>) -> Result<(), String> {
    const LIMIT: usize = 10;
    if history.len() > LIMIT {
        let drop = history.len() - LIMIT;
        history.drain(..drop);
    }
    let path = history_file()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir history parent: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(&history).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

pub fn snapshot_current() -> Result<(), String> {
    let path = config_path()?;
    let content = if path.exists() {
        fs::read_to_string(&path).map_err(|e| format!("read config.toml: {e}"))?
    } else {
        String::new()
    };
    let mut history = read_history();
    // dedup
    if let Some(pos) = history.iter().position(|e| e.applied_content == content) {
        history.remove(pos);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    history.push(HistoryEntry {
        managed_content: String::new(),
        applied_content: content,
        timestamp: ts,
    });
    write_history(history)
}

pub fn restore_from_history(index: usize) -> Result<(), String> {
    let history = read_history();
    let Some(entry) = history.get(index) else {
        return Err(format!("history index out of range: {index}"));
    };
    let content = entry.applied_content.clone();
    snapshot_current()?; // pre-backup
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    // atomic tmp+rename 防 crash 中段留 partial config.toml(否则下次 codex 启动炸)
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, content).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// 整个 config.toml raw read(给"Edit raw TOML"折叠用)
pub fn read_raw() -> Result<String, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(&path).map_err(|e| format!("read config.toml: {e}"))
}

/// raw write — 先 parse 验证整个 TOML 文档,失败拒绝。snapshot 跑前置。
pub fn write_raw(content: &str) -> Result<(), String> {
    content
        .parse::<DocumentMut>()
        .map_err(|e| format!("invalid TOML: {e}"))?;
    snapshot_current()?;
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, content).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

#[allow(dead_code)]
pub fn _verify_path_exists(_p: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_spec(command: &str) -> McpServerSpec {
        McpServerSpec {
            name: "safe-server".to_owned(),
            transport: McpTransport::Stdio,
            command: Some(command.to_owned()),
            args: Some(vec!["server".to_owned()]),
            env: None,
            cwd: None,
            url: None,
            bearer_token_env_var: None,
            http_headers: None,
            env_http_headers: None,
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            experimental_environment: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
        }
    }

    #[test]
    fn validate_spec_rejects_shell_commands() {
        // 含 BASH.EXE(大写扩展名绕过回归)、全路径 /bin/zsh
        for command in [
            "sh",
            "bash",
            "cmd.exe",
            "powershell.exe",
            "pwsh",
            "BASH.EXE",
            "/bin/zsh",
            "busybox",
        ] {
            let err = validate_spec(&stdio_spec(command)).unwrap_err();
            assert!(
                err.contains("shell"),
                "command {command} should be rejected with shell error, got: {err}"
            );
        }
    }

    #[test]
    fn validate_spec_rejects_env_wrapped_shell() {
        // `env [VAR=val] bash -c …` 包装绕过回归
        let mut spec = stdio_spec("env");
        spec.args = Some(vec![
            "FOO=bar".to_owned(),
            "bash".to_owned(),
            "-c".to_owned(),
            "echo hi".to_owned(),
        ]);
        let err = validate_spec(&spec).unwrap_err();
        assert!(
            err.contains("shell"),
            "env bash should be rejected, got: {err}"
        );
    }

    #[test]
    fn validate_spec_allows_common_mcp_launchers() {
        for command in ["node", "npx", "python", "uvx"] {
            validate_spec(&stdio_spec(command)).unwrap();
        }
    }

    #[test]
    fn validate_spec_allows_user_project_cwd() {
        // 用户项目目录不应被误判为敏感(旧版 `/users` 子串匹配把这些全封死)
        for cwd in [
            "/Users/alice/projects/myapp",
            "/home/bob/dev/mcp-server",
            "/Users/eve/.codex/data",
        ] {
            let mut spec = stdio_spec("node");
            spec.cwd = Some(cwd.to_owned());
            validate_spec(&spec).unwrap_or_else(|e| panic!("cwd {cwd} 应放行,got: {e}"));
        }
    }

    #[test]
    fn validate_spec_rejects_sensitive_cwd() {
        // 系统敏感根(本身或子目录)在所有平台都拒
        for cwd in ["/etc", "/etc/ssl", "/usr/bin", "C:\\Windows\\System32", "/"] {
            let mut spec = stdio_spec("node");
            spec.cwd = Some(cwd.to_owned());
            let err = validate_spec(&spec).unwrap_err();
            assert!(err.contains("cwd 不安全"), "cwd {cwd} 应拒绝,got: {err}");
        }
    }
}
