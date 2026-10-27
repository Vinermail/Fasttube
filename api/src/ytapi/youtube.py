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

# The resolve options do not depend on the request: the proxy is decided once at
# import (`_PROXY` above, same list as the Rust side probes). yt-dlp keeps the
# dict it is handed, so every resolve passes a copy instead of rebuilding it.
_RESOLVE_OPTS = dict(YT_DLP_OPTS)


def _resolve_opts() -> dict:
    return dict(_RESOLVE_OPTS)


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


# YouTube takes playlist edits in one call, but a 350-track copy should still
# be polite: batches, and a pause between them.
ADD_BATCH = 50
ADD_PAUSE = 0.3


def _edit_status(result, lenient: bool = False) -> tuple[bool, str]:
    """ytmusicapi does not raise when YouTube refuses: it returns a status
    string or the whole response. Only SUCCEEDED counts as done.

    `lenient` is for the like endpoints (rate_song/rate_playlist): they answer
    with a plain responseContext and no status at all, so "no status" there
    means "no refusal".
    """
    if isinstance(result, str):
        return ("SUCCEEDED" in result.upper()), result
    if isinstance(result, dict):
        status = str(result.get("status") or "").upper()
        if status:
            return ("SUCCEEDED" in status), status
        if lenient and not result.get("error"):
            return True, "OK"
        # Some endpoints answer with the action list and no status at all.
        if result.get("actions") or result.get("playlistEditResults"):
            return True, "OK"
        return False, json.dumps(result)[:300]
    if result is None and lenient:
        return True, "OK"
    return False, repr(result)


def _looks_like_duplicate(result) -> bool:
    text = json.dumps(result).upper() if not isinstance(result, str) else result.upper()
    return "DUPLICATE" in text or "ALREADY" in text


def _refuse_unowned(playlist_id: str) -> dict | None:
    """A saved (not owned) playlist cannot be edited, and a mix is generated:
    both are rejected here, before any request leaves the process."""
    if _is_mix(playlist_id):
        return {"ok": False, "error": "автоматический микс менять нельзя", "code": "MIX"}
    owned = _playlist_owned(playlist_id)
    if owned is False:
        return {
            "ok": False,
            "error": "это сохранённый чужой плейлист — менять его нельзя (только убрать из медиатеки)",
            "code": "NOT_OWNED",
        }
    return None


def _playlist_owned(playlist_id: str) -> bool | None:
    with _playlist_lock:
        if playlist_id in _OWNED:
            return _OWNED[playlist_id]
        entry = _PLAYLISTS.get(f"playlist:{playlist_id}")
        return None if entry is None else bool(entry.get("owned"))


def _cached_video_ids(playlist_id: str) -> set[str]:
    with _playlist_lock:
        entry = _PLAYLISTS.get(f"playlist:{playlist_id}")
        if entry is None:
            return set()
        return {t.get("video_id", "") for t in entry.get("tracks") or []}


def _apply_to_cache(playlist_id: str, mutate) -> None:
    """Applies a successful edit to the cached page.

    Dropping the cache instead ([`invalidate_account_caches`]) would force a full
    refetch of a 350-track playlist on every single-track edit, and the user
    would watch the list blank out.
    """
    with _playlist_lock:
        entry = _PLAYLISTS.get(f"playlist:{playlist_id}")
        if entry is not None:
            mutate(entry)


def _blank_row(video_id: str, set_video_id: str) -> dict:
    """A row we know exists but have no metadata for yet: the next page fetch
    fills it in, meanwhile the UI shows it instead of a hole."""
    return {
        "video_id": video_id,
        "title": "",
        "artist": "",
        "album": "",
        "duration_secs": 0.0,
        "thumbnail": "",
        "set_video_id": set_video_id,
        "like_status": "",
    }


def _added_items(result) -> list[dict]:
    """`playlistEditResults[].playlistEditVideoAddedResultData` → rows."""
    items = []
    if not isinstance(result, dict):
        return items
    for entry in result.get("playlistEditResults") or []:
        data = entry.get("playlistEditVideoAddedResultData") if isinstance(entry, dict) else None
        if not isinstance(data, dict):
            continue
        video_id = data.get("videoId") or data.get("addedVideoId") or ""
        set_video_id = data.get("setVideoId") or ""
        if video_id:
            items.append({"video_id": video_id, "set_video_id": set_video_id})
    return items


