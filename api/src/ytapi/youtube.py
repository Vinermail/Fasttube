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
    with _lib_lock:
        _LIB_CACHE[key] = (time.time(), data)
    return data


_load_resolve_cache()


QUALITY_STANDARD = "standard"
QUALITY_HIGH = "high"
QUALITIES = (QUALITY_STANDARD, QUALITY_HIGH)


def resolve_url(
    video_id: str,
    cookies_file: str | None = None,
    quality: str = QUALITY_STANDARD,
    force: bool = False,
) -> dict:
    """Extract direct stream URL + metadata for a video id.

    Quality levels:
      "standard" - AAC 128 kbps (itag 140), the maximum without Premium
      "high"     - best available AAC (itag 141 / 256 / 258, up to 384 kbps
                   with YouTube Premium; falls back to 140 without it)
    """
    if quality not in QUALITIES:
        quality = QUALITY_STANDARD
    if not _VIDEO_ID_RE.match(video_id):
        return {"ok": False, "error": "invalid video id"}
    cache_key = f"{video_id}@{quality}"
    if not force:
        with _resolve_lock:
            hit = _RESOLVE_CACHE.get(cache_key)
            if hit and time.time() - hit[0] < _RESOLVE_TTL:
                return hit[1]

    import yt_dlp

    opts = dict(YT_DLP_OPTS)
    if cookies_file:
        opts["cookiefile"] = cookies_file

    with yt_dlp.YoutubeDL(opts) as ydl:
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
    """Calls the innertube search endpoint directly with the browser's
    headers (the same way youtube.com does), including continuation pages."""
    if not os.path.exists(BROWSER_FILE):
        return []
    try:
        import requests

        with open(BROWSER_FILE, encoding="utf8") as f:
            headers = json.load(f)
        if "cookie" not in headers and "Cookie" not in headers:
            return []

        proxy = net.detect_proxy()
        proxies = {"http": proxy, "https": proxy} if proxy else None
        session = requests.Session()

        hdrs = {
            k: v
            for k, v in headers.items()
            if k
            in (
                "authorization",
                "cookie",
                "x-goog-authuser",
                "user-agent",
                "x-youtube-client-name",
                "x-youtube-client-version",
            )
        }
        # The browser headers were captured on music.youtube.com; the web
        # search endpoint requires Origin/Referer to match www.youtube.com,
        # otherwise it rejects the request with a 400.
        hdrs["Origin"] = "https://www.youtube.com"
        hdrs["Referer"] = "https://www.youtube.com/"
        hdrs["X-Origin"] = "https://www.youtube.com"

        context = {
            "context": {
                "client": {
                    "clientName": "WEB",
                    "clientVersion": hdrs.get("x-youtube-client-version", "2.20240926.05.00"),
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
    """Search. `music=False` mirrors the youtube.com web search: when browser
    auth is present the innertube search endpoint is called directly with the
    browser's headers (personalized, logged-in results); otherwise yt-dlp's
    anonymous search is used. `music=True` searches the YouTube Music catalog."""
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

        # Use the same auth source as the rest of the service (browser
        # cookies first, then OAuth), so music search respects the login.
        ytm = _authed_ytm() if has_auth() else YTMusic()
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


def prefetch_resolve(video_ids: list[str], quality: str = QUALITY_STANDARD) -> int:
    """Resolves multiple ids in parallel (fills the cache). Returns the number
    of successfully resolved ids."""
    from concurrent.futures import ThreadPoolExecutor

    video_ids = [vid for vid in video_ids if _VIDEO_ID_RE.match(vid)]
    if not video_ids:
        return 0
    with _resolve_lock:
        pending = [
            vid
            for vid in video_ids
            if f"{vid}@{quality}" not in _RESOLVE_CACHE
            or time.time() - _RESOLVE_CACHE[f"{vid}@{quality}"][0] >= _RESOLVE_TTL
        ]
    if not pending:
        return 0

    def work(vid: str) -> bool:
        try:
            result = resolve_url(vid, quality=quality, force=True)
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

        ytm = _authed_ytm() if has_auth() else YTMusic()
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


OAUTH_FILE = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(__file__))), "oauth.json"
)
OAUTH_CREDS_FILE = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(__file__))), "oauth_creds.json"
)
BROWSER_FILE = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(__file__))), "browser.json"
)


def _authed_ytm():
    from ytmusicapi import YTMusic
    from ytmusicapi.auth.oauth import OAuthCredentials

    # Browser (cookies) auth is the recommended workaround while ytmusicapi's
    # OAuth is broken upstream (github.com/sigma67/ytmusicapi/issues/813).
    if os.path.exists(BROWSER_FILE):
        return YTMusic(BROWSER_FILE)

    oauth_credentials = None
    if os.path.exists(OAUTH_CREDS_FILE):
        try:
            with open(OAUTH_CREDS_FILE, encoding="utf8") as f:
                creds = json.load(f)
            oauth_credentials = OAuthCredentials(
                creds.get("client_id"), creds.get("client_secret")
            )
        except Exception:
            log.exception("failed to load oauth credentials")

    return YTMusic(OAUTH_FILE, oauth_credentials=oauth_credentials)


def has_auth() -> bool:
    return os.path.exists(BROWSER_FILE) or os.path.exists(OAUTH_FILE)


