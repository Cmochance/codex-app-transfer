//! 真实 ChatGPT 账号检测(MOC-104)。
//!
//! 「真实账号 plugin 模式」的基础:判断本机是否已有可用的真实 ChatGPT 登录态
//! (`auth.json` 里 `auth_mode == "chatgpt"` 且 tokens 齐全)。当前 plugins 解锁
//! 靠 CDP 注入伪造 `setAuthMethod('chatgpt')`,没有真实 userID → Codex 启动后要
//! 重新初始化登录态(~5.8s)。真实账号模式用真 `auth.json` 取代伪造,避开代价。
//!
//! 本模块**纯只读**:只 `read` `~/.codex/auth.json` 与 transfer 快照备份,
//! 不写盘、不 spawn 子进程,给上层(检测 UI / 是否需要登录 / 是否需刷新)做判断。
//! 登录获取(spawn `codex login`)、token 刷新、强制启用等写操作在后续增量里加,
//! 各自独立,不混进检测路径。
//!
//! 检测来源(用户要求:只扫官方 `.codex/auth.json` + transfer 备份,不依赖
//! 任何特殊文件夹结构):
//! 1. 官方 `~/.codex/auth.json` —— Codex 自己(或 `codex login`)写的活动凭据。
//! 2. transfer 快照备份 `~/.codex-app-transfer/codex-snapshots/active/<session>/auth.json`
//!    —— transfer apply 前会整文件备份原 `auth.json`(见 codex_integration snapshot)。
//!    用户开 transfer 后官方 `auth.json` 可能被 apply 改成 apikey 模式,原本的
//!    chatgpt 登录态此时仍保留在快照里,可据此提示"备份里有真实账号可恢复"。

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use codex_app_transfer_codex_integration::{read_auth, CodexPaths};

/// 检测到的真实 chatgpt 凭据来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSource {
    /// 官方 `~/.codex/auth.json`(活动凭据)。
    Official,
    /// transfer 快照备份(官方被改成 apikey 后,原 chatgpt 态留在这里)。
    Backup,
    /// 哪里都没找到可用的真实 chatgpt 登录态。
    None,
}

/// 真实 ChatGPT 账号检测结果(只读快照)。
#[derive(Debug, Clone, Serialize)]
pub struct RealAccountStatus {
    /// 是否检测到**可用**的真实 chatgpt 登录态(`auth_mode==chatgpt` + access/refresh token 齐)。
    pub logged_in: bool,
    /// 活动 `auth.json` 的 `auth_mode`(`chatgpt` / `apikey` / 缺失=None)。
    /// 注意:这是**官方活动文件**的模式,即便可用凭据是从 backup 检测到的也反映活动态,
    /// 便于前端区分"活动就是 chatgpt" vs "活动是 apikey、但备份里有 chatgpt"。
    pub active_auth_mode: Option<String>,
    /// chatgpt `account_id`(从被采纳的来源里取,可能缺失)。
    pub account_id: Option<String>,
    /// `logged_in=true` 时,可用凭据来自哪里。
    pub source: AuthSource,
}

impl RealAccountStatus {
    fn none(active_auth_mode: Option<String>) -> Self {
        Self {
            logged_in: false,
            active_auth_mode,
            account_id: None,
            source: AuthSource::None,
        }
    }
}