def create_account_playlist(title: str) -> dict:
    """Creates a private playlist on the account and returns its id."""
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube", "id": ""}
    title = (title or "").strip() or "Новый плейлист"
    try:
        created = ytm.create_playlist(title, "Создано в Fasttube", privacy_status="PRIVATE")
    except Exception as e:
        log.exception("create_playlist failed")
        return {"ok": False, "error": str(e), "id": ""}
    if isinstance(created, dict):
        ok, status = _edit_status(created)
        return {
            "ok": ok,
            "error": "" if ok else f"YouTube отказал: {status}",
            "id": str(created.get("playlistId") or ""),
        }
    if not isinstance(created, str) or not created:
        return {"ok": False, "error": "YouTube не вернул id плейлиста", "id": ""}
    invalidate_account_caches()
    # A brand-new playlist has no rows, and YouTube answers its browse page with
    # something the parser cannot read. Seed the cache so the first open shows an
    # empty list that the account owns instead of an error.
    with _playlist_lock:
        _OWNED[created] = True
    _store_playlist(f"playlist:{created}", [], 0, True, True, _generation())
    return {"ok": True, "error": "", "id": created}


def delete_account_playlist(playlist_id: str) -> dict:
    """Deletes a playlist on the account. There is no undo."""
    refused = _refuse_unowned(playlist_id)
    if refused:
        return refused
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    try:
        result = ytm.delete_playlist(playlist_id)
    except Exception as e:
        log.exception("delete_playlist failed")
        return {"ok": False, "error": str(e)}
    # The delete endpoint answers with a bare responseContext and no status when
    # it works, so "no status" here means "no refusal".
    ok, status = _edit_status(result, lenient=True)
    if not ok:
        return {"ok": False, "error": f"YouTube не подтвердил удаление: {status}"}
    invalidate_account_caches()
    _forget_playlist(playlist_id)
    return {"ok": True, "error": ""}


def add_playlist_items(
    playlist_id: str, video_ids: list[str], allow_duplicates: bool = False
) -> dict:
    """Appends tracks to one of the account's playlists (writes to YouTube).

    With `allow_duplicates` False the cached page is checked first and the
    duplicates are reported instead of being sent — YouTube refuses the whole
    batch when one track is already there.
    """
    wanted = [v for v in dict.fromkeys(video_ids) if v]
    if not wanted:
        return {"ok": True, "error": "", "items": [], "video_ids": []}
    refused = _refuse_unowned(playlist_id)
    if refused:
        return refused
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}

    if not allow_duplicates:
        already = [v for v in wanted if v in _cached_video_ids(playlist_id)]
        if already:
            return {
                "ok": False,
                "error": "уже есть в плейлисте",
                "code": "DUPLICATE",
                "video_ids": already,
            }

    created: list[dict] = []
    failed: list[str] = []
    first_error = ""
    for index, chunk in enumerate(
        [wanted[i : i + ADD_BATCH] for i in range(0, len(wanted), ADD_BATCH)]
    ):
        if index:
            time.sleep(ADD_PAUSE)
        try:
            result = ytm.add_playlist_items(playlist_id, videoIds=list(chunk))
        except Exception as e:
            log.exception("add_playlist_items failed")
            failed.extend(chunk)
            first_error = first_error or str(e)
            continue
        ok, status = _edit_status(result)
        if ok:
            named = _added_items(result)
            if named:
                created.extend(named)
            else:
                # The answer did not list the new rows (it does for some
                # playlists only): read the playlist back and find them.
                try:
                    rows = _fresh_rows(ytm, playlist_id)
                except Exception as e:
                    # The status said it worked; an unreadable read-back is not
                    # proof otherwise, so the add stands.
                    log.warning("cannot read back %s after an add: %s", playlist_id, e)
                    rows = None
                if rows is not None:
                    wanted_here = set(chunk)
                    for row in rows:
                        video = row.get("video_id", "")
                        if video in wanted_here:
                            wanted_here.discard(video)
                            created.append(
                                {
                                    "video_id": video,
                                    "set_video_id": row.get("set_video_id", ""),
                                }
                            )
                    if wanted_here:
                        failed.extend(sorted(wanted_here))
                        first_error = first_error or "YouTube не подтвердил добавление"
            continue
        failed.extend(chunk)
        first_error = first_error or (
            "уже есть в плейлисте" if _looks_like_duplicate(result) else f"YouTube отказал: {status}"
        )

    if created:
        _apply_to_cache(
            playlist_id,
            lambda entry: _cache_after_add(entry, created),
        )
    if failed:
        return {
            "ok": False,
            "error": first_error or "часть треков не добавилась",
            "code": "PARTIAL" if created else "",
            "items": created,
            "video_ids": failed,
        }
    return {"ok": True, "error": "", "items": created, "video_ids": []}


