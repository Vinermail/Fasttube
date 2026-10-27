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
