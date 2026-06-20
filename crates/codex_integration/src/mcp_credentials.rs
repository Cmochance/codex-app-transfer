//! MOC-62: MCP 授权"可移植保险箱"。
//!
//! Codex 默认把 MCP OAuth 凭据存 OS 钥匙串(按 `server_name+url` 索引,与 ChatGPT
//! 账号 / `auth.json` 无关)。本模块在用户开启 `mcpCredentialsPortableStore` 时:
//!
//! 1. [`ensure_file_store_mode`] 往 `~/.codex/config.toml` 写根级
//!    `mcp_oauth_credentials_store = "file"`,让 Codex 改用 file 存储
//!    (`~/.codex/.credentials.json`,单 JSON blob,`server_key` → entry),
//!    使凭据成为一个可被 transfer 备份 / 恢复的普通文件。
//! 2. [`sync_mcp_credentials`] 维护镜像 `~/.codex-app-transfer/mcp-credentials.json`
//!    (在 `~/.codex` 之外,`rsync --delete` / 误删 / 换机都碰不到):
//!    - live 文件**存在** → live 是 Codex 当前权威状态,镜像精确**跟随** live
//!      (捕获新授权 + 传播用户的 logout / 撤销删除),**绝不**写 live。
//!    - live 文件**整个不在** → **不自动恢复**:`SyncReport::restore_available` 报告镜像
//!      里有多少条可恢复,由上层**弹确认**让用户决定(确认 → [`restore_mcp_credentials_from_mirror`];
//!      忽略 → [`discard_mcp_mirror`])。因为 Codex 在登出最后一个 server 时会**删除**
//!      `.credentials.json`(见 upstream `write_fallback_file`:store 空就 `remove_file`),
//!      "整文件不在"既可能是用户有意登出全部、也可能是误删 / 换机,**同步时无从区分**,
//!      所以交给用户确认而非静默复活已撤销的凭据。
//!
//! 边界:这是"备份 + 用户确认恢复",**不**解决 OAuth 过期(过期 token 恢复回去仍过期,
//! 需重新授权)。安全权衡:file 模式 token 明文落盘(0o600,Codex 官方支持的模式)。

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::toml_sync::{sync_root_value, write_atomic};
use crate::{CodexError, CodexPaths};

/// Codex 读这个根级 key 决定 MCP OAuth 凭据存哪(`"auto"` | `"file"` | `"keyring"`)。
const STORE_MODE_KEY: &str = "mcp_oauth_credentials_store";

/// 让 Codex 把 MCP OAuth 凭据写进 `~/.codex/.credentials.json`(file 模式)。
///
/// `enabled=false` 删除该 key,回退 Codex 默认(`Auto`)—— **不删** `.credentials.json`,
/// 非破坏。该 key 是全局偏好,独立于 provider apply/restore(故意不进
/// `MANAGED_TOML_KEYS`,否则会被 restore 剥掉)。
pub fn ensure_file_store_mode(paths: &CodexPaths, enabled: bool) -> Result<(), CodexError> {
    let raw = if enabled { Some("\"file\"") } else { None };
    sync_root_value(&paths.config_toml, STORE_MODE_KEY, raw)
}

/// 一次镜像同步的结果(用于日志 / 测试断言 / 决定是否弹恢复确认)。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// 从 live 捕获进镜像的新增条目数(新授权)。
    pub captured: usize,
    /// 镜像里被丢弃的条目数 —— 对应用户在 live 侧 logout / 撤销的 server(传播删除)。
    pub dropped: usize,
    pub mirror_written: bool,
    /// live 整文件缺失 + 镜像有 N 条 → **不自动写**,N>0 表示需上层弹确认让用户决定恢复。
    pub restore_available: usize,
    /// 非 `None` 表示本次整体跳过(如某侧文件损坏 / 读不动),内容为原因。
    pub skipped: Option<String>,
}

/// 一侧凭据文件的读取结果。三态严格区分:
/// - `Missing`:文件不存在 / 空 —— live 侧视作"整文件丢失";
/// - `Parsed`:正常 JSON object(单个 key 缺失 = 用户登出了那个 server);
/// - `Corrupt`:读不动 / 非 object / parse 失败 —— **绝不**当空,否则会用空覆盖掉
///   可能可恢复的数据。
enum CredRead {
    Missing,
    Parsed(Map<String, Value>),
    Corrupt,
}