def _fresh_rows(ytm, playlist_id: str, limit: int = 300) -> list[dict]:
    """Rows straight from YouTube: the cache is dropped first."""
    with _playlist_lock:
        _PLAYLISTS.pop(f"playlist:{playlist_id}", None)
    try:
        data = ytm.get_playlist(playlist_id, limit=limit) or {}
    except Exception as e:
        if _looks_empty(e):
            return []
        raise
    return _playlist_rows(data)


def _cache_after_add(entry: dict, created: list[dict]) -> None:
    known = {t.get("set_video_id") for t in entry.get("tracks") or []}
    for item in created:
        if item["set_video_id"] and item["set_video_id"] in known:
            continue
        entry.setdefault("tracks", []).append(
            _blank_row(item["video_id"], item["set_video_id"])
        )
    entry["total"] = int(entry.get("total") or 0) + len(created)


def remove_playlist_items(playlist_id: str, items: list[dict]) -> dict:
    """Removes tracks from one of the account's playlists (writes to YouTube).

    Each item needs the `setVideoId` YouTube gave that row: a video id alone is
    ambiguous when the same song appears twice.
    """
    videos = [
        {"videoId": i.get("video_id", ""), "setVideoId": i.get("set_video_id", "")}
        for i in items
        if i.get("video_id") and i.get("set_video_id")
    ]
    if not videos:
        return {
            "ok": False,
            "error": "у этих строк нет id строки плейлиста — обнови список",
        }
    refused = _refuse_unowned(playlist_id)
    if refused:
        return refused
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    try:
        result = ytm.remove_playlist_items(playlist_id, videos)
    except Exception as e:
        log.exception("remove_playlist_items failed")
        return {"ok": False, "error": str(e)}
    ok, status = _edit_status(result)
    if not ok:
        return {"ok": False, "error": f"YouTube не подтвердил удаление: {status}"}

    gone = {v["setVideoId"] for v in videos}
    _apply_to_cache(playlist_id, lambda entry: _cache_after_remove(entry, gone))
    return {"ok": True, "error": ""}


def _cache_after_remove(entry: dict, gone: set[str]) -> None:
    tracks = entry.get("tracks") or []
    removed = sum(1 for t in tracks if t.get("set_video_id") in gone)
    entry["tracks"] = [t for t in tracks if t.get("set_video_id") not in gone]
    entry["total"] = max(0, int(entry.get("total") or 0) - (removed or len(gone)))


def move_playlist_item(playlist_id: str, set_video_id: str, before_set_video_id: str = "") -> dict:
    """Moves one row within a playlist. An empty `before` means "to the end":
    ytmusicapi sends ACTION_MOVE_VIDEO_BEFORE without a successor."""
    if not set_video_id:
        return {"ok": False, "error": "не указана строка для перемещения"}
    refused = _refuse_unowned(playlist_id)
    if refused:
        return refused
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    move = (set_video_id, before_set_video_id) if before_set_video_id else set_video_id
    try:
        result = ytm.edit_playlist(playlistId=playlist_id, moveItem=move)
    except Exception as e:
        log.exception("move_playlist_item failed")
        return {"ok": False, "error": str(e)}
    ok, status = _edit_status(result)
    if not ok:
        return {"ok": False, "error": f"YouTube не подтвердил перемещение: {status}"}
    _apply_to_cache(
        playlist_id,
        lambda entry: _cache_after_move(entry, set_video_id, before_set_video_id),
    )
    return {"ok": True, "error": ""}


def _cache_after_move(entry: dict, moved: str, before: str) -> None:
    tracks = entry.get("tracks") or []
    ids = [t.get("set_video_id") for t in tracks]
    if moved not in ids:
        return
    row = tracks.pop(ids.index(moved))
    if before and before in ids:
        target = [t.get("set_video_id") for t in tracks].index(before)
    else:
        target = len(tracks)
    tracks.insert(target, row)
    entry["tracks"] = tracks


