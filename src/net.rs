use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const PROXY_CANDIDATES: [&str; 3] = [
    "socks5h://127.0.0.1:10808",
    "socks5h://127.0.0.1:1080",
    "http://127.0.0.1:7890",
];

/// Returns a proxy URL usable by reqwest, from `YTAPI_PROXY` or by probing
/// common local VPN/proxy ports (same list as the Python sidecar uses).
pub fn detect_proxy() -> Option<String> {
    if let Ok(env) = std::env::var("YTAPI_PROXY")
        && !env.is_empty()
    {
        return Some(env);
    }
    for candidate in PROXY_CANDIDATES {
        let rest = candidate.split("://").nth(1).unwrap_or(candidate);
        let host = rest.split(':').next().unwrap_or(rest);
        let port: u16 = rest
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        if port == 0 {
            continue;
        }
        let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else {
            continue;
        };
        if TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}