fn read_creds(path: &Path) -> CredRead {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CredRead::Missing,
        // 读不动(权限 / IO)按损坏处理,绝不当空 —— 防止误判后用空覆盖。
        Err(_) => return CredRead::Corrupt,
    };
    if s.trim().is_empty() {
        return CredRead::Missing;
    }
    match serde_json::from_str::<Value>(&s) {
        Ok(Value::Object(map)) => CredRead::Parsed(map),
        // 非 JSON object / parse 失败 → 损坏。
        _ => CredRead::Corrupt,
    }
}

fn write_creds_atomic(path: &Path, map: &Map<String, Value>) -> Result<(), CodexError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut s = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
    s.push('\n');
    write_atomic(path, &s)?;
    // POSIX:0o600,token 明文落盘必须不让其它用户读(与 auth.json 同处理)。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 同步实时 `~/.codex/.credentials.json` 与 transfer 镜像。语义见模块级文档:
/// live 存在 → 镜像精确跟随 live(尊重用户删除,不复活,绝不写 live);
/// live 整文件缺失 → 不自动恢复,只在 `restore_available` 报告可恢复条数(上层弹确认)。
/// 任一侧损坏 → 整体跳过(不写),保留两个文件原样供人工排查。
pub fn sync_mcp_credentials(paths: &CodexPaths) -> Result<SyncReport, CodexError> {
    let live = read_creds(&paths.mcp_credentials);
    let mirror = read_creds(&paths.mcp_credentials_mirror);

    match (&live, &mirror) {
        (CredRead::Corrupt, _) => {
            return Ok(SyncReport {
                skipped: Some("live credentials file unreadable/corrupt".into()),
                ..Default::default()
            });
        }
        (_, CredRead::Corrupt) => {
            return Ok(SyncReport {
                skipped: Some("mirror credentials file unreadable/corrupt".into()),
                ..Default::default()
            });
        }
        _ => {}
    }
    let mirror_map = match mirror {
        CredRead::Parsed(m) => m,
        _ => Map::new(),
    };
    let mut recovering = read_recovery_state(&paths.mcp_recovery_state);

    match live {
        // 整个 live 文件不在 —— 可能是用户登出全部(Codex 删了文件),也可能是误删 / 换机,
        // 同步时无从区分 → **不自动写 live**:把镜像里尚未跟踪的 key 加入「恢复待处理」状态
        // (MOC-261 一-4:逐条由用户确认恢复/移除/忽略),只报告待处理(未忽略)条数交上层弹窗。
        CredRead::Missing => {
            let mut changed = false;
            for k in mirror_map.keys() {
                if !recovering.contains_key(k) {
                    recovering.insert(k.clone(), json!({ "ignored": false }));
                    changed = true;
                }
            }
            let before = recovering.len();
            recovering.retain(|k, _| mirror_map.contains_key(k)); // 清掉镜像已无的过期状态项
            if changed || recovering.len() != before {
                write_recovery_state(&paths.mcp_recovery_state, &recovering)?;
            }
            let pending = recovering.iter().filter(|(_, v)| !entry_ignored(v)).count();
            Ok(SyncReport {
                restore_available: pending,
                ..Default::default()
            })
        }
        // live 存在 → 它是 Codex 当前权威状态(用户 logout / 撤销会从 live 删掉对应 key)。
        // 镜像跟随 live:捕获新授权 + 传播真登出删除,**绝不**把 live 没有的 key 写回 live。
        // **但**:恢复态里的 key(整文件 wipe 后待用户处理的备份)受保护 —— 不被当登出清掉,
        // 这样「部分恢复」后剩余项仍留在镜像 + 下次继续提示(不静默丢备份)。已回到 live 的恢复项
        // (用户 restore 或在 Codex 重新授权)从恢复态清除。
        CredRead::Parsed(live_map) => {
            let mut state_changed = false;
            let before_state = recovering.len();
            recovering.retain(|k, _| !live_map.contains_key(k)); // 已回 live → 不再是「丢失待恢复」
                                                                 // 新镜像 = 当前 live ∪ 仍受保护的恢复态 key(用镜像里现存的备份值)。
            let mut new_mirror = live_map.clone();
            for k in recovering.keys() {
                if !new_mirror.contains_key(k) {
                    if let Some(v) = mirror_map.get(k) {
                        new_mirror.insert(k.clone(), v.clone());
                    }
                }
            }
            recovering.retain(|k, _| new_mirror.contains_key(k)); // 镜像里没有的恢复态项清掉
            if recovering.len() != before_state {
                state_changed = true;
            }
            let mirror_written = new_mirror != mirror_map;
            if mirror_written {
                write_creds_atomic(&paths.mcp_credentials_mirror, &new_mirror)?;
            }
            if state_changed {
                write_recovery_state(&paths.mcp_recovery_state, &recovering)?;
            }
            let captured = live_map
                .keys()
                .filter(|k| !mirror_map.contains_key(*k))
                .count();
            let dropped = mirror_map
                .keys()
                .filter(|k| !new_mirror.contains_key(*k))
                .count();
            Ok(SyncReport {
                captured,
                dropped,
                mirror_written,
                ..Default::default()
            })
        }
        CredRead::Corrupt => unreachable!("corrupt live filtered above"),
    }
}

