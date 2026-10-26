"""gRPC server for the YouTube catalog service."""

import hmac
import logging
import os
import sys
import threading
from concurrent import futures

import grpc

sys.path.insert(0, os.path.dirname(__file__))

from ytapi.pb import player_api_pb2, player_api_pb2_grpc  # noqa: E402
from ytapi import net, youtube  # noqa: E402

net.force_ipv4_dns()

log = logging.getLogger("ytapi.server")

VERSION = "0.1.0"
# 0 lets the OS pick a free port; the chosen one is printed as `PORT=<n>` on
# stdout, which is the handshake the Rust supervisor reads.
PORT = int(os.environ.get("YTAPI_PORT", "0"))

API_TOKEN = os.environ.get("FASTTUBE_API_TOKEN", "")


class _TokenInterceptor(grpc.ServerInterceptor):
    """Rejects calls without the shared app<->sidecar token, so other local
    processes cannot use the YouTube session via the open port."""

    def intercept_service(self, continuation, handler_call_details):
        md = dict(handler_call_details.invocation_metadata or [])
        got = md.get("x-fasttube-token")
        if not isinstance(got, str) or not hmac.compare_digest(
            got.encode("utf-8"), API_TOKEN.encode("utf-8")
        ):

            def deny(request, context):
                context.abort(grpc.StatusCode.UNAUTHENTICATED, "invalid or missing token")

            return grpc.unary_unary_rpc_method_handler(deny)
        return continuation(handler_call_details)

def _to_pb_items(items: list[dict]) -> list[player_api_pb2.SearchItem]:
    return [
        player_api_pb2.SearchItem(
            kind=item.get("kind", ""),
            id=item.get("id", ""),
            title=item.get("title", ""),
            subtitle=item.get("subtitle", ""),
            thumbnail=item.get("thumbnail", ""),
            duration_secs=item.get("duration_secs", 0.0),
            video_type=item.get("video_type", ""),
            browse_id=item.get("browse_id", ""),
            playlist_id=item.get("playlist_id", ""),
        )
        for item in items or []
    ]


def _to_pb_section(section: dict) -> player_api_pb2.SearchSection:
    return player_api_pb2.SearchSection(
        title=section.get("title", ""),
        kind=section.get("kind", ""),
        items=_to_pb_items(section.get("items", [])),
    )


def _to_pb_track(t: dict) -> player_api_pb2.Track:
    return player_api_pb2.Track(
        video_id=t.get("video_id", ""),
        title=t.get("title", ""),
        artist=t.get("artist", ""),
        album=t.get("album", ""),
        duration_secs=t.get("duration_secs", 0.0),
        thumbnail=t.get("thumbnail", ""),
    )


_SESSION_STATES = {
    "signed_out": player_api_pb2.SIGNED_OUT,
    "active": player_api_pb2.ACTIVE,
    "expired": player_api_pb2.EXPIRED,
}


def _to_session_status(result: dict) -> player_api_pb2.SessionStatus:
    return player_api_pb2.SessionStatus(
        state=_SESSION_STATES.get(result.get("state", ""), player_api_pb2.SESSION_STATE_UNSPECIFIED),
        account_name=result.get("account_name", ""),
        error=result.get("error", ""),
    )


