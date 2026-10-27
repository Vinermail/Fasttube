"""Stage 1: batched adds, duplicates, moves, metadata, ratings, copy, cache updates."""

import threading
import time

import ytapi.youtube as youtube

MIX = "RDTMAK5uy_abc"


class Client:
    def __init__(self, answers=None):
        self.calls = []
        self.answers = answers or {}

    def _answer(self, verb, *args):
        self.calls.append((verb, *args))
        value = self.answers.get(verb, "STATUS_SUCCEEDED")
        if isinstance(value, list):
            return value.pop(0) if value else "STATUS_SUCCEEDED"
        if isinstance(value, Exception):
            raise value
        return value() if callable(value) else value

    def add_playlist_items(self, playlist_id, videoIds=None, **_):
        ids = list(videoIds or [])
        answer = self.answers.get("add", "SUCCEEDED")
        if isinstance(answer, list):
            answer = answer.pop(0) if answer else "SUCCEEDED"
        self.calls.append(("add", playlist_id, ids))
        if isinstance(answer, Exception):
            raise answer
        if callable(answer):
            return answer(ids)
        return answer

    def remove_playlist_items(self, playlist_id, videos):
        return self._answer("remove", playlist_id, videos)

    def edit_playlist(self, **kwargs):
        return self._answer("edit", kwargs)

    def rate_song(self, video_id, rating):
        return self._answer("rate_song", video_id, rating)

    def rate_playlist(self, playlist_id, rating):
        return self._answer("rate_playlist", playlist_id, rating)

    def get_playlist(self, playlist_id, limit=None):
        return self._answer("get_playlist", playlist_id, limit)

    def create_playlist(self, title, description, privacy_status=None, video_ids=None, **_):
        return self._answer("create", title, privacy_status, list(video_ids or []))

    def create_playlist_src(self, **kwargs):
        return self.create_playlist(**kwargs)


def add_answer(ids, status="STATUS_SUCCEEDED"):
    """What YouTube really answers for a successful add."""
    return {
        "status": status,
        "playlistEditResults": [
            {"playlistEditVideoAddedResultData": {"videoId": v, "setVideoId": f"sv-{v}"}}
            for v in ids
        ],
    }


def use(monkeypatch, client, *, owned=True, playlist_id="PL1", rows=None):
    monkeypatch.setattr(youtube, "_session_client", lambda: client)
    monkeypatch.setattr(youtube, "_PLAYLISTS", {})
    if owned is not None:
        youtube._PLAYLISTS[f"playlist:{playlist_id}"] = {
            "tracks": rows if rows is not None else [],
            "total": len(rows or []),
            "owned": owned,
            "complete": True,
            "at": time.time(),
            "loading": False,
            "done": threading.Event(),
        }


def row(video_id, set_video_id):
    return {
        "video_id": video_id,
        "title": video_id,
        "artist": "",
        "album": "",
        "duration_secs": 0.0,
        "thumbnail": "",
        "set_video_id": set_video_id,
        "like_status": "",
    }


# ---------------------------------------------------------------------- add ---
def test_add_reports_the_created_rows(monkeypatch):
    client = Client()
    client.answers["add"] = add_answer(["v1", "v2"])
    use(monkeypatch, client)

    result = youtube.add_playlist_items("PL1", ["v1", "v2"])

    assert result["ok"]
    assert result["items"] == [
        {"video_id": "v1", "set_video_id": "sv-v1"},
        {"video_id": "v2", "set_video_id": "sv-v2"},
    ]
    # And the cached page grew without a refetch.
    entry = youtube._PLAYLISTS["playlist:PL1"]
    assert entry["total"] == 2
    assert [t["set_video_id"] for t in entry["tracks"]] == ["sv-v1", "sv-v2"]


def test_a_known_duplicate_is_reported_without_a_request(monkeypatch):
    client = Client()
    use(monkeypatch, client, rows=[row("v1", "sv-v1")])

    result = youtube.add_playlist_items("PL1", ["v1", "v2"])

    assert not result["ok"]
    assert result["code"] == "DUPLICATE"
    assert result["video_ids"] == ["v1"]
    assert client.calls == [], "nothing may reach YouTube"


