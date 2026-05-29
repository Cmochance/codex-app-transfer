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
//!    - live 文件**整个不在**(灾难性丢失)→ 从镜像**恢复**回 live;
//!    - live 文件**存在** → live 是 Codex 当前权威状态,镜像精确**跟随** live
//!      (捕获新授权 + 传播用户的 logout / 撤销删除),**绝不**把 live 没有的 key 写回
//!      live —— 否则会"复活"用户已 `codex mcp logout` / 撤销的凭据。
//!
//! 镜像是"整文件灾难性丢失"的备份,不是逐 key 的并集:单个 key 在 live 缺失 = 用户删了
//! 它(尊重删除),只有整文件没了才视作意外丢失去恢复。
//!
//! 边界:这是"防整文件擦除 / 可迁移",**不**解决 OAuth 过期(过期 token 恢复回去仍过期,
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
    /// live 整文件丢失后,从镜像恢复回 live 的条目数。
    pub restored: usize,
    /// 从 live 捕获进镜像的新增条目数(新授权)。
    pub captured: usize,
    /// 镜像里被丢弃的条目数 —— 对应用户在 live 侧 logout / 撤销的 server(传播删除)。
    pub dropped: usize,
    pub live_written: bool,
    pub mirror_written: bool,
    /// 非 `None` 表示本次整体跳过(如某侧文件损坏 / 读不动),内容为原因。
    pub skipped: Option<String>,
}

/// 一侧凭据文件的读取结果。三态严格区分:
/// - `Missing`:文件不存在 / 空 —— live 侧视作"整文件丢失"(可恢复);
/// - `Parsed`:正常 JSON object(含 `{}`,代表用户登出了所有 server);
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
/// live 整文件丢失 → 从镜像恢复;live 存在 → 镜像精确跟随 live(尊重用户删除,不复活)。
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

    match live {
        // 整个 live 文件不在 → 灾难性丢失(rsync --delete / 误删 / 换机)→ 从镜像恢复。
        // 这是镜像唯一会写回 live 的场景。
        CredRead::Missing => {
            if mirror_map.is_empty() {
                return Ok(SyncReport::default());
            }
            write_creds_atomic(&paths.mcp_credentials, &mirror_map)?;
            Ok(SyncReport {
                restored: mirror_map.len(),
                live_written: true,
                ..Default::default()
            })
        }
        // live 存在 → 它是 Codex 当前权威状态(用户 logout / 撤销会从 live 删掉对应 key)。
        // 镜像精确跟随 live:捕获新授权 + 传播删除,**绝不**把 live 没有的 key 写回 live。
        CredRead::Parsed(live_map) => {
            if live_map == mirror_map {
                return Ok(SyncReport::default());
            }
            let captured = live_map
                .keys()
                .filter(|k| !mirror_map.contains_key(*k))
                .count();
            let dropped = mirror_map
                .keys()
                .filter(|k| !live_map.contains_key(*k))
                .count();
            write_creds_atomic(&paths.mcp_credentials_mirror, &live_map)?;
            Ok(SyncReport {
                captured,
                dropped,
                mirror_written: true,
                ..Default::default()
            })
        }
        // 上面已 early-return,这里不可达。
        CredRead::Corrupt => unreachable!("corrupt live filtered above"),
    }
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
    fn restores_when_live_file_missing() {
        // live 整文件不在(模拟 codex switch 的 rsync --delete 擦掉 ~/.codex)→ 从镜像恢复。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry("n"), "vercel|cd": entry("v")}),
        );
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep.skipped, None);
        assert!(rep.live_written, "live restored from mirror");
        assert_eq!(rep.restored, 2);
        let live = read_map(&paths.mcp_credentials);
        assert!(live.contains_key("notion|ab") && live.contains_key("vercel|cd"));
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
        // chatgpt-codex-connector P2:live 存在但少了一个 key(用户 codex mcp logout),
        // 镜像有该 key。镜像必须丢弃它(传播删除),且**绝不**把它写回 live(不复活)。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        // 之前两个都授权过(镜像)
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry("n"), "vercel|cd": entry("v")}),
        );
        // 用户登出了 notion,live 只剩 vercel
        write_json(&paths.mcp_credentials, &json!({"vercel|cd": entry("v")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(
            rep.mirror_written,
            "mirror updated to drop the logged-out key"
        );
        assert_eq!(rep.dropped, 1, "notion 被传播删除");
        assert_eq!(rep.captured, 0);
        // live 不被改动 —— notion 不应被复活
        let live = read_map(&paths.mcp_credentials);
        assert!(
            !live.contains_key("notion|ab"),
            "登出的 server 绝不复活回 live"
        );
        assert!(live.contains_key("vercel|cd"));
        // 镜像现在精确等于 live
        assert_eq!(read_map(&paths.mcp_credentials_mirror), live);
    }

    #[test]
    fn logout_all_clears_mirror() {
        // 用户登出全部 → Codex 写 `{}`。镜像应清空(传播),不复活任何旧凭据。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry("n")}),
        );
        write_json(&paths.mcp_credentials, &json!({}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.mirror_written && rep.dropped == 1);
        assert!(read_map(&paths.mcp_credentials_mirror).is_empty());
        assert!(read_map(&paths.mcp_credentials).is_empty(), "live 保持空");
    }

    #[test]
    fn live_present_takes_precedence_over_stale_mirror_value() {
        // 共享 key 上 live 与 mirror 值不同:live 是 Codex 现状,镜像跟随 live(取 live 值)。
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials_mirror, &json!({"a|1": entry("old")}));
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("fresh")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.mirror_written && rep.captured == 0 && rep.dropped == 0);
        assert_eq!(
            read_map(&paths.mcp_credentials_mirror)["a|1"]["access_token"],
            "fresh"
        );
        // live 不动
        assert_eq!(
            read_map(&paths.mcp_credentials)["a|1"]["access_token"],
            "fresh"
        );
    }

    #[test]
    fn skips_on_corrupt_live_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        std::fs::create_dir_all(paths.mcp_credentials.parent().unwrap()).unwrap();
        std::fs::write(&paths.mcp_credentials, b"{ not valid json").unwrap();
        write_json(&paths.mcp_credentials_mirror, &json!({"x|9": entry("x")}));
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(rep.skipped.is_some(), "corrupt live → skip");
        assert!(!rep.live_written && !rep.mirror_written);
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
        assert!(rep.skipped.is_some() && !rep.live_written && !rep.mirror_written);
    }

    #[test]
    fn noop_when_already_equal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        let same = json!({"a|1": entry("a")});
        write_json(&paths.mcp_credentials, &same);
        write_json(&paths.mcp_credentials_mirror, &same);
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert!(!rep.live_written && !rep.mirror_written);
    }

    #[test]
    fn noop_when_both_absent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        let rep = sync_mcp_credentials(&paths).unwrap();
        assert_eq!(rep, SyncReport::default());
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
