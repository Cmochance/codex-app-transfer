//! MOC-62: MCP 授权"可移植保险箱"。
//!
//! Codex 默认把 MCP OAuth 凭据存 OS 钥匙串(按 `server_name+url` 索引,与 ChatGPT
//! 账号 / `auth.json` 无关)。本模块在用户开启 `mcpCredentialsPortableStore` 时:
//!
//! 1. [`ensure_file_store_mode`] 往 `~/.codex/config.toml` 写根级
//!    `mcp_oauth_credentials_store = "file"`,让 Codex 改用 file 存储
//!    (`~/.codex/.credentials.json`,单 JSON blob,`server_key` → entry),
//!    使凭据成为一个可被 transfer 备份 / 恢复的普通文件。
//! 2. [`sync_mcp_credentials`] 把该文件与 transfer 镜像
//!    (`~/.codex-app-transfer/mcp-credentials.json`,在 `~/.codex` 之外)并集合并:
//!    实时缺失的从镜像恢复、镜像缺失的从实时捕获,同 key 取 `expires_at` 较新者。
//!
//! 边界:这是"防擦除 / 可迁移",**不**解决 OAuth 过期(过期 token 恢复回去仍过期,
//! 需重新授权)。安全权衡:file 模式 token 明文落盘(0o600,Codex 官方支持的模式)。

use std::path::Path;

use serde_json::{Map, Value};

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

/// 一次镜像同步的结果(用于日志 / 测试断言)。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// 实时文件新增的 server 条目数(被外部擦除后从镜像救回的)。
    pub restored: usize,
    /// 镜像新增的 server 条目数(实时文件里新授权 / 刷新捕获进来的)。
    pub captured: usize,
    pub live_written: bool,
    pub mirror_written: bool,
    /// 非 `None` 表示本次整体跳过(如某侧文件损坏 / 读不动),内容为原因。
    pub skipped: Option<String>,
}

/// 一侧凭据文件的读取结果。区分"缺失"(可当空 map 参与合并)与"损坏"(绝不当空,
/// 否则会用空覆盖掉可能可恢复的数据)。
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

/// 取条目的 `expires_at`(epoch millis,Codex 存 `u64`);缺失 / null / 非整数视作 0
/// (= "无已知到期",最旧)。Codex 在 token 响应不含 `expires_in` 时确实会写 `None`。
fn entry_expiry(entry: &Value) -> u64 {
    entry.get("expires_at").and_then(Value::as_u64).unwrap_or(0)
}

