"""Parsing tests for the sectioned music search.

The fixtures are dictionaries in the shape ytmusicapi's own parsers produce
(see ytmusicapi/mixins/search.py docstring and parsers/search.py), so the tests
exercise our grouping/filtering without touching the network.
"""

from ytapi.youtube import (
    HIDDEN_VIDEO_TYPES,
    sections_from_results,
    search_music,
    suggestions,
)


def _thumb(name: str = "t") -> list[dict]:
    return [{"url": f"https://i.ytimg.com/{name}=s180", "width": 180, "height": 180}]


def song(video_id: str, title: str, category: str | None = "Songs", **extra):
    result = {
        "category": category,
        "resultType": "song",
        "videoId": video_id,
        "title": title,
        "artists": [{"name": "Oasis", "id": "UCtestArtistChannelCcccccc"}],
        "album": {"name": "(What's The Story) Morning Glory?", "id": "MPREb_9nqEki4ZDpp"},
        "duration": "4:19",
        "duration_seconds": 259,
        "videoType": "MUSIC_VIDEO_TYPE_ATV",
        "thumbnails": _thumb(video_id),
    }
    result.update(extra)
    return result


def video(video_id: str, title: str, video_type: str, category: str | None = "Videos"):
    return {
        "category": category,
        "resultType": "video",
        "videoId": video_id,
        "title": title,
        "artists": [{"name": "Oasis", "id": "UCtestArtistChannelCcccccc"}],
        "views": "386M",
        "duration": "4:38",
        "duration_seconds": 278,
        "videoType": video_type,
        "thumbnails": _thumb(video_id),
    }


def album(browse_id: str = "MPREb_IInSY5QXXrW"):
    return {
        "category": "Albums",
        "resultType": "album",
        "browseId": browse_id,
        "playlistId": "OLAK5uy_kunInnOpcKECWIBQGB0Qj6ZjquxDvfckg",
        "title": "(What's The Story) Morning Glory?",
        "type": "Album",
        "artist": "Oasis",
        "year": "1995",
        "isExplicit": False,
        "thumbnails": _thumb("album"),
    }


def artist(browse_id: str = "UCtestArtistChannelCcccccc"):
    return {
        "category": "Artists",
        "resultType": "artist",
        "browseId": browse_id,
        "artist": "Oasis",
        "subscribers": "12.5M",
        "shuffleId": "RDAOkjHYJjL1a3xspEyVkhHAsg",
        "radioId": "RDEMkjHYJjL1a3xspEyVkhHAsg",
        "thumbnails": _thumb("artist"),
    }


def playlist(browse_id: str, title: str, category: str | None = "Playlists"):
    return {
        "category": category,
        "resultType": "playlist",
        "browseId": browse_id,
        "title": title,
        "author": "Tate Henderson",
        "itemCount": 174,
        "thumbnails": _thumb("pl"),
    }


def top_result_song(video_id: str = "vU05Eksc_iM", title: str = "Wonderwall"):
    return {
        "category": "Top result",
        "resultType": "song",
        "videoId": video_id,
        "title": title,
        "artists": [{"name": "Oasis", "id": "UCtestArtistChannelCcccccc"}],
        "duration": "4:38",
        "duration_seconds": 278,
        "videoType": "MUSIC_VIDEO_TYPE_OMV",
        "thumbnails": _thumb(video_id),
    }


DEFAULT_RESULTS = [
    top_result_song(),
    song("ZrOKjDZOtkA", "Wonderwall"),
    song("bx1Bh8ZvH84", "Wonderwall (Remastered)"),
    video("ugc00000001", "Wonderwall live cover by fan", "MUSIC_VIDEO_TYPE_UGC"),
    video("omv00000001", "Wonderwall (Official Video)", "MUSIC_VIDEO_TYPE_OMV"),
    album(),
    artist(),
    playlist("VLPLK1PkWQlWtnNfovRdGWpKffO1Wdi2kvDx", "Wonderwall - Oasis"),
    playlist("VLPLcommunity0000000000000000", "Britpop classics", "Community playlists"),
    {
        "category": "Profiles",
        "resultType": "profile",
        "title": "Taylor Swift Time",
        "name": "@TaylorSwiftTime",
        "browseId": "UCSCRK7XlVQ6fBdEl00kX6pQ",
        "thumbnails": _thumb("p"),
    },
]


