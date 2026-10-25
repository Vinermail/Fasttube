"""YouTube backend: yt-dlp / ytmusicapi wrappers."""

import json
import logging
import os
import re
import threading
import time

from ytapi import net

log = logging.getLogger("ytapi.youtube")

YT_DLP_OPTS = {
    "quiet": True,
    "no_warnings": True,
    "skip_download": True,
    "noplaylist": True,
    "socket_timeout": 20,
    "forceipv4": True,
    # Progressive AAC streams only (symphonia decodes AAC natively, and
    # progressive URLs support byte-range streaming). Prefer 140 (128kbps).
    "format": "140/141/139/bestaudio[ext=m4a]/bestaudio",
}

_PROXY = net.detect_proxy()
if _PROXY:
    YT_DLP_OPTS["proxy"] = _PROXY
    net.apply_env_proxy()

_VIDEO_ID_RE = re.compile(r"^[A-Za-z0-9_-]{11}$")


def _track_from_info(info: dict) -> dict:
    return {
        "video_id": info.get("id") or "",
        "title": info.get("title") or "",
        "artist": info.get("artist") or info.get("uploader") or "",
        "album": info.get("album") or "",
        "duration_secs": float(info.get("duration") or 0.0),
        "thumbnail": info.get("thumbnail") or "",
    }


_RESOLVE_CACHE: dict[str, tuple[float, dict]] = {}
_resolve_lock = threading.Lock()
_RESOLVE_TTL = 14400  # 4 hours (googlevideo URLs expire in ~6h)

CACHE_DIR = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(__file__))), "cache"
)
_RESOLVE_CACHE_FILE = os.path.join(CACHE_DIR, "resolve.json")

_LIB_CACHE: dict[str, tuple[float, dict]] = {}
_lib_lock = threading.Lock()


def _load_resolve_cache() -> None:
    try:
        with open(_RESOLVE_CACHE_FILE, encoding="utf8") as f:
            raw = json.load(f)
        for k, (ts, v) in raw.items():
            _RESOLVE_CACHE[k] = (float(ts), v)
        log.info("loaded %d cached resolutions", len(_RESOLVE_CACHE))
    except FileNotFoundError:
        pass
    except Exception:
        log.exception("failed to load resolve cache")


def _save_resolve_cache() -> None:
    try:
        os.makedirs(CACHE_DIR, exist_ok=True)
        now = time.time()
        # Prune entries older than a day: the file would only grow forever.
        pruned = {k: (ts, v) for k, (ts, v) in _RESOLVE_CACHE.items() if now - ts < 86400}
        tmp = _RESOLVE_CACHE_FILE + ".tmp"
        with open(tmp, "w", encoding="utf8") as f:
            json.dump(pruned, f)
        os.replace(tmp, _RESOLVE_CACHE_FILE)
    except Exception:
        log.exception("failed to save resolve cache")


def _cached_lib(key: str, ttl: int, loader):
    hit = _LIB_CACHE.get(key)
    if hit and time.time() - hit[0] < ttl:
        return hit[1]
    data = loader()
    # Never cache a failure: a transient error (network blip, expired cookies)
    # would otherwise be served for the whole TTL, long after it recovered.
    if isinstance(data, dict) and data.get("error"):
        return data
    with _lib_lock:
        _LIB_CACHE[key] = (time.time(), data)
    return data


def invalidate_account_caches() -> None:
    """Drop per-account library data (playlists, likes) after a session change.

    Must be called on login and logout: cached lists belong to the account
    that was signed in when they were fetched.
    """
    with _lib_lock:
        _LIB_CACHE.clear()


_load_resolve_cache()


QUALITY_STANDARD = "standard"
QUALITY_HIGH = "high"
QUALITIES = (QUALITY_STANDARD, QUALITY_HIGH)