def edit_playlist_meta(
    playlist_id: str, title: str = "", description: str = "", privacy: str = ""
) -> dict:
    """Renames a playlist, edits its description or privacy (writes to YouTube)."""
    if not (title or description or privacy):
        return {"ok": True, "error": ""}
    refused = _refuse_unowned(playlist_id)
    if refused:
        return refused
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    kwargs = {"playlistId": playlist_id}
    if title:
        kwargs["title"] = title
    if description:
        kwargs["description"] = description
    if privacy:
        kwargs["privacyStatus"] = privacy
    try:
        result = ytm.edit_playlist(**kwargs)
    except Exception as e:
        log.exception("edit_playlist failed")
        return {"ok": False, "error": str(e)}
    ok, status = _edit_status(result)
    if not ok:
        return {"ok": False, "error": f"YouTube отказал: {status}"}
    return {"ok": True, "error": ""}


def rate_song(video_id: str, rating: str = "LIKE") -> dict:
    """Thumbs up / no rating for one track. The response carries no status."""
    from ytmusicapi.models.content.enums import LikeStatus

    if not _VIDEO_ID_RE.match(video_id or ""):
        return {"ok": False, "error": "неверный id трека"}
    value = (rating or "LIKE").upper()
    if value not in ("LIKE", "INDIFFERENT"):
        return {"ok": False, "error": "поддерживаются только LIKE и INDIFFERENT"}
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    try:
        result = ytm.rate_song(video_id, getattr(LikeStatus, value))
    except Exception as e:
        log.exception("rate_song failed")
        return {"ok": False, "error": str(e)}
    ok, status = _edit_status(result, lenient=True)
    if not ok:
        return {"ok": False, "error": f"YouTube отказал: {status}"}
    return {"ok": True, "error": ""}


def save_playlist_to_library(playlist_id: str) -> dict:
    """Saves someone else's playlist into the account's library."""
    if _is_mix(playlist_id):
        return {"ok": False, "error": "микс нельзя сохранить в медиатеку", "code": "MIX"}
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    try:
        result = ytm.rate_playlist(playlist_id, "LIKE")
    except Exception as e:
        log.exception("rate_playlist failed")
        return {"ok": False, "error": str(e)}
    ok, status = _edit_status(result, lenient=True)
    if not ok:
        return {"ok": False, "error": f"YouTube не подтвердил сохранение: {status}"}
    invalidate_account_caches()
    return {"ok": True, "error": ""}


def copy_playlist(source_id: str, title: str = "") -> dict:
    """Copies a playlist into a new private one on the account."""
    if _is_mix(source_id):
        return {"ok": False, "error": "микс нельзя скопировать", "code": "MIX"}
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube", "id": ""}
    try:
        data = ytm.get_playlist(source_id, limit=None) or {}
    except Exception as e:
        log.exception("copy: cannot read the source")
        return {"ok": False, "error": f"не удалось прочитать плейлист: {e}", "id": ""}
    video_ids = [t.get("videoId") for t in data.get("tracks") or [] if t.get("videoId")]
    if not video_ids:
        return {"ok": False, "error": "в плейлисте нет треков", "id": ""}
    name = (title or "").strip() or f"{data.get('title') or 'Плейлист'} (копия)"
    try:
        created = ytm.create_playlist(
            name,
            "Скопировано в Fasttube",
            privacy_status="PRIVATE",
            video_ids=video_ids,
        )
    except Exception as e:
        log.exception("copy_playlist failed")
        return {"ok": False, "error": str(e), "id": ""}
    if not isinstance(created, str) or not created:
        return {"ok": False, "error": "YouTube не вернул id копии", "id": ""}
    invalidate_account_caches()
    return {"ok": True, "error": "", "id": created}


def remove_from_library(playlist_id: str) -> dict:
    """Drops a saved playlist out of the account's library (it stays on YouTube)."""
    if _is_mix(playlist_id):
        return {"ok": False, "error": "автоматический микс нельзя убрать из медиатеки", "code": "MIX"}
    ytm = _session_client()
    if ytm is None:
        return {"ok": False, "error": "нужен вход в аккаунт YouTube"}
    try:
        result = ytm.rate_playlist(playlist_id, "INDIFFERENT")
    except Exception as e:
        log.exception("rate_playlist failed")
        return {"ok": False, "error": str(e)}
    ok, status = _edit_status(result, lenient=True)
    if not ok:
        return {"ok": False, "error": f"YouTube не подтвердил удаление из медиатеки: {status}"}
    invalidate_account_caches()
    _forget_playlist(playlist_id)
    return {"ok": True, "error": ""}


