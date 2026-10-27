"""set_proxy: yt-dlp options, requests env and the resolve snapshot follow."""

import os

import ytapi.youtube as youtube

URL = "socks5h://127.0.0.1:10808"


def test_set_proxy_reaches_every_client(monkeypatch):
    before = dict(os.environ)
    try:
        result = youtube.set_proxy(URL)
        assert result["ok"]

        assert youtube.YT_DLP_OPTS["proxy"] == URL
        # The resolve path uses its own copy, built once at import: it has to
        # follow too, otherwise new tracks would bypass the proxy.
        assert youtube._resolve_opts()["proxy"] == URL
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            assert os.environ[name] == URL
    finally:
        youtube.set_proxy("none")
        for key in set(os.environ) - set(before):
            if key.upper().endswith("_PROXY"):
                os.environ.pop(key, None)


def test_switching_the_proxy_off_clears_it(monkeypatch):
    youtube.set_proxy(URL)
    youtube.set_proxy("none")

    assert "proxy" not in youtube.YT_DLP_OPTS
    assert "proxy" not in youtube._resolve_opts()
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        assert name not in os.environ


def test_an_empty_url_also_means_no_proxy():
    youtube.set_proxy(URL)
    youtube.set_proxy("")

    assert "proxy" not in youtube.YT_DLP_OPTS


def test_http_proxies_pass_through_untouched():
    youtube.set_proxy("http://192.168.1.10:8080")
    try:
        assert youtube.YT_DLP_OPTS["proxy"] == "http://192.168.1.10:8080"
    finally:
        youtube.set_proxy("none")


def test_account_playlist_edits_go_through_the_client(monkeypatch):
    import ytapi.youtube as yt

    class Client:
        def __init__(self):
            self.calls = []

        def add_playlist_items(self, playlist_id, videoIds=None):
            self.calls.append(("add", playlist_id, list(videoIds or [])))

        def remove_playlist_items(self, playlist_id, videos):
            self.calls.append(("remove", playlist_id, videos))

        def create_playlist(self, title, description, privacy_status=None):
            self.calls.append(("create", title, privacy_status))
            return "PLnew"

        def delete_playlist(self, playlist_id):
            self.calls.append(("delete", playlist_id))

    client = Client()
    monkeypatch.setattr(yt, "_session_client", lambda: client)

    assert yt.add_playlist_items("PL1", ["v1", "v2"])["ok"]
    assert client.calls[-1] == ("add", "PL1", ["v1", "v2"])

    # A row without setVideoId cannot be removed: the same video may be in the
    # list twice, and YouTube needs the per-item id.
    assert not yt.remove_playlist_items("PL1", [{"video_id": "v1", "set_video_id": ""}])["ok"]
    assert yt.remove_playlist_items("PL1", [{"video_id": "v1", "set_video_id": "abc"}])["ok"]
    assert client.calls[-1] == ("remove", "PL1", [{"videoId": "v1", "setVideoId": "abc"}])

    assert yt.create_account_playlist("Тест")["id"] == "PLnew"
    assert client.calls[-1] == ("create", "Тест", "PRIVATE")

    assert yt.delete_account_playlist("PLnew")["ok"]
    assert client.calls[-1] == ("delete", "PLnew")