/// 并集合并:mirror 打底,live 覆盖。结果 ⊇ live 且 ⊇ mirror —— 只增不删,任何一侧的
/// server 都不会丢。同 key 的取舍:**默认取 live**(live 是 Codex 刚写的现状 / source
/// of truth);只有当 live 自己有可比的真实到期、且 mirror 严格更新时才取 mirror。
/// 这样可避免"live 是刚重新授权但 token 无 `expires_at`(读作 0)、mirror 是旧 token 带
/// 具体到期"时错误地用旧 token 覆盖掉新授权(Codex 对无 `expires_in` 的 token 会写 None)。
fn merge(live: &Map<String, Value>, mirror: &Map<String, Value>) -> Map<String, Value> {
    let mut out = mirror.clone();
    for (k, v) in live {
        let take_live = match out.get(k) {
            // lv==0:live 无已知到期 → 它是 Codex 现状,直接取 live。
            // 否则按到期时间比,live 不更旧(含并列)就取 live。
            Some(existing) => {
                let lv = entry_expiry(v);
                lv == 0 || lv >= entry_expiry(existing)
            }
            None => true,
        };
        if take_live {
            out.insert(k.clone(), v.clone());
        }
    }
    out
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

/// 把实时 `~/.codex/.credentials.json` 与 transfer 镜像并集合并并双向落盘:
/// 实时缺失的从镜像恢复、镜像缺失的从实时捕获。任一侧损坏 → 整体跳过(不写),
/// 保留两个文件原样供人工排查。只在合并结果与现状不同的那一侧才写。
pub fn sync_mcp_credentials(paths: &CodexPaths) -> Result<SyncReport, CodexError> {
    let live = read_creds(&paths.mcp_credentials);
    let mirror = read_creds(&paths.mcp_credentials_mirror);

    if matches!(live, CredRead::Corrupt) {
        return Ok(SyncReport {
            skipped: Some("live credentials file unreadable/corrupt".into()),
            ..Default::default()
        });
    }
    if matches!(mirror, CredRead::Corrupt) {
        return Ok(SyncReport {
            skipped: Some("mirror credentials file unreadable/corrupt".into()),
            ..Default::default()
        });
    }

    let live_map = match live {
        CredRead::Parsed(map) => map,
        _ => Map::new(),
    };
    let mirror_map = match mirror {
        CredRead::Parsed(map) => map,
        _ => Map::new(),
    };

    // 两侧都空 → 无凭据可同步。
    if live_map.is_empty() && mirror_map.is_empty() {
        return Ok(SyncReport::default());
    }

    let merged = merge(&live_map, &mirror_map);
    let restored = merged.keys().filter(|k| !live_map.contains_key(*k)).count();
    let captured = merged
        .keys()
        .filter(|k| !mirror_map.contains_key(*k))
        .count();

    let mut report = SyncReport {
        restored,
        captured,
        ..Default::default()
    };
    // 与现状不同才写(省 IO + 不必要的 mtime 抖动)。
    if merged != live_map {
        write_creds_atomic(&paths.mcp_credentials, &merged)?;
        report.live_written = true;
    }
    if merged != mirror_map {
        write_creds_atomic(&paths.mcp_credentials_mirror, &merged)?;
        report.mirror_written = true;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(expires_at: i64) -> Value {
        json!({"access_token": format!("tok-{expires_at}"), "expires_at": expires_at})
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
    fn merge_unions_and_prefers_fresher() {
        let mut live = Map::new();
        live.insert("a|1".into(), entry(200)); // shared, live fresher
        live.insert("c|3".into(), entry(50)); // live-only
        let mut mirror = Map::new();
        mirror.insert("a|1".into(), entry(100)); // older
        mirror.insert("b|2".into(), entry(70)); // mirror-only
        let out = merge(&live, &mirror);
        assert_eq!(out.len(), 3, "union of a,b,c");
        assert_eq!(
            entry_expiry(&out["a|1"]),
            200,
            "shared key takes fresher (live)"
        );
        assert!(out.contains_key("b|2"), "mirror-only preserved (restore)");
        assert!(out.contains_key("c|3"), "live-only preserved (capture)");
    }

    #[test]
    fn merge_prefers_live_when_live_expiry_unknown() {
        // live 是刚重新授权但 token 无 expires_at(entry_expiry 读作 0),mirror 是旧
        // token 带具体到期 —— 不能用旧 mirror 覆盖新授权(IMPORTANT 复核回归)。
        let mut live = Map::new();
        live.insert("a|1".into(), json!({"access_token": "fresh-no-expiry"}));
        let mut mirror = Map::new();
        mirror.insert(
            "a|1".into(),
            json!({"access_token": "old", "expires_at": 1_700_000_000_000u64}),
        );
        let out = merge(&live, &mirror);
        assert_eq!(
            out["a|1"]["access_token"], "fresh-no-expiry",
            "无到期的 live(新授权)不应被带到期的旧 mirror 覆盖"
        );
    }

    #[test]
    fn merge_tie_prefers_live() {
        let mut live = Map::new();
        live.insert(
            "a|1".into(),
            json!({"access_token": "live", "expires_at": 100}),
        );
        let mut mirror = Map::new();
        mirror.insert(
            "a|1".into(),
            json!({"access_token": "mirror", "expires_at": 100}),
        );
        let out = merge(&live, &mirror);
        assert_eq!(out["a|1"]["access_token"], "live");
    }

    #[test]
    fn sync_restores_when_live_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        // mirror 有内容,live 不存在(模拟 codex switch 的 rsync --delete 擦掉 ~/.codex)
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry(500)}),
        );
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep.skipped, None);
        assert!(rep.live_written, "live should be restored from mirror");
        assert_eq!(rep.restored, 1);
        assert!(paths.mcp_credentials.exists());
        assert!(read_map(&paths.mcp_credentials).contains_key("notion|ab"));
    }

    #[test]
    fn sync_captures_when_mirror_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials, &json!({"vercel|cd": entry(600)}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.mirror_written, "mirror should capture from live");
        assert_eq!(rep.captured, 1);
        assert!(read_map(&paths.mcp_credentials_mirror).contains_key("vercel|cd"));
    }

    #[test]
    fn sync_merges_divergent_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials,
            &json!({"a|1": entry(100), "live|2": entry(10)}),
        );
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry(300), "mir|3": entry(20)}),
        );
        let rep = sync_mcp_credentials(&paths).unwrap();
        let live = read_map(&paths.mcp_credentials);
        let mirror = read_map(&paths.mcp_credentials_mirror);
        // 两侧最终一致 = 三个 key 的并集,a|1 取 mirror 的较新值(300)
        assert_eq!(live, mirror);
        assert_eq!(live.len(), 3);
        assert_eq!(entry_expiry(&live["a|1"]), 300);
        assert!(rep.live_written && rep.mirror_written);
    }

    #[test]
    fn sync_skips_on_corrupt_live_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        std::fs::create_dir_all(paths.mcp_credentials.parent().unwrap()).unwrap();
        std::fs::write(&paths.mcp_credentials, b"{ not valid json").unwrap();
        write_json(&paths.mcp_credentials_mirror, &json!({"x|9": entry(1)}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.skipped.is_some(), "corrupt live → skip");
        assert!(!rep.live_written && !rep.mirror_written);
        // 损坏文件原样保留,绝不覆盖
        assert_eq!(
            std::fs::read_to_string(&paths.mcp_credentials).unwrap(),
            "{ not valid json"
        );
    }

    #[test]
    fn sync_noop_when_already_equal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        let same = json!({"a|1": entry(100)});
        write_json(&paths.mcp_credentials, &same);
        write_json(&paths.mcp_credentials_mirror, &same);
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(
            !rep.live_written && !rep.mirror_written,
            "no write when equal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn written_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials, &json!({"a|1": entry(1)}));
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