def resolve_playlist_url(url: str) -> dict:
    """`...&list=PL…` → the playlist id, read through the same paging path."""
    match = re.search(r"[?&]list=([A-Za-z0-9_-]+)", url or "")
    if not match:
        return {"tracks": [], "total": 0, "error": "в ссылке нет параметра list=", "id": ""}
    playlist_id = match.group(1)
    page = playlist_tracks(playlist_id, offset=0, limit=PLAYLIST_PAGE)
    page["id"] = playlist_id
    return page


def set_proxy(url: str) -> dict:
    """Route every request through `url` ("" or "none" means no proxy).

    yt-dlp takes the proxy per call, ytmusicapi reads it from the environment
    when it builds a requests session — so the session client is rebuilt here
    instead of waiting for the next app start.
    """
    global _PROXY, _RESOLVE_OPTS
    from ytmusicapi import YTMusic
    url = (url or "").strip()
    proxy = "" if url.lower() in ("", "none", "off", "direct") else url

    _PROXY = proxy or None
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        if proxy:
            os.environ[name] = proxy
        else:
            os.environ.pop(name, None)
    if proxy:
        YT_DLP_OPTS["proxy"] = proxy
        log.info("proxy set to %s", proxy)
    else:
        YT_DLP_OPTS.pop("proxy", None)
        log.info("proxy disabled")
    _RESOLVE_OPTS = dict(YT_DLP_OPTS)

    # An existing client keeps its own requests.Session, with the old proxy.
    with _session_lock:
        if _session is not None and _session.get("state") == "active":
            try:
                headers = _browser_headers(
                    _session["cookie"], _session["user_agent"], _session["authuser"]
                )
                _session["client"] = YTMusic(auth=headers, language=YTM_LANGUAGE)
            except Exception as e:  # keep the old client rather than sign out
                log.warning("could not rebuild the session client: %s", e)

    invalidate_account_caches()
    return {"ok": True, "error": ""}


def invalidate_account_caches() -> None:
    """Drop per-account library data (playlists, likes) after a session change.

    Must be called on login and logout: cached lists belong to the account
    that was signed in when they were fetched.
    """
    with _lib_lock:
        _LIB_CACHE.clear()
    with _home_lock:
        _HOME_CACHE.clear()
    with _playlist_lock:
        _OWNED.clear()
    # Also bumps the generation, so a background playlist load that started
    # before this reset cannot store the previous account's tracks.
    invalidate_playlists()


_load_resolve_cache()


QUALITY_STANDARD = "standard"
QUALITY_HIGH = "high"
QUALITIES = (QUALITY_STANDARD, QUALITY_HIGH)


def _itag(format_id) -> int:
    """yt-dlp format id -> itag number.

    Since yt-dlp 2025 the id can be composite, for example 140-drc or 140-0: the
    number in front of the dash is the itag the rest of the app cares about, and
    a bare int() on the whole string kills resolve_url.
    """
    head = str(format_id or "").split("-", 1)[0].strip()
    try:
        return int(head)
    except ValueError:
        return 0


def resolve_url(
    video_id: str,
    cookie_header: str | None = None,
    quality: str = QUALITY_STANDARD,
    force: bool = False,
) -> dict:
    """Timed wrapper around the extraction; see `_resolve_url_impl`."""
    started = time.perf_counter()
    key = f"{video_id}@{quality}@{'account' if cookie_header else 'anon'}"
    cached = False
    if not force:
        with _resolve_lock:
            hit = _RESOLVE_CACHE.get(key)
            cached = bool(hit and time.time() - hit[0] < _RESOLVE_TTL)
    result = _resolve_url_impl(video_id, cookie_header, quality, force)
    log.info(
        "[perf] resolve_ms=%.1f video=%s quality=%s cached=%s ok=%s",
        (time.perf_counter() - started) * 1000.0,
        video_id,
        quality,
        cached,
        bool(result.get("ok")),
    )
    return result