def test_default_search_sections_follow_ytm_order():
    sections = sections_from_results(DEFAULT_RESULTS)

    assert [s["title"] for s in sections] == [
        "Top result",
        "Songs",
        "Videos",
        "Albums",
        "Artists",
        "Playlists",
    ]
    assert [s["kind"] for s in sections] == [
        "song",
        "song",
        "video",
        "album",
        "artist",
        "playlist",
    ]
    # Profiles are not something the player can open.
    assert all(
        item["kind"] != "profile" for section in sections for item in section["items"]
    )


def test_ugc_videos_are_filtered_out_by_video_type():
    videos = next(s for s in sections_from_results(DEFAULT_RESULTS) if s["kind"] == "video")

    ids = [item["id"] for item in videos["items"]]
    assert ids == ["omv00000001"]
    assert "MUSIC_VIDEO_TYPE_UGC" in HIDDEN_VIDEO_TYPES


def test_official_video_types_are_kept():
    results = [
        video("omv1", "Official", "MUSIC_VIDEO_TYPE_OMV"),
        video("atv1", "Art track", "MUSIC_VIDEO_TYPE_ATV"),
        video("src1", "Official source", "MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC"),
    ]

    videos = sections_from_results(results)[0]

    assert [i["id"] for i in videos["items"]] == ["omv1", "atv1", "src1"]


def test_duplicate_ids_appear_once():
    results = [
        song("same0000001", "One"),
        song("same0000001", "One (duplicate from another shelf)", "Videos"),
        video("same0000001", "One", "MUSIC_VIDEO_TYPE_OMV"),
    ]

    items = [i for s in sections_from_results(results) for i in s["items"]]

    assert [i["id"] for i in items] == ["same0000001"]
    assert items[0]["title"] == "One"


def test_items_without_ids_are_dropped():
    results = [
        song("", "Broken song"),
        video("", "Broken video", "MUSIC_VIDEO_TYPE_OMV"),
        {**album(), "browseId": ""},
        {**artist(), "browseId": ""},
        {**playlist("VLPLx", "No id"), "browseId": "", "playlistId": ""},
        song("good0000001", "Fine"),
    ]

    items = [i for s in sections_from_results(results) for i in s["items"]]

    assert [i["id"] for i in items] == ["good0000001"]


def test_filtered_search_is_one_section():
    results = [song("a0000000001", "A", category=None), song("a0000000002", "B", category=None)]

    sections = sections_from_results(results, filter_kind="song")

    assert len(sections) == 1
    assert sections[0]["kind"] == "song"
    assert sections[0]["title"] == "Songs"
    assert [i["id"] for i in sections[0]["items"]] == ["a0000000001", "a0000000002"]


def test_filtered_search_drops_other_kinds():
    results = [song("a0000000001", "A", category=None), album(), artist()]

    sections = sections_from_results(results, filter_kind="song")

    assert [i["kind"] for i in sections[0]["items"]] == ["song"]


def test_playlist_categories_are_merged_into_one_section():
    sections = sections_from_results(DEFAULT_RESULTS)
    playlists = next(s for s in sections if s["kind"] == "playlist")

    assert len(playlists["items"]) == 2
    assert playlists["items"][0]["playlist_id"] == "PLK1PkWQlWtnNfovRdGWpKffO1Wdi2kvDx"
    assert playlists["items"][0]["subtitle"].startswith("Tate Henderson")


def test_section_items_are_capped_by_the_limit():
    results = [song(f"cap{i:08d}", f"Track {i}", category=None) for i in range(5)]

    sections = sections_from_results(results, limit=2)

    assert len(sections[0]["items"]) == 2


def test_artist_and_album_items_carry_navigation_ids():
    sections = {s["kind"]: s for s in sections_from_results(DEFAULT_RESULTS)}

    album_item = sections["album"]["items"][0]
    assert album_item["browse_id"] == "MPREb_IInSY5QXXrW"
    assert album_item["playlist_id"].startswith("OLAK5uy_")

    artist_item = sections["artist"]["items"][0]
    assert artist_item["browse_id"] == "UCtestArtistChannelCcccccc"
    assert artist_item["subtitle"] == "12.5M"


def test_unknown_filter_is_rejected_without_network():
    result = search_music("daft punk", "podcasts", 10)

    assert result["sections"] == []
    assert "unknown filter" in result["error"]


def test_empty_query_returns_nothing_without_network():
    assert search_music("   ", "songs", 10) == {"sections": [], "error": ""}


def test_suggestions_need_at_least_two_characters():
    assert suggestions("") == []
    assert suggestions(" a ") == []
