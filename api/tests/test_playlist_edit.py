"""Playlist edits: ytmusicapi reports refusals as data, not exceptions."""

import threading
import time

import ytapi.youtube as youtube

MIX = "RDTMAK5uy_abc"


class Client:
    """Records calls; each verb returns whatever `answers` says."""

    def __init__(self, answers=None):
        self.calls = []
        self.answers = answers or {}

    def _answer(self, verb, *args):
        self.calls.append((verb, *args))
        value = self.answers.get(verb, "STATUS_SUCCEEDED")
        if isinstance(value, Exception):
            raise value
        return value() if callable(value) else value

    def add_playlist_items(self, playlist_id, videoIds=None, **_):
        return self._answer("add", playlist_id, list(videoIds or []))

    def remove_playlist_items(self, playlist_id, videos):
        return self._answer("remove", playlist_id, videos)

    def create_playlist(self, title, description, privacy_status=None, **_):
        return self._answer("create", title, privacy_status)

    def delete_playlist(self, playlist_id):
        return self._answer("delete", playlist_id)

    def rate_playlist(self, playlist_id, rating):
        return self._answer("rate", playlist_id, rating)

    def get_playlist(self, playlist_id, limit=None, **_):
        return self._answer("read", playlist_id)


def use(monkeypatch, client, *, owned: bool | None = True, playlist_id="PL1"):
    monkeypatch.setattr(youtube, "_session_client", lambda: client)
    monkeypatch.setattr(youtube, "_PLAYLISTS", {})
    if owned is not None:
        youtube._PLAYLISTS[f"playlist:{playlist_id}"] = {
            "tracks": [],
            "total": 0,
            "owned": owned,
            "complete": True,
            "at": time.time(),
            "loading": False,
            "done": threading.Event(),
        }


# ------------------------------------------------------------------ success ---
def test_a_succeeded_status_string_counts_as_done(monkeypatch):
    client = Client({"remove": "STATUS_SUCCEEDED"})
    use(monkeypatch, client)

    result = youtube.remove_playlist_items(
        "PL1", [{"video_id": "v1", "set_video_id": "s1"}]
    )

    assert result["ok"] and client.calls[0][0] == "remove"


def test_a_succeeded_status_inside_a_dict_counts_as_done(monkeypatch):
    client = Client({"add": {"status": "STATUS_SUCCEEDED", "playlistEditResults": []}})
    use(monkeypatch, client)

    assert youtube.add_playlist_items("PL1", ["v1"])["ok"]


# ------------------------------------------------------------------ refusals ---
def test_a_failed_status_is_not_success(monkeypatch):
    client = Client({"remove": {"status": "STATUS_FAILED"}})
    use(monkeypatch, client)

    result = youtube.remove_playlist_items(
        "PL1", [{"video_id": "v1", "set_video_id": "s1"}]
    )

    assert not result["ok"]
    assert "STATUS_FAILED" in result["error"]


def test_a_failed_status_from_a_plain_string_is_not_success(monkeypatch):
    client = Client({"delete": "STATUS_FAILED"})
    use(monkeypatch, client)

    result = youtube.delete_account_playlist("PL1")

    assert not result["ok"]
    assert client.calls[0][0] == "delete"


def test_a_full_response_without_a_status_is_not_success(monkeypatch):
    # YouTube answered with something else entirely: no SUCCEEDED anywhere.
    client = Client({"add": {"responseContext": {"visitorData": "x"}}})
    use(monkeypatch, client)

    result = youtube.add_playlist_items("PL1", ["v1"])

    assert not result["ok"]
    assert result["error"]


def test_an_exception_is_reported_as_an_error(monkeypatch):
    client = Client({"add": RuntimeError("boom")})
    use(monkeypatch, client)

    result = youtube.add_playlist_items("PL1", ["v1"])

    assert not result["ok"] and "boom" in result["error"]


def test_a_duplicate_is_named_as_such(monkeypatch):
    client = Client(
        {"add": {"status": "STATUS_FAILED", "message": "DUPLICATE_VIDEO_IDS"}}
    )
    use(monkeypatch, client)

    result = youtube.add_playlist_items("PL1", ["v1"])

    assert not result["ok"]
    assert result["error"] == "уже есть в плейлисте"


