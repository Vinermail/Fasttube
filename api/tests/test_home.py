"""Home shelves: parsing, explore fallback and the 15-minute cache."""

import pytest

from ytapi import youtube


@pytest.fixture(autouse=True)
def _fresh_caches():
    youtube.invalidate_account_caches()
    yield
    youtube.invalidate_account_caches()


def thumb(name="t"):
    return [{"url": f"https://i.ytimg.com/{name}=s200", "width": 200, "height": 200}]


def home_song(video_id="vid00000001", title="Track", video_type="MUSIC_VIDEO_TYPE_ATV"):
    return {
        "title": title,
        "videoId": video_id,
        "videoType": video_type,
        "artists": [{"name": "Artist", "id": "UCx"}],
        "album": {"name": "Album", "id": "MPREb_x"},
        "thumbnails": thumb(video_id),
    }


def home_album(browse_id="MPREb_album", title="Record"):
    return {
        "title": title,
        "browseId": browse_id,
        "artists": [{"name": "Artist"}],
        "year": "2024",
        "type": "Album",
        "thumbnails": thumb("album"),
    }


def home_artist(browse_id="UCartist", title="Artist"):
    return {"title": title, "browseId": browse_id, "subscribers": "1.2M", "thumbnails": thumb("a")}


def home_playlist(playlist_id="PLlist", title="Playlist"):
    return {
        "title": title,
        "playlistId": playlist_id,
        "description": "someone • 42 songs",
        "thumbnails": thumb("pl"),
    }


class FakeClient:
    def __init__(self, home=None, explore=None, home_error=None, explore_error=None):
        self._home = home or []
        self._explore = explore or {}
        self._home_error = home_error
        self._explore_error = explore_error
        self.home_calls = 0
        self.explore_calls = 0

    def get_home(self, limit=4):
        self.home_calls += 1
        if self._home_error:
            raise RuntimeError(self._home_error)
        return self._home

    def get_explore(self):
        self.explore_calls += 1
        if self._explore_error:
            raise RuntimeError(self._explore_error)
        return self._explore


def test_shelves_map_every_supported_item_kind():
    raw = [
        {
            "title": "Quick picks",
            "contents": [home_song(), home_song("vid00000002")],
        },
        {"title": "Records", "contents": [home_album()]},
        {"title": "Artists you like", "contents": [home_artist()]},
        {"title": "Playlists", "contents": [home_playlist()]},
    ]

    shelves = youtube.shelves_from_home(raw)

    assert [s["title"] for s in shelves] == [
        "Quick picks",
        "Records",
        "Artists you like",
        "Playlists",
    ]
    assert [s["kind"] for s in shelves] == ["personal"] * 4
    assert shelves[0]["items"][0]["kind"] == "song"
    assert shelves[0]["items"][0]["subtitle"] == "Artist"
    assert shelves[1]["items"][0]["kind"] == "album"
    assert shelves[1]["items"][0]["browse_id"] == "MPREb_album"
    assert "2024" in shelves[1]["items"][0]["subtitle"]
    assert shelves[2]["items"][0]["kind"] == "artist"
    assert shelves[2]["items"][0]["subtitle"] == "1.2M"
    assert shelves[3]["items"][0]["kind"] == "playlist"
    assert shelves[3]["items"][0]["playlist_id"] == "PLlist"


def test_junk_items_and_empty_shelves_are_dropped():
    raw = [
        {
            "title": "Mixed",
            "contents": [
                home_song(),
                {"title": "Podcast", "videoType": "MUSIC_VIDEO_TYPE_PODCAST_EPISODE"},
                {"title": "Nothing useful"},
                home_song("vid00000001"),  # duplicate id
            ],
        },
        {"title": "Empty shelf", "contents": []},
        {"title": "Only junk", "contents": [{"title": "x"}]},
        {"title": "", "contents": [home_song("vid00000003")]},
    ]

    shelves = youtube.shelves_from_home(raw)

    assert len(shelves) == 1
    assert shelves[0]["title"] == "Mixed"
    assert [i["id"] for i in shelves[0]["items"]] == ["vid00000001"]


def test_shelf_items_are_capped():
    raw = [{"title": "Big", "contents": [home_song(f"vid{i:08d}") for i in range(30)]}]

    shelves = youtube.shelves_from_home(raw, limit=5)

    assert len(shelves[0]["items"]) == 5


