//! Codex 原配置完整性自检(#268).
//!
//! 扫描 `~/.codex/config.toml` 跟 codex-snapshots 下的 active / recovery 快照,
//! 检测是否含 transfer apply 残留字段并提供针对性清除。
//!
//! 设计目标:
//! - **高精度签名**:只用 `model_catalog_json` 指向 app_home 与 `openai_base_url`
//!   指向 transfer proxy(`http://127.0.0.1:<port>`)这两个值绝对来自 transfer
//!   的特征字段判定污染,避免误清用户合法手写的 `sandbox_mode` 等 key。
//! - **关联 strip**:
//!   - 命中 `model_catalog_json` → 删该键
//!   - 命中 `openai_base_url` → 该键加 `model_context_window`/`sandbox_mode`/
//!     `approval_policy` 一起删(transfer apply 套餐固定四件套,见
//!     [`crate::apply::MANAGED_TOML_KEYS`])
//! - **快照 != live**:active snapshot 存在时 live 的 transfer 字段属于当前
//!   apply 生效状态,不算污染;但任何 snapshot 自己带 transfer 字段都算
//!   污染 — 因为 snapshot 的语义是"apply 之前的用户原始配置"。

use crate::paths::CodexPaths;
use crate::snapshot::has_snapshot;
use crate::toml_sync::sync_root_value;
use crate::CodexError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 命中签名后 strip 的所有 root 级 keys。`openai_base_url` 命中触发整组。
const BASE_URL_BUNDLE: &[&str] = &[
    "openai_base_url",
    "model_context_window",
    "sandbox_mode",
    "approval_policy",
];

