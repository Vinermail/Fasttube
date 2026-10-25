"""YouTube backend: yt-dlp / ytmusicapi wrappers."""

import contextlib
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
    with _home_lock:
        _HOME_CACHE.clear()


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


# Search filters accepted by the sidecar, mirroring ytmusicapi's search().
SEARCH_FILTERS = ("", "songs", "videos", "albums", "artists", "playlists")

# Every ytmusicapi client — the signed-in one and the anonymous ones — asks
# YouTube Music for Russian metadata, so section titles, artist names and
# release dates match the rest of the interface.
YTM_LANGUAGE = "ru"

# Video results are filtered by innertube's videoType. UGC uploads (fan
# uploads, lyric videos, gameplay clips riding a song title) are what made the
# old youtube.com search unusable; official videos (OMV/ATV) and
# MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC stay.
HIDDEN_VIDEO_TYPES = frozenset({"MUSIC_VIDEO_TYPE_UGC"})

_KIND_BY_RESULT_TYPE = {
    "song": "song",
    "video": "video",
    "album": "album",
    "artist": "artist",
    "playlist": "playlist",
}

_DEFAULT_SECTION_TITLES = {
    "song": "Песни",
    "video": "Видео",
    "album": "Альбомы",
    "artist": "Исполнители",
    "playlist": "Плейлисты",
}


def _item_from_result(result: dict) -> dict | None:
    """One ytmusicapi search result -> SearchItem dict.

    None when the result is not something the app can show or play (profiles,
    podcasts, UGC videos, entries without an id).
    """
    kind = _KIND_BY_RESULT_TYPE.get(result.get("resultType") or "")
    if kind is None:
        return None

    video_type = result.get("videoType") or ""
    if kind == "video" and video_type in HIDDEN_VIDEO_TYPES:
        return None

    thumbnails = result.get("thumbnails") or []
    item = {
        "kind": kind,
        "id": "",
        "title": "",
        "subtitle": "",
        "thumbnail": thumbnails[-1].get("url", "") if thumbnails else "",
        "duration_secs": 0.0,
        "video_type": video_type,
        "browse_id": "",
        "playlist_id": "",
    }

    if kind in ("song", "video"):
        video_id = result.get("videoId") or ""
        if not video_id:
            return None
        artists = ", ".join(
            a.get("name", "") for a in (result.get("artists") or []) if a.get("name")
        )
        item.update(
            {
                "id": video_id,
                "title": result.get("title") or "",
                "subtitle": artists,
                "duration_secs": float(result.get("duration_seconds") or 0.0),
            }
        )
        return item

    if kind == "album":
        browse_id = result.get("browseId") or ""
        if not browse_id:
            return None
        subtitle = " • ".join(
            part for part in (result.get("artist"), result.get("year")) if part
        )
        item.update(
            {
                "id": browse_id,
                "title": result.get("title") or "",
                "subtitle": subtitle,
                "browse_id": browse_id,
                "playlist_id": result.get("playlistId") or "",
            }
        )
        return item

    if kind == "artist":
        browse_id = result.get("browseId") or ""
        if not browse_id:
            return None
        item.update(
            {
                "id": browse_id,
                "title": result.get("artist") or "",
                "subtitle": result.get("subscribers") or "",
                "browse_id": browse_id,
            }
        )
        return item

    # playlist
    browse_id = result.get("browseId") or ""
    playlist_id = result.get("playlistId") or ""
    if not playlist_id and browse_id.startswith("VL"):
        playlist_id = browse_id[2:]
    item_id = browse_id or playlist_id
    if not item_id:
        return None
    count = result.get("itemCount")
    subtitle = " • ".join(
        str(part)
        for part in (result.get("author"), f"{count} songs" if count else "")
        if part
    )
    item.update(
        {
            "id": item_id,
            "title": result.get("title") or "",
            "subtitle": subtitle,
            "browse_id": browse_id,
            "playlist_id": playlist_id,
        }
    )
    return item