/// 从一个 `auth.json` Value 判断是否是**可用**的 chatgpt 登录态。
/// 可用 = `auth_mode=="chatgpt"` 且 `tokens.{access_token,refresh_token}` 均非空。
/// 返回 `account_id`(可能为 None)。
fn parse_chatgpt_auth(v: &Value) -> Option<ChatgptAuth> {
    if v.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return None;
    }
    let tokens = v.get("tokens").and_then(Value::as_object)?;
    let nonempty = |key: &str| {
        tokens
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    // refresh_token 是刷新续期的前提;access_token 是当下能用的前提。两者缺一
    // 则视作不可用(残缺/登出中),不报 logged_in,避免误导上层去"用"它。
    if !nonempty("access_token") || !nonempty("refresh_token") {
        return None;
    }
    Some(ChatgptAuth {
        account_id: tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

struct ChatgptAuth {
    account_id: Option<String>,
}

/// 扫 transfer 快照备份目录,找第一个 `auth_mode==chatgpt` 且可用的 `auth.json`。
/// 备份布局:`<active_snapshots_dir>/<session>/auth.json`(见 codex_integration
/// snapshot)。只读;任何 IO 错误都当"该项没有",不上抛(检测不该因坏目录失败)。
fn detect_backup_chatgpt(active_snapshots_dir: &Path) -> Option<ChatgptAuth> {
    let entries = std::fs::read_dir(active_snapshots_dir).ok()?;
    for entry in entries.flatten() {
        let auth_path = entry.path().join("auth.json");
        if !auth_path.is_file() {
            continue;
        }
        if let Ok(v) = read_auth(&auth_path) {
            if let Some(found) = parse_chatgpt_auth(&v) {
                return Some(found);
            }
        }
    }
    None
}

/// 检测真实 chatgpt 账号:先看官方活动 `auth.json`,可用则采纳;否则看 transfer
/// 备份(覆盖"开 transfer 后官方被 apply 改成 apikey,真实 chatgpt 态留在备份"
/// 的情形)。纯只读,绝不写盘 / spawn。
pub fn detect() -> RealAccountStatus {
    let Ok(paths) = CodexPaths::from_home_env() else {
        // 连 home 都解析不到 —— 当作"没有",不 panic。
        return RealAccountStatus::none(None);
    };

    // 1) 官方活动 auth.json。读不到(不存在/坏)= 活动模式未知,继续看备份。
    let active = read_auth(&paths.auth_json).ok();
    let active_auth_mode = active
        .as_ref()
        .and_then(|v| v.get("auth_mode"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    if let Some(found) = active.as_ref().and_then(parse_chatgpt_auth) {
        return RealAccountStatus {
            logged_in: true,
            active_auth_mode,
            account_id: found.account_id,
            source: AuthSource::Official,
        };
    }

    // 2) transfer 快照备份。
    if let Some(found) = detect_backup_chatgpt(&paths.active_snapshots_dir) {
        return RealAccountStatus {
            logged_in: true,
            active_auth_mode,
            account_id: found.account_id,
            source: AuthSource::Backup,
        };
    }

    RealAccountStatus::none(active_auth_mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chatgpt_auth() -> Value {
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "acc_xxx",
                "refresh_token": "ref_xxx",
                "id_token": "id_xxx",
                "account_id": "acct_123"
            },
            "last_refresh": "2026-05-31T00:00:00Z"
        })
    }

    #[test]
    fn parses_valid_chatgpt_auth() {
        let parsed = parse_chatgpt_auth(&chatgpt_auth()).expect("应识别为可用 chatgpt");
        assert_eq!(parsed.account_id.as_deref(), Some("acct_123"));
    }

    #[test]
    fn apikey_mode_is_not_chatgpt() {
        let v = json!({ "auth_mode": "apikey", "OPENAI_API_KEY": "cas_x" });
        assert!(parse_chatgpt_auth(&v).is_none());
    }

    #[test]
    fn chatgpt_missing_refresh_token_is_unusable() {
        let v = json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "acc_xxx" }
        });
        assert!(
            parse_chatgpt_auth(&v).is_none(),
            "缺 refresh_token 不能续期,视作不可用"
        );
    }

    #[test]
    fn chatgpt_empty_token_is_unusable() {
        let v = json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "  ", "refresh_token": "ref_xxx" }
        });
        assert!(
            parse_chatgpt_auth(&v).is_none(),
            "空白 access_token 视作不可用"
        );
    }

    #[test]
    fn empty_object_is_not_chatgpt() {
        assert!(parse_chatgpt_auth(&json!({})).is_none());
    }

    #[test]
    fn detect_backup_finds_chatgpt_among_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path();
        // session a: apikey(不算)
        let a = active.join("session-a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(
            a.join("auth.json"),
            serde_json::to_string(&json!({"auth_mode": "apikey"})).unwrap(),
        )
        .unwrap();
        // session b: chatgpt(应命中)
        let b = active.join("session-b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            b.join("auth.json"),
            serde_json::to_string(&chatgpt_auth()).unwrap(),
        )
        .unwrap();

        let found = detect_backup_chatgpt(active).expect("备份里应找到 chatgpt");
        assert_eq!(found.account_id.as_deref(), Some("acct_123"));
    }

    #[test]
    fn detect_backup_missing_dir_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(detect_backup_chatgpt(&missing).is_none());
    }
}