/// 命中 `model_catalog_json` 时单独 strip。
const CATALOG_KEY: &str = "model_catalog_json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PollutionSourceKind {
    /// `~/.codex/config.toml`(当前 live 配置)
    LiveConfig,
    /// `<app_home>/codex-snapshots/active/<id>/config.toml`
    ActiveSnapshot,
    /// `<app_home>/codex-snapshots/recovery/<id>/config.toml`
    RecoverySnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MatchedSignature {
    /// `model_catalog_json` 值是 `<app_home>/config.json`(100% transfer 写过)
    #[serde(rename_all = "camelCase")]
    ModelCatalogJsonAppHome { value: String },
    /// `openai_base_url` 值是 `http://127.0.0.1:<known_proxy_port>`
    #[serde(rename_all = "camelCase")]
    OpenaiBaseUrlTransferProxy { value: String, proxy_port: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollutedFile {
    pub path: PathBuf,
    pub kind: PollutionSourceKind,
    pub matched_signatures: Vec<MatchedSignature>,
    /// 即将被 strip 的 root key 列表(已 dedupe + 排序)
    pub fields_to_strip: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResidualScanReport {
    pub polluted: Vec<PollutedFile>,
    /// 当前 transfer 是否处于 apply 生效状态(active snapshot 存在)。
    /// 用来在 UI 解释为什么 live config 可能含 transfer 字段:
    /// - `true` + live 干净 → 上报为干净
    /// - `true` + live 含字段 → 当前 apply 生效,**不算污染**(scan 已过滤)
    /// - `false` + live 含字段 → 残留污染(目标 case)
    pub transfer_currently_applied: bool,
}

impl ResidualScanReport {
    pub fn is_clean(&self) -> bool {
        self.polluted.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairedFile {
    pub path: PathBuf,
    pub kind: PollutionSourceKind,
    pub stripped_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RepairReport {
    pub repaired: Vec<RepairedFile>,
    pub dry_run: bool,
}

/// 扫描所有目标文件,返回污染清单。
///
/// - `proxy_ports`:`openai_base_url` 值要匹配的 transfer proxy 端口列表。
///   推荐 caller 传 `[settings.proxyPort, 18080]`(当前配置 + 历史默认),
///   覆盖端口被改过后老 snapshot 仍能识别的场景。
pub fn scan_residual_pollution(
    paths: &CodexPaths,
    proxy_ports: &[u16],
) -> Result<ResidualScanReport, CodexError> {
    let transfer_applied = has_snapshot(paths);
    let mut polluted = Vec::new();
    let app_config_json = &paths.model_catalog_json;

    if !transfer_applied {
        if let Some(file) = scan_one_file(
            &paths.config_toml,
            PollutionSourceKind::LiveConfig,
            app_config_json,
            proxy_ports,
        )? {
            polluted.push(file);
        }
    }

    for cfg in iter_snapshot_config_files(&paths.active_snapshots_dir) {
        if let Some(file) = scan_one_file(
            &cfg,
            PollutionSourceKind::ActiveSnapshot,
            app_config_json,
            proxy_ports,
        )? {
            polluted.push(file);
        }
    }
    for cfg in iter_snapshot_config_files(&paths.recovery_snapshots_dir) {
        if let Some(file) = scan_one_file(
            &cfg,
            PollutionSourceKind::RecoverySnapshot,
            app_config_json,
            proxy_ports,
        )? {
            polluted.push(file);
        }
    }

    Ok(ResidualScanReport {
        polluted,
        transfer_currently_applied: transfer_applied,
    })
}

/// 按 [`ResidualScanReport`] 计划对每个文件执行 strip。
///
/// `dry_run=true` 时不写盘,仅返回会被 strip 的字段列表,用于 UI 预览。
pub fn repair_residual_pollution(
    report: &ResidualScanReport,
    dry_run: bool,
) -> Result<RepairReport, CodexError> {
    let mut repaired = Vec::with_capacity(report.polluted.len());
    for file in &report.polluted {
        let mut stripped = Vec::with_capacity(file.fields_to_strip.len());
        for key in &file.fields_to_strip {
            if !dry_run {
                sync_root_value(&file.path, key, None)?;
            }
            stripped.push(key.clone());
        }
        repaired.push(RepairedFile {
            path: file.path.clone(),
            kind: file.kind,
            stripped_keys: stripped,
        });
    }
    Ok(RepairReport { repaired, dry_run })
}

fn iter_snapshot_config_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let cfg = entry.path().join("config.toml");
        if cfg.is_file() {
            out.push(cfg);
        }
    }
    out.sort();
    out
}

fn scan_one_file(
    path: &Path,
    kind: PollutionSourceKind,
    app_config_json: &Path,
    proxy_ports: &[u16],
) -> Result<Option<PollutedFile>, CodexError> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let matched = detect_signatures_in_text(&content, app_config_json, proxy_ports);
    if matched.is_empty() {
        return Ok(None);
    }
    let fields_to_strip = compute_fields_to_strip(&matched);
    Ok(Some(PollutedFile {
        path: path.to_path_buf(),
        kind,
        matched_signatures: matched,
        fields_to_strip,
    }))
}

/// 给定一段 TOML 文本,返回应被 strip 的 root key 列表(已 dedupe + 排序)。
///
/// 用途场景:
/// - 残留扫描 UI(#268)对每个文件计算 strip 计划
/// - `apply::restore_from_snapshot_values`(#270)在按快照字面量回写前用
///   本函数计算"snapshot 自带的污染字段集合",对这些字段不回写(写 None
///   即 strip),防止循环固化
pub fn signature_fields_to_strip(
    content: &str,
    app_config_json: &Path,
    proxy_ports: &[u16],
) -> Vec<String> {
    let matched = detect_signatures_in_text(content, app_config_json, proxy_ports);
    compute_fields_to_strip(&matched)
}

/// 纯函数:扫描一段 TOML 文本,返回命中的 signature 列表。
pub fn detect_signatures_in_text(
    content: &str,
    app_config_json: &Path,
    proxy_ports: &[u16],
) -> Vec<MatchedSignature> {
    let mut out = Vec::new();
    let app_path_str = app_config_json.to_string_lossy();

    for line in content.lines() {
        let stripped = line.trim_start();

        if let Some(value) = parse_root_string_value(stripped, "model_catalog_json") {
            if value == app_path_str {
                out.push(MatchedSignature::ModelCatalogJsonAppHome { value });
            }
        }

        if let Some(value) = parse_root_string_value(stripped, "openai_base_url") {
            for port in proxy_ports {
                let expected = format!("http://127.0.0.1:{port}");
                if value == expected {
                    out.push(MatchedSignature::OpenaiBaseUrlTransferProxy {
                        value: value.clone(),
                        proxy_port: *port,
                    });
                    break;
                }
            }
        }
    }
    out
}

fn compute_fields_to_strip(matched: &[MatchedSignature]) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    let has_catalog = matched
        .iter()
        .any(|m| matches!(m, MatchedSignature::ModelCatalogJsonAppHome { .. }));
    let has_base_url = matched
        .iter()
        .any(|m| matches!(m, MatchedSignature::OpenaiBaseUrlTransferProxy { .. }));
    if has_catalog {
        fields.push(CATALOG_KEY.to_string());
    }
    if has_base_url {
        for k in BASE_URL_BUNDLE {
            fields.push((*k).to_string());
        }
    }
    fields.sort();
    fields.dedup();
    fields
}

/// 解析 `<key> = "<value>"` 形式,返回**已 unescape** 的 value。
///
/// 容忍 `=` 两侧空白,要求 key 后紧跟 `=` 或空白(防 `model` 误匹 `model_provider`)。
///
/// **Windows 路径 unescape**(#269 devin review #3):
/// `crate::toml_sync::toml_string_literal` 用 `serde_json::to_string` 生成 TOML
/// basic string 字面量,Windows 路径里的 `\` 会被写成 `\\`。读回来若不 unescape
/// 就会拿到双反斜杠的字符串,跟 `PathBuf::to_string_lossy()` 的单反斜杠比对
/// 永不命中 → Windows 用户的 `model_catalog_json` signature 检测永失效。
///
/// 实现策略:用 `serde_json::from_str` 反向 parse(TOML basic string 跟 JSON
/// string 在我们用到的转义子集 — `\\` `\"` `\n` `\r` `\t` — 上完全一致,跟
/// 写入端 `toml_string_literal` 对称),自动处理转义。闭合 `"` 用手工状态机
/// 找(裸 `.find('"')` 会被字符串内的 `\"` 误终结)。
fn parse_root_string_value(stripped: &str, key: &str) -> Option<String> {
    let rest = stripped.strip_prefix(key)?;
    let next = rest.chars().next()?;
    if next != '=' && !next.is_ascii_whitespace() {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    // 找闭合引号位置(跳过转义的 \" )。byte-index 安全:Rust 字符串迭代用
    // char_indices 给的是 UTF-8 byte offset,直接拿来 slice 不会破坏 UTF-8。
    let bytes = rest.as_bytes();
    let mut end_byte_idx = None;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if esc {
            esc = false;
            continue;
        }
        if b == b'\\' {
            esc = true;
            continue;
        }
        if b == b'"' {
            end_byte_idx = Some(i);
            break;
        }
    }
    let end = end_byte_idx?;
    let quoted = rest.get(..=end)?;
    // serde_json::from_str 反向 parse(对称 toml_string_literal)
    serde_json::from_str::<String>(quoted).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn app_config_json() -> PathBuf {
        PathBuf::from("/Users/alice/.codex-app-transfer/config.json")
    }

    // ── detect_signatures_in_text ──────────────────────────────────────

    #[test]
    fn detects_model_catalog_json_pointing_to_app_home() {
        let toml = "model_catalog_json = \"/Users/alice/.codex-app-transfer/config.json\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert_eq!(m.len(), 1);
        matches!(m[0], MatchedSignature::ModelCatalogJsonAppHome { .. });
    }

    #[test]
    fn ignores_model_catalog_json_pointing_elsewhere() {
        let toml = "model_catalog_json = \"/tmp/user-catalog.json\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert!(m.is_empty(), "user-owned catalog path must not be flagged");
    }

    #[test]
    fn detects_openai_base_url_matching_known_proxy_port() {
        let toml = "openai_base_url = \"http://127.0.0.1:18080\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert_eq!(m.len(), 1);
        match &m[0] {
            MatchedSignature::OpenaiBaseUrlTransferProxy { proxy_port, .. } => {
                assert_eq!(*proxy_port, 18080)
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn detects_openai_base_url_against_multiple_known_ports() {
        // 用户改了 proxy port 到 19000,但老 snapshot 还存着 18080 → 都要识别
        let toml = "openai_base_url = \"http://127.0.0.1:18080\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[19000, 18080]);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn ignores_openai_base_url_pointing_to_third_party_proxy() {
        let toml = "openai_base_url = \"http://my-private-proxy.example.com:8080\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert!(
            m.is_empty(),
            "user-owned third-party proxy must not be flagged"
        );
    }

    #[test]
    fn ignores_openai_base_url_pointing_to_unknown_localhost_port() {
        let toml = "openai_base_url = \"http://127.0.0.1:9999\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert!(m.is_empty(), "未知 port 不该误判");
    }

    #[test]
    fn does_not_mismatch_keys_with_same_prefix() {
        // 防 `openai_base_url` 把 `openai_base_url_alt` 也匹掉
        let toml = "openai_base_url_alt = \"http://127.0.0.1:18080\"\n";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert!(m.is_empty(), "前缀同名 key 不应被匹配");
    }

    #[test]
    fn detects_both_signatures_in_full_apply_toml() {
        let toml = "\
openai_base_url = \"http://127.0.0.1:18080\"
model_context_window = 1000000
model_catalog_json = \"/Users/alice/.codex-app-transfer/config.json\"
model = \"gpt-5.5\"
sandbox_mode = \"danger-full-access\"
approval_policy = \"never\"
";
        let m = detect_signatures_in_text(toml, &app_config_json(), &[18080]);
        assert_eq!(m.len(), 2, "应识别 catalog + base_url 两条签名: {m:?}");
    }

    // ── compute_fields_to_strip ────────────────────────────────────────

    #[test]
    fn fields_to_strip_for_catalog_only() {
        let matched = vec![MatchedSignature::ModelCatalogJsonAppHome { value: "x".into() }];
        let fields = compute_fields_to_strip(&matched);
        assert_eq!(fields, vec!["model_catalog_json"]);
    }

    #[test]
    fn fields_to_strip_for_base_url_includes_full_apply_bundle() {
        let matched = vec![MatchedSignature::OpenaiBaseUrlTransferProxy {
            value: "x".into(),
            proxy_port: 18080,
        }];
        let mut fields = compute_fields_to_strip(&matched);
        fields.sort();
        let mut expected = vec![
            "approval_policy",
            "model_context_window",
            "openai_base_url",
            "sandbox_mode",
        ];
        expected.sort();
        assert_eq!(fields, expected);
    }

    #[test]
    fn fields_to_strip_for_both_signatures_dedupes() {
        let matched = vec![
            MatchedSignature::ModelCatalogJsonAppHome { value: "x".into() },
            MatchedSignature::OpenaiBaseUrlTransferProxy {
                value: "x".into(),
                proxy_port: 18080,
            },
        ];
        let fields = compute_fields_to_strip(&matched);
        assert_eq!(fields.len(), 5);
        assert!(fields.contains(&"model_catalog_json".to_string()));
        assert!(fields.contains(&"openai_base_url".to_string()));
        assert!(fields.contains(&"model_context_window".to_string()));
        assert!(fields.contains(&"sandbox_mode".to_string()));
        assert!(fields.contains(&"approval_policy".to_string()));
    }

    // ── parse_root_string_value 边界 ───────────────────────────────────

    #[test]
    fn parses_value_with_extra_whitespace() {
        let v = parse_root_string_value("model = \"gpt-5.5\"", "model");
        assert_eq!(v.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn ignores_unquoted_value() {
        let v = parse_root_string_value("foo = 123", "foo");
        assert!(v.is_none(), "integer value 不应被当成 string");
    }

    #[test]
    fn ignores_commented_line() {
        let v = parse_root_string_value("# model = \"gpt\"", "model");
        assert!(v.is_none());
    }

    /// **devin #269 review #3 防回归**:Windows 路径 `C:\Users\…` 经
    /// `toml_string_literal` 序列化后是 `"C:\\Users\\…"`(双反斜杠);
    /// 读回必须 unescape 才能跟 `PathBuf::to_string_lossy()` 拿到的
    /// 单反斜杠形态对齐,否则 signature 检测永失效。
    #[test]
    fn parses_windows_path_with_escaped_backslashes() {
        let line =
            "model_catalog_json = \"C:\\\\Users\\\\alice\\\\.codex-app-transfer\\\\config.json\"";
        let v = parse_root_string_value(line, "model_catalog_json");
        assert_eq!(
            v.as_deref(),
            Some("C:\\Users\\alice\\.codex-app-transfer\\config.json"),
            "double-backslash 必须被 unescape 成单反斜杠(Windows path 兼容)"
        );
    }

    /// 同上:用 Windows 风格的 app_config_json 路径,scan 必须能在
    /// snapshot/live 文件里识别 transfer signature。
    #[test]
    fn detects_signature_for_windows_style_app_config_json_path() {
        let toml =
            "model_catalog_json = \"C:\\\\Users\\\\alice\\\\.codex-app-transfer\\\\config.json\"";
        let app_path = PathBuf::from("C:\\Users\\alice\\.codex-app-transfer\\config.json");
        let m = detect_signatures_in_text(toml, &app_path, &[18080]);
        assert_eq!(m.len(), 1, "Windows path signature 必须能识别: {m:?}");
        matches!(m[0], MatchedSignature::ModelCatalogJsonAppHome { .. });
    }

    /// 值含转义引号 `\"` 时不能被 raw `.find('"')` 截断
    #[test]
    fn handles_escaped_quote_inside_value() {
        let line = "openai_base_url = \"http://example.com/\\\"path\\\"\"";
        let v = parse_root_string_value(line, "openai_base_url");
        assert_eq!(v.as_deref(), Some("http://example.com/\"path\""));
    }

    // ── scan_residual_pollution (集成) ────────────────────────────────

    fn make_paths() -> (tempfile::TempDir, CodexPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(tmp.path());
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::create_dir_all(&paths.app_home).unwrap();
        std::fs::create_dir_all(&paths.active_snapshots_dir).unwrap();
        std::fs::create_dir_all(&paths.recovery_snapshots_dir).unwrap();
        (tmp, paths)
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn polluted_toml(app_home: &Path) -> String {
        let cat = app_home.join("config.json");
        format!(
            "model = \"gpt-5.5\"\nopenai_base_url = \"http://127.0.0.1:18080\"\nmodel_catalog_json = \"{}\"\nmodel_context_window = 1000000\nsandbox_mode = \"danger-full-access\"\napproval_policy = \"never\"\n",
            cat.display()
        )
    }

    #[test]
    fn scan_clean_environment_reports_clean() {
        let (_t, paths) = make_paths();
        write(&paths.config_toml, "model = \"gpt-5.5\"\n");
        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        assert!(report.is_clean());
        assert!(!report.transfer_currently_applied);
    }

    #[test]
    fn scan_detects_pollution_in_live_config_when_transfer_not_applied() {
        let (_t, paths) = make_paths();
        write(&paths.config_toml, &polluted_toml(&paths.app_home));
        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.polluted.len(), 1);
        assert_eq!(report.polluted[0].kind, PollutionSourceKind::LiveConfig);
        assert_eq!(report.polluted[0].fields_to_strip.len(), 5);
    }

    #[test]
    fn scan_skips_live_config_pollution_when_transfer_is_applied() {
        let (_t, paths) = make_paths();
        // 制造 legacy snapshot manifest 让 has_snapshot=true(单元测试无法假
        // 装 current_session_id,走 legacy 路径达到同效果)
        write(&paths.snapshot_manifest, "{}");
        // live config 含 transfer 字段(apply 生效状态正常会有)
        write(&paths.config_toml, &polluted_toml(&paths.app_home));
        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        // live config 因为 transfer_currently_applied=true 被跳过
        assert!(
            report.is_clean(),
            "transfer applying 时 live config 含字段不算污染: {:?}",
            report
        );
        assert!(report.transfer_currently_applied);
    }

    #[test]
    fn scan_always_flags_polluted_snapshot_regardless_of_apply_state() {
        let (_t, paths) = make_paths();
        // recovery snapshot 自带 transfer 字段 = bug
        let snap = paths
            .recovery_snapshots_dir
            .join("20260518T013518322-p7265");
        write(&snap.join("config.toml"), &polluted_toml(&paths.app_home));
        // live config 干净
        write(&paths.config_toml, "model = \"gpt-5.5\"\n");
        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        assert_eq!(report.polluted.len(), 1);
        assert_eq!(
            report.polluted[0].kind,
            PollutionSourceKind::RecoverySnapshot
        );
    }

    #[test]
    fn scan_does_not_flag_user_owned_catalog_path_in_snapshot() {
        // 用户原本就配过 model_catalog_json 指向自己的文件 → 不该被误识别
        let (_t, paths) = make_paths();
        let snap = paths.recovery_snapshots_dir.join("user-snapshot");
        write(
            &snap.join("config.toml"),
            "model_catalog_json = \"/Users/alice/my-own-catalog.json\"\n",
        );
        write(&paths.config_toml, "model = \"gpt-5.5\"\n");
        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        assert!(
            report.is_clean(),
            "用户自己的 catalog 路径不该被误判: {:?}",
            report
        );
    }

    // ── repair_residual_pollution ─────────────────────────────────────

    #[test]
    fn repair_strips_only_listed_fields_keeps_user_lines() {
        let (_t, paths) = make_paths();
        let user_extras = "personality = \"pragmatic\"\nmodel = \"gpt-5.5\"\n";
        let full = format!(
            "{}{}",
            user_extras,
            polluted_toml_lines_only(&paths.app_home)
        );
        write(&paths.config_toml, &full);

        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        assert!(!report.is_clean());
        let repair = repair_residual_pollution(&report, false).unwrap();
        assert_eq!(repair.repaired.len(), 1);
        assert!(!repair.dry_run);

        let after = std::fs::read_to_string(&paths.config_toml).unwrap();
        // strip 后用户字段保留
        assert!(after.contains("personality = \"pragmatic\""));
        assert!(after.contains("model = \"gpt-5.5\""));
        // transfer 字段全清
        for k in [
            "openai_base_url",
            "model_context_window",
            "model_catalog_json",
            "sandbox_mode",
            "approval_policy",
        ] {
            assert!(!after.contains(k), "key {k} 应被 strip: \n{after}");
        }

        // 再扫一次应该干净
        let after_report = scan_residual_pollution(&paths, &[18080]).unwrap();
        assert!(after_report.is_clean());
    }

    #[test]
    fn repair_dry_run_does_not_touch_files() {
        let (_t, paths) = make_paths();
        write(&paths.config_toml, &polluted_toml(&paths.app_home));
        let before = std::fs::read_to_string(&paths.config_toml).unwrap();

        let report = scan_residual_pollution(&paths, &[18080]).unwrap();
        let dry = repair_residual_pollution(&report, true).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.repaired.len(), 1);
        assert_eq!(dry.repaired[0].stripped_keys.len(), 5);

        let after = std::fs::read_to_string(&paths.config_toml).unwrap();
        assert_eq!(before, after, "dry_run 不应写盘");
    }

    /// helper:只产 transfer 套餐的 6 行(给上面 user_extras 拼)
    fn polluted_toml_lines_only(app_home: &Path) -> String {
        let cat = app_home.join("config.json");
        format!(
            "openai_base_url = \"http://127.0.0.1:18080\"\nmodel_catalog_json = \"{}\"\nmodel_context_window = 1000000\nsandbox_mode = \"danger-full-access\"\napproval_policy = \"never\"\n",
            cat.display()
        )
    }
}