def test_duplicates_are_allowed_when_asked(monkeypatch):
    client = Client()
    client.answers["add"] = add_answer(["v1"])
    use(monkeypatch, client, rows=[row("v1", "sv-old")])

    result = youtube.add_playlist_items("PL1", ["v1"], allow_duplicates=True)

    assert result["ok"] and client.calls


def test_large_adds_go_in_batches_with_a_pause(monkeypatch):
    client = Client()
    client.answers["add"] = lambda ids: add_answer(ids)
    use(monkeypatch, client)

    pauses = []
    monkeypatch.setattr(youtube.time, "sleep", lambda s: pauses.append(s))

    result = youtube.add_playlist_items("PL1", [f"v{i}" for i in range(120)])

    assert result["ok"]
    sizes = [len(c[2]) for c in client.calls]
    assert sizes == [50, 50, 20], sizes
    assert len(result["items"]) == 120
    assert pauses == [youtube.ADD_PAUSE, youtube.ADD_PAUSE]


def test_a_failed_batch_keeps_the_successful_one(monkeypatch):
    client = Client()
    answers = [add_answer([f"v{i}" for i in range(50)]), {"status": "STATUS_FAILED"}]
    client.answers["add"] = answers
    use(monkeypatch, client)
    monkeypatch.setattr(youtube.time, "sleep", lambda s: None)

    result = youtube.add_playlist_items("PL1", [f"v{i}" for i in range(70)])

    assert not result["ok"]
    assert result["code"] == "PARTIAL"
    assert len(result["items"]) == 50
    assert len(result["video_ids"]) == 20


def test_a_failed_status_is_an_error(monkeypatch):
    client = Client()
    client.answers["add"] = {"status": "STATUS_FAILED"}
    use(monkeypatch, client)

    assert not youtube.add_playlist_items("PL1", ["v1"])["ok"]


def test_a_full_response_without_status_is_an_error(monkeypatch):
    client = Client()
    client.answers["add"] = {"responseContext": {"visitorData": "x"}}
    use(monkeypatch, client)

    assert not youtube.add_playlist_items("PL1", ["v1"])["ok"]


def test_an_exception_is_an_error(monkeypatch):
    client = Client()
    client.answers["add"] = RuntimeError("boom")
    use(monkeypatch, client)

    result = youtube.add_playlist_items("PL1", ["v1"])

    assert not result["ok"] and "boom" in result["error"]


# --------------------------------------------------------------------- move ---
def test_moving_to_the_end_sends_a_plain_id(monkeypatch):
    client = Client()
    use(monkeypatch, client, rows=[row("a", "sa"), row("b", "sb"), row("c", "sc")])

    assert youtube.move_playlist_item("PL1", "sa")["ok"]

    kwargs = client.calls[0][1]
    assert kwargs["moveItem"] == "sa"
    # The cached page follows the move.
    assert [t["set_video_id"] for t in youtube._PLAYLISTS["playlist:PL1"]["tracks"]] == [
        "sb",
        "sc",
        "sa",
    ]


def test_moving_before_a_row_sends_a_pair(monkeypatch):
    client = Client()
    use(monkeypatch, client, rows=[row("a", "sa"), row("b", "sb"), row("c", "sc")])

    assert youtube.move_playlist_item("PL1", "sc", "sa")["ok"]

    assert client.calls[0][1]["moveItem"] == ("sc", "sa")
    assert [t["set_video_id"] for t in youtube._PLAYLISTS["playlist:PL1"]["tracks"]] == [
        "sc",
        "sa",
        "sb",
    ]


def test_a_refused_move_is_reported(monkeypatch):
    client = Client({"edit": {"status": "STATUS_FAILED"}})
    use(monkeypatch, client, rows=[row("a", "sa")])

    assert not youtube.move_playlist_item("PL1", "sa")["ok"]


# --------------------------------------------------------------------- meta ---
def test_meta_edits_pass_what_was_asked(monkeypatch):
    client = Client()
    use(monkeypatch, client)

    assert youtube.edit_playlist_meta("PL1", title="Новое", privacy="UNLISTED")["ok"]

    kwargs = client.calls[0][1]
    assert kwargs["title"] == "Новое" and kwargs["privacyStatus"] == "UNLISTED"
    assert "description" not in kwargs