def test_ugc_videos_are_skipped_on_the_home_page():
    raw = [
        {
            "title": "Videos",
            "contents": [
                home_song("ugc00000001", "Fan upload", "MUSIC_VIDEO_TYPE_UGC"),
                home_song("omv00000001", "Official", "MUSIC_VIDEO_TYPE_OMV"),
            ],
        }
    ]

    items = youtube.shelves_from_home(raw)[0]["items"]

    assert [i["id"] for i in items] == ["omv00000001"]
    assert items[0]["kind"] == "video"


def test_explore_builds_public_shelves():
    explore = {
        "new_releases": [home_album()],
        "trending": {"playlist": "VLPLx", "items": [home_song()]},
        "new_videos": [home_song("omv00000001", "Clip", "MUSIC_VIDEO_TYPE_OMV")],
        "moods_and_genres": [{"title": "Chill", "params": "abc"}],
    }

    shelves = youtube.shelves_from_explore(explore)

    assert [s["title"] for s in shelves] == ["Новые релизы", "В тренде", "Новые клипы"]
    assert all(s["kind"] == "explore" for s in shelves)
    assert shelves[2]["items"][0]["kind"] == "video"


def test_explore_without_usable_shelves_is_empty():
    assert youtube.shelves_from_explore({}) == []
    assert youtube.shelves_from_explore({"moods_and_genres": [{"title": "x", "params": "y"}]}) == []


def test_signed_out_uses_explore(monkeypatch):
    monkeypatch.setattr(youtube, "has_auth", lambda: False)
    client = FakeClient(
        home=[{"title": "Personal", "contents": [home_song()]}],
        explore={"new_releases": [home_album()]},
    )

    result = youtube.get_home(4, client=client)

    assert result["error"] == ""
    assert [s["kind"] for s in result["shelves"]] == ["explore"]


def test_signed_in_home_is_personal(monkeypatch):
    monkeypatch.setattr(youtube, "has_auth", lambda: True)
    client = FakeClient(home=[{"title": "Quick picks", "contents": [home_song()]}])

    result = youtube.get_home(4, client=client)

    assert [s["kind"] for s in result["shelves"]] == ["personal"]
    assert "Quick picks" == result["shelves"][0]["title"]


def test_empty_personal_page_falls_back_to_explore(monkeypatch):
    monkeypatch.setattr(youtube, "has_auth", lambda: True)
    client = FakeClient(home=[], explore={"new_releases": [home_album()]})

    result = youtube.get_home(4, client=client)

    assert [s["kind"] for s in result["shelves"]] == ["explore"]


def test_home_is_cached_for_the_mode_and_invalidated(monkeypatch):
    monkeypatch.setattr(youtube, "has_auth", lambda: True)
    client = FakeClient(home=[{"title": "Quick picks", "contents": [home_song()]}])

    youtube.get_home(4, client=client)
    youtube.get_home(4, client=client)
    assert client.home_calls == 1, "second call must come from the cache"

    youtube.invalidate_account_caches()
    youtube.get_home(4, client=client)
    assert client.home_calls == 2, "a session change must drop the cache"


def test_errors_are_not_cached(monkeypatch):
    monkeypatch.setattr(youtube, "has_auth", lambda: False)
    broken = FakeClient(home_error="boom", explore_error="boom")

    first = youtube.get_home(4, client=broken)
    assert first["shelves"] == []
    assert "boom" in first["error"]

    working = FakeClient(home=[], explore={"new_releases": [home_album()]})
    second = youtube.get_home(4, client=working)

    assert second["error"] == ""
    assert second["shelves"], "the failure must not have been cached"


def test_home_cache_is_per_mode(monkeypatch):
    anon_client = FakeClient(home=[], explore={"new_releases": [home_album()]})
    monkeypatch.setattr(youtube, "has_auth", lambda: False)
    youtube.get_home(4, client=anon_client)

    personal_client = FakeClient(home=[{"title": "Quick picks", "contents": [home_song()]}])
    monkeypatch.setattr(youtube, "has_auth", lambda: True)
    result = youtube.get_home(4, client=personal_client)

    assert [s["kind"] for s in result["shelves"]] == ["personal"]