def _section_key(result: dict, item: dict) -> str:
    """Groups results the way YTM groups them.

    Default search carries a localized `category` ("Top result", "Songs", ...);
    a filtered search carries none and is a single group. Community/featured
    playlists are folded into the playlists group so the UI always shows one
    playlist section.
    """
    if item["kind"] == "playlist":
        return "playlist"
    category = (result.get("category") or "").strip()
    return f"{item['kind']}:{category}" if category else item["kind"]


def sections_from_results(
    results: list[dict], filter_kind: str = "", limit: int = 20
) -> list[dict]:
    """Flat ytmusicapi results -> sections, in YouTube Music's order."""
    sections: list[dict] = []
    by_key: dict[str, dict] = {}
    seen: set[str] = set()

    for result in results or []:
        if not isinstance(result, dict):
            continue
        item = _item_from_result(result)
        if item is None or not item["id"] or item["id"] in seen:
            continue
        if filter_kind and item["kind"] != filter_kind:
            continue

        seen.add(item["id"])
        key = _section_key(result, item)
        section = by_key.get(key)
        if section is None:
            if filter_kind:
                # One tab, one section: the name is ours (the server answers in
                # whatever language the request asked for).
                title = _DEFAULT_SECTION_TITLES.get(filter_kind, filter_kind.title())
            else:
                title = (result.get("category") or "").strip() or _DEFAULT_SECTION_TITLES.get(
                    item["kind"], item["kind"].title()
                )
            section = {"title": title, "kind": item["kind"], "items": []}
            by_key[key] = section
            sections.append(section)
        if len(section["items"]) < limit:
            section["items"].append(item)

    return [section for section in sections if section["items"]]


@contextlib.contextmanager
def _english_categories(client):
    """Ask YouTube for English shelf names for the duration of one call.

    ytmusicapi 1.12.2 decides whether a shelf belongs to the requested filter by
    looking for the English word in the shelf title ("song" in "Songs"). With
    `hl=ru` the title is "Песни", the check fails for every filter, and a
    filtered search silently comes back empty. The request context carries `hl`,
    so it is flipped around the call and restored afterwards; the section title
    the user sees is ours anyway (see `_DEFAULT_SECTION_TITLES`).
    """
    context = getattr(client, "context", None)
    if not isinstance(context, dict):
        yield
        return
    client_context = context.get("context")
    if not isinstance(client_context, dict) or not isinstance(client_context.get("client"), dict):
        yield
        return
    slot = client_context["client"]
    previous = slot.get("hl")
    slot["hl"] = "en"
    try:
        yield
    finally:
        if previous is None:
            slot.pop("hl", None)
        else:
            slot["hl"] = previous


def search_music(query: str, filter_name: str = "", limit: int = 20) -> dict:
    """YouTube Music search, sectioned for the UI.

    Returns {"sections": [{"title", "kind", "items": [...]}], "error": ""}.
    """
    query = (query or "").strip()
    filter_name = (filter_name or "").strip()
    if filter_name not in SEARCH_FILTERS:
        return {"sections": [], "error": f"unknown filter: {filter_name}"}
    if not query:
        return {"sections": [], "error": ""}
    limit = max(1, min(int(limit or 20), 50))

    try:
        from ytmusicapi import YTMusic

        ytm = _session_client() or YTMusic(language=YTM_LANGUAGE)
        if filter_name:
            with _english_categories(ytm):
                results = ytm.search(query, filter=filter_name, limit=limit)
        else:
            results = ytm.search(query, limit=limit)
        filter_kind = filter_name[:-1] if filter_name else ""
        return {
            "sections": sections_from_results(results, filter_kind, limit),
            "error": "",
        }
    except Exception as e:
        # No yt-dlp fallback: a raw ytsearch answer is not a YouTube Music
        # answer (UGC videos, no sections), so the UI offers «Повторить» instead.
        log.warning("music search failed (%s): %s", filter_name or "all", e)
        return {"sections": [], "error": f"search failed: {e}"}