def resolve_url(
    video_id: str,
    cookie_header: str | None = None,
    quality: str = QUALITY_STANDARD,
    force: bool = False,
) -> dict:
    """Extract direct stream URL + metadata for a video id.

    Quality levels:
      "standard" - AAC 128 kbps (itag 140), the maximum without Premium
      "high"     - best available AAC (itag 141 / 256 / 258, up to 384 kbps
                   with YouTube Premium; falls back to 140 without it)

    `cookie_header` is only passed when the user opted into streaming with the
    account; the cookies then live in a yt-dlp cookiejar in memory.
    """
    if quality not in QUALITIES:
        quality = QUALITY_STANDARD
    if not _VIDEO_ID_RE.match(video_id):
        return {"ok": False, "error": "invalid video id"}
    cache_key = f"{video_id}@{quality}@{'account' if cookie_header else 'anon'}"
    if not force:
        with _resolve_lock:
            hit = _RESOLVE_CACHE.get(cache_key)
            if hit and time.time() - hit[0] < _RESOLVE_TTL:
                return hit[1]

    import yt_dlp

    opts = dict(YT_DLP_OPTS)
    with yt_dlp.YoutubeDL(opts) as ydl:
        if cookie_header:
            _attach_cookies(ydl, cookie_header)
        info = ydl.extract_info(video_id, download=False)

    if info is None:
        return {"ok": False, "error": "no info extracted"}

    if "entries" in info:  # playlist passed by accident
        info = info["entries"][0]

    formats = info.get("formats") or []
    # Progressive audio-only formats with a direct URL (no HLS/DASH manifests).
    audio = [
        f
        for f in formats
        if f.get("vcodec") == "none"
        and f.get("acodec") not in (None, "none")
        and f.get("protocol") in ("https", "http")
    ]
    audio.sort(
        key=lambda f: (f.get("ext") == "m4a", f.get("tbr") or 0), reverse=True
    )

    if not audio:
        result = {
            "ok": False,
            "error": "no progressive audio format found",
            "track": _track_from_info(info),
        }
    else:
        if quality == QUALITY_STANDARD:
            # Stick to 128 kbps AAC: itag 140 (or the closest lower bitrate).
            std = [
                f
                for f in audio
                if str(f.get("format_id")) == "140" or (f.get("tbr") or 0) <= 132
            ]
            chosen = std[0] if std else audio[0]
        else:
            # Highest bitrate AAC available (Premium gives 141/256/258).
            chosen = audio[0]
        result = {
            "ok": True,
            "track": _track_from_info(info),
            "stream_url": chosen.get("url") or "",
            "mime_type": _mime_for_ext(chosen.get("ext")),
            "content_length": int(chosen.get("filesize") or chosen.get("filesize_approx") or 0),
            "itag": int(chosen.get("format_id") or 0),
        }

    with _resolve_lock:
        _RESOLVE_CACHE[cache_key] = (time.time(), result)
        _save_resolve_cache()
    return result