# ---------------------------------------------------------------- ownership ---
def test_a_saved_foreign_playlist_is_refused_before_any_request(monkeypatch):
    client = Client()
    use(monkeypatch, client, owned=False)

    for call in (
        lambda: youtube.add_playlist_items("PL1", ["v1"]),
        lambda: youtube.remove_playlist_items(
            "PL1", [{"video_id": "v1", "set_video_id": "s1"}]
        ),
        lambda: youtube.delete_account_playlist("PL1"),
    ):
        result = call()
        assert not result["ok"]
        assert "чужой" in result["error"]

    assert client.calls == [], "nothing may reach YouTube"


def test_a_mix_is_refused(monkeypatch):
    client = Client()
    use(monkeypatch, client, owned=None)

    result = youtube.add_playlist_items(MIX, ["v1"])

    assert not result["ok"] and "микс" in result["error"]
    assert client.calls == []


# ------------------------------------------------------------------ library ---
def test_removing_from_the_library_uses_rate_playlist(monkeypatch):
    client = Client({"rate": "STATUS_SUCCEEDED"})
    use(monkeypatch, client)

    assert youtube.remove_from_library("PL1")["ok"]
    assert client.calls == [("rate", "PL1", "INDIFFERENT")]


def test_a_refused_library_removal_is_not_success(monkeypatch):
    client = Client({"rate": {"status": "STATUS_FAILED"}})
    use(monkeypatch, client)

    assert not youtube.remove_from_library("PL1")["ok"]


# ------------------------------------------------------------------- create ---
def test_create_returns_the_new_id(monkeypatch):
    client = Client({"create": "PLnew"})
    use(monkeypatch, client)

    result = youtube.create_account_playlist("Тест")

    assert result["ok"] and result["id"] == "PLnew"


def test_create_with_a_failed_dict_returns_no_id(monkeypatch):
    client = Client({"create": {"status": "STATUS_FAILED"}})
    use(monkeypatch, client)

    result = youtube.create_account_playlist("Тест")

    assert not result["ok"] and result["id"] == ""


# --------------------------------------------------------------- generations ---
def test_a_background_load_started_before_a_reset_cannot_store(monkeypatch):
    class SlowClient(Client):
        def get_playlist(self, playlist_id, limit=None):
            if limit is None:
                time.sleep(0.2)
                return {
                    "tracks": [{"videoId": f"v{i}", "title": f"T{i}"} for i in range(150)],
                    "trackCount": 150,
                    "owned": True,
                }
            return {
                "tracks": [{"videoId": f"v{i}", "title": f"T{i}"} for i in range(100)],
                "trackCount": 150,
                "owned": True,
            }

    client = SlowClient()
    monkeypatch.setattr(youtube, "_session_client", lambda: client)
    monkeypatch.setattr(youtube, "_PLAYLISTS", {})

    youtube.playlist_tracks("PLgen", offset=0, limit=100)
    # The account changes while the second page is still in flight.
    youtube.invalidate_playlists()
    time.sleep(0.45)

    assert youtube._PLAYLISTS == {}, "the stale load must not resurrect the cache"


def test_a_succeeded_add_without_ids_reads_the_rows_back(monkeypatch):
    # YouTube sometimes says SUCCEEDED without naming the new rows.
    client = Client(
        {
            "add": {"status": "STATUS_SUCCEEDED", "playlistEditResults": []},
            # The shape ytmusicapi's own parser produces for a browse page.
            "read": {
                "trackCount": 1,
                "tracks": [
                    {
                        "videoId": "v1",
                        "title": "One",
                        "artists": [{"name": "Somebody"}],
                        "duration_seconds": 200,
                        "setVideoId": "sv1",
                    }
                ],
            },
        }
    )
    use(monkeypatch, client)

    result = youtube.add_playlist_items("PL1", ["v1"])

    assert result["ok"] is True
    assert result["items"] == [{"video_id": "v1", "set_video_id": "sv1"}]
    assert ("read", "PL1") in client.calls


def test_a_read_back_that_lags_behind_keeps_the_add(monkeypatch):
    # YouTube says SUCCEEDED; a read right after may not show the row yet.
    client = Client(
        {
            "add": {"status": "STATUS_SUCCEEDED", "playlistEditResults": []},
            "read": {"trackCount": 0, "tracks": []},
        }
    )
    use(monkeypatch, client)
    monkeypatch.setattr(youtube.time, "sleep", lambda _s: None)

    result = youtube.add_playlist_items("PL1", ["v1"])

    assert result["ok"] is True, "the status is YouTube's own word"
    assert result["items"] == []