def suggestions(query: str, limit: int = 8) -> list[str]:
    """Search suggestions for the query box; empty below two characters."""
    query = (query or "").strip()
    if len(query) < 2:
        return []
    try:
        from ytmusicapi import YTMusic

        ytm = _session_client() or YTMusic(language=YTM_LANGUAGE)
        raw = ytm.get_search_suggestions(query)
    except Exception as e:
        log.warning("search suggestions failed: %s", e)
        return []

    out: list[str] = []
    for entry in raw or []:
        text = entry.get("text") if isinstance(entry, dict) else entry
        text = (text or "").strip()
        if text and text not in out:
            out.append(text)
        if len(out) >= limit:
            break
    return out


def _song_item(result: dict, fallback_thumbnail: str = "") -> dict | None:
    """Album/artist song entry -> SearchItem dict."""
    video_id = result.get("videoId") or ""
    if not video_id:
        return None
    thumbnails = result.get("thumbnails") or []
    artist = ", ".join(
        a.get("name", "") for a in (result.get("artists") or []) if a.get("name")
    )
    return {
        "kind": "song",
        "id": video_id,
        "title": result.get("title") or "",
        "subtitle": artist,
        "thumbnail": thumbnails[-1].get("url", "") if thumbnails else fallback_thumbnail,
        "duration_secs": float(result.get("duration_seconds") or 0.0),
        "video_type": result.get("videoType") or "",
        "browse_id": "",
        "playlist_id": "",
    }


def album(browse_id: str) -> dict:
    """Album page: header line plus one section of tracks."""
    browse_id = (browse_id or "").strip()
    if not browse_id:
        return {"title": "", "subtitle": "", "thumbnail": "", "sections": [], "error": "empty album id"}
    try:
        from ytmusicapi import YTMusic

        ytm = _session_client() or YTMusic(language=YTM_LANGUAGE)
        data = ytm.get_album(browse_id)
    except Exception as e:
        log.warning("get_album failed: %s", e)
        return {"title": "", "subtitle": "", "thumbnail": "", "sections": [], "error": str(e)}

    thumbnails = data.get("thumbnails") or []
    cover = thumbnails[-1].get("url", "") if thumbnails else ""
    tracks = []
    for entry in data.get("tracks") or []:
        item = _song_item(entry, cover)
        if item is not None:
            tracks.append(item)

    artists = ", ".join(a.get("name", "") for a in (data.get("artists") or []) if a.get("name"))
    parts = [artists, data.get("type"), data.get("year")]
    if data.get("trackCount"):
        parts.append(f"{data['trackCount']} tracks")
    subtitle = " • ".join(str(p) for p in parts if p)

    return {
        "title": data.get("title") or "",
        "subtitle": subtitle,
        "thumbnail": cover,
        "sections": [{"title": "Треки", "kind": "song", "items": tracks}] if tracks else [],
        "error": "",
    }


def artist(browse_id: str) -> dict:
    """Artist page: popular songs plus the album shelf as cards."""
    browse_id = (browse_id or "").strip()
    if not browse_id:
        return {"name": "", "subtitle": "", "thumbnail": "", "sections": [], "error": "empty artist id"}
    try:
        from ytmusicapi import YTMusic

        ytm = _session_client() or YTMusic(language=YTM_LANGUAGE)
        data = ytm.get_artist(browse_id)
    except Exception as e:
        log.warning("get_artist failed: %s", e)
        return {"name": "", "subtitle": "", "thumbnail": "", "sections": [], "error": str(e)}

    thumbnails = data.get("thumbnails") or []
    cover = thumbnails[-1].get("url", "") if thumbnails else ""

    songs = []
    for entry in (data.get("songs") or {}).get("results") or []:
        item = _song_item(entry, cover)
        if item is not None:
            songs.append(item)

    albums = []
    for entry in (data.get("albums") or {}).get("results") or []:
        album_id = entry.get("browseId") or ""
        if not album_id:
            continue
        thumbs = entry.get("thumbnails") or []
        albums.append(
            {
                "kind": "album",
                "id": album_id,
                "title": entry.get("title") or "",
                "subtitle": " • ".join(
                    str(part) for part in (entry.get("year"), entry.get("type")) if part
                ),
                "thumbnail": thumbs[-1].get("url", "") if thumbs else "",
                "duration_secs": 0.0,
                "video_type": "",
                "browse_id": album_id,
                "playlist_id": "",
            }
        )

    sections = []
    if songs:
        sections.append({"title": "Популярные песни", "kind": "song", "items": songs})
    if albums:
        sections.append({"title": "Альбомы", "kind": "album", "items": albums})

    parts = [data.get("subscribers") and f"{data['subscribers']} подписчиков"]
    if data.get("monthlyListeners"):
        parts.append(f"{data['monthlyListeners']} слушателей в месяц")
    return {
        "name": data.get("name") or "",
        "subtitle": " • ".join(str(p) for p in parts if p),
        "thumbnail": cover,
        "sections": sections,
        "error": "",
    }