def _resolve_url_impl(
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

    with yt_dlp.YoutubeDL(_resolve_opts()) as ydl:
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
                if _itag(f.get("format_id")) == 140 or (f.get("tbr") or 0) <= 132
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
            "itag": _itag(chosen.get("format_id")),
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


# Thumbnail targets: what the app actually draws, with room for a high-DPI
# screen. YouTube offers 60/120/180/320/480/640…
THUMB_ROW = 120
THUMB_CARD = 320
THUMB_PLAYER = 544


def pick_thumbnail(thumbnails, target_px: int) -> str:
    """Smallest thumbnail at least `target_px` wide, otherwise the biggest one.

    Blindly taking the last entry meant downloading 640 px covers for 40 px rows.
    Entries without dimensions are only used when nothing else is available.
    """
    candidates = []
    for entry in thumbnails or []:
        if not isinstance(entry, dict):
            continue
        url = entry.get("url") or ""
        if not url:
            continue
        size = max(int(entry.get("width") or 0), int(entry.get("height") or 0))
        candidates.append((size, url))
    if not candidates:
        return ""
    if not any(size for size, _ in candidates):
        # No metadata at all: keep the old "last one is the biggest" guess.
        return candidates[-1][1]
    big_enough = [c for c in candidates if c[0] >= target_px]
    if big_enough:
        return min(big_enough, key=lambda c: c[0])[1]
    return max(candidates, key=lambda c: c[0])[1]


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
        "thumbnail": pick_thumbnail(
            thumbnails, THUMB_ROW if kind in ("song", "video") else THUMB_CARD
        ),
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
    artist = ", ".join(
        a.get("name", "") for a in (result.get("artists") or []) if a.get("name")
    )
    return {
        "kind": "song",
        "id": video_id,
        "title": result.get("title") or "",
        "subtitle": artist,
        "thumbnail": pick_thumbnail(result.get("thumbnails"), THUMB_ROW) or fallback_thumbnail,
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

    cover = pick_thumbnail(data.get("thumbnails"), THUMB_PLAYER)
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

    cover = pick_thumbnail(data.get("thumbnails"), THUMB_PLAYER)

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
        albums.append(
            {
                "kind": "album",
                "id": album_id,
                "title": entry.get("title") or "",
                "subtitle": " • ".join(
                    str(part) for part in (entry.get("year"), entry.get("type")) if part
                ),
                "thumbnail": pick_thumbnail(entry.get("thumbnails"), THUMB_CARD),
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
    thumbnail = pick_thumbnail(entry.get("thumbnails"), THUMB_CARD)

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
                    "thumbnail": pick_thumbnail(r.get("thumbnails"), THUMB_ROW),
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
                    "thumbnail": pick_thumbnail(p.get("thumbnails"), THUMB_ROW),
                }
                for p in items
                if p.get("playlistId")
            ]
            return {"has_auth": True, "hint": "", "playlists": playlists}
        except Exception:
            log.exception("get_library_playlists failed")
            return {"has_auth": True, "hint": "", "playlists": [], "error": "failed to load playlists"}

    return _cached_lib("user_playlists", 600, load)


# A mix is endless by nature: the app takes the first N and stops there.
MIX_TRACK_LIMIT = 100

# One page is what the UI needs to start drawing; the rest of a long playlist is
# fetched by a background thread and served from memory afterwards.
PLAYLIST_PAGE = 100
_PLAYLIST_TTL = 4 * 3600

_playlist_lock = threading.Lock()
# "playlist:<id>" / "liked" -> {"tracks", "total", "owned", "complete", "at",
# "loading", "done"}
_PLAYLISTS: dict[str, dict] = {}
# playlist id -> True/False. Ownership is a fact about the account, not about the
# page in hand: an empty playlist has no page to read it from, and a refresh must
# not forget it.
_OWNED: dict[str, bool] = {}

# Bumped by every account invalidation. A background load remembers the value
# it started with, so a fetch that began before a logout/session change cannot
# write its (other account's) tracks into the fresh cache.
_playlist_generation = 0


def _generation() -> int:
    with _playlist_lock:
        return _playlist_generation


def _playlist_state(key: str) -> dict | None:
    with _playlist_lock:
        entry = _PLAYLISTS.get(key)
        if entry is not None and time.time() - entry["at"] < _PLAYLIST_TTL:
            return entry
        return None


def _store_playlist(
    key: str,
    tracks: list[dict],
    total: int,
    complete: bool,
    owned: bool = False,
    generation: int | None = None,
) -> None:
    with _playlist_lock:
        if generation is not None and generation != _playlist_generation:
            # The cache was cleared while this fetch was in flight.
            return
        previous = _PLAYLISTS.get(key)
        # Keep the same event across a replacement: a request already waiting on
        # it must be woken by the background load, not by a copy.
        done = (previous or {}).get("done") or threading.Event()
        if complete:
            done.set()
        _PLAYLISTS[key] = {
            "tracks": tracks,
            "total": total,
            "owned": owned,
            "complete": complete,
            "at": time.time(),
            "loading": False,
            "done": done,
        }


def _ensure_full_load(key: str, fetch_all) -> None:
    """Fetch the remaining pages once, in the background."""
    with _playlist_lock:
        entry = _PLAYLISTS.get(key)
        if entry is None or entry["complete"] or entry["loading"]:
            return
        entry["loading"] = True

    generation = _generation()

    def run() -> None:
        try:
            tracks, total, owned = fetch_all()
        except Exception as e:  # keep the first page usable, cache nothing
            log.warning("full playlist load failed (%s): %s", key, e)
            with _playlist_lock:
                entry = _PLAYLISTS.get(key)
                if entry is not None:
                    entry["loading"] = False
                    entry["done"].set()
            return
        _store_playlist(key, tracks, total, True, owned, generation)

    threading.Thread(target=run, name="playlist-load", daemon=True).start()


def _page(tracks: list[dict], offset: int, limit: int) -> list[dict]:
    return tracks[offset : offset + limit]


def _wait_for_more(key: str, page: list[dict], offset: int, limit: int, timeout: float = 3.0):
    """A short page while the background fetch is still running would look like
    the end of the list: wait for it instead of handing over a truncated page."""
    entry = _playlist_state(key)
    if entry is None or entry["complete"] or len(page) >= limit:
        return page
    done = entry.get("done")
    if done is not None and done.wait(timeout):
        entry = _playlist_state(key) or entry
        return _page(entry["tracks"], offset, limit)
    return page


def invalidate_playlists() -> None:
    """Drop the paged caches (a session change means other lists)."""
    global _playlist_generation
    with _playlist_lock:
        _PLAYLISTS.clear()
        _playlist_generation += 1


def _forget_playlist(playlist_id: str) -> None:
    """Drops one playlist's page cache (after it was edited or deleted)."""
    with _playlist_lock:
        _PLAYLISTS.pop(f"playlist:{playlist_id}", None)
        _OWNED.pop(playlist_id, None)


def _is_mix(playlist_id: str) -> bool:
    """Auto-generated mixes and radios (RDTMAK…, RDCLAK…, RDAMVM…).

    Their page has no playlist structure at all — get_playlist dies on a missing
    `contents` path — while the watch-playlist endpoint returns exactly the list
    of tracks the mix is.
    """
    return playlist_id.startswith("RD")


def _playlist_rows(data: dict) -> list[dict]:
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
                "thumbnail": pick_thumbnail(t.get("thumbnails"), THUMB_ROW),
                "set_video_id": t.get("setVideoId") or "",
                "like_status": t.get("likeStatus") or "",
            }
        )
    return tracks


