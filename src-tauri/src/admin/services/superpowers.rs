//! Superpowers 强约束插件挂载 —— 把内置 vendored 的 obra/superpowers Codex 插件
//! 在启用 Antigravity 提供商时装进 `~/.codex/plugins`,切走/关闭时卸载(MOC-277)。
//!
//! 设计要点:
//! - 内置树由 `include_dir!` 编译期嵌入(`vendor/superpowers`,pin SHA 见 `vendor/superpowers/VENDOR.md`)。
//! - 用受管 market 命名空间 [`MANAGED_MARKET`] 做归属隔离:卸载只认这个 market,**绝不**
//!   误删用户自装的 superpowers;反过来,检测到其它 market 的 superpowers 即"用户自有"。
//! - 安装/卸载本身复用 `codex_plugins`(staged→atomic rename→`set_enabled` / `uninstall`)。
//!
//! gate 接线(开关 + api_format==antigravity + 已装检测)在 apply 流程,见后续提交。

use include_dir::{include_dir, Dir};

use super::codex_plugins::{self, PluginEntry};

/// 编译期内置的 superpowers 插件目录树(`src-tauri/vendor/superpowers`)。
static SUPERPOWERS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/vendor/superpowers");

/// 受管 market 命名空间 —— 跟用户自装 superpowers 隔离的归属标记。
pub const MANAGED_MARKET: &str = "cas-antigravity";

/// 插件名(对齐 vendored `.codex-plugin/plugin.json` 的 name)。
pub const PLUGIN_NAME: &str = "superpowers";

/// 内置树里需要恢复可执行位的文件(`include_dir` 不保留 +x)。
///
/// 由 `executable_list_matches_vendored_tree` 测试对照真实 vendored 树自校验 —— 上游
/// 版本升级若增删可执行文件,该测试会失败,提示更新此列表。
const EXECUTABLE_FILES: &[&str] = &[
    "hooks/run-hook.cmd",
    "hooks/session-start",
    "hooks/session-start-codex",
    "skills/writing-skills/render-graphs.js",
    "skills/systematic-debugging/find-polluter.sh",
    "skills/subagent-driven-development/scripts/review-package",
    "skills/subagent-driven-development/scripts/sdd-workspace",
    "skills/subagent-driven-development/scripts/task-brief",
    "skills/brainstorming/scripts/stop-server.sh",
    "skills/brainstorming/scripts/start-server.sh",
];

/// 内置 superpowers 版本(读内置 `.codex-plugin/plugin.json` 的 `version`;落后上游由 CI 检测)。
pub fn vendored_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            SUPERPOWERS_DIR
                .get_file(".codex-plugin/plugin.json")
                .and_then(|f| f.contents_utf8())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v| {
                    v.get("version")
                        .and_then(|x| x.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "local".to_owned())
        })
        .as_str()
}

/// 受管插件 key:`superpowers@cas-antigravity`。
pub fn managed_key() -> String {
    format!("{PLUGIN_NAME}@{MANAGED_MARKET}")
}

/// 挂载内置 superpowers 到 Codex(幂等;atomic rename + enable)。
pub fn install() -> Result<PluginEntry, String> {
    codex_plugins::install_embedded(
        PLUGIN_NAME,
        MANAGED_MARKET,
        vendored_version(),
        &SUPERPOWERS_DIR,
        EXECUTABLE_FILES,
    )
}

/// 卸载我方挂载的 superpowers(只动受管 market,不碰用户自装)。
pub fn uninstall() -> Result<(), String> {
    codex_plugins::uninstall(&managed_key())
}

/// settings 里控制本功能的开关键(三态:未设 / true / false)。
pub const SETTING_KEY: &str = "antigravitySuperpowersEnabled";

/// 期望态:仅 antigravity provider 才考虑;显式开关优先,未设时默认开 —— 除非用户已自装
/// superpowers(避免与用户自有双装,见 [`user_has_foreign_superpowers`])。
fn desired(api_format_lower: &str, setting: Option<bool>) -> bool {
    if !matches!(api_format_lower, "antigravity_oauth" | "antigravity") {
        return false;
    }
    setting.unwrap_or_else(|| !user_has_foreign_superpowers())
}

/// 用户是否自装过 superpowers(任何**非**受管 market 下名为 superpowers 的 plugin)。
/// 命中即"用户自有",默认不挂我方(避免双装);只读扫 cache,不写盘。
pub fn user_has_foreign_superpowers() -> bool {
    codex_plugins::list_installed()
        .map(|list| {
            list.iter()
                .any(|e| e.name == PLUGIN_NAME && e.marketplace != MANAGED_MARKET)
        })
        .unwrap_or(false)
}