def _search_web_direct(query: str, limit: int) -> list[dict]:
    """Calls the innertube search endpoint directly with the session's headers
    (the same way youtube.com does), including continuation pages.

    Returns [] when there is no active session; the caller then falls back to
    yt-dlp's anonymous search. Headers are rebuilt on every call because the
    SAPISIDHASH they carry embeds a timestamp and expires.
    """
    snapshot = _session_snapshot()
    if snapshot is None:
        return []
    try:
        import requests

        hdrs = dict(
            _browser_headers(
                snapshot["cookie"], snapshot["user_agent"], snapshot["authuser"]
            )
        )
        # The session was captured on music.youtube.com; the web search
        # endpoint requires Origin/Referer to match www.youtube.com, otherwise
        # it rejects the request with a 400.
        hdrs["origin"] = "https://www.youtube.com"
        hdrs["referer"] = "https://www.youtube.com/"
        hdrs["x-origin"] = "https://www.youtube.com"
        # ytmusicapi's defaults describe a response body, not a request body:
        # sending content-encoding makes the server try to gunzip our JSON.
        hdrs.pop("content-encoding", None)
        hdrs.pop("accept-encoding", None)
        client_version = _web_client_version()
        hdrs["x-youtube-client-name"] = "1"
        hdrs["x-youtube-client-version"] = client_version

        proxy = net.detect_proxy()
        proxies = {"http": proxy, "https": proxy} if proxy else None
        session = requests.Session()

        context = {
            "context": {
                "client": {
                    "clientName": "WEB",
                    "clientVersion": client_version,
                    "hl": "ru",
                    "gl": "RU",
                }
            }
        }

        tracks: list[dict] = []
        continuation: str | None = None

        for _ in range(max(1, (limit // 20) + 1)):
            url = "https://www.youtube.com/youtubei/v1/search?prettyPrint=false"
            body: dict = dict(context)
            if continuation:
                url += f"&continuation={continuation}"
                body["continuation"] = continuation
            else:
                body["query"] = query

            resp = session.post(url, headers=hdrs, json=body, proxies=proxies, timeout=30)
            if resp.status_code != 200:
                log.warning("direct search HTTP %s", resp.status_code)
                break
            data = resp.json()

            items: list[dict] = []
            next_token = None
            if continuation is not None or "continuation" in body:
                # Continuation responses use `continuationContents`.
                cc = data.get("continuationContents") or {}
                slc = cc.get("sectionListContinuation") or {}
                for section in slc.get("contents", []) or []:
                    for item in (section.get("itemSectionRenderer") or {}).get("contents", []):
                        items.append(item)
                if not items:
                    isc = cc.get("itemSectionContinuation") or {}
                    items = isc.get("continuationItems", []) or []
                if not items:
                    # Older shape fallback.
                    for cmd in data.get("onResponseReceivedCommands") or []:
                        for item in (
                            cmd.get("appendContinuationItemsAction") or {}
                        ).get("continuationItems", []):
                            items.append(item)
            else:
                primary = (
                    (data.get("contents") or {})
                    .get("twoColumnSearchResultsRenderer", {})
                    .get("primaryContents", {})
                    .get("sectionListRenderer", {})
                )
                for section in primary.get("contents", []) or []:
                    for item in (section.get("itemSectionRenderer") or {}).get("contents", []):
                        items.append(item)
                # Modern responses keep the token in `continuations`, not in items.
                for c in primary.get("continuations", []) or []:
                    tok = (c.get("nextContinuationData") or {}).get("continuation")
                    if tok:
                        next_token = tok

            for item in items:
                vr = item.get("videoRenderer")
                if vr:
                    vid = vr.get("videoId")
                    if not vid:
                        continue
                    title = "".join(r.get("text", "") for r in (vr.get("title") or {}).get("runs", []))
                    artist = "".join(r.get("text", "") for r in (vr.get("ownerText") or {}).get("runs", []))
                    duration = 0.0
                    for part in ((vr.get("lengthText") or {}).get("simpleText") or "").split(":"):
                        duration = duration * 60 + float(part or 0)
                    thumbnails = (vr.get("thumbnail") or {}).get("thumbnails") or []
                    thumbnail = thumbnails[-1].get("url", "") if thumbnails else ""
                    tracks.append(
                        {
                            "video_id": vid,
                            "title": title,
                            "artist": artist,
                            "album": "",
                            "duration_secs": duration,
                            "thumbnail": thumbnail,
                        }
                    )
                    if len(tracks) >= limit:
                        return tracks
                    continue
                cont = item.get("continuationItemRenderer")
                if cont:
                    endpoint = (cont.get("continuationEndpoint") or {}).get("continuationCommand") or {}
                    next_token = endpoint.get("token")
            continuation = next_token
            if not continuation:
                break

        return tracks
    except Exception:
        log.exception("direct web search failed")
        return []


def search(query: str, limit: int = 20, music: bool = False) -> list[dict]:
    """Search. `music=False` mirrors the youtube.com web search: with an active
    session the innertube search endpoint is called directly with the session's
    headers (personalized, logged-in results); otherwise yt-dlp's anonymous
    search is used. `music=True` searches the YouTube Music catalog."""
    if not music:
        tracks = _search_web_direct(query, limit)
        if tracks:
            return tracks
        import yt_dlp

        opts = {"quiet": True, "no_warnings": True, "skip_download": True, "noplaylist": True}
        with yt_dlp.YoutubeDL(opts) as ydl:
            info = ydl.extract_info(f"ytsearch{limit}:{query}", download=False)
        out = []
        for entry in (info or {}).get("entries", []) or []:
            if entry:
                out.append(_track_from_info(entry))
        return out

    try:
        from ytmusicapi import YTMusic

        # Same client as the rest of the service: the signed-in one when a
        # session is active, an anonymous one otherwise.
        ytm = _session_client() or YTMusic()
        results = ytm.search(query, filter="songs", limit=max(limit, 10))
        tracks = []
        seen = set()
        for r in results or []:
            vid = r.get("videoId") or ""
            if not vid or vid in seen:
                continue
            seen.add(vid)
            tracks.append(
                {
                    "video_id": vid,
                    "title": r.get("title") or "",
                    "artist": ", ".join(a.get("name", "") for a in (r.get("artists") or [])),
                    "album": (r.get("album") or {}).get("name") or "",
                    "duration_secs": float(r.get("duration_seconds") or 0.0),
                    "thumbnail": (r.get("thumbnails") or [{}])[-1].get("url") or "",
                }
            )
        if tracks:
            return tracks
    except Exception:
        log.exception("ytmusicapi search failed, falling back to yt-dlp")

    import yt_dlp

    opts = {"quiet": True, "no_warnings": True, "skip_download": True, "noplaylist": True}
    with yt_dlp.YoutubeDL(opts) as ydl:
        info = ydl.extract_info(f"ytsearch{limit}:{query}", download=False)
    out = []
    for entry in (info or {}).get("entries", []) or []:
        if entry:
            out.append(_track_from_info(entry))
    return out


def prefetch_resolve(
    video_ids: list[str], quality: str = QUALITY_STANDARD, use_account: bool = False
) -> int:
    """Resolves multiple ids in parallel (fills the cache). Returns the number
    of successfully resolved ids."""
    from concurrent.futures import ThreadPoolExecutor

    video_ids = [vid for vid in video_ids if _VIDEO_ID_RE.match(vid)]
    if not video_ids:
        return 0
    cookies = session_cookie() if use_account else None
    suffix = "account" if cookies else "anon"

    def key(vid: str) -> str:
        return f"{vid}@{quality}@{suffix}"

    with _resolve_lock:
        pending = [
            vid
            for vid in video_ids
            if key(vid) not in _RESOLVE_CACHE
            or time.time() - _RESOLVE_CACHE[key(vid)][0] >= _RESOLVE_TTL
        ]
    if not pending:
        return 0

    def work(vid: str) -> bool:
        try:
            result = resolve_url(vid, cookies, quality=quality, force=True)
            return bool(result.get("ok"))
        except Exception:
            log.exception("prefetch resolve failed for %s", vid)
            return False

    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(work, pending))
    return sum(1 for ok in results if ok)


def _mime_for_ext(ext: str | None) -> str:
    return {
        "m4a": "audio/mp4",
        "mp4": "audio/mp4",
        "webm": "audio/webm",
        "ogg": "audio/ogg",
        "mp3": "audio/mpeg",
    }.get(ext or "", "audio/mpeg")


_LYRICS_CACHE: dict[str, tuple[float, dict]] = {}
_lyrics_lock = threading.Lock()
_LYRICS_TTL = 86400  # lyrics rarely change: cache for a day


def get_lyrics(artist: str, title: str, duration_secs: float) -> dict:
    """Fetch lyrics from lrclib.net (synced when available)."""
    import requests

    key = f"{artist}\x00{title}"
    with _lyrics_lock:
        hit = _LYRICS_CACHE.get(key)
        if hit and time.time() - hit[0] < _LYRICS_TTL:
            return hit[1]

    result = {"ok": False, "synced": "", "plain": "", "error": ""}

    # Clean video-suffix junk for better matching, e.g. "(Official Video)".
    clean_title = re.sub(r"[\s\-_]*[\[\(].{0,60}?[\]\)]\s*$", "", title).strip() or title

    proxies = None
    proxy = net.detect_proxy()
    if proxy:
        proxies = {"http": proxy, "https": proxy}

    try:
        params = {"artist_name": artist, "track_name": clean_title}
        if duration_secs and duration_secs > 0:
            params["duration"] = int(duration_secs)
        resp = requests.get(
            "https://lrclib.net/api/get", params=params, proxies=proxies, timeout=15
        )
        if resp.status_code == 200:
            data = resp.json()
            result["ok"] = True
            result["synced"] = data.get("syncedLyrics") or ""
            result["plain"] = data.get("plainLyrics") or ""
        else:
            result["error"] = f"lrclib returned HTTP {resp.status_code}"
    except Exception as e:
        log.exception("get_lyrics failed")
        result["error"] = str(e)

    with _lyrics_lock:
        _LYRICS_CACHE[key] = (time.time(), result)
    return result


def related_tracks(video_id: str, limit: int = 20) -> dict:
    """Related songs via YouTube Music.

    The `player/next` related endpoint is broken server-side (HTTP 400) in
    many regions, so fall back to a music search by the track's artist.
    """
    if not _VIDEO_ID_RE.match(video_id):
        return {"tracks": [], "error": "invalid video id"}
    try:
        from ytmusicapi import YTMusic

        ytm = _session_client() or YTMusic()
        data = ytm.get_song_related(video_id) or []
        tracks = []
        for r in data:
            vid = r.get("videoId") or ""
            if not vid:
                continue
            tracks.append(
                {
                    "video_id": vid,
                    "title": r.get("title") or "",
                    "artist": ", ".join(a.get("name", "") for a in (r.get("artists") or [])),
                    "album": (r.get("album") or {}).get("name") or "",
                    "duration_secs": float(r.get("duration_seconds") or 0.0),
                    "thumbnail": (r.get("thumbnails") or [{}])[-1].get("url") or "",
                }
            )
            if len(tracks) >= limit:
                break
        if tracks:
            return {"tracks": tracks, "error": ""}
    except Exception as e:
        log.warning("get_song_related failed (%s), falling back to artist search", e)

    try:
        resolved = resolve_url(video_id)
        artist = (resolved.get("track") or {}).get("artist") or ""
        if not artist:
            return {"tracks": [], "error": "related tracks unavailable"}
        found = search(artist, limit=limit + 5, music=True)
        tracks = [t for t in found if t.get("video_id") != video_id][:limit]
        return {"tracks": tracks, "error": ""}
    except Exception as e:
        log.exception("related fallback failed")
        return {"tracks": [], "error": str(e)}



# ---------------------------------------------------------------- session
#
# The YouTube session lives in memory only; this process never writes cookies
# to disk. Rust owns the encrypted bundle (src/secrets.rs) and pushes the
# fields here through SetSession after every start or sign-in.

YTM_ORIGIN = "https://music.youtube.com"
# Fallback client version for the plain www.youtube.com innertube endpoint
# (the web client, not the music one). The real one is read from youtube.com and
# cached for a day; this constant only covers a failed fetch.
WEB_CLIENT_VERSION = "2.20240926.05.00"
WEB_CLIENT_VERSION_URL = "https://www.youtube.com/"
WEB_CLIENT_VERSION_TTL = 24 * 60 * 60

_client_version_lock = threading.Lock()
# (fetched_at, version) of the last successful read.
_client_version_cache: tuple[float, str] | None = None


def _web_client_version() -> str:
    """INNERTUBE_CONTEXT_CLIENT_VERSION of the live youtube.com page.

    Cached for a day. A failed fetch returns the constant fallback without
    poisoning the cache, so the next call tries again.
    """
    global _client_version_cache

    now = time.time()
    with _client_version_lock:
        cached = _client_version_cache
    if cached is not None and now - cached[0] < WEB_CLIENT_VERSION_TTL:
        return cached[1]

    version = ""
    try:
        import requests

        proxy = net.detect_proxy()
        proxies = {"http": proxy, "https": proxy} if proxy else None
        resp = requests.get(
            WEB_CLIENT_VERSION_URL,
            headers={"user-agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"},
            proxies=proxies,
            timeout=15,
        )
        if resp.status_code == 200:
            match = re.search(
                r'"INNERTUBE_CONTEXT_CLIENT_VERSION"\s*:\s*"([0-9.]+)"', resp.text
            )
            if match is None:
                match = re.search(
                    r'"INNERTUBE_CLIENT_VERSION"\s*:\s*"([0-9.]+)"', resp.text
                )
            if match is not None:
                version = match.group(1)
    except Exception:
        log.warning("could not read the web client version", exc_info=True)

    if not version:
        return WEB_CLIENT_VERSION

    with _client_version_lock:
        _client_version_cache = (now, version)
    log.info("web client version: %s", version)
    return version

_session_lock = threading.RLock()
# {"client": YTMusic, "cookie": str, "user_agent": str, "authuser": str,
#  "state": "active"|"expired", "account": str, "validated_at": float}
_session: dict | None = None


def _browser_headers(cookie: str, user_agent: str, authuser: str) -> dict:
    """Headers for YTMusic(auth=...) — the same shape setup_browser would
    write, but built in memory. Raises KeyError without __Secure-3PAPISID."""
    from ytmusicapi.helpers import get_authorization, initialize_headers, sapisid_from_cookie

    cookie = (cookie or "").strip()
    if not cookie:
        raise ValueError("empty cookie")
    sapisid = sapisid_from_cookie(cookie)
    headers = dict(initialize_headers())
    headers.update(
        {
            "authorization": get_authorization(f"{sapisid} {YTM_ORIGIN}"),
            "cookie": cookie,
            "origin": YTM_ORIGIN,
            "x-origin": YTM_ORIGIN,
            "x-goog-authuser": authuser or "0",
        }
    )
    if user_agent:
        headers["user-agent"] = user_agent
    return headers


def set_session(cookie: str, user_agent: str, x_goog_authuser: str) -> dict:
    """Install a session from the app (memory only)."""
    global _session
    from ytmusicapi import YTMusic

    try:
        headers = _browser_headers(cookie, user_agent, x_goog_authuser)
        client = YTMusic(auth=headers)
    except KeyError:
        return {
            "state": "signed_out",
            "account_name": "",
            "error": "cookie has no __Secure-3PAPISID: sign-in did not complete",
        }
    except Exception as e:
        log.exception("set_session failed")
        return {"state": "signed_out", "account_name": "", "error": f"invalid session: {e}"}

    with _session_lock:
        _session = {
            "client": client,
            "cookie": cookie.strip(),
            # Kept for the direct web search, which rebuilds its headers
            # (and the SAPISIDHASH timestamp) on every request.
            "user_agent": (user_agent or "").strip(),
            "authuser": (x_goog_authuser or "0").strip() or "0",
            "state": "active",
            "account": "",
            "validated_at": 0.0,
        }
    invalidate_account_caches()
    log.info("session installed (memory only)")
    return {"state": "active", "account_name": "", "error": ""}


def _find_active_account(node):
    """Depth-first search for activeAccountHeaderRenderer in the account menu.

    Tolerant on purpose: ytmusicapi's own get_account_info() walks a fixed path
    and raises KeyError on any response reshuffle.
    """
    if isinstance(node, dict):
        header = node.get("activeAccountHeaderRenderer")
        if isinstance(header, dict):
            return header
        for value in node.values():
            found = _find_active_account(value)
            if found is not None:
                return found
    elif isinstance(node, list):
        for value in node:
            found = _find_active_account(value)
            if found is not None:
                return found
    return None


def _account_name(header: dict) -> str:
    try:
        return header["accountName"]["runs"][0]["text"]
    except Exception:
        return ""


def _expire_session(reason: str) -> dict:
    with _session_lock:
        if _session is not None:
            _session["state"] = "expired"
            account = _session["account"]
        else:
            account = ""
    invalidate_account_caches()
    log.warning("session expired: %s", reason)
    return {"state": "expired", "account_name": account, "error": reason}


def validate_session() -> dict:
    """One cheap authenticated request.

    The account menu carries activeAccountHeaderRenderer only while the cookie
    is still signed in: a 200 without it means the session went stale. HTTP
    401/403 mean the same. Network errors and 5xx do NOT change the state.
    """
    from ytmusicapi.exceptions import YTMusicServerError

    with _session_lock:
        session = _session
        state = session["state"] if session else "signed_out"
        account = session["account"] if session else ""
    if session is None:
        return {"state": "signed_out", "account_name": "", "error": ""}
    if state == "expired":
        return {"state": "expired", "account_name": account, "error": ""}

    try:
        data = session["client"]._send_request("account/account_menu", {})
    except YTMusicServerError as e:
        message = str(e)
        if "HTTP 401" in message or "HTTP 403" in message:
            return _expire_session("YouTube rejected the session")
        log.warning("session validation: server error: %s", message.splitlines()[0])
        return {"state": state, "account_name": account, "error": message}
    except Exception as e:
        log.warning("session validation: network error: %s", e)
        return {"state": state, "account_name": account, "error": str(e)}

    header = _find_active_account(data)
    if header is None:
        return _expire_session("no active account in the account menu")

    name = _account_name(header)
    with _session_lock:
        session["state"] = "active"
        session["account"] = name
        session["validated_at"] = time.time()
    return {"state": "active", "account_name": name, "error": ""}


def clear_session() -> dict:
    global _session
    with _session_lock:
        had = _session is not None
        _session = None
    invalidate_account_caches()
    log.info("session cleared (had session: %s)", had)
    return {"state": "signed_out", "account_name": "", "error": ""}


def has_auth() -> bool:
    """A session is in memory and the last validation did not fail.

    (Not "a file exists": nothing about the session is read from disk here.)
    """
    with _session_lock:
        return _session is not None and _session["state"] == "active"


def session_state() -> dict:
    with _session_lock:
        if _session is None:
            return {"state": "signed_out", "account_name": ""}
        return {"state": _session["state"], "account_name": _session["account"]}


def session_cookie() -> str | None:
    """Cookie header for yt-dlp, only while the session is usable."""
    with _session_lock:
        if _session is None or _session["state"] == "expired":
            return None
        return _session["cookie"]


def _session_client():
    with _session_lock:
        if _session is None or _session["state"] == "expired":
            return None
        return _session["client"]


def _session_snapshot() -> dict | None:
    """Cookie/UA/authuser of an active session, or None.

    A copy, so the caller builds headers without holding the lock.
    """
    with _session_lock:
        if _session is None or _session["state"] != "active":
            return None
        return {
            "cookie": _session.get("cookie", ""),
            "user_agent": _session.get("user_agent", ""),
            "authuser": _session.get("authuser", "0"),
        }


def _parse_cookie_header(cookie_header: str) -> list[tuple[str, str]]:
    pairs = []
    for part in cookie_header.split(";"):
        name, sep, value = part.strip().partition("=")
        if sep and name:
            pairs.append((name.strip(), value.strip().strip('"')))
    return pairs


def _attach_cookies(ydl, cookie_header: str) -> None:
    """Feed the session cookies to yt-dlp in memory (no cookiefile)."""
    import http.cookiejar

    for name, value in _parse_cookie_header(cookie_header):
        cookie = http.cookiejar.Cookie(
            version=0,
            name=name,
            value=value,
            port=None,
            port_specified=False,
            domain=".youtube.com",
            domain_specified=True,
            domain_initial_dot=True,
            path="/",
            path_specified=True,
            secure=True,
            expires=None,
            discard=True,
            comment=None,
            comment_url=None,
            rest={},
            rfc2109=False,
        )
        ydl.cookiejar.set_cookie(cookie)


def parse_auth_headers(text: str) -> dict:
    """Normalise pasted request headers (DevTools or cURL) into session fields.

    Uses the same normaliser the old paste flow used; no files involved.
    """
    normalized = _normalize_auth_text(text or "")
    headers = {}
    for line in normalized.splitlines():
        key, sep, value = line.partition(":")
        if sep:
            headers[key.strip().lower()] = value.strip()
    cookie = headers.get("cookie", "")
    if not cookie:
        raise ValueError("no Cookie header found in the pasted text")
    return {
        "cookie": cookie,
        "user_agent": headers.get("user-agent", ""),
        "x_goog_authuser": headers.get("x-goog-authuser", "0") or "0",
    }


def _normalize_auth_text(text: str) -> str:
    """Accept both 'Copy request headers' and 'Copy as cURL' (Firefox/Chrome) formats."""
    import re
    import urllib.parse

    lines = text.strip().splitlines()
    out = []
    for raw in lines:
        line = raw.strip()
        if line.endswith("^") or line.endswith("\\"):
            line = line[:-1].rstrip()
        if not line or line.startswith("curl"):
            continue
        if line.startswith("-H"):
            rest = line[2:].strip()
            # cmd.exe caret escaping from Firefox 'Copy as cURL (Windows)'
            rest = rest.replace("^", "")
            rest = rest.strip()
            if rest and rest[0] in ("'", '"'):
                rest = rest[1:]
            if rest.endswith(("'", '"')):
                rest = rest[:-1]
            try:
                rest = urllib.parse.unquote(rest)
            except Exception:
                pass
            key = rest.split(":", 1)[0].strip()
            if re.match(r"^[A-Za-z0-9-]+$", key):
                out.append(rest)
            continue
        if ": " in line:
            key = line.split(":", 1)[0].strip()
            if re.match(r"^[A-Za-z0-9-]+$", key):
                out.append(line)
    return "\n".join(out)


def user_playlists() -> dict:
    if _session_client() is None:
        return {
            "has_auth": False,
            "hint": "sign in to YouTube in the app",
            "playlists": [],
        }

    def load() -> dict:
        try:
            ytm = _session_client()
            if ytm is None:
                return {"has_auth": False, "hint": "not signed in", "playlists": []}
            items = ytm.get_library_playlists(limit=None) or []
            playlists = [
                {
                    "id": p.get("playlistId") or "",
                    "name": p.get("title") or "",
                    "track_count": int(p.get("count") or 0),
                    "thumbnail": (p.get("thumbnails") or [{}])[-1].get("url") or "",
                }
                for p in items
                if p.get("playlistId")
            ]
            return {"has_auth": True, "hint": "", "playlists": playlists}
        except Exception:
            log.exception("get_library_playlists failed")
            return {"has_auth": True, "hint": "", "playlists": [], "error": "failed to load playlists"}

    return _cached_lib("user_playlists", 600, load)


def playlist_tracks(playlist_id: str) -> dict:
    if _session_client() is None:
        return {
            "tracks": [],
            "error": "sign in to YouTube in the app",
        }

    def load() -> dict:
        try:
            ytm = _session_client()
            if ytm is None:
                return {"tracks": [], "error": "not signed in"}
            # limit=None fetches every track (paginated via continuations)
            data = ytm.get_playlist(playlist_id, limit=None) or {}
            tracks = []
            for t in data.get("tracks") or []:
                vid = t.get("videoId")
                if not vid:
                    continue
                artists = ", ".join(a.get("name", "") for a in (t.get("artists") or []))
                tracks.append(
                    {
                        "video_id": vid,
                        "title": t.get("title") or "",
                        "artist": artists,
                        "album": (t.get("album") or {}).get("name") or "",
                        "duration_secs": float(t.get("duration_seconds") or 0.0),
                        "thumbnail": (t.get("thumbnails") or [{}])[-1].get("url") or "",
                    }
                )
            return {"tracks": tracks, "error": ""}
        except Exception as e:
            log.exception("get_playlist failed")
            return {"tracks": [], "error": str(e)}

    return _cached_lib(f"playlist:{playlist_id}", 600, load)


def liked_songs() -> dict:
    if _session_client() is None:
        return {
            "tracks": [],
            "error": "sign in to YouTube in the app",
        }

    def load() -> dict:
        try:
            ytm = _session_client()
            if ytm is None:
                return {"tracks": [], "error": "not signed in"}
            items = ytm.get_liked_songs(limit=None) or {}
            tracks = []
            for t in items.get("tracks") or []:
                vid = t.get("videoId")
                if not vid:
                    continue
                artists = ", ".join(a.get("name", "") for a in (t.get("artists") or []))
                tracks.append(
                    {
                        "video_id": vid,
                        "title": t.get("title") or "",
                        "artist": artists,
                        "album": (t.get("album") or {}).get("name") or "",
                        "duration_secs": float(t.get("duration_seconds") or 0.0),
                        "thumbnail": (t.get("thumbnails") or [{}])[-1].get("url") or "",
                    }
                )
            return {"tracks": tracks, "error": ""}
        except Exception as e:
            log.exception("get_liked_songs failed")
            return {"tracks": [], "error": str(e)}

    return _cached_lib("liked_songs", 600, load)
