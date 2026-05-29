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

    match live {
        // 整个 live 文件不在 —— 可能是用户登出全部(Codex 删了文件),也可能是误删 / 换机,
        // 同步时无从区分 → **不自动写 live**,只报告可恢复条数,交上层弹确认。
        CredRead::Missing => Ok(SyncReport {
            restore_available: mirror_map.len(),
            ..Default::default()
        }),
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
        CredRead::Corrupt => unreachable!("corrupt live filtered above"),
    }
}

/// 用户在"凭据文件丢失,从备份恢复?"确认里点**恢复**后调用:把镜像写回 live。
/// 仅当镜像非空、且 live 仍缺失 / 空时才写(避免覆盖用户已重新授权出来的 live)。
/// 返回写回的条目数(0 = 没可恢复 / live 已有内容 / 损坏,未写)。
pub fn restore_mcp_credentials_from_mirror(paths: &CodexPaths) -> Result<usize, CodexError> {
    let mirror_map = match read_creds(&paths.mcp_credentials_mirror) {
        CredRead::Parsed(m) if !m.is_empty() => m,
        _ => return Ok(0),
    };
    // live 已有内容 / 损坏 → 不覆盖。
    match read_creds(&paths.mcp_credentials) {
        CredRead::Parsed(m) if !m.is_empty() => return Ok(0),
        CredRead::Corrupt => return Ok(0),
        _ => {}
    }
    let n = mirror_map.len();
    write_creds_atomic(&paths.mcp_credentials, &mirror_map)?;
    Ok(n)
}

/// 用户在恢复确认里点**忽略**后调用:删掉镜像,接受"凭据已不在"的现状,避免每次启动
/// 都重复弹确认。非破坏:live 不动;用户日后重新授权时 [`sync_mcp_credentials`] 会重新
/// 捕获生成镜像。
pub fn discard_mcp_mirror(paths: &CodexPaths) -> Result<(), CodexError> {
    match std::fs::remove_file(&paths.mcp_credentials_mirror) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// 只读检查:live 整文件缺失、且镜像非空 → 返回镜像可恢复条数;否则 0。**无副作用**。
/// 供前端 load 时轮询(状态端点),避免一次性 startup event 在 listener 注册前就 emit
/// 而丢失(chatgpt-codex-connector P2)。live 存在 / 损坏、镜像缺失 / 损坏 / 空都返回 0。
pub fn restore_available_count(paths: &CodexPaths) -> usize {
    // live 存在(含 `{}`)/ 损坏 → 不是"整文件丢失",无需提示恢复。
    if !matches!(read_creds(&paths.mcp_credentials), CredRead::Missing) {
        return 0;
    }
    match read_creds(&paths.mcp_credentials_mirror) {
        CredRead::Parsed(m) => m.len(),
        _ => 0,
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

    #[test]
    fn restore_writes_live_from_mirror_when_live_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"notion|ab": entry("n")}),
        );
        let n = restore_mcp_credentials_from_mirror(&paths).unwrap();
        assert_eq!(n, 1);
        assert!(read_map(&paths.mcp_credentials).contains_key("notion|ab"));
    }

    #[test]
    fn restore_does_not_clobber_present_live() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials_mirror, &json!({"a|1": entry("mir")}));
        write_json(&paths.mcp_credentials, &json!({"a|1": entry("live")}));
        let n = restore_mcp_credentials_from_mirror(&paths).unwrap();
        assert_eq!(n, 0, "live 已有内容 → 不覆盖");
        assert_eq!(
            read_map(&paths.mcp_credentials)["a|1"]["access_token"],
            "live"
        );
    }

    #[test]
    fn discard_removes_mirror_and_leaves_live() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        write_json(&paths.mcp_credentials_mirror, &json!({"a|1": entry("m")}));
        discard_mcp_mirror(&paths).unwrap();
        assert!(
            !paths.mcp_credentials_mirror.exists(),
            "镜像被删,停止再弹确认"
        );
        // 再删一次(已不存在)不报错
        discard_mcp_mirror(&paths).unwrap();
    }

    #[test]
    fn restore_available_count_only_when_live_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(dir.path());
        assert_eq!(restore_available_count(&paths), 0, "都不存在 → 0");
        write_json(
            &paths.mcp_credentials_mirror,
            &json!({"a|1": entry("a"), "b|2": entry("b")}),
        );
        assert_eq!(
            restore_available_count(&paths),
            2,
            "live 缺失 + 镜像 2 条 → 2"
        );
        write_json(&paths.mcp_credentials, &json!({}));
        assert_eq!(restore_available_count(&paths), 0, "live 存在(含空)→ 0");
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