def _fetch_playlist_page(
    ytm, playlist_id: str, limit: int
) -> tuple[list[dict], int, bool]:
    """One request: `limit` tracks, the playlist's own track count, and whether
    the account owns it (a saved foreign playlist cannot be edited)."""
    if _is_mix(playlist_id):
        try:
            data = ytm.get_watch_playlist(playlistId=playlist_id, limit=MIX_TRACK_LIMIT) or {}
        except Exception as e:
            log.warning("mix %s via watch_playlist failed: %s", playlist_id, e)
            data = {}
        tracks = _playlist_rows(data)
        if not tracks:
            # Some mixes answer the watch endpoint with nothing usable.
            data = ytm.get_playlist(playlist_id, limit=limit) or {}
            tracks = _playlist_rows(data)
        return tracks, len(tracks), False
    try:
        data = ytm.get_playlist(playlist_id, limit=limit) or {}
    except Exception as e:
        # An empty playlist (or one just created) has no "contents" to parse.
        if _looks_empty(e):
            log.info("playlist %s reads as empty: %s", playlist_id, e)
            entry = _playlist_state(f"playlist:{playlist_id}") or {}
            owned = _OWNED.get(playlist_id, bool(entry.get("owned")))
            return [], 0, owned
        raise
    tracks = _playlist_rows(data)
    total = int(data.get("trackCount") or 0) or len(tracks)
    owned = bool(data.get("owned"))
    with _playlist_lock:
        _OWNED[playlist_id] = owned
    return tracks, max(total, len(tracks)), owned


