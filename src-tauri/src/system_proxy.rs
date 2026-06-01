//! 系统代理(梯子)连通性探测 —— MOC-114。
//!
//! relay 真账号模式下,chatgpt backend 透传(plugins/getAccount)与第三方模型路由
//! 都依赖**系统代理**可达;而 auth 检测是纯本地 JWT exp 判断、**不反映网络**。于是
//! 会出现"账号检测可用但梯子没挂 → 全 502/超时、却显示已登录"的静默失效误导态。
//!
//! 本模块把"梯子挂没挂"变成显式信号:读系统代理配置 + 对其 host:port 做**短超时
//! TCP 探测**,喂状态行 + plugins 解锁 gate。探测只连代理端口本身,**绝不碰
//! chatgpt.com**(零封控风险,符合"避免频繁触账号")。
//!
//! 判定三态:
//! - `configured=false` → 压根没挂系统代理。
//! - `configured=true && connected=false` → 配了但端口连不上(梯子没开/挂了)。
//! - `configured=true && connected=true` → 代理端口活着(梯子在跑)。
//!   注:代理活着 ≠ 一定能到 OAI(出口可能挂),那是极少数,留运行时错误兜底。

use std::time::Duration;

/// TCP 探测超时:本地代理多在 127.0.0.1,800ms 足够建连又不拖慢状态行轮询。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemProxyStatus {
    /// 系统是否配置了代理(scutil 读到 enabled 的 HTTP/HTTPS/SOCKS,或非空 *_PROXY env)。
    pub configured: bool,
    /// 代理 host:port 是否 TCP 可连(梯子真活着)。PAC 自动配置时恒 false(无法探端口)。
    pub connected: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// "https" / "http" / "socks" / "pac" / "env"。
    pub kind: Option<String>,
    /// 不可连/未配置原因 —— 诊断 + UI 提示文案来源。
    pub reason: String,
}

impl SystemProxyStatus {
    fn not_configured() -> Self {
        Self {
            configured: false,
            connected: false,
            host: None,
            port: None,
            kind: None,
            reason: "未检测到系统代理配置".into(),
        }
    }

    fn pac() -> Self {
        Self {
            configured: true,
            connected: false,
            host: None,
            port: None,
            kind: Some("pac".into()),
            reason: "系统使用 PAC 自动配置,无法直接探测代理端口".into(),
        }
    }
}

/// 解析出的代理端点。
enum ProxyEndpoint {
    HostPort {
        host: String,
        port: u16,
        kind: &'static str,
    },
    /// PAC 自动配置 —— 拿不到固定 host:port,无法 TCP 探测。
    Pac,
}

/// 探测系统代理连通性(异步:含一次短超时 TCP connect)。
pub async fn probe() -> SystemProxyStatus {
    match read_system_proxy() {
        None => SystemProxyStatus::not_configured(),
        Some(ProxyEndpoint::Pac) => SystemProxyStatus::pac(),
        Some(ProxyEndpoint::HostPort { host, port, kind }) => {
            let connected = tcp_reachable(&host, port).await;
            let reason = if connected {
                format!("{kind} 代理 {host}:{port} 可连")
            } else {
                format!("{kind} 代理 {host}:{port} 连接失败(梯子未开?)")
            };
            SystemProxyStatus {
                configured: true,
                connected,
                host: Some(host),
                port: Some(port),
                kind: Some(kind.to_owned()),
                reason,
            }
        }
    }
}

/// 对 host:port 做短超时 TCP connect —— 成功即认为代理端口活着。
async fn tcp_reachable(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}