# ---------------------------------------------------------------- home
#
# Shelves are cached in memory for a quarter of an hour, keyed by the mode they
# were fetched in: an anonymous page must never be served to a signed-in user
# and vice versa. Failures are not cached.

HOME_CACHE_TTL = 15 * 60

_home_lock = threading.Lock()
# mode ("anon"|"account") -> (fetched_at, shelves)
_HOME_CACHE: dict[str, tuple[float, list[dict]]] = {}


def _home_mode() -> str:
    return "account" if has_auth() else "anon"


def _item_from_home(entry: dict) -> dict | None:
    """One get_home()/get_explore() entry -> SearchItem dict.

    Home items are a mixed bag: songs and videos (videoId + videoType), albums
    and artists (browseId), playlists (playlistId). Podcast episodes and
    anything without a usable id are skipped.
    """
    thumbnails = entry.get("thumbnails") or []
    thumbnail = thumbnails[-1].get("url", "") if thumbnails else ""

    video_id = entry.get("videoId") or ""
    if video_id:
        video_type = entry.get("videoType") or ""
        if video_type in HIDDEN_VIDEO_TYPES:
            return None
        artists = ", ".join(
            a.get("name", "") for a in (entry.get("artists") or []) if a.get("name")
        )
        return {
            "kind": "song" if video_type == "MUSIC_VIDEO_TYPE_ATV" else "video",
            "id": video_id,
            "title": entry.get("title") or "",
            "subtitle": artists,
            "thumbnail": thumbnail,
            "duration_secs": float(entry.get("duration_seconds") or 0.0),
            "video_type": video_type,
            "browse_id": "",
            "playlist_id": entry.get("playlistId") or "",
        }

    playlist_id = entry.get("playlistId") or ""
    if playlist_id:
        subtitle = entry.get("description") or ""
        return {
            "kind": "playlist",
            "id": playlist_id,
            "title": entry.get("title") or "",
            "subtitle": subtitle,
            "thumbnail": thumbnail,
            "duration_secs": 0.0,
            "video_type": "",
            "browse_id": "",
            "playlist_id": playlist_id,
        }

    browse_id = entry.get("browseId") or ""
    if browse_id.startswith("MPREb") or entry.get("audioPlaylistId"):
        parts = [
            ", ".join(a.get("name", "") for a in (entry.get("artists") or []) if a.get("name")),
            entry.get("year"),
            entry.get("type"),
        ]
        return {
            "kind": "album",
            "id": browse_id,
            "title": entry.get("title") or "",
            "subtitle": " • ".join(str(p) for p in parts if p),
            "thumbnail": thumbnail,
            "duration_secs": 0.0,
            "video_type": "",
            "browse_id": browse_id,
            "playlist_id": "",
        }

    if browse_id.startswith("UC") or browse_id.startswith("MPLA"):
        return {
            "kind": "artist",
            "id": browse_id,
            "title": entry.get("title") or entry.get("artist") or "",
            "subtitle": entry.get("subscribers") or "",
            "thumbnail": thumbnail,
            "duration_secs": 0.0,
            "video_type": "",
            "browse_id": browse_id,
            "playlist_id": "",
        }

    return None