/// 我方受管 superpowers 当前安装的版本(未装 → None)。
fn managed_installed_version() -> Option<String> {
    codex_plugins::list_installed()
        .ok()?
        .into_iter()
        .find(|e| e.key == managed_key())
        .map(|e| e.version)
}

/// 我方受管 superpowers 是否已安装(供设置页 status 端点)。
pub fn is_managed_installed() -> bool {
    managed_installed_version().is_some()
}

/// 据 api_format + 开关 + 已装检测,收敛内置 superpowers 的装/卸态。
///
/// best-effort:任何失败只 log,不影响 provider apply 主流程。返回收敛后的期望态(供日志)。
/// 幂等:已是期望态则跳过;版本与内置不一致则先卸旧再装新(覆盖 app 升级后 vendored 版本变化)。
pub fn reconcile(api_format_lower: &str, setting: Option<bool>) -> bool {
    let want = desired(api_format_lower, setting);
    let result = if want {
        if managed_installed_version().as_deref() != Some(vendored_version()) {
            let _ = uninstall(); // 清旧版本(若有),best-effort
            install().map(|_| ())
        } else {
            Ok(()) // 已装且版本一致,跳过
        }
    } else if managed_installed_version().is_some() {
        uninstall()
    } else {
        Ok(()) // 未装且不需要
    };
    if let Err(e) = result {
        tracing::warn!(
            error_id = "SUPERPOWERS_RECONCILE",
            "superpowers reconcile (want={want}) failed: {e}"
        );
    }
    want
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tree_has_codex_manifest() {
        // include_dir 必须纳入 dotfile 目录 .codex-plugin,否则装出来 Codex 不认作插件。
        assert!(
            SUPERPOWERS_DIR
                .get_file(".codex-plugin/plugin.json")
                .is_some(),
            ".codex-plugin/plugin.json 未被 include_dir 嵌入"
        );
    }

    #[test]
    fn embedded_tree_has_bootstrap_and_hook() {
        // bootstrap 总闸 + Codex SessionStart hook —— 缺任一则 skill 不会自动触发。
        assert!(
            SUPERPOWERS_DIR
                .get_file("skills/using-superpowers/SKILL.md")
                .is_some(),
            "缺 using-superpowers 总闸"
        );
        assert!(
            SUPERPOWERS_DIR.get_file("hooks/hooks-codex.json").is_some(),
            "缺 hooks-codex.json"
        );
    }

    #[test]
    fn vendored_version_parses_real() {
        let v = vendored_version();
        assert_ne!(v, "local", "应从 plugin.json 读出真实版本而非兜底 local");
        assert!(
            v.split('.').next().and_then(|s| s.parse::<u32>().ok()).is_some(),
            "version 形态异常: {v}"
        );
    }

    #[test]
    fn managed_key_is_namespaced() {
        assert_eq!(managed_key(), "superpowers@cas-antigravity");
    }

    #[test]
    fn desired_only_for_antigravity() {
        // 非 antigravity provider 一律不挂(即便显式开)。
        assert!(!desired("openai_chat", Some(true)));
        assert!(!desired("gemini_native", Some(true)));
        assert!(!desired("openai_responses", None));
    }

    #[test]
    fn desired_respects_explicit_setting() {
        // 显式开关优先于默认/检测;两种 antigravity api_format 形态都认。
        assert!(desired("antigravity_oauth", Some(true)));
        assert!(desired("antigravity", Some(true)));
        assert!(!desired("antigravity_oauth", Some(false)));
    }

    /// EXECUTABLE_FILES 必须等于 vendored 树里真实带 +x 的文件集合 —— 防上游升级后漏更。
    #[cfg(unix)]
    #[test]
    fn executable_list_matches_vendored_tree() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else if let Ok(meta) = std::fs::metadata(&p) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        out.push(
                            p.strip_prefix(root)
                                .unwrap_or(&p)
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                }
            }
        }

        let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/superpowers");
        let mut actual = Vec::new();
        walk(&root, &root, &mut actual);
        actual.sort();
        let mut expected: Vec<String> = EXECUTABLE_FILES.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(
            actual, expected,
            "vendored 可执行文件集合与 EXECUTABLE_FILES 不一致(上游升级后请更新列表)"
        );
    }
}