// ── MOC-261 一-4:逐条恢复状态机 ────────────────────────────────────────────
// 恢复状态文件(`mcp-recovery.json`):`server_key` → `{"ignored": bool}`。仅含 key + 标志,
// 无 token。语义见 [`CodexPaths::mcp_recovery_state`]。

fn entry_ignored(v: &Value) -> bool {
    v.get("ignored").and_then(Value::as_bool).unwrap_or(false)
}

fn read_recovery_state(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
}

fn write_recovery_state(path: &Path, map: &Map<String, Value>) -> Result<(), CodexError> {
    if map.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut s = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
    s.push('\n');
    write_atomic(path, &s)?;
    Ok(())
}

/// 若 live 整文件缺失(wipe)→ 把镜像里尚未跟踪的 server_key 加入恢复状态(待处理),并清掉
/// 镜像已不存在的过期项。**仅在 live 缺失时引入新恢复项**(live 存在时由 [`sync_mcp_credentials`]
/// 的 Parsed 分支收敛)。只写恢复状态文件,不碰 live / 镜像。返回最新状态。
fn ensure_recovering(paths: &CodexPaths) -> Map<String, Value> {
    let mut state = read_recovery_state(&paths.mcp_recovery_state);
    if !matches!(read_creds(&paths.mcp_credentials), CredRead::Missing) {
        return state;
    }
    let mirror = match read_creds(&paths.mcp_credentials_mirror) {
        CredRead::Parsed(m) => m,
        _ => Map::new(),
    };
    let mut changed = false;
    for k in mirror.keys() {
        if !state.contains_key(k) {
            state.insert(k.clone(), json!({ "ignored": false }));
            changed = true;
        }
    }
    let before = state.len();
    state.retain(|k, _| mirror.contains_key(k));
    if changed || state.len() != before {
        let _ = write_recovery_state(&paths.mcp_recovery_state, &state);
    }
    state
}

/// 一条待处理的 MCP 凭据恢复项(供前端逐行显示)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryItem {
    /// Codex 的 server_key(`server_name|hash`),行标识 + 显示用。
    pub key: String,
    /// 用户已点「忽略」:不再触发自动弹窗,但仍在列表里可手动处理。
    pub ignored: bool,
}

/// 列出当前「待处理」的恢复项 = 恢复状态里、镜像有、live 没有的 server_key(按 key 排序)。
/// 先 ensure(检测 wipe 引入新项),供状态端点 / 弹窗。
pub fn list_recovery(paths: &CodexPaths) -> Vec<RecoveryItem> {
    let state = ensure_recovering(paths);
    if state.is_empty() {
        return Vec::new();
    }
    let mirror = match read_creds(&paths.mcp_credentials_mirror) {
        CredRead::Parsed(m) => m,
        _ => Map::new(),
    };
    let live = match read_creds(&paths.mcp_credentials) {
        CredRead::Parsed(m) => m,
        _ => Map::new(),
    };
    let mut items: Vec<RecoveryItem> = state
        .iter()
        .filter(|(k, _)| mirror.contains_key(*k) && !live.contains_key(*k))
        .map(|(k, v)| RecoveryItem {
            key: k.clone(),
            ignored: entry_ignored(v),
        })
        .collect();
    items.sort_by(|a, b| a.key.cmp(&b.key));
    items
}