def _looks_empty(error: Exception) -> bool:
    text = str(error)
    return "Unable to find 'contents'" in text or isinstance(error, KeyError)


def _fetch_playlist_all(ytm, playlist_id: str) -> tuple[list[dict], int, bool]:
    if _is_mix(playlist_id):
        return _fetch_playlist_page(ytm, playlist_id, MIX_TRACK_LIMIT)
    try:
        data = ytm.get_playlist(playlist_id, limit=None) or {}
    except Exception as e:
        if _looks_empty(e):
            log.info("playlist %s reads as empty on the full load: %s", playlist_id, e)
            return [], 0, False
        raise
    tracks = _playlist_rows(data)
    total = max(int(data.get("trackCount") or 0), len(tracks))
    return tracks, total, bool(data.get("owned"))


def playlist_tracks(
    playlist_id: str,
    offset: int = 0,
    limit: int = PLAYLIST_PAGE,
    refresh: bool = False,
) -> dict:
    """One page of a playlist.

    The first call asks YouTube for a single page and starts a background fetch
    of the rest, so the UI can draw immediately; later calls slice memory.
    `refresh` throws that copy away first: revalidation must see YouTube, not a
    page that may be hours old.
    """
    if _session_client() is None:
        return {"tracks": [], "error": "sign in to YouTube in the app", "total": 0}

    offset = max(0, int(offset or 0))
    limit = max(1, min(int(limit or PLAYLIST_PAGE), 500))
    key = f"playlist:{playlist_id}"
    if refresh:
        with _playlist_lock:
            _PLAYLISTS.pop(key, None)
    entry = _playlist_state(key)
    if entry is None:
        try:
            ytm = _session_client()
            if ytm is None:
                return {"tracks": [], "error": "not signed in", "total": 0}
            tracks, total, owned = _fetch_playlist_page(ytm, playlist_id, PLAYLIST_PAGE)
        except Exception as e:
            log.exception("get_playlist failed")
            return {"tracks": [], "error": str(e), "total": 0, "owned": False}
        complete = len(tracks) >= total
        _store_playlist(key, tracks, total, complete, owned, _generation())
        if not complete:
            _ensure_full_load(key, lambda: _fetch_playlist_all(ytm, playlist_id))
        entry = _playlist_state(key) or {"tracks": tracks, "total": total, "owned": owned}

    page = _page(entry["tracks"], offset, limit)
    return {
        "tracks": _wait_for_more(key, page, offset, limit),
        "error": "",
        "total": int(entry["total"]),
        "owned": bool(entry.get("owned")),
    }


def liked_songs(offset: int = 0, limit: int = PLAYLIST_PAGE) -> dict:
    """Same paging as playlists, over the account's liked songs."""
    if _session_client() is None:
        return {"tracks": [], "error": "sign in to YouTube in the app", "total": 0}

    offset = max(0, int(offset or 0))
    limit = max(1, min(int(limit or PLAYLIST_PAGE), 500))
    key = "liked"
    entry = _playlist_state(key)
    if entry is None:
        try:
            ytm = _session_client()
            if ytm is None:
                return {"tracks": [], "error": "not signed in", "total": 0}
            first = ytm.get_liked_songs(limit=PLAYLIST_PAGE) or {}
            tracks = _playlist_rows(first)
            total = int(first.get("trackCount") or 0)
        except Exception as e:
            log.exception("get_liked_songs failed")
            return {"tracks": [], "error": str(e), "total": 0}
        # YouTube does not always report a count here: 0 means "keep asking
        # until a short page comes back".
        _store_playlist(key, tracks, max(total, len(tracks)), False, True, _generation())

        def fetch_all() -> tuple[list[dict], int, bool]:
            ytm2 = _session_client()
            if ytm2 is None:
                raise RuntimeError("not signed in")
            data = ytm2.get_liked_songs(limit=None) or {}
            rows = _playlist_rows(data)
            return rows, len(rows), True

        _ensure_full_load(key, fetch_all)
        entry = _playlist_state(key) or {"tracks": tracks, "total": total, "owned": True}

    page = _page(entry["tracks"], offset, limit)
    return {
        "tracks": _wait_for_more(key, page, offset, limit),
        "error": "",
        "total": int(entry["total"]),
        "owned": True,
    }
