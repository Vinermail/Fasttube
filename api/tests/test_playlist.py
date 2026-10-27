"""Playlist loading: mixes go through get_watch_playlist, playlists do not."""

import pytest

from ytapi import youtube


@pytest.fixture(autouse=True)
def _fresh_caches():
    youtube.invalidate_account_caches()
    yield
    youtube.invalidate_account_caches()


def track(video_id="vid00000001", title="Track"):
    return {
        "videoId": video_id,
        "title": title,
        "artists": [{"name": "Artist", "id": "UCx"}],
        "album": {"name": "Album", "id": "MPREb_x"},
        "duration_seconds": 123,
        "thumbnails": [{"url": "https://i.ytimg.com/a=s200"}],
    }


class FakeClient:
    """Records which endpoint answered and with which arguments."""

    def __init__(self, watch=None, playlist=None, watch_error=None, playlist_error=None):
        self.calls = []
        self._watch = watch if watch is not None else {"tracks": [track()]}
        self._playlist = playlist if playlist is not None else {"tracks": [track()]}
        self._watch_error = watch_error
        self._playlist_error = playlist_error

    def get_watch_playlist(self, playlistId=None, limit=None):  # noqa: N803 - ytmusicapi API
        self.calls.append(("watch", playlistId, limit))
        if self._watch_error:
            raise RuntimeError(self._watch_error)
        return self._watch

    def get_playlist(self, playlist_id, limit=None):
        self.calls.append(("playlist", playlist_id, limit))
        if self._playlist_error:
            raise RuntimeError(self._playlist_error)
        return self._playlist


def use_client(monkeypatch, client):
    monkeypatch.setattr(youtube, "_session_client", lambda: client)


def test_a_mix_is_opened_through_watch_playlist(monkeypatch):
    client = FakeClient()
    use_client(monkeypatch, client)

    result = youtube.playlist_tracks("RDTMAK5uy_testRadioaaaaaaaaaa")

    assert client.calls == [("watch", "RDTMAK5uy_testRadioaaaaaaaaaa", youtube.MIX_TRACK_LIMIT)]
    assert result["error"] == ""
    assert result["tracks"] == [
        {
            "video_id": "vid00000001",
            "title": "Track",
            "artist": "Artist",
            "album": "Album",
            "duration_secs": 123.0,
            "thumbnail": "https://i.ytimg.com/a=s200",
            # YouTube's per-item id: needed to remove this row later.
            "set_video_id": "",
        }
    ]


def test_a_mix_falls_back_to_the_playlist_endpoint(monkeypatch):
    client = FakeClient(watch_error="No content returned by the server")
    use_client(monkeypatch, client)

    result = youtube.playlist_tracks("RDAMVMtestMixAaaaaaaaaa")

    assert [c[0] for c in client.calls] == ["watch", "playlist"]
    assert result["error"] == ""
    assert len(result["tracks"]) == 1


def test_an_empty_mix_answer_also_falls_back(monkeypatch):
    client = FakeClient(watch={"tracks": []})
    use_client(monkeypatch, client)

    youtube.playlist_tracks("RDCLAK5uy_testMixBbbbbbbbbb")

    assert [c[0] for c in client.calls] == ["watch", "playlist"]


def test_a_real_playlist_still_uses_get_playlist(monkeypatch):
    client = FakeClient()
    use_client(monkeypatch, client)

    youtube.playlist_tracks("PLtestPlaylistAaaaaaaaaaaaaaaa")

    # The first page only: the rest is fetched in the background.
    assert client.calls == [("playlist", "PLtestPlaylistAaaaaaaaaaaaaaaa", 100)]


def test_a_failing_playlist_reports_the_error(monkeypatch):
    client = FakeClient(playlist_error="boom")
    use_client(monkeypatch, client)

    result = youtube.playlist_tracks("PLtestPlaylistAaaaaaaaaaaaaaaa")

    assert result["tracks"] == []
    assert "boom" in result["error"]


def test_without_a_session_nothing_is_requested(monkeypatch):
    use_client(monkeypatch, None)

    result = youtube.playlist_tracks("PLtestPlaylistAaaaaaaaaaaaaaaa")

    assert result["tracks"] == []
    assert "sign in" in result["error"]