# -------------------------------------------------------------------- songs ---
def test_rating_a_song_accepts_a_response_without_a_status(monkeypatch):
    client = Client({"rate_song": {"responseContext": {}, "trackingParams": "x"}})
    use(monkeypatch, client)

    assert youtube.rate_song("dQw4w9WgXcQ", "LIKE")["ok"]
    assert client.calls[0][0] == "rate_song"
    assert str(client.calls[0][2]).endswith("LIKE")


def test_a_failed_rating_is_an_error(monkeypatch):
    client = Client({"rate_song": {"status": "STATUS_FAILED"}})
    use(monkeypatch, client)

    assert not youtube.rate_song("dQw4w9WgXcQ", "LIKE")["ok"]


def test_an_unknown_rating_makes_no_request(monkeypatch):
    client = Client()
    use(monkeypatch, client)

    assert not youtube.rate_song("dQw4w9WgXcQ", "HATE")["ok"]
    assert client.calls == []


# ------------------------------------------------------------- library/copy ---
def test_saving_to_the_library_uses_a_like(monkeypatch):
    client = Client({"rate_playlist": {"responseContext": {}}})
    use(monkeypatch, client)

    assert youtube.save_playlist_to_library("PLx")["ok"]
    assert client.calls[0] == ("rate_playlist", "PLx", "LIKE")


def test_copy_reads_the_source_and_creates_a_private_playlist(monkeypatch):
    client = Client()
    client.answers["get_playlist"] = {"tracks": [{"videoId": "a"}, {"videoId": "b"}], "title": "Src"}
    client.answers["create"] = "PLcopy"
    use(monkeypatch, client)

    result = youtube.copy_playlist("PLsrc")

    assert result["ok"] and result["id"] == "PLcopy"
    create = [c for c in client.calls if c[0] == "create"][0]
    assert create[1] == "Src (копия)"
    assert create[2] == "PRIVATE"
    assert create[3] == ["a", "b"]


def test_a_mix_cannot_be_copied(monkeypatch):
    client = Client()
    use(monkeypatch, client)

    assert not youtube.copy_playlist(MIX)["ok"]
    assert client.calls == []


# ------------------------------------------------------------------- resolve ---
def test_a_playlist_url_becomes_a_page(monkeypatch):
    client = Client()
    client.answers["get_playlist"] = {
        "tracks": [{"videoId": "a", "title": "A"}],
        "trackCount": 1,
        "owned": True,
    }
    use(monkeypatch, client)
    monkeypatch.setattr(youtube, "_PLAYLISTS", {})

    page = youtube.resolve_playlist_url("https://music.youtube.com/playlist?list=PLabc&si=x")

    assert page["id"] == "PLabc"
    assert page["owned"] and page["total"] == 1


def test_a_url_without_the_list_parameter_is_an_error(monkeypatch):
    client = Client()
    use(monkeypatch, client)

    result = youtube.resolve_playlist_url("https://music.youtube.com/watch?v=x")

    assert result["error"] and result["total"] == 0
    assert client.calls == []


# ---------------------------------------------------------------- ownership ---
def test_every_edit_refuses_a_foreign_playlist(monkeypatch):
    client = Client()
    use(monkeypatch, client, owned=False)

    results = [
        youtube.add_playlist_items("PL1", ["v1"]),
        youtube.move_playlist_item("PL1", "sv1"),
        youtube.edit_playlist_meta("PL1", title="x"),
    ]

    for result in results:
        assert not result["ok"] and result["code"] == "NOT_OWNED"
    assert client.calls == []


# -------------------------------------------------------------- cache: remove ---
def test_removing_a_track_updates_the_cached_page(monkeypatch):
    client = Client()
    use(monkeypatch, client, rows=[row("a", "sa"), row("b", "sb")])

    assert youtube.remove_playlist_items("PL1", [{"video_id": "a", "set_video_id": "sa"}])["ok"]

    entry = youtube._PLAYLISTS["playlist:PL1"]
    assert [t["set_video_id"] for t in entry["tracks"]] == ["sb"]
    assert entry["total"] == 1
