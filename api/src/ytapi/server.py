"""gRPC server for the YouTube catalog service."""

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
PORT = int(os.environ.get("YTAPI_PORT", "50051"))

COOKIES_FILE = os.environ.get(
    "YTAPI_COOKIES",
    os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(__file__))), "cookies.txt"),
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
            tracks = youtube.search(
                request.query, limit=request.limit or 20, music=request.music
            )
            return player_api_pb2.SearchResponse(
                tracks=[_to_pb_track(t) for t in tracks]
            )
        except Exception as e:
            log.exception("search failed")
            context.abort(grpc.StatusCode.INTERNAL, str(e))

    def ResolveUrl(self, request, context):
        try:
            cookies = COOKIES_FILE if os.path.exists(COOKIES_FILE) else None
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
        result = youtube.playlist_tracks(request.id)
        return player_api_pb2.PlaylistTracksResponse(
            tracks=[_to_pb_track(t) for t in result.get("tracks", [])],
            error=result.get("error", ""),
        )

    def GetLikedSongs(self, request, context):
        result = youtube.liked_songs()
        return player_api_pb2.PlaylistTracksResponse(
            tracks=[_to_pb_track(t) for t in result.get("tracks", [])],
            error=result.get("error", ""),
        )

    def StartOAuth(self, request, context):
        result = youtube.start_oauth(request.client_id, request.client_secret)
        return player_api_pb2.StartOAuthResponse(
            ok=result.get("ok", False),
            error=result.get("error", ""),
            url=result.get("url", ""),
        )

    def PollOAuth(self, request, context):
        result = youtube.poll_oauth()
        return player_api_pb2.PollOAuthResponse(
            done=result.get("done", False),
            error=result.get("error", ""),
        )

    def SetBrowserAuth(self, request, context):
        result = youtube.set_browser_auth(request.auth_text)
        return player_api_pb2.SetBrowserAuthResponse(
            ok=result.get("ok", False),
            error=result.get("error", ""),
        )


def serve():
    logging.basicConfig(level=logging.INFO)
    _warm_up_ytdlp()
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
    player_api_pb2_grpc.add_PlayerApiServicer_to_server(PlayerApiServicer(), server)
    server.add_insecure_port(f"127.0.0.1:{PORT}")
    server.start()
    log.info("ytapi listening on 127.0.0.1:%s", PORT)
    server.wait_for_termination()


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