def shelves_from_home(raw: list[dict], limit: int = 20) -> list[dict]:
    """get_home() rows -> Shelf dicts, dropping junk and empty shelves."""
    shelves: list[dict] = []
    seen_titles: set[str] = set()
    for row in raw or []:
        title = (row.get("title") or "").strip()
        items: list[dict] = []
        ids: set[str] = set()
        for entry in row.get("contents") or []:
            if not isinstance(entry, dict):
                continue
            item = _item_from_home(entry)
            if item is None or not item["id"] or item["id"] in ids:
                continue
            ids.add(item["id"])
            items.append(item)
            if len(items) >= limit:
                break
        if not items or not title:
            continue
        if title in seen_titles:
            continue
        seen_titles.add(title)
        shelves.append({"title": title, "kind": "personal", "items": items})
    return shelves


def shelves_from_explore(explore: dict, limit: int = 20) -> list[dict]:
    """get_explore() -> public shelves for signed-out users."""
    shelves: list[dict] = []

    def add(title: str, entries: list[dict]) -> None:
        items: list[dict] = []
        ids: set[str] = set()
        for entry in entries or []:
            item = _item_from_home(entry)
            if item is None or not item["id"] or item["id"] in ids:
                continue
            ids.add(item["id"])
            items.append(item)
            if len(items) >= limit:
                break
        if items:
            shelves.append({"title": title, "kind": "explore", "items": items})

    if not isinstance(explore, dict):
        return shelves
    add("Новые релизы", explore.get("new_releases") or [])
    add("В тренде", (explore.get("trending") or {}).get("items") or [])
    add("Новые клипы", explore.get("new_videos") or [])
    return shelves


def get_home(limit: int = 4, client=None) -> dict:
    """Home shelves: personal when signed in, public explore otherwise.

    `client` exists for tests; in production it is the signed-in ytmusicapi
    client or a fresh anonymous one.
    """
    limit = max(1, min(int(limit or 4), 10))
    mode = _home_mode()

    with _home_lock:
        cached = _HOME_CACHE.get(mode)
    if cached is not None and time.time() - cached[0] < HOME_CACHE_TTL:
        return {"shelves": cached[1], "error": ""}

    if client is None:
        try:
            from ytmusicapi import YTMusic

            client = _session_client() or YTMusic(language=YTM_LANGUAGE)
        except Exception as e:
            log.warning("could not create a client for the home page: %s", e)
            client = None

    personalized = mode == "account"
    shelves: list[dict] = []
    error = ""
    if client is not None and personalized:
        # The anonymous home page belongs to nobody: skip it entirely and show
        # the public explore shelves instead.
        try:
            shelves = shelves_from_home(client.get_home(limit=limit), limit=20)
        except Exception as e:
            log.warning("get_home failed: %s", e)
            error = str(e)

    if not shelves and client is not None:
        # No personal page (signed out) or it came back empty: the public
        # explore page still gives the user something to tap.
        try:
            shelves = shelves_from_explore(client.get_explore(), limit=20)
        except Exception as e:
            log.warning("get_explore failed: %s", e)
            error = error or str(e)
        if error and shelves:
            error = ""  # the fallback worked, no reason to warn the UI

    if not shelves:
        return {"shelves": [], "error": error or "no home content available"}

    with _home_lock:
        _HOME_CACHE[mode] = (time.time(), shelves)
    return {"shelves": shelves, "error": ""}


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

        ytm = _session_client() or YTMusic(language=YTM_LANGUAGE)
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
        found = search_music(artist, "songs", limit + 5)
        tracks = []
        for section in found.get("sections") or []:
            for item in section.get("items") or []:
                if item.get("kind") not in ("song", "video") or item.get("id") == video_id:
                    continue
                tracks.append(
                    {
                        "video_id": item.get("id", ""),
                        "title": item.get("title", ""),
                        "artist": item.get("subtitle", ""),
                        "album": "",
                        "duration_secs": item.get("duration_secs", 0.0),
                        "thumbnail": item.get("thumbnail", ""),
                    }
                )
                if len(tracks) >= limit:
                    break
            if len(tracks) >= limit:
                break
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
        client = YTMusic(auth=headers, language=YTM_LANGUAGE)
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