class PlayerApiServicer(player_api_pb2_grpc.PlayerApiServicer):
    def Health(self, request, context):
        ytdlp_ready = True
        try:
            import yt_dlp  # noqa: F401
        except Exception:
            ytdlp_ready = False
        return player_api_pb2.HealthResponse(
            status="ok",
            version=VERSION,
            ytdlp_ready=ytdlp_ready,
            auth_ok=youtube.has_auth(),
        )

    def Search(self, request, context):
        try:
            result = youtube.search_music(
                request.query, request.filter, request.limit or 20
            )
        except Exception as e:  # search_music reports errors, this is a bug path
            log.exception("search failed")
            return player_api_pb2.SearchResponse(error=str(e))
        return player_api_pb2.SearchResponse(
            sections=[_to_pb_section(s) for s in result.get("sections", [])],
            error=result.get("error", ""),
        )

    def GetAlbum(self, request, context):
        try:
            data = youtube.album(request.browse_id)
        except Exception as e:
            log.exception("get_album failed")
            return player_api_pb2.AlbumResponse(error=str(e))
        tracks = data.get("sections") or []
        section = tracks[0] if tracks else {"title": "", "kind": "song", "items": []}
        return player_api_pb2.AlbumResponse(
            title=data.get("title", ""),
            subtitle=data.get("subtitle", ""),
            thumbnail=data.get("thumbnail", ""),
            tracks=_to_pb_section(section),
            error=data.get("error", ""),
        )

    def GetHome(self, request, context):
        try:
            data = youtube.get_home(request.limit or 4)
        except Exception as e:
            log.exception("get_home failed")
            return player_api_pb2.HomeResponse(error=str(e))
        return player_api_pb2.HomeResponse(
            shelves=[
                player_api_pb2.Shelf(
                    title=shelf.get("title", ""),
                    kind=shelf.get("kind", ""),
                    items=_to_pb_items(shelf.get("items", [])),
                )
                for shelf in data.get("shelves", [])
            ],
            error=data.get("error", ""),
        )

    def GetArtist(self, request, context):
        try:
            data = youtube.artist(request.browse_id)
        except Exception as e:
            log.exception("get_artist failed")
            return player_api_pb2.ArtistResponse(error=str(e))
        return player_api_pb2.ArtistResponse(
            name=data.get("name", ""),
            subtitle=data.get("subtitle", ""),
            thumbnail=data.get("thumbnail", ""),
            sections=[_to_pb_section(s) for s in data.get("sections") or []],
            error=data.get("error", ""),
        )

    def Suggest(self, request, context):
        try:
            return player_api_pb2.SuggestResponse(
                suggestions=youtube.suggestions(request.query)
            )
        except Exception:
            log.exception("suggest failed")
            return player_api_pb2.SuggestResponse()

    def ResolveUrl(self, request, context):
        try:
            cookies = youtube.session_cookie() if request.use_account else None
            result = youtube.resolve_url(
                request.video_id,
                cookies,
                request.audio_quality or youtube.QUALITY_STANDARD,
                force=request.force,
            )
            resp = player_api_pb2.ResolveResponse(
                ok=result.get("ok", False),
                error=result.get("error", ""),
                stream_url=result.get("stream_url", ""),
                mime_type=result.get("mime_type", ""),
                content_length=result.get("content_length", 0),
                itag=result.get("itag", 0),
            )
            if result.get("track"):
                resp.track.CopyFrom(_to_pb_track(result["track"]))
            return resp
        except Exception as e:
            log.exception("resolve failed")
            context.abort(grpc.StatusCode.INTERNAL, str(e))

    def PrefetchResolve(self, request, context):
        try:
            count = youtube.prefetch_resolve(
                list(request.ids),
                request.audio_quality or youtube.QUALITY_STANDARD,
                use_account=request.use_account,
            )
            return player_api_pb2.PrefetchResolveResponse(resolved=count)
        except Exception as e:
            log.exception("prefetch resolve failed")
            context.abort(grpc.StatusCode.INTERNAL, str(e))

    def Recommend(self, request, context):
        try:
            result = youtube.related_tracks(request.video_id, limit=request.limit or 20)
            return player_api_pb2.RecommendResponse(
                tracks=[_to_pb_track(t) for t in result.get("tracks", [])]
            )
        except Exception as e:
            log.exception("recommend failed")
            context.abort(grpc.StatusCode.INTERNAL, str(e))

    def GetLyrics(self, request, context):
        result = youtube.get_lyrics(request.artist, request.title, request.duration_secs)
        return player_api_pb2.LyricsResponse(
            ok=result.get("ok", False),
            synced=result.get("synced", ""),
            plain=result.get("plain", ""),
            error=result.get("error", ""),
        )

    def GetUserPlaylists(self, request, context):
        result = youtube.user_playlists()
        return player_api_pb2.UserPlaylistsResponse(
            playlists=[
                player_api_pb2.UserPlaylist(
                    id=p.get("id", ""),
                    name=p.get("name", ""),
                    track_count=p.get("track_count", 0),
                    thumbnail=p.get("thumbnail", ""),
                )
                for p in result.get("playlists", [])
            ],
            has_auth=result.get("has_auth", False),
            hint=result.get("hint", ""),
            error=result.get("error", ""),
        )

    def GetPlaylistTracks(self, request, context):
        result = youtube.playlist_tracks(
            request.id, offset=request.offset, limit=request.limit
        )
        return player_api_pb2.PlaylistTracksResponse(
            tracks=[_to_pb_track(t) for t in result.get("tracks", [])],
            error=result.get("error", ""),
            total=result.get("total", 0),
        )

    def GetLikedSongs(self, request, context):
        result = youtube.liked_songs(offset=request.offset, limit=request.limit)
        return player_api_pb2.PlaylistTracksResponse(
            tracks=[_to_pb_track(t) for t in result.get("tracks", [])],
            error=result.get("error", ""),
            total=result.get("total", 0),
        )

    def SetSession(self, request, context):
        result = youtube.set_session(
            request.cookie, request.user_agent, request.x_goog_authuser
        )
        return _to_session_status(result)

    def ParseAuthHeaders(self, request, context):
        try:
            fields = youtube.parse_auth_headers(request.text)
        except Exception as e:
            context.abort(grpc.StatusCode.INVALID_ARGUMENT, str(e))
            return player_api_pb2.SetSessionRequest()
        return player_api_pb2.SetSessionRequest(
            cookie=fields["cookie"],
            user_agent=fields["user_agent"],
            x_goog_authuser=fields["x_goog_authuser"],
        )

    def ValidateSession(self, request, context):
        return _to_session_status(youtube.validate_session())

    def ClearSession(self, request, context):
        return _to_session_status(youtube.clear_session())