/// macOS:`scutil --proxy` 读系统代理(env var 在 GUI app 里通常拿不到,系统设置走 scutil)。
#[cfg(target_os = "macos")]
fn read_system_proxy() -> Option<ProxyEndpoint> {
    let out = std::process::Command::new("scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_scutil(&String::from_utf8_lossy(&out.stdout))
}

/// 非 macOS(Windows/Linux):退回读 `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` 环境变量。
#[cfg(not(target_os = "macos"))]
fn read_system_proxy() -> Option<ProxyEndpoint> {
    parse_env_proxy(|k| std::env::var(k).ok())
}

/// 解析 `scutil --proxy` 的 key : value 输出。优先级 HTTPS > HTTP > SOCKS
/// (chatgpt.com 走 https,HTTPS 代理最相关);PAC enable 单独返回 Pac。
#[cfg(any(target_os = "macos", test))]
fn parse_scutil(text: &str) -> Option<ProxyEndpoint> {
    let mut m = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let enabled = |key: &str| m.get(key).map(|v| v == "1").unwrap_or(false);
    let get = |key: &str| m.get(key).cloned();
    let port = |key: &str| m.get(key).and_then(|v| v.parse::<u16>().ok());

    // PAC 自动配置:有 host:port 优先按显式代理探测,否则标记 PAC。
    let pac = enabled("ProxyAutoConfigEnable") || enabled("ProxyAutoDiscoveryEnable");

    for (en, host_key, port_key, kind) in [
        ("HTTPSEnable", "HTTPSProxy", "HTTPSPort", "https"),
        ("HTTPEnable", "HTTPProxy", "HTTPPort", "http"),
        ("SOCKSEnable", "SOCKSProxy", "SOCKSPort", "socks"),
    ] {
        if enabled(en) {
            if let (Some(host), Some(port)) = (get(host_key), port(port_key)) {
                if !host.is_empty() {
                    return Some(ProxyEndpoint::HostPort { host, port, kind });
                }
            }
        }
    }
    if pac {
        return Some(ProxyEndpoint::Pac);
    }
    None
}

/// 解析 `*_PROXY` 环境变量(非 macOS fallback)。形如 `http://127.0.0.1:7897`。
#[cfg(any(not(target_os = "macos"), test))]
fn parse_env_proxy(mut getenv: impl FnMut(&str) -> Option<String>) -> Option<ProxyEndpoint> {
    for (key, kind) in [
        ("HTTPS_PROXY", "https"),
        ("https_proxy", "https"),
        ("HTTP_PROXY", "http"),
        ("http_proxy", "http"),
        ("ALL_PROXY", "socks"),
        ("all_proxy", "socks"),
    ] {
        if let Some(raw) = getenv(key) {
            if let Some(ep) = parse_proxy_url(&raw, kind) {
                return Some(ep);
            }
        }
    }
    None
}

/// 从 `scheme://host:port` 抠出 host:port(忽略 scheme/credentials/path)。
#[cfg(any(not(target_os = "macos"), test))]
fn parse_proxy_url(raw: &str, kind: &'static str) -> Option<ProxyEndpoint> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let after_scheme = s.split("://").last().unwrap_or(s);
    // 去掉可能的 user:pass@ 与 trailing /path
    let authority = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    let hostport = authority.split('/').next().unwrap_or(authority);
    let (host, port) = hostport.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some(ProxyEndpoint::HostPort {
        host: host.to_string(),
        port,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_hostport(ep: Option<ProxyEndpoint>) -> Option<(String, u16, &'static str)> {
        match ep {
            Some(ProxyEndpoint::HostPort { host, port, kind }) => Some((host, port, kind)),
            _ => None,
        }
    }

    #[test]
    fn scutil_picks_https_first() {
        let text = "  HTTPEnable : 1\n  HTTPProxy : 127.0.0.1\n  HTTPPort : 1080\n  HTTPSEnable : 1\n  HTTPSProxy : 127.0.0.1\n  HTTPSPort : 7897\n  SOCKSEnable : 0\n";
        assert_eq!(
            as_hostport(parse_scutil(text)),
            Some(("127.0.0.1".to_string(), 7897, "https"))
        );
    }

    #[test]
    fn scutil_falls_back_to_http_then_socks() {
        let http_only = "  HTTPEnable : 1\n  HTTPProxy : 10.0.0.1\n  HTTPPort : 3128\n  HTTPSEnable : 0\n  SOCKSEnable : 0\n";
        assert_eq!(
            as_hostport(parse_scutil(http_only)),
            Some(("10.0.0.1".to_string(), 3128, "http"))
        );
        let socks_only = "  HTTPEnable : 0\n  HTTPSEnable : 0\n  SOCKSEnable : 1\n  SOCKSProxy : 127.0.0.1\n  SOCKSPort : 1080\n";
        assert_eq!(
            as_hostport(parse_scutil(socks_only)),
            Some(("127.0.0.1".to_string(), 1080, "socks"))
        );
    }

    #[test]
    fn scutil_none_when_all_disabled() {
        let text =
            "  HTTPEnable : 0\n  HTTPSEnable : 0\n  SOCKSEnable : 0\n  ProxyAutoConfigEnable : 0\n";
        assert!(parse_scutil(text).is_none());
    }

    #[test]
    fn scutil_pac_when_autoconfig_and_no_explicit() {
        let text =
            "  HTTPEnable : 0\n  HTTPSEnable : 0\n  SOCKSEnable : 0\n  ProxyAutoConfigEnable : 1\n";
        assert!(matches!(parse_scutil(text), Some(ProxyEndpoint::Pac)));
    }

    #[test]
    fn scutil_explicit_beats_pac() {
        // 同时开 PAC 和显式 HTTPS 代理 → 优先可探测的显式端口。
        let text = "  HTTPSEnable : 1\n  HTTPSProxy : 127.0.0.1\n  HTTPSPort : 7897\n  ProxyAutoConfigEnable : 1\n";
        assert_eq!(
            as_hostport(parse_scutil(text)),
            Some(("127.0.0.1".to_string(), 7897, "https"))
        );
    }

    #[test]
    fn env_proxy_parsed_with_scheme_and_creds() {
        let ep = parse_env_proxy(|k| match k {
            "HTTPS_PROXY" => Some("http://user:pass@127.0.0.1:7897".to_string()),
            _ => None,
        });
        assert_eq!(
            as_hostport(ep),
            Some(("127.0.0.1".to_string(), 7897, "https"))
        );
    }

    #[test]
    fn env_proxy_none_when_unset() {
        assert!(parse_env_proxy(|_| None).is_none());
    }
}