def set_browser_auth(auth_text: str) -> dict:
    """Parse request headers pasted from the browser (DevTools copy) and
    store them for browser-based auth."""
    from ytmusicapi.auth.browser import setup_browser

    normalized = _normalize_auth_text(auth_text)
    try:
        setup_browser(BROWSER_FILE, headers_raw=normalized)
        return {"ok": True, "error": ""}
    except Exception as e:
        log.exception("set_browser_auth failed")
        msg = str(e)
        if "missing in your headers" in msg:
            msg += (
                " Pick the request https://music.youtube.com/youtubei/v1/browse "
                "(DevTools -> Network -> refresh the page)."
            )
        return {"ok": False, "error": msg}


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
    if not has_auth():
        return {
            "has_auth": False,
            "hint": "run: uv run --project api ytmusicapi oauth --file api/oauth.json",
            "playlists": [],
        }

    def load() -> dict:
        try:
            ytm = _authed_ytm()
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
    if not has_auth():
        return {
            "tracks": [],
            "error": "run: uv run --project api ytmusicapi oauth --file api/oauth.json",
        }

    def load() -> dict:
        try:
            ytm = _authed_ytm()
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
    if not has_auth():
        return {
            "tracks": [],
            "error": "run: uv run --project api ytmusicapi oauth --file api/oauth.json",
        }

    def load() -> dict:
        try:
            ytm = _authed_ytm()
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


# ---- OAuth (in-app flow) ----

_oauth_lock = threading.Lock()
_oauth_credentials = None
_oauth_device_code: str | None = None


def start_oauth(client_id: str, client_secret: str) -> dict:
    """Step 1: obtain a device code and return the consent URL."""
    global _oauth_credentials, _oauth_device_code
    import requests

    from ytmusicapi.auth.oauth import OAuthCredentials

    client_id = (client_id or "").strip()
    client_secret = (client_secret or "").strip()
    if not client_id or not client_secret:
        return {"ok": False, "error": "client_id and client_secret are required", "url": ""}
    if "apps.googleusercontent.com" not in client_id:
        return {
            "ok": False,
            "error": "client_id must look like '1234-abc.apps.googleusercontent.com' "
            "(OAuth 2.0 Client ID from Google Cloud Console)",
            "url": "",
        }

    proxies = None
    proxy = net.detect_proxy()
    if proxy:
        proxies = {"http": proxy, "https": proxy}
    session = requests.Session()
    if proxies:
        session.proxies.update(proxies)

    try:
        creds = OAuthCredentials(client_id, client_secret, session, proxies)
        code = creds.get_code()
    except Exception as e:
        msg = str(e)
        log.exception("start_oauth failed")
        if "Invalid client type" in msg or "client type" in msg.lower():
            msg = (
                "Invalid client type. In Google Cloud Console create a NEW OAuth client "
                "with application type 'TVs and Limited Input devices' "
                "(existing client type cannot be changed)."
            )
        elif "invalid_client" in msg or "client" in msg.lower():
            msg = (
                "Google rejected the OAuth client. Check: "
                "1) YouTube Data API v3 is enabled for this project; "
                "2) OAuth client type is 'TVs and Limited Input devices'; "
                "3) client_id/secret copied without spaces or line breaks."
            )
        return {"ok": False, "error": msg, "url": ""}

    with _oauth_lock:
        _oauth_credentials = creds
        _oauth_device_code = code["device_code"]

    url = f"{code['verification_url']}?user_code={code['user_code']}"
    return {"ok": True, "error": "", "url": url}


def poll_oauth() -> dict:
    """Step 2: poll Google until the user completes the flow (or fails)."""
    global _oauth_credentials, _oauth_device_code
    with _oauth_lock:
        creds = _oauth_credentials
        device_code = _oauth_device_code
    if creds is None or device_code is None:
        return {"done": False, "error": "no pending oauth flow"}

    for _ in range(150):
        try:
            raw = creds.token_from_code(device_code)
        except Exception as e:
            msg = str(e)
            if "authorization_pending" in msg or "slow_down" in msg:
                time.sleep(2)
                continue
            return {"done": False, "error": msg}
        if "access_token" in raw:
            now = int(time.time())
            token = {
                "scope": raw.get("scope", "https://www.googleapis.com/auth/youtube"),
                "token_type": raw.get("token_type", "Bearer"),
                "access_token": raw["access_token"],
                "refresh_token": raw.get("refresh_token", ""),
                "expires_at": now + int(raw.get("expires_in", 3600)),
                "expires_in": int(raw.get("expires_in", 3600)),
            }
            with open(OAUTH_FILE, "w", encoding="utf8") as f:
                json.dump(token, f, indent=True)
            try:
                with open(OAUTH_CREDS_FILE, "w", encoding="utf8") as f:
                    json.dump(
                        {"client_id": creds.client_id, "client_secret": creds.client_secret},
                        f,
                        indent=True,
                    )
            except Exception:
                log.exception("failed to store oauth credentials")
            with _oauth_lock:
                _oauth_credentials = None
                _oauth_device_code = None
            return {"done": True, "error": ""}
        if raw.get("error") not in ("authorization_pending", "slow_down"):
            return {"done": False, "error": raw.get("error_description", raw.get("error", "oauth error"))}
        time.sleep(2)
    return {"done": False, "error": "timed out waiting for authorization"}


