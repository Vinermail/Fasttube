"""Paged playlists and likes: one page first, the rest in the background."""

import threading
import time

import ytapi.youtube as youtube


def rows(count, start=0):
    return [
        {
            "videoId": f"vid{i:08d}",
            "title": f"Track {i}",
            "artists": [{"name": f"Artist {i}"}],
            "album": {"name": "Album"},
            "duration_seconds": 100 + i,
            "thumbnails": [{"url": f"https://i.ytimg.com/{i}.jpg", "width": 120, "height": 120}],
        }
        for i in range(start, start + count)
    ]


class FakeClient:
    """Serves a fixed playlist; `get_playlist(limit=None)` returns everything."""

    def __init__(self, total=250, fail=False):
        self.calls = []
        self.total = total
        self.fail = fail
        self.all_fetched = threading.Event()

    def get_playlist(self, playlist_id, limit=None):
        self.calls.append((playlist_id, limit))
        if self.fail:
            raise RuntimeError("boom")
        if limit is None:
            self.all_fetched.set()
            return {"tracks": rows(self.total), "trackCount": self.total}
        return {
            "tracks": rows(min(limit, self.total)),
            "trackCount": self.total,
        }

    def get_liked_songs(self, limit=None):
        self.calls.append(("liked", limit))
        if self.fail:
            raise RuntimeError("boom")
        if limit is None:
            self.all_fetched.set()
            return {"tracks": rows(200)}
        return {"tracks": rows(min(limit, 200))}

    def get_watch_playlist(self, playlistId, limit=100):
        self.calls.append(("watch", playlistId))
        return {"tracks": rows(min(limit, self.total))}


def use_client(monkeypatch, client):
    monkeypatch.setattr(youtube, "_session_client", lambda: client)
    monkeypatch.setattr(youtube, "_PLAYLISTS", {})


def test_the_first_call_asks_for_one_page_only(monkeypatch):
    client = FakeClient(total=250)
    use_client(monkeypatch, client)

    page = youtube.playlist_tracks("PL1", offset=0, limit=100)

    assert len(page["tracks"]) == 100
    assert page["total"] == 250
    assert client.calls[0] == ("PL1", 100), "no full fetch on the first call"


def test_the_rest_arrives_in_the_background_and_is_served_from_memory(monkeypatch):
    client = FakeClient(total=250)
    use_client(monkeypatch, client)

    youtube.playlist_tracks("PL1", offset=0, limit=100)
    assert client.all_fetched.wait(5), "the background fetch never ran"

    # The background thread replaces the entry; give it a moment to store.
    for _ in range(100):
        page = youtube.playlist_tracks("PL1", offset=100, limit=100)
        if len(page["tracks"]) == 100:
            break
        time.sleep(0.02)

    assert len(page["tracks"]) == 100
    assert page["tracks"][0]["video_id"] == "vid00000100"
    assert page["total"] == 250
    assert ("PL1", 100) in client.calls[1:] or ("PL1", None) in client.calls


class SlowClient(FakeClient):
    """The full fetch takes a moment, like a real continuation chain."""

    def get_playlist(self, playlist_id, limit=None):
        if limit is None:
            time.sleep(0.25)
        return super().get_playlist(playlist_id, limit)


def test_a_short_page_waits_for_the_background_fetch(monkeypatch):
    client = SlowClient(total=250)
    use_client(monkeypatch, client)

    youtube.playlist_tracks("PLs", offset=0, limit=100)
    page = youtube.playlist_tracks("PLs", offset=100, limit=100)

    # Without the wait this came back with 0-99 tracks and the UI concluded the
    # playlist had ended.
    assert len(page["tracks"]) == 100
    assert page["tracks"][0]["video_id"] == "vid00000100"


def test_slicing_is_clamped_to_what_is_known(monkeypatch):
    client = FakeClient(total=30)
    use_client(monkeypatch, client)

    assert len(youtube.playlist_tracks("PL2", offset=0, limit=100)["tracks"]) == 30
    assert len(youtube.playlist_tracks("PL2", offset=25, limit=100)["tracks"]) == 5
    assert youtube.playlist_tracks("PL2", offset=30, limit=100)["tracks"] == []
    assert youtube.playlist_tracks("PL2", offset=999, limit=100)["tracks"] == []


def test_a_failure_is_not_cached(monkeypatch):
    client = FakeClient(total=10, fail=True)
    use_client(monkeypatch, client)

    first = youtube.playlist_tracks("PL3")
    assert first["error"]
    assert first["tracks"] == [] and first["total"] == 0

    # The next call must try again instead of replaying the error.
    youtube.playlist_tracks("PL3")
    assert len(client.calls) == 2


def test_without_a_session_nothing_is_requested(monkeypatch):
    client = FakeClient()
    use_client(monkeypatch, None)

    playlist = youtube.playlist_tracks("PLx")
    liked = youtube.liked_songs()
    for page in (playlist, liked):
        assert page["tracks"] == [] and page["total"] == 0 and page["error"]
    assert client.calls == []


def test_liked_songs_page_the_same_way(monkeypatch):
    client = FakeClient()
    use_client(monkeypatch, client)

    page = youtube.liked_songs(offset=0, limit=100)
    assert len(page["tracks"]) == 100
    assert client.calls[0] == ("liked", 100)
    assert client.all_fetched.wait(5)


def test_invalidate_playlists_drops_the_pages(monkeypatch):
    client = FakeClient(total=10)
    use_client(monkeypatch, client)

    youtube.playlist_tracks("PL4")
    youtube.invalidate_playlists()
    youtube.playlist_tracks("PL4")

    assert len([c for c in client.calls if c[0] == "PL4"]) == 2