def serve():
    logging.basicConfig(level=logging.INFO)
    if not API_TOKEN:
        # Fail closed: an unauthenticated sidecar would expose the YouTube
        # session to every process on the machine.
        log.error("FASTTUBE_API_TOKEN is not set; refusing to start")
        sys.exit(2)

    server = grpc.server(
        futures.ThreadPoolExecutor(max_workers=8),
        interceptors=[_TokenInterceptor()],
    )
    player_api_pb2_grpc.add_PlayerApiServicer_to_server(PlayerApiServicer(), server)
    port = server.add_insecure_port(f"127.0.0.1:{PORT}")
    if port == 0:
        log.error("could not bind 127.0.0.1:%s", PORT)
        sys.exit(1)
    server.start()
    # Handshake: the first stdout line tells the Rust supervisor where to dial.
    print(f"PORT={port}", flush=True)
    log.info("ytapi listening on 127.0.0.1:%s", port)
    if os.environ.get("FASTTUBE_PARENT_PIPE") == "1":
        _watch_parent_pipe()
    _warm_up_ytdlp()
    server.wait_for_termination()


def _watch_parent_pipe() -> None:
    """Exits when the app closes our stdin.

    The Rust supervisor holds the write end of this pipe. If the app dies
    without killing us (crash, task manager), the pipe hits EOF and we must
    not linger as an orphaned process nobody can authenticate against.
    """

    def watch():
        try:
            while True:
                if sys.stdin.readline() == "":
                    log.info("stdin closed (app exited); shutting down")
                    break
        except Exception:
            log.exception("stdin watchdog failed; shutting down")
        os._exit(0)

    threading.Thread(target=watch, name="parent-watchdog", daemon=True).start()


def _warm_up_ytdlp() -> None:
    """Pre-fetches yt-dlp's player data in the background so the first real
    resolve is fast instead of paying the full extraction cost."""

    def warm():
        try:
            import yt_dlp

            opts = dict(youtube.YT_DLP_OPTS)
            with yt_dlp.YoutubeDL(opts) as ydl:
                ydl.extract_info("dQw4w9WgXcQ", download=False)
            log.info("yt-dlp warm-up done")
        except Exception as e:
            log.warning("yt-dlp warm-up failed: %s", e)

    threading.Thread(target=warm, name="ytdlp-warmup", daemon=True).start()


if __name__ == "__main__":
    serve()
