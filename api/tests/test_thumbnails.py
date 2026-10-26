"""pick_thumbnail: right size per context, sensible fallbacks."""

from ytapi.youtube import (
    THUMB_CARD,
    THUMB_PLAYER,
    THUMB_ROW,
    _item_from_result,
    _song_item,
    pick_thumbnail,
)

YOUTUBE_SIZES = [
    {"url": "https://i.ytimg.com/x=s60", "width": 60, "height": 60},
    {"url": "https://i.ytimg.com/x=s120", "width": 120, "height": 120},
    {"url": "https://i.ytimg.com/x=s320", "width": 320, "height": 320},
    {"url": "https://i.ytimg.com/x=s640", "width": 640, "height": 640},
]


def test_picks_the_smallest_size_that_is_big_enough():
    assert pick_thumbnail(YOUTUBE_SIZES, THUMB_ROW).endswith("s120")
    assert pick_thumbnail(YOUTUBE_SIZES, THUMB_CARD).endswith("s320")
    assert pick_thumbnail(YOUTUBE_SIZES, 100).endswith("s120")
    assert pick_thumbnail(YOUTUBE_SIZES, 61).endswith("s120")


def test_falls_back_to_the_biggest_when_nothing_is_big_enough():
    assert pick_thumbnail(YOUTUBE_SIZES, THUMB_PLAYER).endswith("s640")
    assert pick_thumbnail(YOUTUBE_SIZES, 5000).endswith("s640")


def test_empty_and_broken_lists():
    assert pick_thumbnail([], THUMB_ROW) == ""
    assert pick_thumbnail(None, THUMB_ROW) == ""
    assert pick_thumbnail([{"url": ""}, {}], THUMB_ROW) == ""


def test_entries_without_dimensions_keep_the_last_one():
    # Nothing to compare: behave like the old code (YouTube lists grow).
    assert pick_thumbnail([{"url": "first"}, {"url": "last"}], THUMB_ROW) == "last"


def test_width_and_height_are_both_considered():
    thumbs = [
        {"url": "tall", "width": 90, "height": 200},
        {"url": "wide", "width": 400, "height": 100},
    ]
    assert pick_thumbnail(thumbs, 150) == "tall"


def song(video_id="vid00000001"):
    return {
        "category": "Songs",
        "resultType": "song",
        "videoId": video_id,
        "title": "Track",
        "artists": [{"name": "Artist"}],
        "duration_seconds": 200,
        "videoType": "MUSIC_VIDEO_TYPE_ATV",
        "thumbnails": YOUTUBE_SIZES,
    }


def album_entry():
    return {
        "category": "Albums",
        "resultType": "album",
        "browseId": "MPREb_x",
        "title": "Record",
        "artist": "Artist",
        "year": "2024",
        "thumbnails": YOUTUBE_SIZES,
    }


def test_search_rows_and_cards_use_different_sizes():
    song_item = _item_from_result(song())
    album_item = _item_from_result(album_entry())

    assert song_item["thumbnail"].endswith("s120"), "song rows are 40 px"
    assert album_item["thumbnail"].endswith("s320"), "album cards are ~150 px"


def test_album_track_rows_use_the_row_size():
    item = _song_item(song("vid00000002"), fallback_thumbnail="cover")

    assert item["thumbnail"].endswith("s120")


def test_album_track_rows_fall_back_to_the_album_cover():
    bare = song("vid00000003")
    bare["thumbnails"] = []

    assert _song_item(bare, fallback_thumbnail="cover")["thumbnail"] == "cover"
