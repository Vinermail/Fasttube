use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const PROXY_CANDIDATES: [&str; 3] = [
    "socks5h://127.0.0.1:10808",
    "socks5h://127.0.0.1:1080",
    "http://127.0.0.1:7890",
];

/// How long a detected proxy (or its absence) is trusted. Every probe opens
/// up to three TCP connections with a 400 ms budget, so doing it per download
/// was both slow and blocking.
const PROXY_TTL: Duration = Duration::from_secs(60);

static PROXY_CACHE: OnceLock<Mutex<Option<(Option<String>, Instant)>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<(Option<String>, Instant)>> {
    PROXY_CACHE.get_or_init(|| Mutex::new(None))
}

/// Cached proxy lookup for the sync paths.
pub fn detect_proxy_cached() -> Option<String> {
    let mut slot = cache().lock();
    if let Some((proxy, at)) = slot.as_ref()
        && at.elapsed() < PROXY_TTL
    {
        return proxy.clone();
    }
    let proxy = detect_proxy();
    *slot = Some((proxy.clone(), Instant::now()));
    proxy
}

/// Same lookup, but off the async runtime: the probes are blocking connects and
/// must not stall a tokio worker.
pub async fn detect_proxy_async() -> Option<String> {
    tokio::task::spawn_blocking(detect_proxy_cached)
        .await
        .ok()
        .flatten()
}

/// Forget the cached answer; the next lookup probes again. Used when a request
/// fails to connect, which usually means the proxy came or went.
pub fn invalidate_proxy_cache() {
    *cache().lock() = None;
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Probing three ports with a 400 ms budget each is the slow path; a cached
    /// answer must not repeat it (and must survive invalidation the way the
    /// retry path expects).
    #[test]
    fn proxy_lookup_is_cached_and_invalidatable() {
        invalidate_proxy_cache();
        let started = Instant::now();
        let first = detect_proxy_cached();
        let first_ms = started.elapsed().as_millis();

        let second_started = Instant::now();
        let second = detect_proxy_cached();
        let second_ms = second_started.elapsed().as_millis();

        assert_eq!(first, second);
        assert!(
            second_ms < 50,
            "cached lookup took {second_ms} ms (first probe {first_ms} ms)"
        );

        invalidate_proxy_cache();
        let third = detect_proxy_cached();
        assert_eq!(third, first, "the detection is deterministic here");
    }
}
