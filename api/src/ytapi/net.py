"""Network workarounds for hostile networks.

Some networks (VM hosts, misconfigured routers, ISP filtering) return only
AAAA records for YouTube while IPv6 is unreachable, so every TLS handshake
hangs. This module patches ``socket.getaddrinfo`` to fall back to DNS-over-HTTPS
(Cloudflare, reached by IP literal) for IPv4 answers.

Networks that filter YouTube by SNI need a local proxy (VPN client) instead.
``detect_proxy`` picks it up from ``YTAPI_PROXY`` or by probing common ports.
"""

import http.client
import json
import logging
import os
import socket
import ssl

log = logging.getLogger("ytapi.net")

_DOH_CACHE: dict[str, list[str]] = {}
_orig_getaddrinfo = socket.getaddrinfo
_patched = False

PROXY_CANDIDATES = [
    "socks5://127.0.0.1:10808",
    "socks5://127.0.0.1:1080",
    "http://127.0.0.1:7890",
]


def detect_proxy() -> str | None:
    env = os.environ.get("YTAPI_PROXY")
    if env:
        return env
    for cand in PROXY_CANDIDATES:
        host = cand.split("://", 1)[1].split(":", 1)[0]
        port = int(cand.rsplit(":", 1)[1])
        try:
            with socket.create_connection((host, port), timeout=0.4):
                log.info("detected local proxy %s", cand)
                return cand
        except OSError:
            continue
    return None


def apply_env_proxy() -> None:
    """Expose the detected proxy via env vars for requests-based clients
    (ytmusicapi)."""
    proxy = detect_proxy()
    if not proxy:
        return
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        if not os.environ.get(name):
            os.environ[name] = proxy


class _DohConnection(http.client.HTTPSConnection):
    """Connects to the DoH server by IP literal but verifies the TLS
    certificate against the real hostname (cloudflare-dns.com)."""

    _DOH_HOSTNAME = "cloudflare-dns.com"

    def connect(self) -> None:
        sock = socket.create_connection((self.host, self.port), self.timeout)
        ctx = ssl.create_default_context()
        self.sock = ctx.wrap_socket(sock, server_hostname=self._DOH_HOSTNAME)


def _doh_a_records(host: str) -> list[str]:
    if host in _DOH_CACHE:
        return _DOH_CACHE[host]
    try:
        conn = _DohConnection("1.1.1.1", 443, timeout=10)
        conn.request(
            "GET",
            f"/dns-query?name={host}&type=A",
            headers={"Host": "cloudflare-dns.com", "Accept": "application/dns-json"},
        )
        resp = conn.getresponse()
        data = json.loads(resp.read())
        addrs = [a["data"] for a in data.get("Answer", []) if a.get("type") == 1]
        _DOH_CACHE[host] = addrs
        log.info("DoH resolved %s -> %s", host, addrs)
        return addrs
    except Exception:
        log.exception("DoH lookup failed for %s", host)
        _DOH_CACHE[host] = []
        return []


# Only these hosts are ever routed through the DoH fallback: the patch must
# not affect unrelated HTTP calls in the process.
_DOH_HOSTS = (
    "youtube.com",
    "googlevideo.com",
    "googleusercontent.com",
    "ytimg.com",
    "googleapis.com",
    "lrclib.net",
)


def force_ipv4_dns() -> None:
    """Patch getaddrinfo: try IPv4 via system DNS, fall back to DoH (only
    for YouTube/Google hosts)."""
    global _patched
    if _patched:
        return
    _patched = True

    def patched(host, port, family=0, type=socket.SOCK_STREAM, proto=0, flags=0):
        if family in (0, socket.AF_INET) and any(s in host for s in _DOH_HOSTS):
            try:
                return _orig_getaddrinfo(host, port, socket.AF_INET, type, proto, flags)
            except socket.gaierror:
                pass
            addrs = _doh_a_records(host)
            if addrs:
                return [(socket.AF_INET, type, proto, "", (a, port)) for a in addrs]
        return _orig_getaddrinfo(host, port, family, type, proto, flags)

    socket.getaddrinfo = patched