/// 自动弹窗触发数 = 待处理且未忽略的项数。供状态端点决定是否启动自动弹窗。
pub fn pending_recovery_count(paths: &CodexPaths) -> usize {
    list_recovery(paths).iter().filter(|i| !i.ignored).count()
}

/// 向后兼容别名:旧调用方(服务层弹窗判定)用它拿「需提示」条数 = 待处理未忽略数。
pub fn restore_available_count(paths: &CodexPaths) -> usize {
    pending_recovery_count(paths)
}

/// **选择性恢复**:把指定 server_key 从镜像写回 live(merge,**不覆盖** live 已有的 = 不动你已
/// 重新授权的),并把这些 key 从恢复状态清除(已处理)。返回真正写回 live 的条数。
pub fn restore_mcp_credentials_keys(
    paths: &CodexPaths,
    keys: &[String],
) -> Result<usize, CodexError> {
    let mirror = match read_creds(&paths.mcp_credentials_mirror) {
        CredRead::Parsed(m) => m,
        _ => Map::new(),
    };
    // live 损坏 → 不动(避免覆盖不可读但可能存在的凭据)。缺失 → 从空开始建。
    let mut live = match read_creds(&paths.mcp_credentials) {
        CredRead::Parsed(m) => m,
        CredRead::Missing => Map::new(),
        CredRead::Corrupt => return Ok(0),
    };
    let mut restored = 0;
    for k in keys {
        if let Some(v) = mirror.get(k) {
            if !live.contains_key(k) {
                live.insert(k.clone(), v.clone());
                restored += 1;
            }
        }
    }
    if restored > 0 {
        write_creds_atomic(&paths.mcp_credentials, &live)?;
    }
    // 处理过的 key 从恢复状态清除(无论是否真写回:已在 live = 已处理)。
    let mut state = read_recovery_state(&paths.mcp_recovery_state);
    let before = state.len();
    for k in keys {
        state.remove(k);
    }
    if state.len() != before {
        write_recovery_state(&paths.mcp_recovery_state, &state)?;
    }
    Ok(restored)
}

/// **选择性移除**:从镜像 + 恢复状态删除指定 server_key(用户「不要这些备份」)。**不动 live**。
/// 镜像清空则删文件。返回从镜像真正删除的条数。
pub fn remove_mcp_credentials_keys(
    paths: &CodexPaths,
    keys: &[String],
) -> Result<usize, CodexError> {
    let mut mirror = match read_creds(&paths.mcp_credentials_mirror) {
        CredRead::Parsed(m) => m,
        CredRead::Missing => Map::new(),
        CredRead::Corrupt => return Ok(0),
    };
    let mut removed = 0;
    for k in keys {
        if mirror.remove(k).is_some() {
            removed += 1;
        }
    }
    if removed > 0 {
        if mirror.is_empty() {
            match std::fs::remove_file(&paths.mcp_credentials_mirror) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        } else {
            write_creds_atomic(&paths.mcp_credentials_mirror, &mirror)?;
        }
    }
    let mut state = read_recovery_state(&paths.mcp_recovery_state);
    let before = state.len();
    for k in keys {
        state.remove(k);
    }
    if state.len() != before {
        write_recovery_state(&paths.mcp_recovery_state, &state)?;
    }
    Ok(removed)
}

/// **标记忽略**:恢复状态里把指定 server_key 设 `ignored=true`(留镜像 + 列表,不再触发自动
/// 弹窗,仍可手动处理)。只对已在恢复状态里的 key 生效。返回标记条数。
pub fn ignore_mcp_credentials_keys(
    paths: &CodexPaths,
    keys: &[String],
) -> Result<usize, CodexError> {
    let mut state = read_recovery_state(&paths.mcp_recovery_state);
    let mut n = 0;
    for k in keys {
        if state.contains_key(k) {
            state.insert(k.clone(), json!({ "ignored": true }));
            n += 1;
        }
    }
    if n > 0 {
        write_recovery_state(&paths.mcp_recovery_state, &state)?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(tag: &str) -> Value {
        json!({"access_token": tag, "refresh_token": format!("r-{tag}")})
    }

    fn write_json(path: &Path, v: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    fn read_map(path: &Path) -> Map<String, Value> {
        match serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()).unwrap() {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn missing_live_reports_restore_available_without_writing() {
        // live 整文件不在 + 镜像有内容 → 不自动写 live,只报告 restore_available(交上层弹确认)。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry("n"), "vercel|cd": entry("v")}),
        );
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep.restore_available, 2);
        assert!(!rep.mirror_written);
        assert!(!paths.mcp_credentials.exists(), "绝不自动恢复(不静默复活)");
    }

    #[test]
    fn missing_live_empty_mirror_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep, SyncReport::default());
    }

    #[test]
    fn captures_new_auth_into_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials, &json!({"vercel|cd": entry("v")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.mirror_written && rep.captured == 1 && rep.dropped == 0);
        assert!(read_map(&paths.mcp_credentials_mirror).contains_key("vercel|cd"));
    }

    #[test]
    fn logout_one_server_propagates_and_does_not_resurrect() {
        // chatgpt-codex-connector P2(第一轮):live 存在但少了一个 key(codex mcp logout),
        // 镜像有该 key。镜像必须丢弃它(传播删除),且**绝不**写回 live(不复活)。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry("n"), "vercel|cd": entry("v")}),
        );
        write_json(&paths.mcp_credentials, &json!({"vercel|cd": entry("v")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.mirror_written && rep.dropped == 1 && rep.captured == 0);
        let live = read_map(&paths.mcp_credentials);
        assert!(
            !live.contains_key("notion|ab"),
            "登出的 server 绝不复活回 live"
        );
        assert_eq!(
            read_map(&paths.mcp_credentials_mirror),
            live,
            "镜像精确跟随 live"
        );
    }

    #[test]
    fn live_present_takes_precedence_over_stale_mirror_value() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials_mirror, &json!({"a|1": entry("old")}));
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("fresh")}));
        sync_mcp_credentials(&paths).unwrap();
        assert_eq!(
            read_map(&paths.mcp_credentials_mirror)["a|1"]["access_token"],
            "fresh"
        );
        assert_eq!(
            read_map(&paths.mcp_credentials)["a|1"]["access_token"],
            "fresh",
            "live 不被改动"
        );
    }

    fn keys(items: &[RecoveryItem]) -> Vec<String> {
        items.iter().map(|i| i.key.clone()).collect()
    }

    #[test]
    fn list_recovery_lists_mirror_keys_when_live_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b")}),
        );
        let items = list_recovery(&paths);
        assert_eq!(keys(&items), vec!["a|1", "b|2"]);
        assert!(items.iter().all(|i| !i.ignored));
        assert_eq!(pending_recovery_count(&paths), 2);
    }

    #[test]
    fn restore_keys_writes_selected_clears_state_keeps_live() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b")}),
        );
        let _ = list_recovery(&paths); // 引入恢复态
        let n = restore_mcp_credentials_keys(&paths, &["a|1".into()]).unwrap();
        assert_eq!(n, 1);
        let live = read_map(&paths.mcp_credentials);
        assert!(live.contains_key("a|1") && !live.contains_key("b|2"));
        // a 已处理(出 recovery),b 仍待处理。
        assert_eq!(keys(&list_recovery(&paths)), vec!["b|2"]);
    }

    #[test]
    fn restore_keys_does_not_clobber_existing_live_value() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials_mirror, &json!({"a|1": entry("mir")}));
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("live")}));
        let n = restore_mcp_credentials_keys(&paths, &["a|1".into()]).unwrap();
        assert_eq!(n, 0, "live 已有该 key → 不覆盖");
        assert_eq!(
            read_map(&paths.mcp_credentials)["a|1"]["access_token"],
            "live"
        );
    }

    #[test]
    fn remove_keys_drops_from_mirror_and_state_deletes_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b")}),
        );
        let _ = list_recovery(&paths);
        assert_eq!(
            remove_mcp_credentials_keys(&paths, &["a|1".into(), "b|2".into()]).unwrap(),
            2
        );
        assert!(!paths.mcp_credentials_mirror.exists(), "镜像清空 → 删文件");
        assert!(!paths.mcp_recovery_state.exists(), "恢复状态清空 → 删文件");
        assert!(list_recovery(&paths).is_empty());
    }

    #[test]
    fn ignore_keys_marks_ignored_drops_pending_but_stays_listed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b")}),
        );
        let _ = list_recovery(&paths);
        assert_eq!(
            ignore_mcp_credentials_keys(&paths, &["a|1".into()]).unwrap(),
            1
        );
        // a 已忽略:不计入 pending(不自动弹窗),但仍在列表里(可手动处理),标 ignored。
        assert_eq!(pending_recovery_count(&paths), 1);
        let items = list_recovery(&paths);
        assert_eq!(keys(&items), vec!["a|1", "b|2"]);
        assert!(items.iter().find(|i| i.key == "a|1").unwrap().ignored);
    }

    #[test]
    fn partial_restore_keeps_unrestored_recoverable_across_sync() {
        // 关键安全保证:整文件 wipe → 全部进恢复态;只恢复 a 后,下次 sync(live 已部分存在)
        // **不**把未恢复的 b/c 当登出从镜像静默清掉,b/c 仍可恢复、继续可被列出。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b"), "c|3": entry("c")}),
        );
        assert_eq!(sync_mcp_credentials(&paths).unwrap().restore_available, 3);
        restore_mcp_credentials_keys(&paths, &["a|1".into()]).unwrap();
        // 模拟下一次启动 sync:live={a},镜像={a,b,c},恢复态={b,c}。
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep.dropped, 0, "未恢复项受保护,绝不被当登出清掉");
        let mirror = read_map(&paths.mcp_credentials_mirror);
        assert!(
            mirror.contains_key("b|2") && mirror.contains_key("c|3"),
            "b/c 备份仍在镜像"
        );
        assert_eq!(
            keys(&list_recovery(&paths)),
            vec!["b|2", "c|3"],
            "b/c 仍待恢复"
        );
    }

    #[test]
    fn genuine_logout_still_prunes_when_not_in_recovery() {
        // 无恢复态时(正常登出):live 少一个 key,镜像传播删除——保持既有行为不回退。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b")}),
        );
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("a")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep.dropped, 1);
        assert!(!read_map(&paths.mcp_credentials_mirror).contains_key("b|2"));
    }

    #[test]
    fn skips_on_corrupt_live_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        std::fs::create_dir_all(paths.mcp_credentials.parent().unwrap()).unwrap();
        std::fs::write(&paths.mcp_credentials, b"{ not valid json").unwrap();
        write_json(&paths.mcp_credentials_mirror, &json!({"x|9": entry("x")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.skipped.is_some() && !rep.mirror_written && rep.restore_available == 0);
        assert_eq!(
            std::fs::read_to_string(&paths.mcp_credentials).unwrap(),
            "{ not valid json",
            "损坏文件原样保留"
        );
    }

    #[test]
    fn skips_on_corrupt_mirror_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("a")}));
        std::fs::create_dir_all(paths.mcp_credentials_mirror.parent().unwrap()).unwrap();
        std::fs::write(&paths.mcp_credentials_mirror, b"garbage").unwrap();
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.skipped.is_some() && !rep.mirror_written);
    }

    #[test]
    fn noop_when_already_equal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        let same = json!({"a|1": entry("a")});
        write_json(&paths.mcp_credentials, &same);
        write_json(&paths.mcp_credentials_mirror, &same);
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(!rep.mirror_written);
    }

    #[cfg(unix)]
    #[test]
    fn written_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("a")}));
        sync_mcp_credentials(&paths).unwrap();
        let mode = std::fs::metadata(&paths.mcp_credentials_mirror)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn ensure_file_store_mode_writes_and_removes_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        std::fs::create_dir_all(paths.config_toml.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_toml, "model = \"x\"\n").unwrap();

        ensure_file_store_mode(&paths, true).unwrap();
        let after_on = std::fs::read_to_string(&paths.config_toml).unwrap();
        assert!(after_on.contains("mcp_oauth_credentials_store = \"file\""));
        assert!(after_on.contains("model = \"x\""), "其它 key 不动");

        ensure_file_store_mode(&paths, false).unwrap();
        let after_off = std::fs::read_to_string(&paths.config_toml).unwrap();
        assert!(
            !after_off.contains("mcp_oauth_credentials_store"),
            "关闭后删 key"
        );
        assert!(after_off.contains("model = \"x\""));
    }
}
